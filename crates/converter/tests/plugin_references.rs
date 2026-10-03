//! Synthetic plugins only: no game data or copied assets.
use converter::esm::{EsmParser, extractors::SubrecordView, load_order::LoadOrder};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn sub(tag: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    [tag.as_slice(), &(bytes.len() as u16).to_le_bytes(), bytes].concat()
}
fn record(tag: &[u8; 4], id: u32, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    [
        tag.as_slice(),
        &(payload.len() as u32).to_le_bytes(),
        &flags.to_le_bytes(),
        &id.to_le_bytes(),
        &[0; 8],
        &payload,
    ]
    .concat()
}
fn group(kind: i32, id: u32, payload: Vec<u8>) -> Vec<u8> {
    [
        b"GRUP".as_slice(),
        &(payload.len() as u32 + 24).to_le_bytes(),
        &id.to_le_bytes(),
        &kind.to_le_bytes(),
        &[0; 8],
        &payload,
    ]
    .concat()
}
fn plugin(root: &Path, name: &str, masters: &[&str], flags: u32, records: Vec<u8>) -> PathBuf {
    let mut header = Vec::new();
    for master in masters {
        header.extend(sub(b"MAST", format!("{master}\0").as_bytes()));
        header.extend(sub(b"DATA", &[0; 8]));
    }
    let path = root.join(name);
    fs::write(&path, [record(b"TES4", 0, flags, header), records].concat()).unwrap();
    path
}
fn grass_data() -> Vec<u8> {
    let mut data = vec![0; 32];
    data[0..3].copy_from_slice(&[35, 10, 65]);
    data[4..6].copy_from_slice(&450u16.to_le_bytes());
    data[8..12].copy_from_slice(&6u32.to_le_bytes());
    for (offset, value) in [(12, 12.5f32), (16, 0.4), (20, 0.2), (24, 1.5)] {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    data[28] = 7;
    data
}
fn grass(id: u32, density: u8) -> Vec<u8> {
    let mut data = grass_data();
    data[0] = density;
    record(
        b"GRAS",
        id,
        0,
        [
            sub(b"EDID", b"SyntheticGrass\0"),
            sub(b"MODL", b"grass/test.nif\0"),
            sub(b"DATA", &data),
        ]
        .concat(),
    )
}
fn texture_layer(id: u32, quadrant: u8, index: u16) -> Vec<u8> {
    [
        id.to_le_bytes().as_slice(),
        &[quadrant, 0],
        &index.to_le_bytes(),
    ]
    .concat()
}

#[test]
fn remaps_reordered_masters_light_plugins_terrain_and_alternate_textures() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(dir.path(), "Base.esm", &[], 0, grass(0x800, 35));
    let filler = plugin(dir.path(), "Filler.esm", &[], 0, Vec::new());
    let light = plugin(
        dir.path(),
        "Light.esl",
        &["Base.esm"],
        0x200,
        grass(0x01000810, 50),
    );
    let mut mods = 1u32.to_le_bytes().to_vec();
    mods.extend(5u32.to_le_bytes());
    mods.extend(b"Shape");
    mods.extend(0x01000801u32.to_le_bytes());
    mods.extend(2u32.to_le_bytes());
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &["Light.esl", "Base.esm"],
        0,
        [
            record(
                b"LTEX",
                0x02000820,
                0,
                [
                    sub(b"TNAM", &0x01000801u32.to_le_bytes()),
                    sub(b"MNAM", &0x01000802u32.to_le_bytes()),
                    sub(b"GNAM", &0x00000810u32.to_le_bytes()),
                    sub(b"GNAM", &0x01000800u32.to_le_bytes()),
                ]
                .concat(),
            ),
            record(
                b"GRAS",
                0x01000800,
                0,
                [sub(b"DATA", &grass_data()), sub(b"MODS", &mods)].concat(),
            ),
            group(
                1,
                0x01000900,
                group(
                    6,
                    0x01000901,
                    record(
                        b"LAND",
                        0x02000830,
                        0,
                        [
                            sub(b"BTXT", &texture_layer(0x02000820, 2, 0)),
                            sub(b"ATXT", &texture_layer(0x01000802, 2, 3)),
                            sub(
                                b"VTEX",
                                &[0x02000820u32.to_le_bytes(), 0u32.to_le_bytes()].concat(),
                            ),
                        ]
                        .concat(),
                    ),
                ),
            ),
        ]
        .concat(),
    );
    let paths = vec![base.clone(), filler.clone(), light.clone(), patch.clone()];
    let merged = EsmParser::merge_plugins(&paths).unwrap();
    let ltex = SubrecordView::new(&merged[&0x02000820].subrecords);
    assert_eq!(ltex.get_form_id(b"TNAM"), Some(0x801));
    assert_eq!(ltex.get_form_id(b"MNAM"), Some(0x802));
    assert_eq!(ltex.get_form_id(b"GNAM"), Some(0xfe000810));
    let land = &merged[&0x02000830];
    assert_eq!(land.cell_form_id, Some(0x901));
    assert_eq!(land.worldspace_form_id, Some(0x900));
    let view = SubrecordView::new(&land.subrecords);
    assert_eq!(view.get_form_id(b"BTXT"), Some(0x02000820));
    assert_eq!(view.get_form_id(b"ATXT"), Some(0x802));
    assert_eq!(view.find(b"ATXT").unwrap()[4..], [2, 0, 3, 0]);
    assert_eq!(view.find(b"VTEX").unwrap()[4..], [0; 4]);
    let override_record = &merged[&0x800];
    assert_eq!(override_record.load_order, 3);
    assert_eq!(
        SubrecordView::new(&override_record.subrecords).get_alternate_textures(b"MODS")[0]
            .texture_form_id,
        0x801
    );
    let id = LoadOrder::read(&paths).unwrap().identity(0x800).unwrap();
    assert_eq!(id.plugin, "base.esm");
    assert_eq!(id.local_id, 0x800);
    let changed = vec![filler, base, light, patch];
    assert_eq!(
        LoadOrder::read(&changed)
            .unwrap()
            .identity(0x01000800)
            .unwrap(),
        id
    );
    assert!(
        EsmParser::merge_plugins(&changed)
            .unwrap()
            .contains_key(&0x01000800)
    );
}

#[test]
fn remaps_cell_and_world_water_references_and_preserves_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let filler = plugin(dir.path(), "Filler.esm", &[], 0, Vec::new());
    let base = plugin(dir.path(), "Base.esm", &[], 0, Vec::new());
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &["Base.esm"],
        0,
        [
            record(
                b"CELL",
                0x01000800,
                0,
                sub(b"XCWT", &0x900u32.to_le_bytes()),
            ),
            record(
                b"WRLD",
                0x01000801,
                0,
                [
                    sub(b"NAM2", &0x901u32.to_le_bytes()),
                    sub(b"NAM3", &0u32.to_le_bytes()),
                ]
                .concat(),
            ),
        ]
        .concat(),
    );
    let merged = EsmParser::merge_plugins(&[filler, base, patch]).unwrap();
    assert_eq!(
        SubrecordView::new(&merged[&0x02000800].subrecords).get_form_id(b"XCWT"),
        Some(0x01000900)
    );
    let world = SubrecordView::new(&merged[&0x02000801].subrecords);
    assert_eq!(world.get_form_id(b"NAM2"), Some(0x01000901));
    assert_eq!(world.get_form_id(b"NAM3"), Some(0));
}

#[test]
fn rejects_malformed_land_and_alternate_texture_references() {
    for (kind, payload) in [
        (b"LAND", sub(b"BTXT", &[0; 7])),
        (b"LAND", sub(b"ATXT", &[0; 9])),
        (b"LAND", sub(b"VTEX", &[0; 3])),
        (b"GRAS", sub(b"MODS", &1u32.to_le_bytes())),
        (b"GRAS", sub(b"MODS", &[0; 5])),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let base = plugin(
            dir.path(),
            "Base.esm",
            &[],
            0,
            record(kind, 0x800, 0, payload),
        );
        assert!(EsmParser::merge_plugins(&[base]).is_err());
    }
}

#[test]
fn remaps_every_vtex_entry_without_layer_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let filler = plugin(dir.path(), "Filler.esm", &[], 0, Vec::new());
    let base = plugin(dir.path(), "Base.esm", &[], 0, Vec::new());
    let land = record(
        b"LAND",
        0x01000900,
        0,
        sub(
            b"VTEX",
            &[
                0x800u32.to_le_bytes(),
                0x801u32.to_le_bytes(),
                0u32.to_le_bytes(),
                0x01000802u32.to_le_bytes(),
            ]
            .concat(),
        ),
    );
    let patch = plugin(dir.path(), "Patch.esp", &["Base.esm"], 0, land);
    let merged = EsmParser::merge_plugins(&[filler, base, patch]).unwrap();
    assert_eq!(
        SubrecordView::new(&merged[&0x02000900].subrecords)
            .find(b"VTEX")
            .unwrap(),
        &[
            0x01000800u32.to_le_bytes(),
            0x01000801u32.to_le_bytes(),
            0u32.to_le_bytes(),
            0x02000802u32.to_le_bytes(),
        ]
        .concat()
    );
}

#[test]
fn group_parser_handles_deep_nesting_and_restores_sibling_context_in_file_order() {
    let mut payload = record(b"STAT", 1, 0, Vec::new());
    // Well beyond a recursive parser's practical call-stack depth. Building
    // headers in one pass keeps the fixture linear in size and construction time.
    let depth = 10_000usize;
    let mut nested = Vec::with_capacity(depth * 24 + payload.len());
    for remaining in (1..=depth).rev() {
        nested.extend(b"GRUP");
        nested.extend(((remaining * 24 + payload.len()) as u32).to_le_bytes());
        nested.extend([0; 16]);
    }
    nested.append(&mut payload);
    let input = [
        group(1, 0x900, group(6, 0x901, nested)),
        group(1, 0xa00, group(6, 0xa01, record(b"STAT", 2, 0, Vec::new()))),
        record(b"STAT", 3, 0, Vec::new()),
    ]
    .concat();
    let mut records = Vec::new();
    converter::esm::binary::parse_group(&input, None, None, &mut records).unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| (
                record.form_id,
                record.cell_form_id,
                record.worldspace_form_id
            ))
            .collect::<Vec<_>>(),
        [
            (1, Some(0x901), Some(0x900)),
            (2, Some(0xa01), Some(0xa00)),
            (3, None, None)
        ]
    );
}

#[test]
fn group_parser_rejects_partial_headers_in_nested_payloads_and_after_siblings() {
    for length in 1..24 {
        let partial = vec![0; length];
        for input in [
            partial.clone(),
            group(0, 0, partial.clone()),
            [
                group(0, 0, record(b"STAT", 1, 0, Vec::new())),
                partial.clone(),
            ]
            .concat(),
        ] {
            let result = converter::esm::binary::parse_group(&input, None, None, &mut Vec::new());
            assert!(result.unwrap_err().to_string().contains("trailing"));
        }
    }
}

#[test]
fn overrides_replace_grass_and_deletions_do_not_resurrect_it() {
    let dir = tempfile::tempdir().unwrap();
    let a = plugin(dir.path(), "Base.esm", &[], 0, grass(0x800, 35));
    let b = plugin(
        dir.path(),
        "Override.esp",
        &["Base.esm"],
        0,
        grass(0x800, 75),
    );
    let merged = EsmParser::merge_plugins(&[a.clone(), b.clone()]).unwrap();
    assert_eq!(
        SubrecordView::new(&merged[&0x800].subrecords)
            .find(b"DATA")
            .unwrap()[0],
        75
    );
    let c = plugin(
        dir.path(),
        "Delete.esp",
        &["Base.esm"],
        0,
        record(b"GRAS", 0x800, 0x20, Vec::new()),
    );
    assert!(
        !EsmParser::merge_plugins(&[a, b, c])
            .unwrap()
            .contains_key(&0x800)
    );
}

#[test]
fn rejects_duplicate_plugins_missing_masters_and_invalid_indices() {
    let dir = tempfile::tempdir().unwrap();
    // Non-grass GMST identity is a separate legacy path; do not reject it here.
    let base = plugin(
        dir.path(),
        "Base.esm",
        &[],
        0,
        record(b"GMST", 0x01000120, 0, sub(b"EDID", b"fSyntheticSetting\0")),
    );
    assert!(EsmParser::merge_plugins(std::slice::from_ref(&base)).is_ok());
    assert!(LoadOrder::read(&[base.clone(), base.clone()]).is_err());
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &["Base.esm"],
        0,
        grass(0x02000800, 35),
    );
    assert!(LoadOrder::read(std::slice::from_ref(&patch)).is_err());
    assert!(LoadOrder::read(&[patch.clone(), base.clone()]).is_err());
    assert!(EsmParser::merge_plugins(&[base, patch]).is_err());
    let oversized = plugin(dir.path(), "Oversized.esl", &[], 0x200, grass(0x1800, 35));
    assert!(EsmParser::merge_plugins(&[oversized]).is_err());
}

#[test]
#[ignore = "requires explicit MUDCRAB_GRASS_DATA and MUDCRAB_GRASS_PLUGINS paths"]
fn full_local_load_order_merges() {
    let data = PathBuf::from(std::env::var_os("MUDCRAB_GRASS_DATA").expect("MUDCRAB_GRASS_DATA"));
    let list =
        PathBuf::from(std::env::var_os("MUDCRAB_GRASS_PLUGINS").expect("MUDCRAB_GRASS_PLUGINS"));
    let paths = converter::esm::read_plugins_txt(&list, &data).unwrap();
    assert!(!paths.is_empty());
    let records = EsmParser::merge_plugins(&paths).unwrap();
    assert!(
        records
            .values()
            .any(|record| record.record_type == *b"GRAS")
    );
    eprintln!(
        "Merged {} plugins and {} effective records",
        paths.len(),
        records.len()
    );
}

#[test]
fn stable_identity_uses_independent_full_and_light_slot_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let paths = vec![
        plugin(dir.path(), "Base.esm", &[], 0, Vec::new()),
        plugin(dir.path(), "Light.esl", &[], 0x200, Vec::new()),
        plugin(dir.path(), "Patch.esp", &[], 0, Vec::new()),
        plugin(dir.path(), "Flagged.esp", &[], 0x200, Vec::new()),
    ];
    let mut order = LoadOrder::read(&paths).unwrap();
    for (form_id, name, local_id) in [
        (0x00000800, "base.esm", 0x800),
        (0x01000801, "patch.esp", 0x801),
        (0xfe000802, "light.esl", 0x802),
        (0xfe001803, "flagged.esp", 0x803),
    ] {
        let identity = order.identity(form_id).unwrap();
        assert_eq!(identity.plugin, name);
        assert_eq!(identity.local_id, local_id);
    }
    assert!(
        order
            .identity(0)
            .unwrap_err()
            .to_string()
            .contains("null reference")
    );
    for form_id in [0x02000800, 0xfe002800, 0xff000800] {
        assert!(
            order
                .identity(form_id)
                .unwrap_err()
                .to_string()
                .contains("unresolved slot")
        );
    }
    // Public forward maps must not silently invalidate the cached reverse ownership.
    order.normal.insert("base.esm".into(), 1);
    assert!(order.identity(0x800).is_err());
    order.light.remove("light.esl");
    assert!(order.identity(0xfe000800).is_err());
}

/// A master index past the plugin's master list is treated as the plugin's own, as shipped
/// data relies on (Skyrim.esm and Dawnguard.esm each carry one such ID), so a lone one
/// still converts.
#[test]
fn out_of_range_master_index_resolves_to_the_plugin_itself() {
    let dir = tempfile::tempdir().unwrap();
    let path = plugin(
        dir.path(),
        "Masterless.esm",
        &[],
        1,
        record(b"STAT", 0x0200_0800, 0, sub(b"EDID", b"Stray\0")),
    );
    let merged = EsmParser::merge_plugins(&[path]).unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[&0x0000_0800].subrecords[0].1, b"Stray\0");
}

/// An out-of-range master index that lands on another record of the same plugin would
/// silently replace it, so the merge rejects it whichever record comes first.
#[test]
fn out_of_range_master_index_may_not_replace_another_record() {
    for ids in [[0x0000_0800, 0x0200_0800], [0x0200_0800, 0x0000_0800]] {
        let dir = tempfile::tempdir().unwrap();
        let path = plugin(
            dir.path(),
            "Masterless.esm",
            &[],
            1,
            [
                record(b"STAT", ids[0], 0, sub(b"EDID", b"First\0")),
                record(b"STAT", ids[1], 0, sub(b"EDID", b"Second\0")),
            ]
            .concat(),
        );
        let error = format!("{:?}", EsmParser::merge_plugins(&[path]).unwrap_err());
        assert!(
            error.contains("both resolve to 00000800")
                && error.contains("00000800")
                && error.contains("02000800"),
            "{error}"
        );
    }
}

/// Reference and cell fields that hold FormIDs are rewritten into load-order numbering.
/// The last plugin mirrors Dragonborn.esm: its masters are only Skyrim.esm and Update.esm,
/// so its own records are numbered 0x02xxxxxx locally but sit in slot 4 of the load order,
/// where 0x02 is a different plugin.
#[test]
fn reference_and_cell_form_id_fields_use_load_order_numbering() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = vec![
        plugin(dir.path(), "Skyrim.esm", &[], 1, Vec::new()),
        plugin(dir.path(), "Update.esm", &["Skyrim.esm"], 1, Vec::new()),
        plugin(
            dir.path(),
            "Dawnguard.esm",
            &["Skyrim.esm", "Update.esm"],
            1,
            Vec::new(),
        ),
        plugin(
            dir.path(),
            "HearthFires.esm",
            &["Skyrim.esm", "Update.esm"],
            1,
            Vec::new(),
        ),
    ];
    let mut xtel = 0x0200_0801u32.to_le_bytes().to_vec();
    xtel.extend([0u8; 24]); // position and rotation
    xtel.extend(0x1u32.to_le_bytes()); // flags must survive untouched
    let mut xlkr = 0x0000_0042u32.to_le_bytes().to_vec(); // keyword from Skyrim.esm
    xlkr.extend(0x0200_0802u32.to_le_bytes()); // linked reference in this plugin
    let mut xclr = 0x0000_0123u32.to_le_bytes().to_vec();
    xclr.extend(0x0200_0124u32.to_le_bytes());
    paths.push(plugin(
        dir.path(),
        "Dragonborn.esm",
        &["Skyrim.esm", "Update.esm"],
        1,
        [
            record(
                b"REFR",
                0x0200_0800,
                0,
                [sub(b"XTEL", &xtel), sub(b"XLKR", &xlkr)].concat(),
            ),
            record(
                b"CELL",
                0x0200_0900,
                0,
                [
                    sub(b"LTMP", &0x0200_0901u32.to_le_bytes()),
                    sub(b"XCLR", &xclr),
                ]
                .concat(),
            ),
        ]
        .concat(),
    ));
    let merged = EsmParser::merge_plugins(&paths).unwrap();
    let field = |form_id: u32, tag: &[u8; 4]| -> Vec<u8> {
        merged[&form_id]
            .subrecords
            .iter()
            .find(|(name, _)| name.as_slice() == tag)
            .map(|(_, data)| data.clone())
            .unwrap()
    };
    let word = |data: &[u8], offset: usize| {
        u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
    };

    let xtel = field(0x0400_0800, b"XTEL");
    assert_eq!(
        word(&xtel, 0),
        0x0400_0801,
        "door destination is Dragonborn's own reference"
    );
    assert_eq!(word(&xtel, 28), 0x1, "XTEL flags are untouched");
    let xlkr = field(0x0400_0800, b"XLKR");
    assert_eq!(
        word(&xlkr, 0),
        0x0000_0042,
        "Skyrim.esm keyword keeps slot 0"
    );
    assert_eq!(word(&xlkr, 4), 0x0400_0802);
    assert_eq!(word(&field(0x0400_0900, b"LTMP"), 0), 0x0400_0901);
    let xclr = field(0x0400_0900, b"XCLR");
    assert_eq!([word(&xclr, 0), word(&xclr, 4)], [0x0000_0123, 0x0400_0124]);
}

/// A FormID-bearing field with the wrong size is malformed data, not something to remap
/// at a guessed offset.
#[test]
fn reference_form_id_fields_reject_malformed_lengths() {
    for (tag, bytes) in [
        (b"XTEL", vec![0u8; 28]),
        (b"XESP", vec![0u8; 4]),
        (b"XESP", vec![0u8; 12]),
        (b"XLKR", vec![0u8; 6]),
        (b"XNDP", vec![0u8; 4]),
        (b"XAPR", vec![0u8; 12]),
        (b"XAPR", vec![0u8; 16]),
        (b"XLRT", vec![0u8; 6]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = plugin(
            dir.path(),
            "Base.esm",
            &[],
            1,
            record(b"REFR", 0x0000_0800, 0, sub(tag, &bytes)),
        );
        let error = format!("{:?}", EsmParser::merge_plugins(&[path]).unwrap_err());
        assert!(error.contains(std::str::from_utf8(tag).unwrap()), "{error}");
    }
}

/// Forms seen in the official plugins: 14 references use a 4-byte `XLKR` holding only the
/// linked reference, and worldspaces carry `LTMP` lighting templates as well as cells.
#[test]
fn legacy_linked_references_and_worldspace_lighting_templates_are_remapped() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
    let filler = plugin(dir.path(), "Filler.esm", &["Base.esm"], 1, Vec::new());
    // Masters are only Base.esm, so this plugin's own records are 0x01xxxxxx locally but
    // sit in slot 2 of the load order.
    let patch = plugin(
        dir.path(),
        "Patch.esm",
        &["Base.esm"],
        1,
        [
            record(
                b"REFR",
                0x0100_0800,
                0,
                sub(b"XLKR", &0x0100_0801u32.to_le_bytes()),
            ),
            record(
                b"WRLD",
                0x0100_0900,
                0,
                sub(b"LTMP", &0x0100_0901u32.to_le_bytes()),
            ),
        ]
        .concat(),
    );
    let merged = EsmParser::merge_plugins(&[base, filler, patch]).unwrap();
    let word = |form_id: u32, tag: &[u8; 4]| {
        let data = &merged[&form_id]
            .subrecords
            .iter()
            .find(|(name, _)| name.as_slice() == tag)
            .unwrap()
            .1;
        assert_eq!(data.len(), 4);
        u32::from_le_bytes(data[..4].try_into().unwrap())
    };
    assert_eq!(word(0x0200_0800, b"XLKR"), 0x0200_0801);
    assert_eq!(word(0x0200_0900, b"LTMP"), 0x0200_0901);
}

/// Every field in the converter's layout table, so a mistyped tag or offset fails here.
/// The plugin's masters are only Base.esm, so its own records (0x01xxxxxx locally) sit in
/// slot 2, and each covered FormID must come out as 0x02xxxxxx.
#[test]
fn every_covered_reference_and_cell_field_is_remapped_at_its_offsets() {
    /// Record type, subrecord tag, payload length, and FormID offsets.
    type FieldCase = (&'static [u8; 4], &'static [u8; 4], usize, &'static [usize]);
    let fields: &[FieldCase] = &[
        (b"REFR", b"XTEL", 32, &[0]),
        (b"REFR", b"XESP", 8, &[0]),
        (b"PHZD", b"XESP", 8, &[0]),
        (b"PHZD", b"NAME", 4, &[0]),
        (b"PARW", b"XESP", 8, &[0]),
        (b"PARW", b"NAME", 4, &[0]),
        (b"PBAR", b"XESP", 8, &[0]),
        (b"PBAR", b"NAME", 4, &[0]),
        (b"PBEA", b"XESP", 8, &[0]),
        (b"PBEA", b"NAME", 4, &[0]),
        (b"PCON", b"XESP", 8, &[0]),
        (b"PCON", b"NAME", 4, &[0]),
        (b"PFLA", b"XESP", 8, &[0]),
        (b"PFLA", b"NAME", 4, &[0]),
        (b"REFR", b"XLKR", 8, &[0, 4]),
        (b"ACHR", b"XLKR", 4, &[0]),
        (b"REFR", b"XNDP", 8, &[0]),
        (b"REFR", b"XEMI", 4, &[0]),
        (b"REFR", b"XAPR", 8, &[0]),
        (b"ACHR", b"XLRT", 8, &[0, 4]),
        (b"ACHR", b"XHOR", 4, &[0]),
        (b"CELL", b"LTMP", 4, &[0]),
        (b"WRLD", b"LTMP", 4, &[0]),
        (b"CELL", b"XCIM", 4, &[0]),
        (b"CELL", b"XCMO", 4, &[0]),
        (b"CELL", b"XCAS", 4, &[0]),
        (b"CELL", b"XCCM", 4, &[0]),
        (b"CELL", b"XCLR", 12, &[0, 4, 8]),
    ];
    for &(record_type, tag, len, offsets) in fields {
        let label = format!(
            "{} {}",
            String::from_utf8_lossy(record_type),
            String::from_utf8_lossy(tag)
        );
        // Non-FormID bytes are 0xAB so a write at a wrong offset is visible.
        let mut payload = vec![0xABu8; len];
        for (i, &offset) in offsets.iter().enumerate() {
            payload[offset..offset + 4].copy_from_slice(&(0x0100_0900 + i as u32).to_le_bytes());
        }
        let dir = tempfile::tempdir().unwrap();
        let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
        let filler = plugin(dir.path(), "Filler.esm", &["Base.esm"], 1, Vec::new());
        let patch = plugin(
            dir.path(),
            "Patch.esm",
            &["Base.esm"],
            1,
            record(record_type, 0x0100_0800, 0, sub(tag, &payload)),
        );
        let merged = EsmParser::merge_plugins(&[base, filler, patch]).unwrap();
        let data = &merged[&0x0200_0800]
            .subrecords
            .iter()
            .find(|(name, _)| name.as_slice() == tag)
            .unwrap_or_else(|| panic!("{label} missing"))
            .1;
        let mut expected = payload.clone();
        for (i, &offset) in offsets.iter().enumerate() {
            expected[offset..offset + 4].copy_from_slice(&(0x0200_0900 + i as u32).to_le_bytes());
        }
        assert_eq!(data, &expected, "{label}");
    }
}

/// Diagnostics identify both the source plugin's record and its resolved load-order ID.
#[test]
fn reference_error_reports_source_and_load_order_record_ids() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
    let filler = plugin(dir.path(), "Filler.esm", &["Base.esm"], 1, Vec::new());
    let mut xesp = 0x0200_0900u32.to_le_bytes().to_vec();
    xesp.extend([0u8; 8]); // Invalid length still reports the record and field.
    let patch = plugin(
        dir.path(),
        "Patch.esm",
        &["Base.esm"],
        1,
        record(b"PHZD", 0x0100_0800, 0, sub(b"XESP", &xesp)),
    );
    let error = format!(
        "{:#}",
        EsmParser::merge_plugins(&[base, filler, patch]).unwrap_err()
    );
    for expected in [
        "patch.esm",
        "PHZD",
        "source 01000800",
        "load-order 02000800",
        "XESP",
        "must be 8 bytes",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

/// Light plugins use the 0xFE prefix with a 12-bit slot; their own references in these
/// fields must land in that space too.
#[test]
fn light_plugin_reference_fields_use_the_light_slot() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
    let mut xtel = 0x0100_0801u32.to_le_bytes().to_vec();
    xtel.extend([0u8; 28]);
    let light = plugin(
        dir.path(),
        "Light.esl",
        &["Base.esm"],
        0x201,
        record(b"REFR", 0x0100_0800, 0, sub(b"XTEL", &xtel)),
    );
    let merged = EsmParser::merge_plugins(&[base, light]).unwrap();
    let data = &merged[&0xFE00_0800]
        .subrecords
        .iter()
        .find(|(name, _)| name.as_slice() == b"XTEL")
        .unwrap()
        .1;
    assert_eq!(
        u32::from_le_bytes(data[..4].try_into().unwrap()),
        0xFE00_0801
    );
}

/// An invalid optional cell link is cleared without rejecting an otherwise valid cell.
#[test]
fn cell_fields_skip_out_of_range_master_indices() {
    let dir = tempfile::tempdir().unwrap();
    let path = plugin(
        dir.path(),
        "Base.esm",
        &[],
        1,
        record(
            b"CELL",
            0x0000_0800,
            0,
            sub(b"LTMP", &0x0500_0001u32.to_le_bytes()),
        ),
    );
    let merged = EsmParser::merge_plugins(&[path]).unwrap();
    assert_eq!(
        SubrecordView::new(&merged[&0x800].subrecords).get_form_id(b"LTMP"),
        Some(0)
    );
}

/// Invalid optional links do not block other records or corrupt the payload's
/// remaining bytes. Cover both bad master indices and wide light-master IDs.
#[test]
fn reference_fields_skip_invalid_master_and_light_ids() {
    for (tag, len, offset) in [
        (b"XTEL", 32, 0),
        (b"XESP", 8, 0),
        (b"XLKR", 8, 4),
        (b"XNDP", 8, 0),
        (b"XAPR", 8, 0),
    ] {
        for (name, flags, value, record_id) in [
            ("Patch.esp", 0, 0x0200_0801u32, 0x0200_0800),
            ("Patch.esl", 0x200, 0x0100_1801, 0xFE00_0800),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
            let filler = plugin(dir.path(), "Filler.esm", &[], 1, Vec::new());
            let mut payload = vec![0xAB; len];
            if tag == b"XLKR" {
                payload[..4].copy_from_slice(&0x0000_0042u32.to_le_bytes());
            }
            payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            let mut expected = payload.clone();
            expected[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
            let patch = plugin(
                dir.path(),
                name,
                &["Base.esm"],
                flags,
                [
                    record(b"REFR", 0x0100_0800, 0, sub(tag, &payload)),
                    record(
                        b"REFR",
                        0x0100_0802,
                        0,
                        sub(b"XEMI", &0x42u32.to_le_bytes()),
                    ),
                ]
                .concat(),
            );
            let merged = EsmParser::merge_plugins(&[base, filler, patch]).unwrap();
            assert_eq!(merged[&record_id].subrecords[0].1, expected);
            assert_eq!(
                SubrecordView::new(&merged[&(record_id + 2)].subrecords).get_form_id(b"XEMI"),
                Some(0x42)
            );
        }
    }
}

/// A broken link to a light master is skipped without truncating it to another
/// light record. Other array elements still resolve using the containing plugin.
#[test]
fn optional_links_to_wide_light_master_ids_preserve_valid_array_elements() {
    let dir = tempfile::tempdir().unwrap();
    let paths = vec![
        plugin(dir.path(), "Base.esm", &[], 1, Vec::new()),
        plugin(dir.path(), "First.esl", &[], 0x201, Vec::new()),
        plugin(dir.path(), "Second.esl", &[], 0x201, Vec::new()),
        plugin(dir.path(), "Filler.esm", &[], 1, Vec::new()),
        plugin(
            dir.path(),
            "Patch.esp",
            &["Second.esl", "Base.esm"],
            0,
            record(
                b"REFR",
                0x0200_0800,
                0,
                sub(
                    b"XLRT",
                    &[
                        0x0000_1801u32.to_le_bytes(), // Too wide for Second.esl.
                        0x0000_0802u32.to_le_bytes(), // Valid light master reference.
                        0x0200_0903u32.to_le_bytes(), // Valid plugin-local reference.
                        0u32.to_le_bytes(),
                    ]
                    .concat(),
                ),
            ),
        ),
    ];
    let merged = EsmParser::merge_plugins(&paths).unwrap();
    assert_eq!(
        merged[&0x0200_0800].subrecords[0].1,
        [
            0u32.to_le_bytes(),
            0xFE00_1802u32.to_le_bytes(),
            0x0200_0903u32.to_le_bytes(),
            0u32.to_le_bytes(),
        ]
        .concat()
    );
}

/// Null links stay null; unrelated record kinds sharing a tag remain opaque.
#[test]
fn null_links_and_unrelated_subrecords_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(dir.path(), "Base.esm", &[], 1, Vec::new());
    let payload = [0u32.to_le_bytes(), 0xABCD_EF12u32.to_le_bytes()].concat();
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &[],
        0,
        [
            record(b"REFR", 0x800, 0, sub(b"XESP", &payload)),
            record(b"STAT", 0x801, 0, sub(b"XTEL", &[1, 2, 3])),
        ]
        .concat(),
    );
    let merged = EsmParser::merge_plugins(&[base, patch]).unwrap();
    assert_eq!(merged[&0x0100_0800].subrecords[0].1, payload);
    assert_eq!(merged[&0x0100_0801].subrecords[0].1, [1, 2, 3]);
}

/// Read all occurrences of a subrecord from a published database projection.
fn database_fields(
    conn: &rusqlite::Connection,
    table: &str,
    id: u32,
    tag: &[u8; 4],
) -> Vec<Vec<u8>> {
    let key = if table == "records" { "form_id" } else { "id" };
    let blob: Vec<u8> = conn
        .query_row(
            &format!("SELECT data FROM \"{table}\" WHERE {key}=?1"),
            [id],
            |row| row.get(0),
        )
        .unwrap();
    let decoded =
        rkyv::from_bytes::<converter::esm::types::ArchivedRecordData, rkyv::rancor::Error>(&blob)
            .unwrap();
    decoded
        .subrecords
        .into_iter()
        .filter(|sub| &sub.tag == tag)
        .map(|sub| sub.data)
        .collect()
}

/// Read one required subrecord from a published database projection.
fn database_field(conn: &rusqlite::Connection, table: &str, id: u32, tag: &[u8; 4]) -> Vec<u8> {
    let fields = database_fields(conn, table, id, tag);
    assert_eq!(fields.len(), 1);
    fields.into_iter().next().unwrap()
}

/// The real pipeline publishes resolved IDs in every blob projection, including
/// non-identity master lists, light masters, overrides and repeated subrecords.
/// Rerunning it replaces stale database blobs without invalidating asset caches.
#[tokio::test]
async fn pipeline_publishes_and_rebuilds_reference_and_cell_links() {
    use converter::{AssetPipeline, PipelineConfig};
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    dummy_content::layout::prepare_directory(&data, false).unwrap();
    dummy_content::layout::generate(
        &data,
        dummy_content::layout::DEFAULT_SEED,
        dummy_content::layout::Formats::all(),
    )
    .unwrap();
    plugin(&data, "Filler.esm", &["Skyrim.esm"], 1, Vec::new());
    plugin(&data, "Filler2.esm", &["Skyrim.esm"], 1, Vec::new());
    plugin(&data, "Light.esl", &["Skyrim.esm"], 0x201, Vec::new());
    let mut xtel = 0x0200_0801u32.to_le_bytes().to_vec();
    xtel.extend([0xAB; 28]);
    let xesp = [0x0000_0802u32.to_le_bytes(), [1, 0, 0, 0]].concat();
    let xapr = [0x0200_0803u32.to_le_bytes(), 1.25f32.to_le_bytes()].concat();
    let cell = 0x0200_0900;
    // Light.esl is local master 0, Skyrim.esm is 1; the plugin itself is 2.
    plugin(
        &data,
        "Links.esp",
        &["Light.esl", "Skyrim.esm"],
        0,
        [
            record(
                b"CELL",
                cell,
                0,
                [
                    sub(b"DATA", &[1, 0]),
                    sub(b"LTMP", &0x0200_0901u32.to_le_bytes()),
                ]
                .concat(),
            ),
            group(
                6,
                cell,
                group(
                    9,
                    cell,
                    [
                        record(
                            b"REFR",
                            0x0200_0800,
                            0,
                            [
                                sub(b"XTEL", &xtel),
                                sub(b"XESP", &xesp),
                                sub(b"XAPR", &xapr),
                                sub(b"XAPR", &xapr),
                            ]
                            .concat(),
                        ),
                        record(
                            b"REFR",
                            0x0200_0806,
                            0,
                            sub(
                                b"XESP",
                                &[0x0300_0802u32.to_le_bytes(), [2, 0, 0, 0]].concat(),
                            ),
                        ),
                    ]
                    .concat(),
                ),
            ),
        ]
        .concat(),
    );
    // Override using a different local master index for Links.esp (1, not 2).
    let mut override_xtel = xtel.clone();
    override_xtel[..4].copy_from_slice(&0x0100_0804u32.to_le_bytes());
    plugin(
        &data,
        "Override.esp",
        &["Skyrim.esm", "Links.esp", "Light.esl"],
        0,
        group(
            6,
            0x0100_0900,
            group(
                9,
                0x0100_0900,
                record(
                    b"REFR",
                    0x0100_0800,
                    0,
                    [
                        sub(b"XTEL", &override_xtel),
                        sub(
                            b"XESP",
                            &[0x0200_0802u32.to_le_bytes(), [1, 0, 0, 0]].concat(),
                        ),
                        sub(
                            b"XAPR",
                            &[0x0100_0803u32.to_le_bytes(), 1.25f32.to_le_bytes()].concat(),
                        ),
                        sub(
                            b"XAPR",
                            &[0x0100_0805u32.to_le_bytes(), 2.5f32.to_le_bytes()].concat(),
                        ),
                    ]
                    .concat(),
                ),
            ),
        ),
    );
    let list = dir.path().join("plugins.txt");
    fs::write(
        &list,
        "Skyrim.esm\nFiller.esm\nFiller2.esm\n*Light.esl\n*Links.esp\n*Override.esp\n",
    )
    .unwrap();
    let output = dir.path().join("modern");
    let mut config = PipelineConfig::new(&data, &output);
    config.plugins_file = Some(list);
    for pass in 0..2 {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let report = AssetPipeline::run_async(config.clone(), tx).await.unwrap();
        drain.await.unwrap();
        assert!(report.complete, "{:?}", report.warnings);
        if pass == 1 {
            assert_eq!(report.converted, 0);
            assert!(report.cache_hits > 0);
        }
        let conn = Connection::open(output.join("skyrim_world.db")).unwrap();
        let mut expected_xtel = xtel.clone();
        expected_xtel[..4].copy_from_slice(&0x0300_0804u32.to_le_bytes());
        for table in ["records", "references"] {
            assert_eq!(
                database_field(&conn, table, 0x0300_0806, b"XESP"),
                [0u32.to_le_bytes(), [2, 0, 0, 0]].concat()
            );
            assert_eq!(
                database_field(&conn, table, 0x0300_0800, b"XTEL"),
                expected_xtel
            );
            assert_eq!(
                database_field(&conn, table, 0x0300_0800, b"XESP"),
                [0xFE00_0802u32.to_le_bytes(), [1, 0, 0, 0]].concat()
            );
            assert_eq!(
                database_fields(&conn, table, 0x0300_0800, b"XAPR"),
                [
                    [0x0300_0803u32.to_le_bytes(), 1.25f32.to_le_bytes()].concat(),
                    [0x0300_0805u32.to_le_bytes(), 2.5f32.to_le_bytes()].concat(),
                ]
            );
        }
        for table in ["records", "cells"] {
            assert_eq!(
                database_field(&conn, table, 0x0300_0900, b"LTMP"),
                0x0300_0901u32.to_le_bytes()
            );
        }
        // Simulate an old pack with plugin-local IDs; the next conversion must
        // replace it even though the source plugins and asset cache are unchanged.
        if pass == 0 {
            let old = converter::esm::extractors::serialize_subrecords(&[(
                b"XTEL".to_vec(),
                override_xtel.clone(),
            )]);
            conn.execute(
                "UPDATE records SET data=?1 WHERE form_id=?2",
                rusqlite::params![old, 0x0300_0800u32],
            )
            .unwrap();
            conn.execute(
                "UPDATE \"references\" SET data=?1 WHERE id=?2",
                rusqlite::params![old, 0x0300_0800u32],
            )
            .unwrap();
        }
    }
}
