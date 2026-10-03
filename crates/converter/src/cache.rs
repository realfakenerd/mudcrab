use color_eyre::{Result, eyre::WrapErr};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const CONVERTER_SCHEMA_VERSION: u32 = 17;

/// Provenance journal the converter keeps inside a staging directory.
///
/// A staging directory outlives the run that filled it, so a resumed run finds
/// outputs this process did not write. The journal records, per output, the
/// source hash, the converter schema and the configuration hash it was produced
/// under, together with the output's size and hash, appended as the output is
/// written. A resumed run reuses a staged output only while its record still
/// matches the current source, schema and configuration and the bytes on disk.
/// The name is dotted so it can never collide with a converted asset, and the
/// file is dropped from the output directory once the staging directory has
/// been published.
pub const STAGING_JOURNAL_FILE: &str = ".conversion-staging-journal.jsonl";

/// What a staged output was produced from, as recorded when it was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedOutput {
    pub schema_version: u32,
    pub configuration_hash: String,
    pub source_hash: String,
    pub output_size: u64,
    pub output_hash: String,
}

impl StagedOutput {
    /// True when the file at `path` is the output this record describes,
    /// produced from `source_hash` by the current converter schema and
    /// configuration.
    pub fn is_current(&self, path: &Path, source_hash: &str, configuration_hash: &str) -> bool {
        self.schema_version == CONVERTER_SCHEMA_VERSION
            && self.configuration_hash == configuration_hash
            && self.source_hash == source_hash
            && fs::metadata(path).is_ok_and(|metadata| metadata.len() == self.output_size)
            && hash_file(path).is_ok_and(|hash| hash == self.output_hash)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StagingJournalLine {
    key: String,
    #[serde(flatten)]
    output: StagedOutput,
}

#[cfg(test)]
thread_local! {
    /// Makes every journal write on this thread fail, for tests of the
    /// pipeline's error path. The batch loop records on the test's own thread.
    pub(crate) static FAIL_JOURNAL_WRITES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

pub struct StagingJournal {
    path: PathBuf,
    file: fs::File,
}

impl StagingJournal {
    pub fn path_in(staging: &Path) -> PathBuf {
        staging.join(STAGING_JOURNAL_FILE)
    }

    /// Opens the journal belonging to `staging`, creating it when the directory
    /// has none yet. A run killed mid-append leaves a partial last line; it is
    /// ended here, so the next record starts a line of its own and only the
    /// partial one is dropped when the journal is read.
    pub fn open(staging: &Path) -> Result<Self> {
        let path = Self::path_in(staging);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .wrap_err_with(|| format!("failed to open staging journal {}", path.display()))?;
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0_u8];
            file.seek(SeekFrom::Start(length - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                // Append mode writes at the end whatever the read position.
                file.write_all(b"\n")
                    .wrap_err_with(|| format!("failed to repair {}", path.display()))?;
            }
        }
        Ok(Self { path, file })
    }

    /// Appends one output's provenance. Each record reaches the journal in a
    /// single write, so a run killed mid-append loses at most the last record,
    /// and the output it describes is converted again. The journal is not
    /// fsynced after each record, so that holds for a crashed or killed process;
    /// a power loss can lose more of the unsynced tail, and those outputs are
    /// converted again too.
    pub fn record(&mut self, key: &str, output: &StagedOutput) -> Result<()> {
        #[cfg(test)]
        if FAIL_JOURNAL_WRITES.with(std::cell::Cell::get) {
            color_eyre::eyre::bail!("injected journal write failure");
        }
        let mut line = serde_json::to_vec(&StagingJournalLine {
            key: key.to_owned(),
            output: output.clone(),
        })?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .wrap_err_with(|| format!("failed to append to {}", self.path.display()))
    }
}

/// Reads the records a previous run left in `staging`, keyed by canonical
/// source key. The last record for a key wins, so an output converted by this
/// run replaces the record of the run it resumes. A truncated or otherwise
/// unreadable line is dropped with a warning: the output it described has no
/// provenance and is converted again.
pub fn load_staged_outputs(staging: &Path) -> Result<BTreeMap<String, StagedOutput>> {
    let mut records = BTreeMap::new();
    let path = StagingJournal::path_in(staging);
    if !path.is_file() {
        return Ok(records);
    }
    let bytes = fs::read(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let mut dropped = 0u32;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<StagingJournalLine>(line) {
            Ok(parsed) => {
                records.insert(parsed.key, parsed.output);
            }
            Err(_) => dropped += 1,
        }
    }
    if dropped > 0 {
        eprintln!(
            "warning: dropped {dropped} unreadable record(s) from {}; those outputs are converted again",
            path.display()
        );
    }
    Ok(records)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub source_hash: String,
    pub output: String,
    pub output_size: u64,
    pub output_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestedFile {
    pub path: String,
    pub size: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngestionCacheEntry {
    pub source_hash: String,
    pub files: Vec<IngestedFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversionManifest {
    pub schema_version: u32,
    /// Metadata-only rebuilds do not upgrade the retained mesh cache contract.
    /// Absent means the meshes follow `schema_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_mesh_schema_version: Option<u32>,
    /// Producer settings of retained bytes, independent of rebuilt metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_asset_configuration_hash: Option<String>,
    pub complete: bool,
    #[serde(default)]
    pub configuration_hash: String,
    #[serde(default)]
    pub inputs_by_kind: BTreeMap<String, u64>,
    #[serde(default)]
    pub failures: BTreeMap<String, String>,
    /// Texture references a published mesh omits because the game data does not
    /// contain that texture, keyed by the published `.glb` and holding the
    /// resolved texture paths it dropped. Kept out of `failures`: nothing failed
    /// to convert, so these do not make the conversion incomplete.
    ///
    /// This is an audit record for whoever reads the published manifest: the
    /// engine and launcher accept an asset set on `complete` plus the converter
    /// schema version, and nothing else in the workspace reads this list.
    #[serde(default)]
    pub pruned_texture_references: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub archives: BTreeMap<String, IngestionCacheEntry>,
    pub entries: BTreeMap<String, CacheEntry>,
}

impl ConversionManifest {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
        }
        let bytes =
            fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let mut manifest: Self =
            serde_json::from_slice(&bytes).wrap_err("invalid conversion manifest")?;
        let retained_meshes_are_stale = manifest.schema_version == CONVERTER_SCHEMA_VERSION
            && match manifest.retained_mesh_schema_version {
                Some(schema) => !(16..=CONVERTER_SCHEMA_VERSION).contains(&schema),
                None => path
                    .parent()
                    .is_some_and(|root| root.join("metadata-rebuild.json").exists()),
            };
        if (matches!(manifest.schema_version, 12..=15) && CONVERTER_SCHEMA_VERSION == 17)
            || retained_meshes_are_stale
        {
            // Schema 16 adds authored collision; schema 17 does not change mesh output.
            // Preserve verified non-mesh assets, but rebuild GLBs and world data.
            manifest.complete = false;
            manifest
                .entries
                .retain(|_, entry| !entry.output.to_ascii_lowercase().ends_with(".glb"));
            return Ok(manifest);
        }
        if manifest.schema_version != CONVERTER_SCHEMA_VERSION && manifest.schema_version != 16 {
            return Ok(Self {
                schema_version: CONVERTER_SCHEMA_VERSION,
                ..Self::default()
            });
        }
        Ok(manifest)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        let temporary = path.with_extension(format!("json.{}.partial", std::process::id()));
        let mut file = fs::File::create(&temporary)
            .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
            .wrap_err_with(|| format!("failed to publish {}", path.display()))
    }
}

pub fn configuration_hash(config: &crate::config::PipelineConfig) -> Result<String> {
    configuration_hash_for_schema(config, CONVERTER_SCHEMA_VERSION)
}

pub fn configuration_hash_for_schema(
    config: &crate::config::PipelineConfig,
    schema: u32,
) -> Result<String> {
    let mut relevant = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_fallback_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "script_abi_version": config.script_abi_version,
    });
    if schema >= 16 {
        relevant["texture_zstd_level"] = serde_json::json!(config.texture_zstd_level);
    }
    Ok(hash_bytes(&serde_json::to_vec(&relevant)?))
}

/// Puts `from`'s bytes at `to` as a hard link where the filesystem allows one, else as a copy.
///
/// Every extracted archive entry is stored twice: once under `vfs` and once as the
/// content-addressed blob in `.ingestion-cache` (a cache hit repeats that split). Copying them
/// stored each asset twice, which is tens of gigabytes of game data for a full conversion.
/// Linking costs nothing where the filesystem supports it (NTFS, ext4 and APFS do) and is
/// impossible otherwise, so a cross-volume or linkless filesystem falls back to a copy.
///
/// Linking is safe because nothing writes a shared file in place: extraction and the loose-asset
/// overlay replace the path (unlink, then write or copy over the new, unshared file) and no cache
/// blob is ever written in place. An existing `to` is removed rather than written through, and is
/// left alone when it already names `from`. The destination's parent directory must exist.
pub(crate) fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    link_or_copy_with(from, to, |from, to| fs::hard_link(from, to))
}

/// `link_or_copy` with the link step injected, so a test can force the copy fallback on a
/// filesystem that has links.
fn link_or_copy_with(
    from: &Path,
    to: &Path,
    link: fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if to.exists() {
        // Removing `to` would delete `from` itself when `to` is the same path as `from`, so that
        // state is success rather than a reason to touch anything.
        let names_one_file = match fs::canonicalize(from) {
            Ok(from) => fs::canonicalize(to).is_ok_and(|to| to == from),
            Err(_) => false,
        };
        if names_one_file {
            return Ok(());
        }
        fs::remove_file(to)?;
    }
    link(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))
}

/// How many spill copies of one blob `link_or_copy_spilling` makes before it gives up and copies.
const MAX_SPILLS: u32 = 64;

/// Like [`link_or_copy`], for a blob that many paths share.
///
/// A file can carry only so many names (1,024 on NTFS), and some game content is stored under
/// thousands of paths (terrain and face textures), more again during a reconversion, while the
/// previous output still holds its own names. When the blob is full, the destination is linked to
/// a spill copy beside it (`<blob>.1`, `<blob>.2`, ...), made once and shared by the next thousand
/// or so paths, instead of each path becoming its own copy. `blob` must be a file this run owns:
/// the spill copies are written beside it.
pub(crate) fn link_or_copy_spilling(blob: &Path, to: &Path) -> std::io::Result<()> {
    link_or_copy_spilling_with(blob, to, |from, to| fs::hard_link(from, to))
}

fn link_or_copy_spilling_with(
    blob: &Path,
    to: &Path,
    link: fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if same_path(blob, to) {
        return Ok(());
    }
    if to.exists() {
        fs::remove_file(to)?;
    }
    match link(blob, to) {
        Ok(()) => return Ok(()),
        Err(error) if !is_too_many_links(&error) => return fs::copy(blob, to).map(|_| ()),
        Err(_) => {}
    }
    for index in 1..=MAX_SPILLS {
        let mut name = blob.as_os_str().to_owned();
        name.push(format!(".{index}"));
        let spill = std::path::PathBuf::from(name);
        if !spill.is_file() {
            // Written under a temporary name and renamed, so a reader never sees half a spill.
            let mut partial = spill.as_os_str().to_owned();
            partial.push(format!(".partial-{}", std::process::id()));
            let partial = std::path::PathBuf::from(partial);
            fs::copy(blob, &partial)?;
            fs::rename(&partial, &spill)?;
        }
        match link(&spill, to) {
            Ok(()) => return Ok(()),
            Err(error) if is_too_many_links(&error) => continue,
            Err(_) => break,
        }
    }
    fs::copy(blob, to).map(|_| ())
}

/// A link refused because the file already has as many names as the filesystem allows.
fn is_too_many_links(error: &std::io::Error) -> bool {
    // ERROR_TOO_MANY_LINKS on Windows; EMLINK elsewhere.
    error.kind() == std::io::ErrorKind::TooManyLinks || error.raw_os_error() == Some(1142)
}

/// Whether `from` and `to` are the same path (removing `to` would then delete `from`).
fn same_path(from: &Path, to: &Path) -> bool {
    match fs::canonicalize(from) {
        Ok(from) => fs::canonicalize(to).is_ok_and(|to| to == from),
        Err(_) => false,
    }
}

/// The native collision producer includes its fixed Zstd level in schema 16.
/// This compatibility route verifies retained bytes, not normal cache reuse.
pub(crate) fn retained_configuration_matches(
    config: &crate::config::PipelineConfig,
    schema: u32,
    recorded: &str,
) -> Result<bool> {
    if recorded == configuration_hash_for_schema(config, schema)? {
        return Ok(true);
    }
    if schema != 16 {
        return Ok(false);
    }
    let native = serde_json::json!({
        "schema": schema,
        "texture_etc1s_quality": config.texture_fallback_quality,
        "texture_uastc_level": config.texture_uastc_level,
        "texture_zstd_level": 6,
        "script_abi_version": config.script_abi_version,
    });
    Ok(recorded == hash_bytes(&serde_json::to_vec(&native)?))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path)
        .wrap_err_with(|| format!("failed to open {} for hashing", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .wrap_err_with(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn manifests_written_before_pruned_reference_tracking_still_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conversion-manifest.json");
        // Every key but `pruned_texture_references`, exactly as manifests were
        // written before that field existed.
        fs::write(
            &path,
            format!(
                r#"{{
                    "schema_version": {CONVERTER_SCHEMA_VERSION},
                    "complete": true,
                    "configuration_hash": "configuration",
                    "inputs_by_kind": {{"nif": 4}},
                    "failures": {{}},
                    "archives": {{}},
                    "entries": {{}}
                }}"#
            ),
        )
        .unwrap();

        let manifest = ConversionManifest::load(&path).unwrap();

        assert_eq!(manifest.schema_version, CONVERTER_SCHEMA_VERSION);
        assert!(manifest.complete);
        assert_eq!(manifest.configuration_hash, "configuration");
        assert_eq!(manifest.inputs_by_kind.get("nif"), Some(&4));
        assert!(manifest.pruned_texture_references.is_empty());
    }

    #[test]
    fn journal_keeps_the_last_record_and_drops_a_truncated_tail() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let stale = StagedOutput {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: "config".to_owned(),
            source_hash: "stale".to_owned(),
            output_size: 4,
            output_hash: "stale".to_owned(),
        };
        let current = StagedOutput {
            source_hash: "current".to_owned(),
            ..stale.clone()
        };
        let mut journal = StagingJournal::open(staging).unwrap();
        journal.record("scripts/one.pex", &stale).unwrap();
        journal.record("scripts/one.pex", &current).unwrap();
        journal.record("scripts/two.pex", &stale).unwrap();
        drop(journal);

        // A run killed mid-append leaves a partial line behind.
        let path = StagingJournal::path_in(staging);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"key\":\"scripts/three.pex\",\"schema_ver");
        fs::write(&path, &bytes).unwrap();

        let records = load_staged_outputs(staging).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records["scripts/one.pex"], current);
        assert_eq!(records["scripts/two.pex"], stale);
        assert!(!records.contains_key("scripts/three.pex"));

        // A resumed run appends after the partial line without losing its record.
        let mut journal = StagingJournal::open(staging).unwrap();
        journal.record("scripts/four.pex", &current).unwrap();
        drop(journal);
        let records = load_staged_outputs(staging).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records["scripts/four.pex"], current);
        assert!(!records.contains_key("scripts/three.pex"));

        let missing = directory.path().join("absent");
        assert!(load_staged_outputs(&missing).unwrap().is_empty());
    }

    #[test]
    fn link_or_copy_leaves_a_file_that_already_names_its_source_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rock.dds");
        fs::write(&path, b"archive bytes").unwrap();

        // `from` and `to` are the same path: removing it first would delete the only copy.
        link_or_copy(&path, &path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"archive bytes");
    }

    #[test]
    fn link_or_copy_accepts_a_destination_that_is_already_linked_to_its_source() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");
        fs::hard_link(&blob, &vfs).unwrap();

        link_or_copy(&blob, &vfs).unwrap();

        assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
    }

    #[test]
    fn a_full_blob_shares_one_spill_copy_instead_of_copying_per_path() {
        // Stands in for ERROR_TOO_MANY_LINKS: the original blob takes no more names, a spill does.
        fn blob_is_full(from: &Path, to: &Path) -> std::io::Result<()> {
            if from.extension().is_none() {
                return Err(std::io::Error::from(std::io::ErrorKind::TooManyLinks));
            }
            fs::hard_link(from, to)
        }
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"terrain").unwrap();
        let first = directory.path().join("first.dds");
        let second = directory.path().join("second.dds");

        link_or_copy_spilling_with(&blob, &first, blob_is_full).unwrap();
        link_or_copy_spilling_with(&blob, &second, blob_is_full).unwrap();

        // Both paths name the one spill copy: a write through one shows in the other, and the
        // blob itself is untouched.
        let spill = directory.path().join("blob.1");
        assert!(spill.is_file());
        fs::OpenOptions::new()
            .append(true)
            .open(&first)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"terrain+");
        assert_eq!(fs::read(&spill).unwrap(), b"terrain+");
        assert_eq!(fs::read(&blob).unwrap(), b"terrain");
    }

    #[test]
    fn a_link_refused_for_another_reason_still_copies_without_spilling() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");

        link_or_copy_spilling_with(&blob, &vfs, |_, _| {
            Err(std::io::Error::other("cross-volume"))
        })
        .unwrap();

        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
        assert!(!directory.path().join("blob.1").exists());
    }

    #[test]
    fn link_or_copy_copies_when_the_filesystem_refuses_a_link() {
        let directory = tempfile::tempdir().unwrap();
        let blob = directory.path().join("blob");
        fs::write(&blob, b"archive bytes").unwrap();
        let vfs = directory.path().join("rock.dds");

        link_or_copy_with(&blob, &vfs, |_, _| {
            Err(std::io::Error::other("no hard links here"))
        })
        .unwrap();

        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes");
        // The fallback is a copy, not a second name for the blob: writing through one must not
        // reach the other.
        fs::OpenOptions::new()
            .append(true)
            .open(&vfs)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        assert_eq!(fs::read(&blob).unwrap(), b"archive bytes");
        assert_eq!(fs::read(&vfs).unwrap(), b"archive bytes+");
    }

    #[test]
    fn recent_schema_migrations_reuse_only_unchanged_asset_kinds() {
        for schema_version in [12, 13, 14, 15, 16] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version,
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.glb", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.to_owned(),
                    CacheEntry {
                        source_hash: "source".to_owned(),
                        output: output.to_owned(),
                        output_size: 1,
                        output_hash: "output".to_owned(),
                    },
                );
            }
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

            let migrated = ConversionManifest::load(&path).unwrap();

            assert_eq!(migrated.schema_version, schema_version);
            assert_eq!(migrated.complete, schema_version == 16);
            assert_eq!(
                migrated.entries.contains_key("meshes/a.glb"),
                schema_version == 16
            );
            assert!(migrated.entries.contains_key("textures/a.ktx2"));
            assert!(migrated.entries.contains_key("scripts/a.luau"));
        }
    }

    #[test]
    fn current_metadata_never_promotes_an_older_or_unknown_mesh_contract() {
        for mesh_schema in [15, 18] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("conversion-manifest.json");
            let mut manifest = ConversionManifest {
                schema_version: CONVERTER_SCHEMA_VERSION,
                retained_mesh_schema_version: Some(mesh_schema),
                complete: true,
                ..ConversionManifest::default()
            };
            for output in ["meshes/a.GLB", "textures/a.ktx2", "scripts/a.luau"] {
                manifest.entries.insert(
                    output.to_owned(),
                    CacheEntry {
                        source_hash: "source".to_owned(),
                        output: output.to_owned(),
                        output_size: 1,
                        output_hash: "output".to_owned(),
                    },
                );
            }
            manifest.save(&path).unwrap();
            let eligible = ConversionManifest::load(&path).unwrap();
            assert!(!eligible.complete);
            assert!(!eligible.entries.contains_key("meshes/a.GLB"));
            assert!(eligible.entries.contains_key("textures/a.ktx2"));
            assert!(eligible.entries.contains_key("scripts/a.luau"));
        }
    }
}
