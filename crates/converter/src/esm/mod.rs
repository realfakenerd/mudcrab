use crate::esm::{
    binary::parse_plugin_file,
    exporter::{create_tables, export_to_db_with_load_order},
    records::RawRecord,
};
use color_eyre::{Result, eyre::WrapErr};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};
pub mod binary;
pub mod cell_cache;
pub mod exporter;
pub mod extractors;
pub mod load_order;
pub mod mmap_reader;
pub mod records;
pub mod types;

pub struct EsmParser;

impl EsmParser {
    /// Parses .esm files and exports world data to skyrim_world.db
    pub fn convert_plugins(plugin_paths: &[PathBuf], db_path: &Path) -> Result<()> {
        Self::convert_plugins_with_records(plugin_paths, db_path).map(|_| ())
    }

    /// Export and return the same merged records used for the database, so the
    /// terrain cache cannot observe a second merge with a different load order.
    pub(crate) fn convert_plugins_with_records(
        plugin_paths: &[PathBuf],
        db_path: &Path,
    ) -> Result<HashMap<u32, RawRecord>> {
        let order = load_order::LoadOrder::read(plugin_paths)?;
        let master = Self::merge_plugins_with_load_order(plugin_paths, &order)?;
        let conn = Connection::open(db_path)?;
        create_tables(&conn)?;
        for (priority, path) in plugin_paths.iter().enumerate() {
            let checksum = Sha256::digest(std::fs::read(path)?);
            conn.execute(
                "INSERT OR REPLACE INTO plugins (id, name, priority, checksum) VALUES (?1, ?2, ?3, ?4)",
                params![priority as i64, path.file_name().unwrap_or_default().to_string_lossy(), priority as i64, checksum.as_slice()],
            )?;
        }
        export_to_db_with_load_order(&conn, &master, &order)?;

        Ok(master)
    }

    /// Merge plugin records with validated slots and EditorID-based game settings.
    pub fn merge_plugins(plugin_paths: &[PathBuf]) -> Result<HashMap<u32, RawRecord>> {
        let order = load_order::LoadOrder::read(plugin_paths)?;
        Self::merge_plugins_with_load_order(plugin_paths, &order)
    }

    /// Use one load-order mapping for both merging and ownership export.
    fn merge_plugins_with_load_order(
        plugin_paths: &[PathBuf],
        order: &load_order::LoadOrder,
    ) -> Result<HashMap<u32, RawRecord>> {
        let mut merged = HashMap::new();
        // Unlike ordinary forms, game settings override by EditorID. Retain
        // the first definition's key/owner while taking the last setting value.
        // Keep keys through deletions so a later restoration has the same ID.
        let mut game_settings = GameSettingIdentities::default();
        let warnings = RemapWarnings::default();
        for (priority, path) in plugin_paths.iter().enumerate() {
            let masters = &order.metadata[priority].masters;
            // Final ID -> (source ID, whether its master index was out of range). Two different
            // source IDs that resolve to one final ID would silently replace each other.
            let mut sources: HashMap<u32, (u32, bool)> = HashMap::new();
            for mut record in parse_plugin_file(path)? {
                record.load_order = priority as u32;
                let source_id = record.form_id;
                let out_of_range = (source_id >> 24) as usize > masters.len();
                remap_record_form_ids(
                    &mut record,
                    &order.names[priority],
                    masters,
                    &order.normal,
                    &order.light,
                    &warnings,
                )
                .wrap_err_with(|| {
                    format!(
                        "{} record {} source {source_id:08X} (load-order {:08X})",
                        order.names[priority],
                        String::from_utf8_lossy(&record.record_type),
                        record.form_id
                    )
                })?;
                if let Some((earlier, earlier_out_of_range)) =
                    sources.insert(record.form_id, (source_id, out_of_range))
                {
                    // Light-ID truncation keeps its documented last-record-wins behaviour; only
                    // an out-of-range master index is rejected when it lands on another record.
                    color_eyre::eyre::ensure!(
                        earlier == source_id || !(out_of_range || earlier_out_of_range),
                        "{}: records {earlier:08X} and {source_id:08X} both resolve to {:08X}; an out-of-range master index would replace another record",
                        order.names[priority],
                        record.form_id
                    );
                }
                if record.record_type == *b"GMST" {
                    let Some(canonical) =
                        game_settings.resolve(&record, &order.names[priority], &merged)?
                    else {
                        continue;
                    };
                    record.form_id = canonical;
                } else {
                    color_eyre::eyre::ensure!(
                        !game_settings.aliases.contains_key(&record.form_id),
                        "record {:08X} collides with a GMST identity",
                        record.form_id
                    );
                }
                if record.is_deleted() {
                    merged.remove(&record.form_id);
                } else {
                    merged.insert(record.form_id, record);
                }
            }
        }
        warnings.report();
        Ok(merged)
    }
}

/// Per-plugin count of out-of-range master indices, with the first ID and its record type.
type OutOfRangeCounts = BTreeMap<String, (u64, u32, [u8; 4])>;

/// Remap diagnostics counted per plugin (including references), each keeping its first
/// example, so memory and log volume scale with plugins, not records.
#[derive(Default)]
struct RemapWarnings {
    /// Light-plugin local IDs wider than 12 bits, keyed by owning plugin.
    truncations: RefCell<BTreeMap<String, (u64, u32)>>,
    /// IDs whose master index lies past the plugin's master list, keyed by the plugin that
    /// contains them; the example keeps its record type.
    out_of_range: RefCell<OutOfRangeCounts>,
    /// Invalid optional links, counted per containing plugin with the first diagnostic.
    skipped_optional: RefCell<BTreeMap<String, (u64, String)>>,
}

impl RemapWarnings {
    /// Accumulate one truncation without emitting a per-reference diagnostic.
    fn truncated(&self, owner: &str, form_id: u32) {
        let mut counts = self.truncations.borrow_mut();
        let entry = counts.entry(owner.to_owned()).or_insert((0, form_id));
        entry.0 += 1;
    }

    /// Accumulate one ID that names a master the plugin does not have.
    fn out_of_range(&self, plugin: &str, form_id: u32, record_type: [u8; 4]) {
        let mut counts = self.out_of_range.borrow_mut();
        let entry = counts
            .entry(plugin.to_owned())
            .or_insert((0, form_id, record_type));
        entry.0 += 1;
    }

    /// Retain one example per plugin rather than logging every broken optional link.
    fn skipped_optional(&self, plugin: &str, example: String) {
        let mut counts = self.skipped_optional.borrow_mut();
        let entry = counts.entry(plugin.to_owned()).or_insert((0, example));
        entry.0 += 1;
    }

    /// Emit one deterministic summary per affected plugin after a successful merge.
    fn report(&self) {
        for (owner, (count, example)) in self.truncations.borrow().iter() {
            eprintln!(
                "warning: {owner}: {count} light-plugin ID occurrences exceeded 12 bits and were truncated (first: {example:08X}); compact the plugin's FormIDs before ESL-flagging it"
            );
        }
        for (plugin, (count, example, record_type)) in self.out_of_range.borrow().iter() {
            eprintln!(
                "warning: {plugin}: {count} FormID occurrences name a master index past the plugin's master list and were treated as the plugin's own (first: {example:08X} in {})",
                String::from_utf8_lossy(record_type)
            );
        }
        for (plugin, (count, example)) in self.skipped_optional.borrow().iter() {
            eprintln!(
                "warning: {plugin}: skipped {count} invalid optional FormID links (set to zero; first: {example})"
            );
        }
    }
}

/// Game settings override by name, but header-only deletions refer to any
/// earlier definition's FormID. Keep every non-null alias, even after deletion.
#[derive(Default)]
struct GameSettingIdentities {
    canonical_ids: HashMap<String, u32>,
    aliases: HashMap<u32, u32>,
}

impl GameSettingIdentities {
    /// Resolve a setting's persistent ID without silently deleting unrelated forms.
    fn resolve(
        &mut self,
        record: &RawRecord,
        plugin: &str,
        merged: &HashMap<u32, RawRecord>,
    ) -> Result<Option<u32>> {
        let source_id = record.form_id;
        let editor_id = extractors::SubrecordView::new(&record.subrecords)
            .get_string(b"EDID")
            .filter(|name| !name.is_empty())
            .map(|name| name.to_ascii_lowercase());
        let Some(editor_id) = editor_id else {
            if record.is_deleted() {
                if let Some(&canonical) = self.aliases.get(&source_id) {
                    return Ok(Some(canonical));
                }
                eprintln!(
                    "warning: {plugin} deleted GMST {source_id:08X} has no EditorID or known setting identity; skipping"
                );
                return Ok(None);
            }
            color_eyre::eyre::bail!("{plugin} GMST {source_id:08X} has no EditorID");
        };
        let known = self.canonical_ids.get(&editor_id).copied();
        let canonical = known.unwrap_or(source_id);
        color_eyre::eyre::ensure!(
            canonical != 0,
            "GMST {editor_id} has a null FormID; needs an EditorID-keyed database representation"
        );
        if source_id != 0 {
            color_eyre::eyre::ensure!(
                self.aliases
                    .get(&source_id)
                    .is_none_or(|id| known == Some(*id))
                    && merged
                        .get(&source_id)
                        .is_none_or(|previous| previous.record_type == *b"GMST"),
                "GMST {editor_id} collides with another record at {source_id:08X}"
            );
            self.aliases.insert(source_id, canonical);
        }
        self.canonical_ids.insert(editor_id, canonical);
        Ok(Some(canonical))
    }
}

pub fn read_plugins_txt(path: &Path, data_dir: &Path) -> Result<Vec<PathBuf>> {
    let contents = std::fs::read_to_string(path)?;
    let mut plugins = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let enabled = line.starts_with('*');
        let name = line.strip_prefix('*').unwrap_or(line).trim();
        if !matches!(
            Path::new(name)
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .as_deref(),
            Some("esm" | "esp" | "esl")
        ) {
            continue;
        }
        if !enabled && !name.to_ascii_lowercase().ends_with(".esm") {
            continue;
        }
        let exact = data_dir.join(name);
        if exact.is_file() {
            plugins.push(exact);
            continue;
        }
        if let Some(found) = std::fs::read_dir(data_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|file| file.to_string_lossy().eq_ignore_ascii_case(name))
            })
        {
            plugins.push(found);
        } else {
            color_eyre::eyre::bail!("active plugin not found: {name}");
        }
    }
    Ok(plugins)
}

/// Determines whether a subrecord within a given parent record type represents
/// a 32-bit FormID that requires load-order remapping.
///
/// This record-aware check ensures that subrecords with shared tag names (such as
/// `CNAM` or `SNAM`) containing strings (e.g. `TES4` author/description), float
/// physics arrays (`TREE` trunk flexibility), or RGBA color structures (`CLFM`/`AACT`)
/// are not inadvertently overwritten as 4-byte FormIDs.
fn is_form_id_subrecord(record_type: &[u8; 4], tag: &[u8], len: usize) -> bool {
    if len != 4 || tag.len() < 4 {
        return false;
    }
    let tag_4: &[u8; 4] = tag[..4].try_into().unwrap();
    match (record_type, tag_4) {
        (b"TES4" | b"CLFM" | b"AACT", _) => false,
        (b"TREE", b"CNAM") => false,
        (b"TREE", b"SNAM" | b"PFIG") if len == 4 => true,
        (b"LTEX", b"TNAM" | b"GNAM" | b"MNAM") => true,
        (b"CELL", b"XCWT") => true,
        (b"WRLD", b"NAM2" | b"NAM3") => true,
        (b"WRLD", b"WNAM" | b"CNAM" | b"RNAM" | b"TNAM") if len == 4 => true,
        (b"CELL", b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL") if len == 4 => true,
        (b"NPC_", b"RNAM" | b"CNAM" | b"INAM") if len == 4 => true,
        (b"RACE", b"WKMV" | b"RNMV") if len == 4 => true,
        (b"NPC_", b"SNAM") if len >= 4 => true,
        (
            b"REFR" | b"ACHR" | b"ACRE" | b"PGRE" | b"PMIS" | b"PHZD" | b"PARW" | b"PBAR" | b"PBEA"
            | b"PCON" | b"PFLA",
            b"NAME" | b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL",
        ) if len == 4 => true,
        (_, b"XOWN" | b"XGLB" | b"XEZN" | b"XLCN" | b"XLRL") if len == 4 => true,
        _ => false,
    }
}

/// Where FormIDs sit inside a reference or cell subrecord that `is_form_id_subrecord`
/// does not cover (it only matches single four-byte fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormIdLayout {
    /// A fixed-size structure: each accepted byte length with its FormID offsets.
    Fixed(&'static [(usize, &'static [usize])]),
    /// A packed array of `stride`-byte entries, each starting with a FormID.
    Array { stride: usize },
}

/// Field layouts per xEdit's TES5 definitions
/// (https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.5/Core/wbDefinitionsTES5.pas).
fn form_id_layout(record_type: &[u8; 4], tag: &[u8]) -> Option<FormIdLayout> {
    use FormIdLayout::{Array, Fixed};
    let tag: &[u8; 4] = tag.try_into().ok()?;
    let placed = matches!(
        record_type,
        b"REFR"
            | b"ACHR"
            | b"ACRE"
            | b"PGRE"
            | b"PMIS"
            | b"PHZD"
            | b"PARW"
            | b"PBAR"
            | b"PBEA"
            | b"PCON"
            | b"PFLA"
    );
    const SINGLE: &[(usize, &[usize])] = &[(4, &[0])];
    Some(match tag {
        // Door destination reference, then position, rotation and flags.
        b"XTEL" if placed => Fixed(&[(32, &[0])]),
        // Enable parent, followed by flags and three unused bytes.
        b"XESP" if placed => Fixed(&[(8, &[0])]),
        // Linked-reference keyword, then the linked reference. 14 references in the
        // official plugins use the older 4-byte form holding only the linked reference.
        b"XLKR" if placed => Fixed(&[(8, &[0, 4]), (4, &[0])]),
        // Navmesh, then a u16 teleport-marker triangle and padding.
        b"XNDP" if placed => Fixed(&[(8, &[0])]),
        // Emitted light or region.
        b"XEMI" if placed => Fixed(SINGLE),
        // Each activate parent is a separate subrecord: reference plus f32 delay.
        b"XAPR" if placed => Fixed(&[(8, &[0])]),
        // Location reference types.
        b"XLRT" if placed => Array { stride: 4 },
        // A rider's horse.
        b"XHOR" if record_type == b"ACHR" => Fixed(SINGLE),
        // Lighting template, on cells and on worldspaces.
        b"LTMP" if matches!(record_type, b"CELL" | b"WRLD") => Fixed(SINGLE),
        // Image space, music, acoustic space, and the region a cell takes its sky and weather
        // from (`XCCM` names a REGN, not a climate).
        b"XCIM" | b"XCMO" | b"XCAS" | b"XCCM" if record_type == b"CELL" => Fixed(SINGLE),
        // Regions.
        b"XCLR" if record_type == b"CELL" => Array { stride: 4 },
        _ => return None,
    })
}

/// Remaps local FormIDs within a record header, parent cell/worldspace references,
/// and relevant subrecords according to master plugin load-order indices.
fn remap_record_form_ids(
    record: &mut RawRecord,
    plugin_name: &str,
    masters: &[String],
    normal_indices: &HashMap<String, u32>,
    light_indices: &HashMap<String, u32>,
    warnings: &RemapWarnings,
) -> Result<()> {
    // Enforce strict reference validation for landscape and grass record kinds.
    // Preserve legacy handling elsewhere until their record-specific exceptions
    // (including shipped GMST IDs outside the master table) have been audited.
    // Optional link fields in `form_id_layout` clear invalid IDs instead, even on
    // these record kinds; see the layout branch below.
    let strict = matches!(
        &record.record_type,
        b"GRAS" | b"LTEX" | b"TXST" | b"LAND" | b"CELL" | b"WRLD"
    );
    let record_type = record.record_type;
    let remap_with_validation = |form_id: u32, strict: bool| -> Result<u32> {
        if form_id == 0 {
            return Ok(0);
        }
        let local_index = (form_id >> 24) as usize;
        color_eyre::eyre::ensure!(
            !strict || local_index <= masters.len(),
            "{plugin_name}: {form_id:08X} has invalid master index {local_index}"
        );
        if local_index > masters.len() {
            // Treated as the plugin's own, which shipped data relies on (Skyrim.esm and
            // Dawnguard.esm each carry one such ID). Reported, and the merge rejects it when
            // it would replace another record.
            warnings.out_of_range(plugin_name, form_id, record_type);
        }
        let owner = if local_index < masters.len() {
            masters[local_index].to_ascii_lowercase()
        } else {
            plugin_name.to_owned()
        };
        if let Some(index) = light_indices.get(&owner) {
            if form_id & 0x00ff_ffff > 0xfff {
                color_eyre::eyre::ensure!(
                    !strict,
                    "{owner}: light-plugin local ID exceeds 12 bits: {form_id:08X}"
                );
                warnings.truncated(&owner, form_id);
            }
            return Ok(0xFE00_0000 | (index << 12) | (form_id & 0xFFF));
        }
        let index = normal_indices.get(&owner).ok_or_else(|| {
            color_eyre::eyre::eyre!("master {owner} is not present in load order")
        })?;
        Ok((index << 24) | (form_id & 0x00FF_FFFF))
    };
    let remap = |form_id| remap_with_validation(form_id, strict);
    let source_id = record.form_id;
    record.form_id = remap(record.form_id)?;
    record.cell_form_id = record.cell_form_id.map(&remap).transpose()?;
    record.worldspace_form_id = record.worldspace_form_id.map(&remap).transpose()?;
    for (tag, data) in &mut record.subrecords {
        if record.record_type == *b"GRAS" && tag.as_slice() == b"MODS" {
            color_eyre::eyre::ensure!(data.len() >= 4, "truncated GRAS alternate textures");
            let count = u32::from_le_bytes(data[..4].try_into().unwrap());
            let mut offset = 4usize;
            for _ in 0..count {
                color_eyre::eyre::ensure!(
                    data.len().saturating_sub(offset) >= 4,
                    "truncated MODS name length"
                );
                let length =
                    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;
                color_eyre::eyre::ensure!(
                    data.len().saturating_sub(offset) >= length.saturating_add(8),
                    "truncated MODS entry"
                );
                offset += length;
                let value = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                data[offset..offset + 4].copy_from_slice(&remap(value)?.to_le_bytes());
                offset += 8;
            }
            color_eyre::eyre::ensure!(offset == data.len(), "trailing bytes in GRAS MODS");
            continue;
        }
        if record.record_type == *b"LAND" && matches!(tag.as_slice(), b"BTXT" | b"ATXT") {
            color_eyre::eyre::ensure!(data.len() == 8, "invalid LAND texture reference length");
            let value = u32::from_le_bytes(data[..4].try_into().unwrap());
            data[..4].copy_from_slice(&remap(value)?.to_le_bytes());
            continue;
        }
        if record.record_type == *b"LAND" && tag.as_slice() == b"VTEX" {
            // xEdit's TES5 LAND definition uses an array of wbFormIDCk(LTEX,
            // NULL), unlike the 8-byte BTXT/ATXT layer structures above:
            // https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.5/Core/wbDefinitionsTES5.pas
            color_eyre::eyre::ensure!(data.len().is_multiple_of(4), "invalid LAND VTEX length");
            for value in data.as_chunks_mut::<4>().0 {
                *value = remap(u32::from_le_bytes(*value))?.to_le_bytes();
            }
            continue;
        }
        if tag.as_slice() == b"VMAD" {
            records::record_type::vmad::remap_primary_form_ids(data, &remap)
                .wrap_err("VMAD references")?;
            continue;
        }
        if let Some(layout) = form_id_layout(&record.record_type, tag) {
            let name = String::from_utf8_lossy(tag).into_owned();
            let offsets: Vec<usize> = match layout {
                FormIdLayout::Fixed(variants) => variants
                    .iter()
                    .find(|(len, _)| *len == data.len())
                    .map(|(_, offsets)| offsets.to_vec())
                    .ok_or_else(|| {
                        let sizes: Vec<String> =
                            variants.iter().map(|(len, _)| len.to_string()).collect();
                        color_eyre::eyre::eyre!(
                            "{name} must be {} bytes, found {}",
                            sizes.join(" or "),
                            data.len()
                        )
                    })?,
                FormIdLayout::Array { stride } => {
                    color_eyre::eyre::ensure!(
                        data.len().is_multiple_of(stride),
                        "{name} length {} is not a multiple of {stride}",
                        data.len()
                    );
                    (0..data.len()).step_by(stride).collect()
                }
            };
            for offset in offsets {
                let value = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                // A broken optional link must not block unrelated content. Do not keep
                // a plugin-local value or guess an owner: publish a null link and report it.
                let resolved = match remap_with_validation(value, true) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        warnings.skipped_optional(
                            plugin_name,
                            format!(
                                "{} record source {source_id:08X} (load-order {:08X}) {name} reference {value:08X}: {error}",
                                String::from_utf8_lossy(&record_type),
                                record.form_id
                            ),
                        );
                        0
                    }
                };
                data[offset..offset + 4].copy_from_slice(&resolved.to_le_bytes());
            }
            continue;
        }
        if is_form_id_subrecord(&record.record_type, tag, data.len()) {
            let value = u32::from_le_bytes(data[..4].try_into().unwrap());
            data[..4].copy_from_slice(
                &remap(value)
                    .wrap_err_with(|| format!("{} reference", String::from_utf8_lossy(tag)))?
                    .to_le_bytes(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_optional_links_are_cleared_and_warnings_are_aggregated() {
        let indices = HashMap::from([("base.esm".into(), 0), ("patch.esp".into(), 2)]);
        let warnings = RemapWarnings::default();
        let mut record = RawRecord {
            form_id: 0x0100_0800,
            record_type: *b"REFR",
            flags: 0,
            subrecords: vec![
                (
                    b"XESP".to_vec(),
                    [0x0200_0900u32.to_le_bytes(), [1, 2, 3, 4]].concat(),
                ),
                (
                    b"XAPR".to_vec(),
                    [0x0300_0901u32.to_le_bytes(), 1.25f32.to_le_bytes()].concat(),
                ),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut record,
            "patch.esp",
            &["base.esm".into()],
            &indices,
            &HashMap::new(),
            &warnings,
        )
        .unwrap();
        assert_eq!(
            record.subrecords[0].1,
            [0u32.to_le_bytes(), [1, 2, 3, 4]].concat()
        );
        assert_eq!(
            record.subrecords[1].1,
            [0u32.to_le_bytes(), 1.25f32.to_le_bytes()].concat()
        );
        let counts = warnings.skipped_optional.borrow();
        assert_eq!(counts.len(), 1);
        let (count, example) = &counts["patch.esp"];
        assert_eq!(*count, 2);
        for expected in [
            "REFR",
            "source 01000800",
            "load-order 02000800",
            "XESP",
            "02000900",
            "invalid master index",
        ] {
            assert!(example.contains(expected), "{example}");
        }
    }

    #[test]
    fn remaps_optional_race_movement_links() {
        let normal_indices = HashMap::from([
            ("skyrim.esm".to_string(), 0),
            ("movement.esp".to_string(), 3),
        ]);
        let mut race = RawRecord {
            form_id: 0x0101_3746,
            record_type: *b"RACE",
            flags: 0,
            subrecords: vec![(b"WKMV".to_vec(), 0x0100_1234u32.to_le_bytes().to_vec())],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut race,
            "movement.esp",
            &["skyrim.esm".to_string()],
            &normal_indices,
            &HashMap::new(),
            &RemapWarnings::default(),
        )
        .unwrap();
        assert_eq!(race.form_id, 0x0301_3746);
        assert_eq!(
            u32::from_le_bytes(race.subrecords[0].1[..4].try_into().unwrap()),
            0x0300_1234
        );
    }

    #[test]
    fn does_not_corrupt_tes4_author_strings_or_clfm_colors() {
        let normal_indices = HashMap::from([("skyrim.esm".to_string(), 0)]);
        let light_indices = HashMap::new();

        let mut tes4 = RawRecord {
            form_id: 0,
            record_type: *b"TES4",
            flags: 0,
            subrecords: vec![
                (b"CNAM".to_vec(), b"Bethesda Game Studios\0".to_vec()),
                (b"SNAM".to_vec(), b"Master description\0".to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut tes4,
            "skyrim.esm",
            &[],
            &normal_indices,
            &light_indices,
            &RemapWarnings::default(),
        )
        .unwrap();
        assert_eq!(tes4.subrecords[0].1, b"Bethesda Game Studios\0");
        assert_eq!(tes4.subrecords[1].1, b"Master description\0");

        let mut clfm = RawRecord {
            form_id: 0x00012345,
            record_type: *b"CLFM",
            flags: 0,
            subrecords: vec![(b"CNAM".to_vec(), vec![128, 64, 32, 255])],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 0,
        };
        remap_record_form_ids(
            &mut clfm,
            "skyrim.esm",
            &[],
            &normal_indices,
            &light_indices,
            &RemapWarnings::default(),
        )
        .unwrap();
        assert_eq!(clfm.subrecords[0].1, vec![128, 64, 32, 255]);
    }

    #[test]
    fn remaps_actual_form_id_subrecords() {
        let normal_indices =
            HashMap::from([("skyrim.esm".to_string(), 0), ("update.esm".to_string(), 1)]);
        let light_indices = HashMap::new();

        let mut refr = RawRecord {
            form_id: 0x00000001,
            record_type: *b"REFR",
            flags: 0,
            subrecords: vec![
                (b"NAME".to_vec(), 0x00000020u32.to_le_bytes().to_vec()),
                (b"XOWN".to_vec(), 0x00000030u32.to_le_bytes().to_vec()),
            ],
            cell_form_id: None,
            worldspace_form_id: None,
            load_order: 1,
        };
        remap_record_form_ids(
            &mut refr,
            "update.esm",
            &["skyrim.esm".to_string()],
            &normal_indices,
            &light_indices,
            &RemapWarnings::default(),
        )
        .unwrap();
        assert_eq!(
            u32::from_le_bytes(refr.subrecords[0].1[..4].try_into().unwrap()),
            0x00000020
        );
        assert_eq!(
            u32::from_le_bytes(refr.subrecords[1].1[..4].try_into().unwrap()),
            0x00000030
        );
    }
}
