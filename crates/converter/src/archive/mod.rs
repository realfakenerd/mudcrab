mod ba2;
mod bsa;

use crate::{
    asset_path::{AssetKind, canonical_asset_path},
    cache::{
        IngestedFile, IngestionCacheEntry, hash_bytes, hash_file, link_or_copy,
        link_or_copy_spilling,
    },
    pipeline::Interrupted,
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail},
};
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// How far an archive extraction has got: files and bytes done, against the archive's own totals
/// (its file table says how many entries it holds and how many bytes each one takes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractionProgress {
    pub completed_files: u64,
    pub completed_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
}

/// Called as an archive is extracted, and reused from its ingestion cache. Extraction runs on many
/// threads, so the callback has to be safe to call from any of them.
pub type ExtractionProgressCallback<'a> = &'a (dyn Fn(ExtractionProgress) + Send + Sync);

/// Asked before each entry of an archive is extracted or restored; `true` means the run was
/// stopped, and the archive is abandoned at that point without recording its cache entry.
/// Checked from the extraction threads, like [`ExtractionProgressCallback`].
pub type StopCheck<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// Returns the [`Interrupted`] error once `stop` says the run was stopped.
fn check_stop(stop: Option<StopCheck<'_>>) -> Result<()> {
    if stop.is_some_and(|stop| stop()) {
        return Err(Interrupted::new().into());
    }
    Ok(())
}

/// How often an extraction reports: every this many files, so a 173,000-file archive sends
/// hundreds of progress events rather than one per file.
const PROGRESS_FILE_STEP: u64 = 512;

/// Counts an archive's finished work and reports it at [`PROGRESS_FILE_STEP`] intervals.
struct ProgressReporter<'a> {
    callback: ExtractionProgressCallback<'a>,
    files: AtomicU64,
    bytes: AtomicU64,
    totals: ExtractionProgress,
}

impl<'a> ProgressReporter<'a> {
    fn new(callback: ExtractionProgressCallback<'a>, total_files: u64, total_bytes: u64) -> Self {
        Self {
            callback,
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            totals: ExtractionProgress {
                completed_files: 0,
                completed_bytes: 0,
                total_files,
                total_bytes,
            },
        }
    }

    /// Records the archive's totals before any of it is written, so the caller learns the
    /// denominator with its first event.
    fn announce(&self) {
        (self.callback)(self.totals);
    }

    fn advance(&self, bytes: u64) {
        let files = self.files.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if files.is_multiple_of(PROGRESS_FILE_STEP) || files == self.totals.total_files {
            (self.callback)(ExtractionProgress {
                completed_files: files,
                completed_bytes: bytes,
                ..self.totals
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveKind {
    Bsa,
    Ba2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFile {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct ExtractionOutcome {
    pub files: Vec<ExtractedFile>,
    pub cache_entry: IngestionCacheEntry,
    pub cache_hit: bool,
}

pub struct ArchiveExtractor;

impl ArchiveExtractor {
    #[allow(clippy::too_many_arguments)]
    pub fn extract_cached(
        archive_path: &Path,
        output_root: &Path,
        previous_cache_root: &Path,
        cache_root: &Path,
        previous: Option<&IngestionCacheEntry>,
        verify_integrity: bool,
        progress: Option<ExtractionProgressCallback<'_>>,
        stop: Option<StopCheck<'_>>,
    ) -> Result<ExtractionOutcome> {
        let source_hash = hash_file(archive_path)?;
        if let Some(entry) = previous.filter(|entry| entry.source_hash == source_hash)
            && let Some(files) = restore_cached_files(
                entry,
                output_root,
                previous_cache_root,
                cache_root,
                verify_integrity,
                progress,
                stop,
            )?
        {
            return Ok(ExtractionOutcome {
                files,
                cache_entry: entry.clone(),
                cache_hit: true,
            });
        }

        let files = extract_reporting(archive_path, output_root, progress, stop)?;
        let cache_entry = IngestionCacheEntry {
            source_hash,
            files: files
                .iter()
                .map(|file| IngestedFile {
                    path: file.path.to_string_lossy().replace('\\', "/"),
                    size: file.bytes_written,
                    hash: file.sha256.clone(),
                })
                .collect(),
        };
        persist_cache_blobs(&files, output_root, cache_root)?;
        Ok(ExtractionOutcome {
            files,
            cache_entry,
            cache_hit: false,
        })
    }

    pub fn extract(archive_path: &Path, output_root: &Path) -> Result<Vec<ExtractedFile>> {
        extract_reporting(archive_path, output_root, None, None)
    }

    pub(crate) fn extract_lod_settings(
        archive_path: &Path,
        output_root: &Path,
    ) -> Result<Vec<ExtractedFile>> {
        extract_selected_reporting(archive_path, output_root, is_lod_setting, None, None)
    }

    pub(crate) fn extract_paths(
        archive_path: &Path,
        output_root: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<Vec<ExtractedFile>> {
        extract_selected_reporting(
            archive_path,
            output_root,
            |path| paths.contains(path),
            None,
            None,
        )
    }
}

/// Extracts every entry of an archive, reporting progress as the archive's file table allows, and
/// stopping before the next entry once `stop` says so.
fn extract_reporting(
    archive_path: &Path,
    output_root: &Path,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Vec<ExtractedFile>> {
    extract_selected_reporting(archive_path, output_root, |_| true, progress, stop)
}

fn extract_selected_reporting(
    archive_path: &Path,
    output_root: &Path,
    include: impl Fn(&Path) -> bool + Sync,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Vec<ExtractedFile>> {
    let file = File::open(archive_path)
        .wrap_err_with(|| format!("failed to open archive {}", archive_path.display()))?;
    // SAFETY: the file remains open and the mapping is read-only for the duration
    // of parsing. No process-local code mutates the archive while it is mapped.
    let bytes = unsafe { Mmap::map(&file) }
        .wrap_err_with(|| format!("failed to map archive {}", archive_path.display()))?;

    match bytes.get(..4) {
        Some(b"BSA\0") => {
            let entries = bsa::iter_raw_entries(&bytes).wrap_err_with(|| {
                format!("failed to parse BSA archive {}", archive_path.display())
            })?;
            let mut seen = BTreeMap::new();
            let entries = entries
                .into_iter()
                .map(|entry| {
                    let relative = safe_relative_path(&entry.name)?;
                    detect_archive_collision(&mut seen, &relative, &entry.name)?;
                    Ok((entry, relative))
                })
                .collect::<Result<Vec<_>>>()?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|(_, relative)| include(relative))
                .collect();
            // A BSA's file records and payloads are both in the mapping, so the bytes it will
            // write are known before the first entry is decompressed.
            let reporter = progress.map(|callback| {
                ProgressReporter::new(
                    callback,
                    entries.len() as u64,
                    entries
                        .iter()
                        .map(|(entry, _)| entry.payload.len() as u64)
                        .sum(),
                )
            });
            if let Some(reporter) = &reporter {
                reporter.announce();
            }
            entries
                .into_par_iter()
                .map(|(entry, relative)| {
                    check_stop(stop)?;
                    let destination = output_root.join(&relative);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let stored_bytes = entry.payload.len() as u64;
                    let data = entry.decompress()?;
                    let bytes_written = data.len() as u64;
                    let sha256 = hash_bytes(&data);

                    atomic_write(&destination, &data)
                        .wrap_err_with(|| format!("failed to extract {}", destination.display()))?;
                    if let Some(reporter) = &reporter {
                        reporter.advance(stored_bytes);
                    }

                    Ok(ExtractedFile {
                        path: relative,
                        bytes_written,
                        sha256,
                    })
                })
                .collect()
        }
        Some(b"BTDX") => {
            let entries =
                ba2::read_entries_matching(&bytes, |name| Ok(include(&safe_relative_path(name)?)))?;
            let mut seen = BTreeMap::new();
            let entries = entries
                .into_iter()
                .map(|(name, data)| {
                    let relative = safe_relative_path(&name)?;
                    detect_archive_collision(&mut seen, &relative, &name)?;
                    Ok((name, data, relative))
                })
                .collect::<Result<Vec<_>>>()?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|(_, _, relative)| include(relative))
                .collect();
            let reporter = progress.map(|callback| {
                ProgressReporter::new(
                    callback,
                    entries.len() as u64,
                    entries.iter().map(|(_, data, _)| data.len() as u64).sum(),
                )
            });
            if let Some(reporter) = &reporter {
                reporter.announce();
            }
            entries
                .into_par_iter()
                .map(|(_name, data, relative)| {
                    check_stop(stop)?;
                    let destination = output_root.join(&relative);
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let bytes_written = data.len() as u64;
                    let sha256 = hash_bytes(&data);

                    atomic_write(&destination, &data)
                        .wrap_err_with(|| format!("failed to extract {}", destination.display()))?;
                    if let Some(reporter) = &reporter {
                        reporter.advance(bytes_written);
                    }

                    Ok(ExtractedFile {
                        path: relative,
                        bytes_written,
                        sha256,
                    })
                })
                .collect()
        }
        _ => bail!("unsupported archive magic in {}", archive_path.display()),
    }
}

fn is_lod_setting(path: &Path) -> bool {
    path.starts_with("lodsettings") && path.extension().is_some_and(|extension| extension == "lod")
}

fn restore_cached_files(
    entry: &IngestionCacheEntry,
    output_root: &Path,
    previous_cache_root: &Path,
    cache_root: &Path,
    verify_integrity: bool,
    progress: Option<ExtractionProgressCallback<'_>>,
    stop: Option<StopCheck<'_>>,
) -> Result<Option<Vec<ExtractedFile>>> {
    for file in &entry.files {
        // Verifying hashes every cached blob, which on a large archive takes as long as a copy.
        check_stop(stop)?;
        let blob = blob_path(previous_cache_root, &file.hash)?;
        if !blob.is_file()
            || fs::metadata(&blob).map_or(true, |metadata| metadata.len() != file.size)
            || (verify_integrity && hash_file(&blob).map_or(true, |hash| hash != file.hash))
        {
            return Ok(None);
        }
    }

    let reporter = progress.map(|callback| {
        ProgressReporter::new(
            callback,
            entry.files.len() as u64,
            entry.files.iter().map(|file| file.size).sum(),
        )
    });
    if let Some(reporter) = &reporter {
        reporter.announce();
    }
    let mut restored = Vec::with_capacity(entry.files.len());
    for file in &entry.files {
        check_stop(stop)?;
        let relative = safe_relative_path(&file.path)?;
        let old_blob = blob_path(previous_cache_root, &file.hash)?;
        let new_blob = blob_path(cache_root, &file.hash)?;
        copy_if_missing(&old_blob, &new_blob)?;
        let destination = output_root.join(&relative);
        share_blob(&new_blob, &destination)?;
        if let Some(reporter) = &reporter {
            reporter.advance(file.size);
        }
        restored.push(ExtractedFile {
            path: relative,
            bytes_written: file.size,
            sha256: file.hash.clone(),
        });
    }
    Ok(Some(restored))
}

fn persist_cache_blobs(
    files: &[ExtractedFile],
    output_root: &Path,
    cache_root: &Path,
) -> Result<()> {
    for file in files {
        let extracted = output_root.join(&file.path);
        let blob = blob_path(cache_root, &file.sha256)?;
        if blob.is_file() {
            // Another entry with the same bytes stored this blob first: make this path a name for
            // it too, so duplicated content is held once.
            share_blob(&blob, &extracted)?;
        } else {
            copy_file(&extracted, &blob)?;
        }
    }
    Ok(())
}

/// Makes `destination` a name for `blob`, a blob in this run's cache that many paths may share
/// (see `link_or_copy_spilling`).
fn share_blob(blob: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    link_or_copy_spilling(blob, destination).wrap_err_with(|| {
        format!(
            "failed to restore cached asset {} to {}",
            blob.display(),
            destination.display()
        )
    })
}

fn blob_path(cache_root: &Path, hash: &str) -> Result<PathBuf> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 cache key");
    }
    Ok(cache_root.join("sha256").join(&hash[..2]).join(hash))
}

fn copy_if_missing(source: &Path, destination: &Path) -> Result<()> {
    if destination.is_file() {
        return Ok(());
    }
    copy_file(source, destination)
}

/// Puts `source`'s bytes at `destination`, replacing any file already there.
///
/// An extracted entry is stored as the `vfs` file and as its cache blob, and both names point at
/// one file (see `link_or_copy` in `cache`), so the destination is replaced, not written through.
fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    link_or_copy(source, destination).wrap_err_with(|| {
        format!(
            "failed to restore cached asset {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

/// Atomically writes data to a file, ensuring the destination is replaced atomically.
fn atomic_write(destination: &Path, data: &[u8]) -> Result<()> {
    let file_name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let temporary =
        destination.with_file_name(format!(".{file_name}.{}.partial", std::process::id()));
    let mut file = File::create(&temporary)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(&temporary, destination)?;
    Ok(())
}

/// Converts an archive-relative path to a safe, relative path, ensuring it does not contain
/// absolute paths or drive letters.
pub(crate) fn safe_relative_path(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() || normalized.contains(':') {
        bail!("archive contains absolute path: {name}");
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => safe.push(part),
            Component::CurDir => {}
            _ => bail!("archive contains unsafe path: {name}"),
        }
    }
    if safe.as_os_str().is_empty() {
        bail!("archive contains an empty path");
    }
    let extension = safe.extension().and_then(|value| value.to_str());
    let kind = extension.and_then(|extension| {
        if extension.eq_ignore_ascii_case("dds") {
            Some(AssetKind::Texture)
        } else if extension.eq_ignore_ascii_case("nif") {
            Some(AssetKind::Mesh)
        } else if extension.eq_ignore_ascii_case("pex") {
            Some(AssetKind::Script)
        } else if extension.eq_ignore_ascii_case("lod") {
            Some(AssetKind::LodSettings)
        } else {
            None
        }
    });
    if let (Some(kind), Some(extension)) = (kind, extension) {
        return canonical_asset_path(name, kind, extension).map(PathBuf::from);
    }
    Ok(safe)
}

fn detect_archive_collision(
    seen: &mut BTreeMap<String, String>,
    relative: &Path,
    original: &str,
) -> Result<()> {
    let key = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if let Some(previous) = seen.insert(key.clone(), original.to_owned()) {
        bail!("archive contains normalized path collision for {key}: {previous} and {original}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_archive_paths() {
        assert_eq!(
            safe_relative_path(r"meshes\actors\wolf.nif").unwrap(),
            PathBuf::from("meshes/actors/wolf.nif")
        );
        assert_eq!(
            safe_relative_path(r"textures\authoring\data\textures\landscape\Rock.DDS").unwrap(),
            PathBuf::from("textures/landscape/rock.dds")
        );
    }

    #[test]
    fn rejects_normalized_collisions_within_an_archive() {
        let mut seen = BTreeMap::new();
        detect_archive_collision(&mut seen, Path::new("textures/a.dds"), "Textures/A.DDS").unwrap();
        assert!(
            detect_archive_collision(&mut seen, Path::new("textures/a.dds"), "textures/a.dds",)
                .is_err()
        );
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_relative_path("../outside.txt").is_err());
        assert!(safe_relative_path("C:/outside.txt").is_err());
        assert!(blob_path(Path::new("cache"), "../../outside").is_err());
    }

    #[test]
    fn rejects_traversal_entries_during_archive_extraction() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("vfs");
        let mut bytes = dummy_content::ba2::general(
            &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
            dummy_content::ba2::Compression::None,
        )
        .unwrap();
        let names_offset = 24 + 36;
        let escaped = b"../../escape";
        bytes[names_offset..names_offset + 2]
            .copy_from_slice(&(escaped.len() as u16).to_le_bytes());
        bytes[names_offset + 2..names_offset + 2 + escaped.len()].copy_from_slice(escaped);

        let archive = directory.path().join("evil.ba2");
        fs::write(&archive, &bytes).unwrap();
        assert!(ArchiveExtractor::extract(&archive, &output).is_err());
        assert!(!directory.path().join("escape").exists());
        assert!(!directory.path().parent().unwrap().join("escape").exists());
    }

    /// Whether the filesystem holding `directory` can hard-link. Where it cannot, `link_or_copy`
    /// falls back to a copy, so a test can only ask for equal bytes there.
    fn hard_links_supported(directory: &Path) -> bool {
        let probe = directory.join(".link-probe");
        let link = directory.join(".link-probe-link");
        fs::write(&probe, b"probe").unwrap();
        let supported = fs::hard_link(&probe, &link).is_ok();
        let _ = fs::remove_file(&link);
        fs::remove_file(&probe).unwrap();
        supported
    }

    /// Asserts every name in `names` is one file, by appending a byte through the first name and
    /// watching it appear through all the others: a hard link sees a write made through another
    /// name, a copy does not. On a filesystem without hard links the names are copies by design,
    /// and only their bytes are compared.
    fn assert_one_file(names: &[&Path]) {
        let (first, rest) = names.split_first().expect("at least one name");
        if !hard_links_supported(first.parent().expect("a file inside a directory")) {
            let bytes = fs::read(first).unwrap();
            for name in rest {
                assert_eq!(fs::read(name).unwrap(), bytes, "{} differs", name.display());
            }
            return;
        }
        fs::OpenOptions::new()
            .append(true)
            .open(first)
            .unwrap()
            .write_all(b"+")
            .unwrap();
        let bytes = fs::read(first).unwrap();
        assert!(
            bytes.last() == Some(&b'+'),
            "{} was not written",
            first.display()
        );
        for name in rest {
            assert_eq!(
                fs::read(name).unwrap(),
                bytes,
                "{} does not share the file at {}",
                name.display(),
                first.display()
            );
        }
    }

    #[test]
    fn fresh_extractions_link_blobs_and_vfs_files_where_the_filesystem_can() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let output = directory.path().join("vfs");
        let cache = directory.path().join(".ingestion-cache");
        let extracted = ArchiveExtractor::extract_cached(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!extracted.cache_hit);

        // A fresh install stores each extracted file as the `vfs` entry and as its blob; the two
        // names have to be one file, or the tree holds every asset twice. NTFS, ext4 and APFS
        // support links, so extraction must not have fallen back to copying.
        let vfs = output.join("textures/test.dds");
        let blob = blob_path(&cache, &extracted.files[0].sha256).unwrap();
        assert_one_file(&[&vfs, &blob]);
        assert!(fs::read(&vfs).unwrap().starts_with(b"DDS "));
    }

    #[test]
    fn fresh_extractions_link_every_path_with_the_same_bytes_to_one_blob() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[
                    dummy_content::Entry::new("textures/first.dds", b"DDS "),
                    dummy_content::Entry::new("textures/second.dds", b"DDS "),
                ],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let output = directory.path().join("vfs");
        let cache = directory.path().join(".ingestion-cache");
        let extracted = ArchiveExtractor::extract_cached(
            &archive,
            &output,
            Path::new("unused"),
            &cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!extracted.cache_hit);
        assert_eq!(extracted.files[0].sha256, extracted.files[1].sha256);

        // Both entries hold the same bytes, so both `vfs` paths are names for their one blob.
        let blob = blob_path(&cache, &extracted.files[0].sha256).unwrap();
        let first = output.join("textures/first.dds");
        let second = output.join("textures/second.dds");
        assert_one_file(&[&blob, &first, &second]);
    }

    #[test]
    fn cache_hits_link_blobs_and_vfs_files_where_the_filesystem_can() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let first_output = directory.path().join("first/vfs");
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &first_output,
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!first.cache_hit);

        let second_output = directory.path().join("second/vfs");
        let second_cache = directory.path().join("second/.ingestion-cache");
        let second = ArchiveExtractor::extract_cached(
            &archive,
            &second_output,
            &first_cache,
            &second_cache,
            Some(&first.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);

        // The reused blob, the blob restored into the new cache and the `vfs` entries of both
        // runs all describe the same bytes, so they are all one file.
        let hash = &second.files[0].sha256;
        let first_blob = blob_path(&first_cache, hash).unwrap();
        let second_blob = blob_path(&second_cache, hash).unwrap();
        let first_vfs = first_output.join("textures/test.dds");
        let second_vfs = second_output.join("textures/test.dds");
        assert_one_file(&[&first_blob, &second_blob, &first_vfs, &second_vfs]);
        assert!(fs::read(&second_vfs).unwrap().starts_with(b"DDS "));
    }

    #[test]
    fn reuses_verified_archive_blobs_and_recovers_from_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("assets.ba2");
        fs::write(
            &archive,
            dummy_content::ba2::general(
                &[dummy_content::Entry::new("textures/test.dds", b"DDS ")],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();

        let first_output = directory.path().join("first/vfs");
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &archive,
            &first_output,
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!first.cache_hit);
        assert_eq!(
            fs::read(first_output.join("textures/test.dds")).unwrap(),
            b"DDS "
        );

        let second_output = directory.path().join("second/vfs");
        let second_cache = directory.path().join("second/.ingestion-cache");
        let second = ArchiveExtractor::extract_cached(
            &archive,
            &second_output,
            &first_cache,
            &second_cache,
            Some(&first.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(second.cache_hit);
        assert_eq!(
            fs::read(second_output.join("textures/test.dds")).unwrap(),
            b"DDS "
        );

        let blob = blob_path(&second_cache, &second.files[0].sha256).unwrap();
        fs::write(&blob, b"BAD!").unwrap();
        let third = ArchiveExtractor::extract_cached(
            &archive,
            &directory.path().join("third/vfs"),
            &second_cache,
            &directory.path().join("third/.ingestion-cache"),
            Some(&second.cache_entry),
            true,
            None,
            None,
        )
        .unwrap();
        assert!(!third.cache_hit);
    }

    /// A stop that turns true after `allowed` checks, so a test can stop an archive at a known
    /// entry.
    fn stop_after(allowed: usize) -> impl Fn() -> bool + Send + Sync {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        move || checks.fetch_add(1, Ordering::Relaxed) >= allowed
    }

    fn count_files(root: &Path) -> usize {
        if !root.exists() {
            return 0;
        }
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter(|entry| entry.as_ref().unwrap().file_type().is_file())
            .count()
    }

    #[test]
    fn a_stop_ends_extraction_and_cache_restore_between_entries() {
        let directory = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..8).map(|index| format!("docs/{index}.txt")).collect();
        let entries: Vec<_> = names
            .iter()
            .map(|name| dummy_content::Entry::new(name, name.as_bytes()))
            .collect();
        let ba2 = directory.path().join("assets.ba2");
        fs::write(
            &ba2,
            dummy_content::ba2::general(&entries, dummy_content::ba2::Compression::None).unwrap(),
        )
        .unwrap();
        let bsa = directory.path().join("assets.bsa");
        fs::write(
            &bsa,
            dummy_content::bsa::v105(&entries, dummy_content::bsa::Compression::None).unwrap(),
        )
        .unwrap();

        // Extraction: one entry is let through, the rest are never started, and nothing of the
        // archive reaches the ingestion cache.
        for archive in [&ba2, &bsa] {
            let output = directory.path().join("stopped/vfs");
            let cache = directory.path().join("stopped/.ingestion-cache");
            let stop = stop_after(1);
            let error = ArchiveExtractor::extract_cached(
                archive,
                &output,
                Path::new("unused"),
                &cache,
                None,
                true,
                None,
                Some(&stop),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("interrupted"));
            assert_eq!(count_files(&output), 1, "{}", archive.display());
            assert_eq!(count_files(&cache), 0, "{}", archive.display());
            fs::remove_dir_all(directory.path().join("stopped")).unwrap();
        }

        // Cache restore: a full run fills the cache, then a restore from it is stopped after
        // its verification pass and three copies.
        let first_cache = directory.path().join("first/.ingestion-cache");
        let first = ArchiveExtractor::extract_cached(
            &ba2,
            &directory.path().join("first/vfs"),
            Path::new("unused"),
            &first_cache,
            None,
            true,
            None,
            None,
        )
        .unwrap();
        let output = directory.path().join("second/vfs");
        let stop = stop_after(names.len() + 3);
        let error = ArchiveExtractor::extract_cached(
            &ba2,
            &output,
            &first_cache,
            &directory.path().join("second/.ingestion-cache"),
            Some(&first.cache_entry),
            true,
            None,
            Some(&stop),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        assert_eq!(count_files(&output), 3);

        // A stop that never fires changes nothing.
        let never = || false;
        let complete = ArchiveExtractor::extract_cached(
            &ba2,
            &directory.path().join("third/vfs"),
            Path::new("unused"),
            &directory.path().join("third/.ingestion-cache"),
            None,
            true,
            None,
            Some(&never),
        )
        .unwrap();
        assert_eq!(complete.files.len(), names.len());
    }
}
