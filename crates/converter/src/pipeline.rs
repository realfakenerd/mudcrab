use crate::{
    archive::{ArchiveExtractor, ExtractionProgress},
    asset_path::{AssetKind, canonical_asset_path, resolve_asset_uri},
    cache::{
        CONVERTER_SCHEMA_VERSION, CacheEntry, ConversionManifest, StagedOutput, StagingJournal,
        configuration_hash, configuration_hash_for_schema, hash_bytes, hash_file, link_or_copy,
        load_staged_outputs,
    },
    config::{PipelineConfig, TextureEncoder},
    esm::{
        EsmParser,
        cell_cache::write_cell_cache,
        exporter::validate_database,
        lodsettings::{LodSettings, sidecar_path},
        read_plugins_txt,
    },
    integration::{IntegrationReport, finalize_world_database},
    lod::{
        albedo::TerrainTextures,
        terrain::{
            compile_world_terrain, exterior_terrain_cells, publish_chunks, read_cached_heights,
        },
    },
    mesh::{MeshConverter, nif_source_hash},
    progress::{AssetOutcome, ProgressEvent, ProgressStage},
    script::ScriptConverter,
    texture::{TextureConverter, TextureEncoding, TextureSemantic, publish_ktx2_file},
    texture_gpu::{self, GpuJob, GpuUastc, PreparedTexture},
};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use shared::{
    asset_lock::{AssetLock, AssetLockMode, resolve_asset_path},
    lod::LodOrigin,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::mpsc::{Sender, unbounded_channel},
    task::spawn_blocking,
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineReport {
    pub complete: bool,
    pub converted: u64,
    pub cache_hits: u64,
    pub skipped: u64,
    /// Dangling texture references the published meshes omit because the game
    /// data does not contain those textures: references this run pruned plus the
    /// records it carried forward for meshes it reused. Matches the
    /// `pruned_texture_references` map published in the manifest. Counted
    /// separately from `skipped` and `warnings`: nothing failed to convert, so a
    /// prune never makes the run incomplete.
    pub pruned_texture_references: u64,
    /// Terrain LOD chunks compiled this run. Zero when no worldspace had a
    /// valid origin; a world without one is recorded as an LOD omission, never
    /// given an assumed origin (GEOM-02).
    pub lod_chunks: u64,
    #[serde(default)]
    pub lod_warnings: Vec<String>,
    pub warnings: Vec<String>,
    /// advisory messages that do not affect completeness
    #[serde(default)]
    pub notices: Vec<String>,
    pub artifacts: Vec<PathBuf>,
    pub inputs_by_kind: BTreeMap<String, u64>,
    pub elapsed_ms: u128,
    pub integration: Option<IntegrationReport>,
}

/// A run is complete when nothing was skipped and nothing warned. Pruned dangling
/// texture references are deliberately absent: the game data does not contain those
/// textures, so dropping the reference is a fact about the source, not a failure to
/// convert. A failed archive, a mesh that will not convert or a failed integration
/// still skip or warn, so they still land here.
fn conversion_is_complete(report: &PipelineReport) -> bool {
    report.skipped == 0 && report.warnings.is_empty()
}

/// A run's stop button. The command line sets it when the user interrupts (Ctrl+C); the pipeline
/// checks it between units of work, finishes the asset in flight, and leaves the staging folder
/// behind so `--resume-staging` can pick the run up where it stopped.
#[derive(Clone, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// The error a run ends with when it was stopped (Ctrl+C, a launcher's Stop button) rather than
/// failing. A stop that raced a real error keeps that error as its cause, so a front end can still
/// show it; a plain stop has no cause. Front ends find it with `report.downcast_ref::<Interrupted>()`
/// instead of matching the error text.
#[derive(Debug, Default)]
pub struct Interrupted {
    cause: Option<color_eyre::Report>,
}

impl Interrupted {
    /// A stop with no error behind it.
    pub fn new() -> Self {
        Self::default()
    }

    /// A stop that landed while `error` was ending the same work. An `error` that is itself a
    /// plain stop (the extractor noticed the stop first) adds nothing, so it is not kept as a cause.
    pub fn after(error: color_eyre::Report) -> Self {
        match error.downcast_ref::<Interrupted>() {
            Some(stop) if stop.cause.is_none() => Self::new(),
            _ => Self { cause: Some(error) },
        }
    }

    /// The error that raced the stop, if there was one.
    pub fn cause(&self) -> Option<&color_eyre::Report> {
        self.cause.as_ref()
    }
}

impl fmt::Display for Interrupted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Some(cause) => write!(formatter, "conversion interrupted ({cause:#})"),
            None => formatter.write_str("conversion interrupted"),
        }
    }
}

impl std::error::Error for Interrupted {}

/// A run that stopped short of publishing. `staging` is the folder left behind, when one was kept,
/// and `cancelled` says whether the user interrupted the run rather than it failing.
#[derive(Debug)]
pub struct PipelineFailure {
    pub error: color_eyre::Report,
    pub staging: Option<PathBuf>,
    pub cancelled: bool,
}

impl fmt::Display for PipelineFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#}", self.error)
    }
}

impl std::error::Error for PipelineFailure {}

impl From<color_eyre::Report> for PipelineFailure {
    fn from(error: color_eyre::Report) -> Self {
        Self {
            error,
            staging: None,
            cancelled: false,
        }
    }
}

/// Renames the staged tree over the output, unless the run was interrupted while it was packing
/// up. The check here is the last one before the output changes, so an interrupt that lands after
/// the final stage stops the run instead of publishing over a good output — the staging folder is
/// kept either way, so the run resumes from where it stopped.
#[cfg(test)]
fn publish_if_not_interrupted(
    staging: &Path,
    output: &Path,
    cancellation: &Cancellation,
) -> Result<(), PipelineFailure> {
    if cancellation.is_cancelled() {
        return Err(failure(Interrupted::new().into(), staging, cancellation));
    }
    publish_directory(staging, output).map_err(|error| failure(error, staging, cancellation))
}

/// A failure with the staging folder attached when one was kept, so the caller can tell the user
/// how to resume.
fn failure(
    error: color_eyre::Report,
    staging: &Path,
    cancellation: &Cancellation,
) -> PipelineFailure {
    PipelineFailure {
        error,
        staging: staging.exists().then(|| staging.to_path_buf()),
        cancelled: cancellation.is_cancelled(),
    }
}

pub struct AssetPipeline;

impl AssetPipeline {
    /// Runs a conversion, reporting on `progress_tx`.
    ///
    /// The channel is bounded, and a front end should keep reading it. Events that matter on their
    /// own (a stage or an archive starting and ending, an asset failing, a notice) wait for room;
    /// the per-file updates sent while an archive is extracted are dropped when the channel is
    /// full, so extraction never waits on the front end. Each of them carries cumulative numbers,
    /// so the next one that gets through is as good as any that were dropped.
    pub async fn run_async(
        config: PipelineConfig,
        progress_tx: Sender<ProgressEvent>,
    ) -> Result<PipelineReport, PipelineFailure> {
        Self::run_async_with_cancel(config, progress_tx, Cancellation::new()).await
    }

    pub async fn run_async_with_cancel(
        mut config: PipelineConfig,
        progress_tx: Sender<ProgressEvent>,
        cancellation: Cancellation,
    ) -> Result<PipelineReport, PipelineFailure> {
        config.validate()?;
        reject_symlink_output(&config.output_dir)?;
        config.output_dir =
            resolve_asset_path(&config.output_dir).wrap_err("failed to resolve asset output")?;
        fs::create_dir_all(
            config
                .output_dir
                .parent()
                .ok_or_else(|| color_eyre::eyre::eyre!("output has no parent"))?,
        )
        .wrap_err("failed to create asset output parent")?;
        let output_lock = AssetLock::acquire_exclusive(&config.output_dir).map_err(|error| {
            let message = if error.is_held() {
                format!(
                    "asset output {} is in use; close its engine or inspector before conversion",
                    config.output_dir.display()
                )
            } else {
                format!(
                    "failed to lock asset output {} before conversion",
                    config.output_dir.display()
                )
            };
            color_eyre::Report::from(error).wrap_err(message)
        })?;
        recover_interrupted_publication(&config.output_dir)?;
        let started = Instant::now();
        send(
            &progress_tx,
            ProgressStage::Discovering,
            0,
            0,
            None,
            "Discovering Skyrim assets",
        )
        .await;
        let loaded_manifest = if config.invalidate_cache {
            ConversionManifest::default()
        } else {
            ConversionManifest::load(&config.output_dir.join("conversion-manifest.json"))?
        };
        let expected_configuration = configuration_hash(&config)?;
        let metadata_configuration_is_compatible = loaded_manifest.configuration_hash
            == expected_configuration
            || (matches!(loaded_manifest.schema_version, 12..=16)
                && loaded_manifest.configuration_hash
                    == configuration_hash_for_schema(&config, loaded_manifest.schema_version)?);
        let retained_configuration_is_compatible =
            match &loaded_manifest.retained_asset_configuration_hash {
                Some(recorded) => {
                    recorded
                        == &configuration_hash_for_schema(
                            &config,
                            loaded_manifest
                                .retained_mesh_schema_version
                                .unwrap_or(loaded_manifest.schema_version),
                        )?
                }
                None => {
                    loaded_manifest.retained_mesh_schema_version.is_none()
                        && !config.output_dir.join("metadata-rebuild.json").exists()
                }
            };
        let configuration_is_compatible =
            metadata_configuration_is_compatible && retained_configuration_is_compatible;
        let previous_manifest = if configuration_is_compatible {
            loaded_manifest
        } else {
            ConversionManifest::default()
        };
        let staging = config
            .resume_staging
            .clone()
            .unwrap_or_else(|| staging_path(&config.output_dir));
        let resumed = config.resume_staging.is_some();
        fs::create_dir_all(staging.join("vfs"))
            .wrap_err_with(|| format!("failed to create {}", staging.join("vfs").display()))?;
        if resumed {
            let mut verified = BTreeSet::new();
            if !config.invalidate_cache {
                if let Ok(staged) = load_staged_outputs(&staging) {
                    for (key, record) in staged {
                        if record.schema_version == CONVERTER_SCHEMA_VERSION
                            && record.configuration_hash == expected_configuration
                        {
                            let mut glb_path = PathBuf::from(key);
                            glb_path.set_extension("glb");
                            let rel = glb_path.to_string_lossy().replace('\\', "/");
                            let full = staging.join(&rel);
                            if full.is_file()
                                && fs::metadata(&full).is_ok_and(|m| m.len() == record.output_size)
                                && hash_file(&full).is_ok_and(|h| h == record.output_hash)
                            {
                                verified.insert(rel);
                            }
                        }
                    }
                }
                for glb in previous_manifest.pruned_texture_references.keys() {
                    let full = staging.join(glb);
                    if full.is_file() {
                        verified.insert(glb.clone());
                    }
                }
                for entry in previous_manifest.entries.values() {
                    if entry.output.ends_with(".glb") {
                        let full = staging.join(&entry.output);
                        if full.is_file() {
                            verified.insert(entry.output.clone());
                        }
                    }
                }
            }
            invalidate_staged_mesh_outputs(&staging, &verified)?;
            invalidate_staged_generated_outputs(&staging)?;
        }
        // A run that stops keeps its staging folder, whether it failed or was interrupted: the
        // folder is everything the run has done so far, and the caller reports the command that
        // resumes from it. Only a successful publish removes it, once the runtime pack is out.
        let run_result = Self::run_into(
            &config,
            &staging,
            &previous_manifest,
            &progress_tx,
            &cancellation,
        )
        .await;
        let mut report = match run_result {
            Ok(report) => report,
            Err(error) => return Err(failure(error, &staging, &cancellation)),
        };
        send(
            &progress_tx,
            ProgressStage::Publishing,
            1,
            1,
            None,
            "Publishing converted assets",
        )
        .await;
        // The run's own checks are behind it, but an interrupt that landed while it was packing up
        // must still stop it before the runtime pack is published. The folder is kept, so the run
        // resumes from where it stopped.
        if cancellation.is_cancelled() {
            return Err(failure(Interrupted::new().into(), &staging, &cancellation));
        }
        publish_runtime_pack(&staging, &config.output_dir, &report, &output_lock)
            .map_err(|error| failure(error, &staging, &cancellation))?;
        let cache_root = config.ingestion_cache_dir().join(".ingestion-cache");
        if staging.join(".ingestion-cache").is_dir() {
            persist_ingestion_cache(&staging.join(".ingestion-cache"), &cache_root)
                .map_err(|error| failure(error, &staging, &cancellation))?;
        }
        let manifest =
            ConversionManifest::load(&config.output_dir.join("conversion-manifest.json"))
                .map_err(|error| failure(error, &staging, &cancellation))?;
        prune_stale_ingestion_blobs(&cache_root, &manifest)
            .map_err(|error| failure(error, &staging, &cancellation))?;
        // Resumed or not, the staging folder has done its job: the pack links the published files,
        // so removing it frees only names. Kept, a resumed folder would be offered for resume
        // again (`find_resumable_staging`) while holding a full copy's worth of disk. The output is
        // already published, so a folder that cannot or must not be removed is only a warning.
        remove_staging(&staging, &config.data_dir);
        report.elapsed_ms = started.elapsed().as_millis();
        if report.complete {
            send(
                &progress_tx,
                ProgressStage::Complete,
                1,
                1,
                None,
                "Asset conversion complete",
            )
            .await;
        }
        Ok(report)
    }

    async fn run_into(
        config: &PipelineConfig,
        staging: &Path,
        previous: &ConversionManifest,
        progress_tx: &Sender<ProgressEvent>,
        cancellation: &Cancellation,
    ) -> Result<PipelineReport> {
        let mut report = PipelineReport::default();
        let expected_configuration = configuration_hash(config)?;
        // Provenance of the outputs already in `staging`, written by the run
        // that was interrupted. Invalidation drops it along with the published
        // manifest, so with `--invalidate-cache` every output is converted
        // again.
        let staged_outputs = Arc::new(if config.invalidate_cache {
            BTreeMap::new()
        } else {
            load_staged_outputs(staging).wrap_err_with(|| {
                format!(
                    "failed to read the staging journal in {}",
                    staging.display()
                )
            })?
        });
        let mut journal = StagingJournal::open(staging)?;
        let mut manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            retained_mesh_schema_version: None,
            retained_asset_configuration_hash: None,
            complete: false,
            configuration_hash: expected_configuration.clone(),
            inputs_by_kind: Default::default(),
            failures: Default::default(),
            pruned_texture_references: Default::default(),
            archives: Default::default(),
            entries: Default::default(),
        };
        let files = discover(&config.data_dir)?;
        let plugins = plugin_paths(config, &files, &mut report.notices)?;
        // Notices go out on the progress channel, like the pruned-texture warnings, so the CLI
        // prints them without splicing into its status line and the launcher shows them in its
        // notice pane. They stay in `report.notices` for the summary and the JSON report.
        for notice in &report.notices {
            send_notice(
                progress_tx,
                ProgressStage::Discovering,
                None,
                &format!("note: {notice}"),
            )
            .await;
        }
        let plugin_hashes = plugins
            .iter()
            .map(|plugin| hash_file(plugin))
            .collect::<Result<Vec<_>>>()?;
        let archives: Vec<_> = files
            .iter()
            .filter(|path| extension(path, &["bsa", "ba2"]))
            .cloned()
            .collect();
        let mut enabled_archives: Vec<_> = archives
            .into_iter()
            .filter(|archive| !extension(archive, &["ba2"]) || config.enable_ba2)
            .collect();
        sort_archives_by_load_order(&mut enabled_archives, &plugins);
        if !enabled_archives.is_empty() {
            send(
                progress_tx,
                ProgressStage::Extracting,
                0,
                enabled_archives.len() as u64,
                None,
                "Extracting Skyrim archives",
            )
            .await;
        }

        let vfs_dir = staging.join("vfs");
        // Reconstruct the effective input set; removed archives or loose files
        // must not survive from an interrupted run's VFS.
        fs::remove_dir_all(&vfs_dir)?;
        fs::create_dir_all(&vfs_dir)?;

        // Bytes already extracted, so each archive reports progress against the whole run instead
        // of restarting at zero.
        let mut extracted_bytes = 0u64;

        for (index, archive) in enabled_archives.iter().enumerate() {
            interrupt(cancellation)?;
            send(
                progress_tx,
                ProgressStage::Extracting,
                index as u64,
                enabled_archives.len() as u64,
                Some(archive.clone()),
                "Extracting archive",
            )
            .await;
            let archive_for_worker = archive.clone();
            let vfs_for_worker = vfs_dir.clone();
            let previous_cache_root = config.ingestion_cache_dir().join(".ingestion-cache");
            let cache_root = staging.join(".ingestion-cache");
            let archive_key = archive
                .strip_prefix(&config.data_dir)
                .unwrap_or(archive)
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            let previous_entry = previous.archives.get(&archive_key).cloned();
            let verify_cache = config.verify_cache;

            // The extractor reports file by file; the event keeps the archive count the launcher
            // reads and carries the smoother per-file fraction for the status line.
            let archive_totals = Arc::new(Mutex::new((0u64, 0u64)));
            let totals_for_worker = Arc::clone(&archive_totals);
            let progress_for_worker = progress_tx.clone();
            let progress_archive = archive.clone();
            let archives_total = enabled_archives.len() as u64;
            let archive_index = index as u64;
            let progress = move |extracted: ExtractionProgress| {
                *totals_for_worker
                    .lock()
                    .expect("archive totals mutex poisoned") =
                    (extracted.total_files, extracted.total_bytes);
                let archive_fraction = if extracted.total_bytes > 0 {
                    extracted.completed_bytes as f32 / extracted.total_bytes as f32
                } else if extracted.total_files > 0 {
                    extracted.completed_files as f32 / extracted.total_files as f32
                } else {
                    1.0
                };
                // The extraction threads never wait on the front end: an update that finds the
                // channel full is dropped, and the next one carries the cumulative numbers. The
                // archive's start and end events, and every failure, are still sent reliably.
                let _ = progress_for_worker.try_send(
                    ProgressEvent::new(
                        ProgressStage::Extracting,
                        archive_index,
                        archives_total,
                        Some(progress_archive.clone()),
                        "Extracting archive",
                    )
                    .with_bytes(
                        extracted_bytes + extracted.completed_bytes,
                        extracted_bytes + extracted.total_bytes,
                    )
                    .with_stage_fraction(
                        (archive_index as f32 + archive_fraction) / archives_total as f32,
                    ),
                );
            };

            // A stop is checked before every entry, so it does not wait for a multi-gigabyte
            // archive to finish.
            let stop_for_worker = cancellation.clone();
            #[cfg(test)]
            let output_for_hook = config.output_dir.clone();
            let stop = move || {
                #[cfg(test)]
                stop_hook::tick(&output_for_hook, &stop_for_worker);
                stop_for_worker.is_cancelled()
            };

            let result = spawn_blocking(move || {
                ArchiveExtractor::extract_cached(
                    &archive_for_worker,
                    &vfs_for_worker,
                    &previous_cache_root,
                    &cache_root,
                    previous_entry.as_ref(),
                    verify_cache,
                    Some(&progress),
                    Some(&stop),
                )
            })
            .await
            .wrap_err("archive worker panicked")?;
            // An archive abandoned by a stop ends the run as an interrupt, the same as a stop
            // between archives, rather than being counted as a skipped archive. Its cache entry
            // was never recorded, so a resume extracts it again from the start.
            let result = match result {
                // A stop that races an archive error keeps the archive's error as its cause.
                Err(error) if cancellation.is_cancelled() => {
                    return Err(Interrupted::after(error).into());
                }
                result => result,
            };

            send(
                progress_tx,
                ProgressStage::Extracting,
                (index + 1) as u64,
                enabled_archives.len() as u64,
                Some(archive.clone()),
                "Extracted archive",
            )
            .await;

            match result {
                Ok(outcome) => {
                    let (_, bytes) = *archive_totals
                        .lock()
                        .expect("archive totals mutex poisoned");
                    extracted_bytes += bytes;
                    if outcome.cache_hit {
                        report.cache_hits += outcome.files.len() as u64;
                    } else {
                        report.converted += outcome.files.len() as u64;
                    }
                    manifest.archives.insert(archive_key, outcome.cache_entry);
                }
                Err(error) if !config.fail_fast => {
                    report.skipped += 1;
                    let message = format!("{}: {error:#}", archive.display());
                    manifest.failures.insert(
                        archive.to_string_lossy().replace('\\', "/"),
                        message.clone(),
                    );
                    report.warnings.push(message);
                }
                Err(error) => return Err(error),
            }
        }

        overlay_loose_assets(&config.data_dir, &staging.join("vfs"), &files)?;
        interrupt(cancellation)?;

        if !plugins.is_empty() {
            send(
                progress_tx,
                ProgressStage::Database,
                0,
                plugins.len() as u64,
                None,
                "Building skyrim_world.db",
            )
            .await;
            let db_path = staging.join("skyrim_world.db");
            // SQLite writes in place; a resumed staging db may share an
            // inode with a previous pack via hard link, so unlink first.
            if db_path.is_file() {
                fs::remove_file(&db_path)?;
            }
            let merged = EsmParser::convert_plugins_with_records(&plugins, &db_path)?;
            validate_database(&Connection::open(&db_path)?)?;
            write_cell_cache(&merged, &staging.join("cell_cache.rkyv"))?;
            report.artifacts.extend([
                PathBuf::from("skyrim_world.db"),
                PathBuf::from("cell_cache.rkyv"),
            ]);
        }

        let vfs_files = discover(&staging.join("vfs"))?;
        // Canonical source texture keys, used to tell a failed publication from absent game data
        // and to detect a pruned texture whose source is back in the installed data.
        let source_textures = texture_source_keys(staging, &vfs_files);
        let restored_meshes = restored_mesh_outputs(previous, &source_textures);
        remove_orphan_staged_assets(staging, &vfs_files)?;
        {
            let mut batch = ConversionBatch {
                config,
                staging,
                previous,
                staged: Arc::clone(&staged_outputs),
                expected_configuration: &expected_configuration,
                journal: &mut journal,
                manifest: &mut manifest,
                report: &mut report,
                progress_tx,
                cancellation,
            };
            batch
                .convert_kind(
                    &vfs_files,
                    "nif",
                    ProgressStage::Meshes,
                    None,
                    &restored_meshes,
                )
                .await?;
        }
        let texture_semantics = collect_texture_semantics(staging)?;
        {
            let mut batch = ConversionBatch {
                config,
                staging,
                previous,
                staged: Arc::clone(&staged_outputs),
                expected_configuration: &expected_configuration,
                journal: &mut journal,
                manifest: &mut manifest,
                report: &mut report,
                progress_tx,
                cancellation,
            };
            batch
                .convert_kind(
                    &vfs_files,
                    "dds",
                    ProgressStage::Textures,
                    Some(&texture_semantics),
                    &BTreeSet::new(),
                )
                .await?;
            let aliases = publish_srgb_texture_aliases(staging)?;
            batch.report.artifacts.extend(aliases);
            // A reused mesh that still holds its published bytes keeps the prune record of the
            // run that wrote it: a prune only removes references, so an older record stays true.
            // The published copy proves that, and so does the previous manifest when the staged
            // mesh has the hash it recorded for the same source: that manifest was published
            // together with the record and holds the pruned bytes' hash, so the published copy
            // may be missing. Captured before the prune pass rewrites any staged GLB, because
            // afterwards a mesh pruned again no longer matches either.
            let mut carried_prunes: BTreeMap<String, Vec<String>> = BTreeMap::new();
            // Resolving an output to the keys that produced it once keeps the scan below linear:
            // a real pack holds hundreds of thousands of entries, and walking them for every
            // pruned candidate would be quadratic.
            // Built only when there are prune records to check, so a fresh run pays nothing.
            let entries_by_output: BTreeMap<&str, Vec<&str>> = {
                let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                let entries = if previous.pruned_texture_references.is_empty() {
                    None
                } else {
                    Some(&batch.manifest.entries)
                };
                for (key, entry) in entries.into_iter().flatten() {
                    map.entry(entry.output.as_str())
                        .or_default()
                        .push(key.as_str());
                }
                map
            };
            for (glb, references) in &previous.pruned_texture_references {
                let staged_glb = staging.join(glb);
                let published = files_are_identical(&staged_glb, &config.output_dir.join(glb));
                if published
                    || staged_matches_previous_entry(&batch, &entries_by_output, glb, &staged_glb)
                {
                    carried_prunes.insert(glb.clone(), references.iter().cloned().collect());
                }
            }
            // A missing artifact whose DDS source exists under `staging/vfs` is a failed
            // publication, not absent game data: the prune leaves those references alone.
            let pruned =
                MeshConverter::prune_dangling_texture_uris_with_sources(staging, &source_textures)?;
            let pruned_uris: u64 = pruned
                .iter()
                .map(|file| file.removed_uris.len() as u64)
                .sum();
            let mut pruned_completed = 0;
            for file in &pruned {
                for uri in &file.removed_uris {
                    pruned_completed += 1;
                    // The warning goes out as a notice on the progress channel rather than to
                    // stderr, so it cannot splice into the status line the CLI is redrawing.
                    send_notice(
                        progress_tx,
                        ProgressStage::Textures,
                        Some(PathBuf::from(&file.glb)),
                        &format!(
                            "warning: pruned dangling texture {uri} referenced by {} (no converted artifact)",
                            file.glb
                        ),
                    )
                    .await;
                    let reference = resolve_asset_uri(staging, &staging.join(&file.glb), uri)
                        .ok()
                        .and_then(|resolved| {
                            resolved.strip_prefix(staging).ok().map(Path::to_path_buf)
                        })
                        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|| uri.clone());
                    if batch.record_pruned_texture_reference(&file.glb, &reference) {
                        send(
                            batch.progress_tx,
                            ProgressStage::Textures,
                            pruned_completed,
                            pruned_uris,
                            Some(PathBuf::from(&file.glb)),
                            "Texture reference pruned",
                        )
                        .await;
                    }
                }
            }
            // Merge the carried records with this run's prunes: a reused mesh that already
            // omitted A and has just had B pruned must report both. A mesh whose source changed
            // was converted again before this pass, so its staged bytes no longer matched the
            // published copy and its stale records were not carried.
            for (glb, references) in &carried_prunes {
                for reference in references {
                    batch.record_pruned_texture_reference(glb, reference);
                }
            }
            // The prune rewrote the GLB after its cache entry was recorded, so refresh every
            // entry that publishes it from the rewritten bytes. Otherwise the next run fails the
            // entry's size and hash check and converts the mesh again.
            //
            // The staging journal is deliberately left alone. A journal record carries no prune
            // references, so certifying the pruned bytes would let a resumed run reuse the mesh
            // with nothing left for the prune pass to find, and the published manifest would lose
            // the record. The mesh's journal record still holds its pre-prune hash, which the
            // pruned file no longer matches, so a resume converts it again and prunes it again.
            let pruned_outputs: BTreeSet<String> = pruned
                .iter()
                .filter(|file| !file.removed_uris.is_empty())
                .map(|file| file.glb.clone())
                .collect();
            for entry in batch.manifest.entries.values_mut() {
                if !pruned_outputs.contains(&entry.output) {
                    continue;
                }
                let path = staging.join(&entry.output);
                entry.output_size = fs::metadata(&path)
                    .wrap_err_with(|| format!("failed to inspect pruned {}", path.display()))?
                    .len();
                entry.output_hash = hash_file(&path)?;
            }
            // The run summary reports what the manifest records, whether this run
            // pruned it or carried the record forward for a reused mesh.
            let recorded: u64 = batch
                .manifest
                .pruned_texture_references
                .values()
                .map(|references| references.len() as u64)
                .sum();
            batch.report.pruned_texture_references = recorded;
            batch
                .convert_kind(
                    &vfs_files,
                    "pex",
                    ProgressStage::Scripts,
                    None,
                    &BTreeSet::new(),
                )
                .await?;
        }
        compile_lod_chunks_with_cancel(
            config,
            staging,
            &plugins,
            &plugin_hashes,
            progress_tx,
            cancellation,
            &mut report,
        )
        .await?;
        interrupt(cancellation)?;
        if let Some(integration) = finalize_world_database(staging)? {
            if !integration.passed {
                report.warnings.push(format!(
                    "asset integration failed: {} missing models, {} invalid models, {} missing textures, terrain/cache cells {}/{}",
                    integration.missing_model_count,
                    integration.invalid_model_count,
                    integration.missing_texture_count,
                    integration.terrain_cells,
                    integration.cache_cells,
                ));
            }
            report.integration = Some(integration);
            report
                .artifacts
                .push(PathBuf::from("integration-report.json"));
        }
        let runtime_path = staging.join("scripts/papyrus_runtime.luau");
        if let Some(parent) = runtime_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // The output tree holds hard links (a `vfs` entry and its cache blob, a runtime texture
        // and its sRGB alias), so every writer replaces its path instead of writing through it.
        if runtime_path.exists() {
            fs::remove_file(&runtime_path)?;
        }
        fs::write(
            &runtime_path,
            include_str!("../../shared/src/papyrus_runtime.luau"),
        )?;
        report
            .artifacts
            .push(PathBuf::from("scripts/papyrus_runtime.luau"));
        send(
            progress_tx,
            ProgressStage::Validating,
            0,
            report.artifacts.len() as u64,
            None,
            "Validating generated artifacts",
        )
        .await;
        validate_artifacts(
            staging,
            &report.artifacts,
            &texture_semantics,
            config.cpu_jobs,
        )?;
        send(
            progress_tx,
            ProgressStage::Validating,
            report.artifacts.len() as u64,
            report.artifacts.len() as u64,
            None,
            "Generated artifacts are valid",
        )
        .await;
        interrupt(cancellation)?;
        manifest.complete = conversion_is_complete(&report);
        report.complete = manifest.complete;
        report.inputs_by_kind = manifest.inputs_by_kind.clone();
        manifest.save(&staging.join("conversion-manifest.json"))?;
        report
            .artifacts
            .push(PathBuf::from("conversion-manifest.json"));
        Ok(report)
    }
}

struct ConversionBatch<'a> {
    config: &'a PipelineConfig,
    staging: &'a Path,
    previous: &'a ConversionManifest,
    /// Provenance of the outputs already in `staging`, keyed by canonical
    /// source key; empty when the cache is invalidated.
    staged: Arc<BTreeMap<String, StagedOutput>>,
    expected_configuration: &'a str,
    journal: &'a mut StagingJournal,
    manifest: &'a mut ConversionManifest,
    report: &'a mut PipelineReport,
    progress_tx: &'a Sender<ProgressEvent>,
    cancellation: &'a Cancellation,
}

/// How a worker produced a conversion output.
enum Produced {
    /// A cached output was reused.
    CacheHit,
    /// Freshly converted; `digest` is the output's (size, SHA-256) when the
    /// converter already computed it, so it need not be read back.
    Converted { digest: Option<(u64, String)> },
}

struct GpuTag {
    index: usize,
    key: String,
    hash: String,
    target_rel: PathBuf,
    relative: PathBuf,
    source: PathBuf,
    target: PathBuf,
    encoding: TextureEncoding,
}

impl ConversionBatch<'_> {
    /// Converts every file of one kind (`dds`, `nif` or `pex`) on the worker pool, reusing cached
    /// and staged outputs, and records each result in the manifest, the journal and the report.
    /// Textures the GPU encoder takes are batched to it instead of being encoded on a worker.
    async fn convert_kind(
        &mut self,
        files: &[PathBuf],
        source_ext: &str,
        stage: ProgressStage,
        texture_semantics: Option<&BTreeMap<String, BTreeSet<TextureSemantic>>>,
        force_reconvert: &BTreeSet<String>,
    ) -> Result<()> {
        let selected_paths: Vec<_> = files
            .iter()
            .filter(|path| extension(path, &[source_ext]))
            .cloned()
            .collect();

        let (target_ext, asset_kind) = match source_ext {
            "dds" => ("ktx2", AssetKind::Texture),
            "nif" => ("glb", AssetKind::Mesh),
            "pex" => ("luau", AssetKind::Script),
            _ => unreachable!(),
        };
        let staging_vfs = self.staging.join("vfs");
        let mut target_sources = BTreeMap::<String, PathBuf>::new();
        let mut selected = Vec::with_capacity(selected_paths.len());
        // The source sizes are one `stat` each and give the status line something to weigh the
        // stage by: a mesh and a texture are the same "item" but not the same work.
        let mut source_sizes = BTreeMap::<String, u64>::new();
        let mut bytes_total = 0u64;
        for source in selected_paths {
            let relative = source.strip_prefix(&staging_vfs)?.to_owned();
            let target_key =
                canonical_asset_path(&relative.to_string_lossy(), asset_kind, target_ext)?;
            if let Some(previous) = target_sources.insert(target_key.clone(), relative.clone()) {
                bail!(
                    "normalized output collision for {target_key}: {} and {}",
                    previous.display(),
                    relative.display()
                );
            }
            let source_key =
                canonical_asset_path(&relative.to_string_lossy(), asset_kind, source_ext)?;
            let encoding = if source_ext == "dds" {
                let known_semantics = texture_semantics
                    .and_then(|semantics| semantics.get(&target_key))
                    .cloned()
                    .unwrap_or_default();
                Some(TextureEncoding::from_semantics(&known_semantics)?)
            } else {
                None
            };
            let source_bytes = fs::metadata(&source).map_or(0, |metadata| metadata.len());
            bytes_total += source_bytes;
            source_sizes.insert(source_key.clone(), source_bytes);
            selected.push((
                source,
                relative,
                PathBuf::from(target_key),
                source_key,
                encoding,
            ));
        }

        self.manifest
            .inputs_by_kind
            .insert(source_ext.to_owned(), selected.len() as u64);

        if selected.is_empty() {
            return Ok(());
        }
        interrupt(self.cancellation)?;

        let total_files = selected.len() as u64;
        let progress_tx = self.progress_tx.clone();
        let (outcome_tx, mut outcome_rx) = unbounded_channel();

        let staging_root = self.staging.to_path_buf();
        let output_dir = self.config.output_dir.clone();
        let source_kind = source_ext.to_owned();
        let etc1s_quality = self.config.texture_fallback_quality;
        let uastc_level = self.config.texture_uastc_level;
        let zstd_level = self.config.texture_zstd_level;
        let texture_encoder = self.config.texture_encoder;
        let cpu_jobs = self.config.cpu_jobs;
        let previous_entries = self.previous.entries.clone();
        let staged_outputs = Arc::clone(&self.staged);
        let expected_configuration = self.expected_configuration.to_owned();
        let force_reconvert = force_reconvert.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let run_cancelled = self.cancellation.clone();

        let rayon_handle = spawn_blocking(move || -> Result<()> {
            use rayon::prelude::*;

            let gpu = if source_kind == "dds" {
                if let TextureEncoder::Gpu { quality, batch_mb } = texture_encoder {
                    match GpuUastc::new(quality, batch_mb) {
                        Ok(mut gpu) => {
                            gpu.zstd_level = zstd_level;
                            // The batch size is capped to what the GPU's buffers allow.
                            eprintln!(
                                "GPU texture encoder: {} (quality {quality}, batch {} MiB)",
                                gpu.adapter_name,
                                gpu.batch_bytes >> 20
                            );
                            Some(gpu)
                        }
                        Err(error) => {
                            eprintln!(
                                "GPU texture encoder unavailable ({error:#}); using CPU encoder"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            // Cache label of GPU-encoded textures (see the hash below).
            let gpu_label = gpu
                .as_ref()
                .map(|gpu| texture_gpu::cache_label(gpu.quality));
            let (gpu_sender, gpu_thread) = if let Some(gpu) = gpu {
                let (sender, receiver) = texture_gpu::job_channel::<GpuTag>(&gpu);
                let gpu_outcomes = outcome_tx.clone();
                let gpu_label = gpu_label.clone().unwrap_or_default();
                let gpu_cancelled = Arc::clone(&worker_cancelled);
                let gpu_run_cancelled = run_cancelled.clone();
                // Like the workers, the GPU stops taking textures once the
                // batch or the run is stopped.
                let stopped = move || {
                    gpu_cancelled.load(Ordering::Relaxed) || gpu_run_cancelled.is_cancelled()
                };
                let handle = std::thread::spawn(move || {
                    // Called on the batcher's writer threads, so the writes
                    // run in parallel.
                    let finish = |tag: GpuTag, result: Result<texture_gpu::EncodedTexture>| {
                        let GpuTag {
                            index,
                            key,
                            hash,
                            target_rel,
                            relative,
                            source,
                            target,
                            encoding,
                        } = tag;
                        let written = result.and_then(|encoded| {
                            publish_ktx2_file(&target, &encoded.bytes)?;
                            Ok((encoded.bytes.len() as u64, encoded.sha256))
                        });
                        let (hash, result, fatal) = match written {
                            Ok(digest) => (
                                hash,
                                Ok(Produced::Converted {
                                    digest: Some(digest),
                                }),
                                false,
                            ),
                            Err(error) => {
                                eprintln!(
                                    "GPU texture conversion failed for {} ({error:#}); using CPU encoder",
                                    relative.display()
                                );
                                // A CPU-encoded fallback does not carry the GPU
                                // label, so the next GPU run retries the GPU.
                                let (result, fatal) = match remove_invalid_staged_output(&target) {
                                    Err(error) => (Err(error), true),
                                    Ok(()) => remove_partial_output_after_failure(
                                        &target,
                                        TextureConverter::convert_dds_to_ktx2_with_options(
                                            &source,
                                            &target,
                                            encoding,
                                            etc1s_quality,
                                            uastc_level,
                                            zstd_level,
                                        )
                                        .map(|_| ()),
                                    ),
                                };
                                (
                                    hash.replace(&gpu_label, ""),
                                    result.map(|_| Produced::Converted { digest: None }),
                                    fatal,
                                )
                            }
                        };
                        let result = result
                            .wrap_err_with(|| format!("failed to convert {}", relative.display()));
                        let _ = gpu_outcomes.send((
                            index, key, hash, target_rel, relative, result, target, fatal,
                        ));
                    };
                    let stats = texture_gpu::run_batcher(&gpu, receiver, cpu_jobs, stopped, finish);
                    eprintln!(
                        "GPU texture encoder: {} textures in {} batches, {:.1} GB uploaded",
                        stats.textures,
                        stats.batches,
                        stats.source_bytes as f64 / 1e9
                    );
                });
                (Some(sender), Some(handle))
            } else {
                (None, None)
            };

            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(cpu_jobs)
                .build()
                .wrap_err("failed to create asset conversion worker pool")?;
            pool.install(|| {
                selected.into_par_iter().enumerate().for_each(
                    |(index, (source, relative, target_rel, key, encoding))| {
                        // An interrupted run stops before starting another asset; the one already
                        // in flight finishes, so no half-written output is left behind.
                        if worker_cancelled.load(Ordering::Relaxed) || run_cancelled.is_cancelled()
                        {
                            return;
                        }
                        // A mesh whose pruned texture source is back must be converted again:
                        // the mesh cache does not hash texture dependencies, so a reused GLB
                        // would never regain the reference.
                        let forced =
                            force_reconvert.contains(target_rel.to_string_lossy().as_ref());
                        let target = staging_root.join(&target_rel);

                        let source_hash = if source_kind == "nif" {
                            nif_source_hash(&source)
                        } else {
                            hash_file(&source)
                        };
                        let mut hash = match source_hash {
                            Ok(h) => h,
                            Err(err) => {
                                let (err, fatal) = match remove_invalid_staged_output(&target) {
                                    Ok(()) => (err, false),
                                    Err(remove_error) => (remove_error, true),
                                };
                                let _ = outcome_tx.send((
                                    index,
                                    key,
                                    String::new(),
                                    target_rel,
                                    relative.clone(),
                                    Err(err),
                                    target,
                                    fatal,
                                ));
                                return;
                            }
                        };

                        if let Some(encoding) = encoding {
                            hash.push_str(&format!(":texture-encoding:{encoding:?}"));
                            // Marks the textures the GPU encodes. Natively preserved
                            // textures and those only the CPU encoder handles
                            // (volumes, arrays) convert the same way in either
                            // mode and keep one cache entry.
                            if gpu_sender.is_some()
                                && let Some(label) = &gpu_label
                                && texture_gpu::takes(&source, encoding)
                            {
                                hash.push_str(label);
                            }
                        }

                        // Check cache
                        if !forced
                            && let Some(entry) =
                                previous_entries.get(&key).filter(|e| e.source_hash == hash)
                        {
                            let old = output_dir.join(&entry.output);
                            if old.is_file()
                                && fs::metadata(&old).is_ok_and(|m| m.len() == entry.output_size)
                                && hash_file(&old).is_ok_and(|h| h == entry.output_hash)
                            {
                                if let Some(parent) = target.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                // `link_or_copy`, not `fs::copy`: on a resumed
                                // run the staged target may already be a hard
                                // link to the pack file, and copying a file
                                // onto its own link truncates both. A shared
                                // inode is already the cached bytes.
                                if link_or_copy(&old, &target).is_ok() {
                                    let _ = outcome_tx.send((
                                        index,
                                        key,
                                        hash,
                                        target_rel,
                                        relative.clone(),
                                        Ok(Produced::CacheHit),
                                        target,
                                        false,
                                    ));
                                    return;
                                };
                            }
                        }

                        // A staged output survives from an earlier run, so it
                        // is reused only when the journal says it was produced
                        // from the current source under the current schema and
                        // configuration and its bytes still match the recorded
                        // size and hash, or the `previous_entries` fallback
                        // finds the previous manifest's entry for the same
                        // source and its recorded output hash matches the
                        // staged bytes (a staging tree copied from a published
                        // pack carries no journal). Any other output is
                        // converted again.
                        let staged_is_current = !forced
                            && (staged_outputs.get(&key).is_some_and(|record| {
                                record.is_current(&target, &hash, &expected_configuration)
                            }) || previous_entries.get(&key).is_some_and(|entry| {
                                entry.source_hash == hash
                                    && entry.output_hash == hash_file(&target).unwrap_or_default()
                            }));
                        let existing_is_valid = staged_is_current
                            && fs::metadata(&target).is_ok_and(|metadata| metadata.len() > 0)
                            && match source_kind.as_str() {
                                "dds" => fs::read(&target).is_ok_and(|bytes| {
                                    crate::texture::inspect_ktx2(
                                        &bytes,
                                        encoding.expect("DDS conversion requires an encoding"),
                                    )
                                    .is_ok()
                                }),
                                "nif" | "pex" => true,
                                _ => false,
                            };

                        if !existing_is_valid
                            && source_kind == "dds"
                            && let Some(sender) = &gpu_sender
                            && let Ok(bytes) = fs::read(&source)
                            && let Some(encoding) = encoding
                            && let Ok(texture) = PreparedTexture::from_dds(bytes, encoding)
                        {
                            if let Err(error) = remove_invalid_staged_output(&target) {
                                let _ = outcome_tx.send((
                                    index,
                                    key,
                                    hash,
                                    target_rel,
                                    relative,
                                    Err(error),
                                    target,
                                    true,
                                ));
                                return;
                            }
                            let tag = GpuTag {
                                index,
                                key: key.clone(),
                                hash: hash.clone(),
                                target_rel: target_rel.clone(),
                                relative: relative.clone(),
                                source: source.clone(),
                                target: target.clone(),
                                encoding,
                            };
                            if sender
                                .send(GpuJob {
                                    texture,
                                    encoding: tag.encoding,
                                    tag,
                                })
                                .is_ok()
                            {
                                return;
                            }
                        }
                        // A labelled texture the GPU path declined after all
                        // (undecodable pixels, a stopped GPU thread) is encoded
                        // on the CPU below; its cache entry must say so.
                        if !existing_is_valid
                            && let Some(label) = &gpu_label
                            && hash.ends_with(label.as_str())
                        {
                            hash.truncate(hash.len() - label.len());
                        }

                        let (result, fatal) = if existing_is_valid {
                            (Ok(()), false)
                        } else {
                            match remove_invalid_staged_output(&target) {
                                Err(error) => (Err(error), true),
                                Ok(()) => {
                                    let conversion = match source_kind.as_str() {
                                        "dds" => {
                                            TextureConverter::convert_dds_to_ktx2_with_options(
                                                &source,
                                                &target,
                                                encoding
                                                    .expect("DDS conversion requires an encoding"),
                                                etc1s_quality,
                                                uastc_level,
                                                zstd_level,
                                            )
                                            .map(|_| ())
                                        }
                                        "nif" => {
                                            MeshConverter::convert_nif_to_glb(&source, &target)
                                        }
                                        "pex" => {
                                            ScriptConverter::convert_pex_to_luau(&source, &target)
                                        }
                                        _ => unreachable!(),
                                    };
                                    remove_partial_output_after_failure(&target, conversion)
                                }
                            }
                        };

                        let result = result
                            .map(|_| Produced::Converted { digest: None })
                            .wrap_err_with(|| format!("failed to convert {}", relative.display()));
                        let _ = outcome_tx.send((
                            index,
                            key,
                            hash,
                            target_rel,
                            relative.to_path_buf(),
                            result,
                            target,
                            fatal,
                        ));
                    },
                );
            });
            drop(gpu_sender);
            if let Some(handle) = gpu_thread {
                handle
                    .join()
                    .map_err(|_| color_eyre::eyre::eyre!("GPU texture worker panicked"))?;
            }
            Ok(())
        });

        let mut completed = 0u64;
        let mut first_error = None;
        let fail_fast = self.config.fail_fast;
        let mut bytes_completed = 0u64;
        while let Some((_, key, hash, target_rel, relative, conversion, target, fatal)) =
            outcome_rx.recv().await
        {
            completed += 1;
            bytes_completed += source_sizes.get(&key).copied().unwrap_or(0);
            if self.cancellation.is_cancelled() {
                break;
            }

            match conversion {
                Ok(produced) => {
                    // Without fail-fast only a journal write sets the first
                    // error, and after one nothing more can be recorded.
                    if first_error.is_some() {
                        continue;
                    }
                    let (is_cache_hit, known_digest) = match produced {
                        Produced::CacheHit => (true, None),
                        Produced::Converted { digest } => (false, digest),
                    };
                    if !is_cache_hit {
                        let known_size = known_digest.as_ref().map(|(size, _)| *size);
                        let size = match known_size
                            .map(Ok)
                            .unwrap_or_else(|| fs::metadata(&target).map(|metadata| metadata.len()))
                        {
                            Ok(size) => size,
                            Err(error) => {
                                if fail_fast {
                                    return Err(error).wrap_err_with(|| {
                                        format!("failed to convert {}", relative.display())
                                    });
                                }
                                self.record_skip(
                                    stage,
                                    completed,
                                    total_files,
                                    key,
                                    relative,
                                    error.into(),
                                )
                                .await;
                                continue;
                            }
                        };
                        let output_hash = match known_digest
                            .map(|(_, sha256)| Ok(sha256))
                            .unwrap_or_else(|| hash_file(&target))
                        {
                            Ok(output_hash) => output_hash,
                            Err(error) => {
                                if fail_fast {
                                    return Err(error).wrap_err_with(|| {
                                        format!("failed to convert {}", relative.display())
                                    });
                                }
                                self.record_skip(
                                    stage,
                                    completed,
                                    total_files,
                                    key,
                                    relative,
                                    error,
                                )
                                .await;
                                continue;
                            }
                        };
                        send_with_bytes(
                            &progress_tx,
                            stage,
                            completed,
                            total_files,
                            Some(relative.clone()),
                            "Converted asset",
                            (bytes_completed, bytes_total),
                        )
                        .await;
                        let entry = CacheEntry {
                            source_hash: hash,
                            output: target_rel.to_string_lossy().into_owned().replace('\\', "/"),
                            output_size: size,
                            output_hash,
                        };
                        if let Err(error) = self
                            .journal
                            .record(&key, &staged_output(&entry, self.expected_configuration))
                        {
                            stop_batch(&cancelled, &mut first_error, error);
                            continue;
                        }
                        self.manifest.entries.insert(key, entry);
                        self.report.converted += 1;
                    } else {
                        send_with_bytes(
                            &progress_tx,
                            stage,
                            completed,
                            total_files,
                            Some(relative.clone()),
                            "Converted asset",
                            (bytes_completed, bytes_total),
                        )
                        .await;
                        if let Some(entry) = self.previous.entries.get(&key).cloned() {
                            // The staged copy holds the published bytes, so the
                            // published entry is its provenance.
                            if let Err(error) = self
                                .journal
                                .record(&key, &staged_output(&entry, self.expected_configuration))
                            {
                                stop_batch(&cancelled, &mut first_error, error);
                                continue;
                            }
                            self.manifest.entries.insert(key, entry);
                        }
                        self.report.cache_hits += 1;
                    }
                    self.report.artifacts.push(target_rel);
                }
                Err(error) => {
                    if fail_fast || fatal {
                        if first_error.is_none() {
                            cancelled.store(true, Ordering::Relaxed);
                            let _ = progress_tx
                                .send(
                                    ProgressEvent::new(
                                        stage,
                                        completed,
                                        total_files,
                                        Some(relative),
                                        "Asset conversion failed",
                                    )
                                    .with_outcome(AssetOutcome::Failed),
                                )
                                .await;
                            first_error = Some(error);
                        }
                    } else {
                        self.record_skip(stage, completed, total_files, key, relative, error)
                            .await;
                    }
                }
            }
        }

        rayon_handle
            .await
            .wrap_err("rayon batch worker panicked")??;
        interrupt(self.cancellation)?;
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn record_skip(
        &mut self,
        stage: ProgressStage,
        completed: u64,
        total: u64,
        key: String,
        relative: PathBuf,
        error: color_eyre::eyre::Error,
    ) {
        let _ = self
            .progress_tx
            .send(
                ProgressEvent::new(
                    stage,
                    completed,
                    total,
                    Some(relative.clone()),
                    "Asset skipped",
                )
                .with_outcome(AssetOutcome::Skipped),
            )
            .await;
        let message = format!("{}: {error:#}", relative.display());
        self.manifest.failures.insert(key, message.clone());
        self.report.warnings.push(message);
        self.report.skipped += 1;
    }

    /// Records a texture reference a published mesh omits because the game data
    /// does not contain that texture, whether this run pruned it or an earlier
    /// run did and the mesh was reused. Returns whether the reference is new, so
    /// the caller reports progress only for work this run performed.
    ///
    /// A prune is loud like a skip - the progress stream and the run summary name
    /// the mesh - but it is not a skip: no warning is recorded and nothing lands
    /// in `manifest.failures`, so the conversion stays complete.
    fn record_pruned_texture_reference(&mut self, glb: &str, reference: &str) -> bool {
        self.manifest
            .pruned_texture_references
            .entry(glb.to_owned())
            .or_default()
            .insert(reference.to_owned())
    }
}

fn collect_texture_semantics(
    staging: &Path,
) -> Result<BTreeMap<String, BTreeSet<TextureSemantic>>> {
    let mut semantics = BTreeMap::<String, BTreeSet<TextureSemantic>>::new();
    for entry in WalkDir::new(staging)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let glb = entry.path();
        if !extension(glb, &["glb"]) {
            continue;
        }
        for dependency in MeshConverter::glb_texture_dependencies(glb)? {
            let resolved = resolve_asset_uri(staging, glb, &dependency.uri)?;
            let relative = resolved.strip_prefix(staging)?;
            let key = canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "ktx2")
                .and_then(|key| source_texture_key(&key))
                .wrap_err_with(|| {
                    format!(
                        "invalid texture dependency {:?} resolved from {}",
                        dependency.uri,
                        glb.display()
                    )
                })?;
            semantics
                .entry(key)
                .or_default()
                .insert(dependency.semantic);
        }
    }

    let database = staging.join("skyrim_world.db");
    if database.is_file() {
        let connection = Connection::open(&database)?;
        let columns = [
            ("diffuse_path", TextureSemantic::BaseColor),
            ("normal_path", TextureSemantic::Normal),
            ("glow_path", TextureSemantic::Emissive),
            ("height_path", TextureSemantic::Height),
            ("environment_path", TextureSemantic::EnvironmentCube),
            ("mask_path", TextureSemantic::EnvironmentMask),
            ("specular_path", TextureSemantic::SpecularGlossiness),
            ("detail_path", TextureSemantic::Detail),
        ];
        for (column, semantic) in columns {
            let query = format!(
                "SELECT {column} FROM texture_sets WHERE {column} IS NOT NULL AND {column} <> ''"
            );
            let mut statement = connection.prepare(&query)?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for path in paths {
                insert_texture_semantic(&mut semantics, &path, semantic).wrap_err_with(|| {
                    format!("invalid texture_sets.{column} reference {path:?}")
                })?;
            }
        }
        let mut statement = connection.prepare(
            "SELECT flow_normal_path FROM waters \
             WHERE flow_normal_path IS NOT NULL AND flow_normal_path <> ''",
        )?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for path in paths {
            insert_texture_semantic(&mut semantics, &path, TextureSemantic::Normal)
                .wrap_err_with(|| format!("invalid waters.flow_normal_path reference {path:?}"))?;
        }
    }

    for (path, texture_semantics) in &semantics {
        TextureEncoding::from_semantics(texture_semantics)
            .wrap_err_with(|| format!("incompatible texture uses for {path}"))?;
    }
    Ok(semantics)
}

fn insert_texture_semantic(
    semantics: &mut BTreeMap<String, BTreeSet<TextureSemantic>>,
    path: &str,
    semantic: TextureSemantic,
) -> Result<()> {
    let key = canonical_asset_path(path, AssetKind::Texture, "ktx2")?;
    semantics.entry(key).or_default().insert(semantic);
    Ok(())
}

/// Canonical source texture keys of every discovered texture under `staging/vfs`, as
/// [`canonical_asset_path`] produces them (`textures/rock01.dds`). Used to tell "the game data
/// never contained this texture" from "the game data contains it and its publication failed".
fn texture_source_keys(staging: &Path, files: &[PathBuf]) -> BTreeSet<String> {
    let vfs = staging.join("vfs");
    files
        .iter()
        .filter(|path| extension(path, &["dds"]))
        .filter_map(|path| {
            let relative = path.strip_prefix(&vfs).ok()?;
            canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "dds").ok()
        })
        .collect()
}

/// The canonical `.dds` source key a pruned mesh reference came from: `textures/foo.ktx2` and
/// its `.opensky-srgb` alias both map to `textures/foo.dds`. Returns `None` for references that
/// are not converted texture paths.
fn texture_reference_source_key(reference: &str) -> Option<String> {
    let stem = reference
        .strip_suffix(".opensky-srgb.ktx2")
        .or_else(|| reference.strip_suffix(".ktx2"))?;
    canonical_asset_path(&format!("{stem}.dds"), AssetKind::Texture, "dds").ok()
}

/// Target outputs of meshes that must be converted again: one of their pruned references has
/// its source back under `staging/vfs`. The mesh cache does not hash texture dependencies, so a
/// reused GLB would never regain the restored reference.
fn restored_mesh_outputs(
    previous: &ConversionManifest,
    source_textures: &BTreeSet<String>,
) -> BTreeSet<String> {
    previous
        .pruned_texture_references
        .iter()
        .filter(|(_, references)| {
            references.iter().any(|reference| {
                texture_reference_source_key(reference)
                    .is_some_and(|key| source_textures.contains(&key))
            })
        })
        .map(|(glb, _)| glb.clone())
        .collect()
}

fn source_texture_key(runtime_key: &str) -> Result<String> {
    if let Some(stem) = runtime_key.strip_suffix(".opensky-srgb.ktx2") {
        return Ok(format!("{stem}.ktx2"));
    }
    Ok(runtime_key.to_owned())
}

fn publish_srgb_texture_aliases(staging: &Path) -> Result<Vec<PathBuf>> {
    let mut aliases = BTreeSet::new();
    for entry in WalkDir::new(staging)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file() && extension(entry.path(), &["glb"]))
    {
        let glb = entry.path();
        for dependency in MeshConverter::glb_texture_dependencies(glb)? {
            let destination = resolve_asset_uri(staging, glb, &dependency.uri)?;
            let relative = destination.strip_prefix(staging)?.to_owned();
            let runtime_key =
                canonical_asset_path(&relative.to_string_lossy(), AssetKind::Texture, "ktx2")?;
            if runtime_key.ends_with(".opensky-srgb.ktx2") {
                aliases.insert(PathBuf::from(runtime_key));
            }
        }
    }
    let mut published = Vec::new();
    for alias in aliases {
        let source = staging.join(source_texture_key(&alias.to_string_lossy())?);
        if !source.is_file() {
            continue;
        }
        let destination = staging.join(&alias);
        if destination.is_file() {
            fs::remove_file(&destination)?;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        link_or_copy(&source, &destination).wrap_err_with(|| {
            format!(
                "failed to publish sRGB alias {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        published.push(alias);
    }
    Ok(published)
}

pub(crate) fn discover(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect();
    files.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    Ok(files)
}

/// Whether two paths hold byte-identical files. Used to decide whether a prune
/// record from an earlier manifest still describes the mesh about to be
/// published, so a record is never repeated for a mesh that changed.
fn files_are_identical(left: &Path, right: &Path) -> bool {
    let (Ok(left_metadata), Ok(right_metadata)) = (fs::metadata(left), fs::metadata(right)) else {
        return false;
    };
    if left_metadata.len() != right_metadata.len() {
        return false;
    }
    match (hash_file(left), hash_file(right)) {
        (Ok(left_hash), Ok(right_hash)) => left_hash == right_hash,
        _ => false,
    }
}

/// Checks the generated artifacts on a pool of `jobs` threads, up to the first failure. Decoding and hashing a KTX2 is
/// CPU work and reading each file waits on the disk, so one file at a time left most cores idle.
/// `jobs` is handed to the pool exactly as the conversion stage hands it `cpu_jobs`, where 0
/// selects rayon's own thread count rather than a single thread.
///
/// The failure of the lowest artifact index is returned, so the reported error does not depend on
/// which thread finished first.
pub(crate) fn validate_artifacts(
    staging: &Path,
    artifacts: &[PathBuf],
    texture_semantics: &BTreeMap<String, BTreeSet<TextureSemantic>>,
    jobs: usize,
) -> Result<()> {
    use rayon::prelude::*;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .wrap_err("failed to create artifact validation worker pool")?;
    // Only the lowest-index failure is kept, so memory stays constant however many fail. Once
    // a failure is known, artifacts after it are skipped; earlier ones are still checked, so
    // the reported failure is the first in list order whatever the thread count.
    let lowest_failure = std::sync::atomic::AtomicUsize::new(usize::MAX);
    let first_failure = Mutex::new(None::<(usize, color_eyre::Report)>);
    pool.install(|| {
        artifacts.par_iter().enumerate().for_each_init(
            // `mlua::Lua` cannot move between threads, so each worker builds its own when it
            // first meets a script.
            || None,
            |lua, (index, relative)| {
                if index > lowest_failure.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if let Err(error) = validate_artifact(staging, relative, texture_semantics, lua) {
                    lowest_failure.fetch_min(index, std::sync::atomic::Ordering::Relaxed);
                    let mut first = first_failure.lock().unwrap();
                    if first.as_ref().is_none_or(|(kept, _)| index < *kept) {
                        *first = Some((index, error));
                    }
                }
            },
        );
    });
    match first_failure.into_inner().unwrap() {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

/// Checks one generated artifact. `lua` is the compiler for Luau scripts, created on first use so
/// a caller that never meets a script never builds one.
fn validate_artifact(
    staging: &Path,
    relative: &Path,
    texture_semantics: &BTreeMap<String, BTreeSet<TextureSemantic>>,
    lua: &mut Option<mlua::Lua>,
) -> Result<()> {
    let path = staging.join(relative);
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("ktx2") => {
            let bytes = fs::read(&path)?;
            let key = source_texture_key(&canonical_asset_path(
                &relative.to_string_lossy(),
                AssetKind::Texture,
                "ktx2",
            )?)?;
            let known_semantics = texture_semantics.get(&key).cloned().unwrap_or_default();
            let encoding = TextureEncoding::from_semantics(&known_semantics)?;
            let metadata = crate::texture::inspect_ktx2(&bytes, encoding)
                .wrap_err_with(|| format!("invalid KTX2 {}", path.display()))?;
            ensure!(
                metadata.encoded_bytes == fs::metadata(&path)?.len()
                    && !metadata.sha256.is_empty()
                    && metadata.expanded_rgba_bytes > 0,
                "KTX2 metadata validation failed for {}",
                path.display()
            );
        }
        Some("glb") => {
            // Only the 12-byte GLB header is checked, so only the header is read.
            let mut header = [0_u8; 12];
            let read = fs::File::open(&path)
                .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header));
            match read {
                Ok(()) if &header[..4] == b"glTF" => {}
                Ok(()) => bail!("invalid GLB artifact {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    bail!("invalid GLB artifact {}", path.display())
                }
                Err(error) => return Err(error.into()),
            }
        }
        Some("luau") => {
            let source = fs::read_to_string(&path)?;
            lua.get_or_insert_with(mlua::Lua::new)
                .load(&source)
                .set_name(path.to_string_lossy())
                .into_function()
                .map_err(|error| {
                    color_eyre::eyre::eyre!("invalid Luau artifact {}: {error}", path.display())
                })?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn overlay_loose_assets(data: &Path, vfs: &Path, files: &[PathBuf]) -> Result<()> {
    let mut seen = BTreeMap::<String, PathBuf>::new();
    for source in files
        .iter()
        .filter(|path| extension(path, &["dds", "nif", "pex", "lod"]))
    {
        let relative = source.strip_prefix(data)?;
        let (kind, extension) = if extension(source, &["dds"]) {
            (AssetKind::Texture, "dds")
        } else if extension(source, &["nif"]) {
            (AssetKind::Mesh, "nif")
        } else if extension(source, &["pex"]) {
            (AssetKind::Script, "pex")
        } else {
            (AssetKind::LodSettings, "lod")
        };
        let canonical = canonical_asset_path(&relative.to_string_lossy(), kind, extension)?;
        if let Some(previous) = seen.insert(canonical.clone(), source.to_owned()) {
            bail!(
                "loose assets contain normalized path collision for {canonical}: {} and {}",
                previous.display(),
                source.display()
            );
        }
        let destination = vfs.join(canonical);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        // A reused archive entry links its `vfs` file to the cache blob, so this override has to
        // replace the path rather than write through it: copying over the link would truncate the
        // file every other name shares, the blob and the previous output included.
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

/// Select explicit plugins or direct Data children, warning about nested-only discovery.
fn plugin_paths(
    config: &PipelineConfig,
    files: &[PathBuf],
    notices: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    if let Some(path) = &config.plugins_file {
        return read_plugins_txt(path, &config.data_dir);
    }
    let discovered: Vec<_> = files
        .iter()
        .filter(|path| extension(path, &["esm", "esp", "esl"]))
        .collect();
    let mut plugins: Vec<_> = discovered
        .iter()
        .copied()
        .filter(|path| {
            path.strip_prefix(&config.data_dir)
                .is_ok_and(|relative| relative.components().count() == 1)
        })
        .cloned()
        .collect();
    if !discovered.is_empty() && plugins.is_empty() {
        // The caller forwards notices on the progress channel; printing here would splice
        // into the CLI's status line.
        notices.push(format!(
            "found {} plugin {}, but none directly in {}; plugins in subfolders are ignored",
            discovered.len(),
            if discovered.len() == 1 {
                "file"
            } else {
                "files"
            },
            config.data_dir.display()
        ));
    }
    plugins.sort_by_key(|path| {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let rank = match name.as_str() {
            "skyrim.esm" => 0,
            "update.esm" => 1,
            "dawnguard.esm" => 2,
            "hearthfires.esm" => 3,
            "dragonborn.esm" => 4,
            _ => 10,
        };
        (rank, name)
    });
    crate::esm::load_order::order_discovered_plugins(plugins)
}

pub(crate) fn sort_archives_by_load_order(archives: &mut [PathBuf], plugins: &[PathBuf]) {
    archives.sort_by_cached_key(|archive| {
        (
            archive_load_order_priority(archive, plugins).unwrap_or(usize::MAX),
            archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_lowercase(),
        )
    });
}

pub(crate) fn archive_load_order_priority(archive: &Path, plugins: &[PathBuf]) -> Option<usize> {
    let plugin_stems = plugins
        .iter()
        .filter_map(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let stem = archive
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    plugin_stems
        .iter()
        .enumerate()
        .filter(|(_, plugin)| {
            stem == plugin.as_str()
                || stem
                    .strip_prefix(plugin.as_str())
                    .and_then(|suffix| suffix.chars().next())
                    .is_some_and(|separator| matches!(separator, ' ' | '-' | '_'))
        })
        .map(|(index, _)| index)
        .next()
}

fn extension(path: &Path, expected: &[&str]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            expected
                .iter()
                .any(|expected| value.eq_ignore_ascii_case(expected))
        })
}

/// Deletes staged meshes no provenance source vouches for.
///
/// `verified` holds staged-relative mesh paths (forward slashes) whose bytes
/// a provenance record describes: the PR31 staging journal once merged. A
/// mesh in that set survives so the journal reuse gate below can certify it;
/// every other staged mesh is unverified and goes, so a resume can never
/// publish bytes this converter did not verify.
fn invalidate_staged_mesh_outputs(staging: &Path, verified: &BTreeSet<String>) -> Result<()> {
    let vfs = staging.join("vfs");
    for entry in WalkDir::new(staging)
        .into_iter()
        .filter_entry(|entry| entry.path() != vfs.as_path())
    {
        let entry = entry?;
        if !entry.file_type().is_file() || !extension(entry.path(), &["glb"]) {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(staging)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if verified.contains(&relative) {
            continue;
        }
        fs::remove_file(entry.path()).wrap_err_with(|| {
            format!(
                "failed to invalidate staged mesh {}",
                entry.path().display()
            )
        })?;
    }
    Ok(())
}

fn invalidate_staged_generated_outputs(staging: &Path) -> Result<()> {
    // Rebuild from current inputs so removed sources cannot leave orphaned
    // metadata. Keep partial assets until the effective VFS is rebuilt: the
    // journal still proves which current outputs a resume can reuse.
    for relative in [
        "skyrim_world.db",
        "skyrim_world.db-wal",
        "skyrim_world.db-shm",
        "cell_cache.rkyv",
        "integration-report.json",
        "lod-manifest.json",
        "metadata-rebuild.json",
        "metadata-source-conversion-manifest.json",
    ] {
        let path = staging.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to invalidate {}", path.display()));
            }
        }
    }
    let lod_dir = staging.join("lod");
    if lod_dir.exists() {
        fs::remove_dir_all(&lod_dir)?;
    }
    Ok(())
}

fn remove_orphan_staged_assets(staging: &Path, sources: &[PathBuf]) -> Result<()> {
    let vfs = staging.join("vfs");
    let mut expected = BTreeSet::new();
    for source in sources {
        let (kind, output_extension) = if extension(source, &["nif"]) {
            (AssetKind::Mesh, "glb")
        } else if extension(source, &["dds"]) {
            (AssetKind::Texture, "ktx2")
        } else if extension(source, &["pex"]) {
            (AssetKind::Script, "luau")
        } else {
            continue;
        };
        expected.insert(PathBuf::from(canonical_asset_path(
            &source.strip_prefix(&vfs)?.to_string_lossy(),
            kind,
            output_extension,
        )?));
    }
    for entry in WalkDir::new(staging)
        .into_iter()
        .filter_entry(|entry| entry.path() != vfs.as_path())
    {
        let entry = entry?;
        if entry.file_type().is_file()
            && extension(entry.path(), &["glb", "ktx2", "luau"])
            && !expected.contains(entry.path().strip_prefix(staging)?)
        {
            fs::remove_file(entry.path()).wrap_err_with(|| {
                format!(
                    "failed to remove orphaned staged asset {}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

/// Removes a staging folder after a successful publish, unless it is or holds the Skyrim Data
/// folder: a `--resume-staging` folder is named by the user, and removing it must never take the
/// game data with it. A folder that is kept, or fails to go, is reported on stderr and left behind;
/// the run has already published, so neither fails it. Returns whether the folder was removed.
fn remove_staging(staging: &Path, data_dir: &Path) -> bool {
    match (fs::canonicalize(staging), fs::canonicalize(data_dir)) {
        (Ok(staging_path), Ok(data_path)) if !data_path.starts_with(&staging_path) => {}
        (Ok(_), Ok(_)) => {
            eprintln!(
                "warning: kept the staging folder {} because it holds the Skyrim Data folder {}",
                staging.display(),
                data_dir.display()
            );
            return false;
        }
        _ => {
            eprintln!(
                "warning: kept the staging folder {}: could not compare it with the Skyrim Data folder {}",
                staging.display(),
                data_dir.display()
            );
            return false;
        }
    }
    match fs::remove_dir_all(staging) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "warning: could not remove the staging folder {}: {error}",
                staging.display()
            );
            false
        }
    }
}

fn remove_invalid_staged_output(target: &Path) -> Result<()> {
    match fs::remove_file(target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "failed to remove invalid staged output {}",
                target.display()
            )
        }),
    }
}

fn remove_partial_output_after_failure(
    target: &Path,
    conversion: Result<()>,
) -> (Result<()>, bool) {
    match conversion {
        Ok(()) => (Ok(()), false),
        Err(error) => match remove_invalid_staged_output(target) {
            Ok(()) => (Err(error), false),
            Err(remove_error) => (Err(remove_error), true),
        },
    }
}

/// Strips the leading asset kind folder (e.g., "textures", "meshes", "scripts")
/// from a relative path in a case-insensitive manner.
///
/// This avoids creating double-nested output directory structures when processing
/// assets extracted from BSA archives or loose mod folders with mixed-case naming
/// (such as `Textures\actors\dragon.dds` or `Meshes\armor\iron.nif`).
pub(crate) fn staging_path(output: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // The whole file name, not `with_extension`: a dotted output name (`Skyrim.Converted`) must
    // give `Skyrim.Converted.staging-...`, the prefix resuming and `find_resumable_staging` expect.
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!("{name}.staging-{}-{stamp}", std::process::id()))
}

/// Where the staging journal waits while its staging directory is published.
#[cfg(test)]
fn parked_journal_path(staging: &Path) -> PathBuf {
    let mut name = staging.file_name().unwrap_or_default().to_os_string();
    name.push(".journal.jsonl");
    staging.with_file_name(name)
}

/// Whether the staged `glb` holds the bytes the previous manifest recorded for it, converted
/// from the same source this run found. The previous manifest was published together with its
/// prune records, so a match means those records describe the staged mesh.
fn staged_matches_previous_entry(
    batch: &ConversionBatch<'_>,
    entries_by_output: &BTreeMap<&str, Vec<&str>>,
    glb: &str,
    staged_glb: &Path,
) -> bool {
    let Ok(staged_hash) = hash_file(staged_glb) else {
        return false;
    };
    entries_by_output.get(glb).is_some_and(|keys| {
        keys.iter().any(|key| {
            let entry = &batch.manifest.entries[*key];
            batch.previous.entries.get(*key).is_some_and(|previous| {
                previous.output == glb
                    && previous.source_hash == entry.source_hash
                    && previous.output_hash == staged_hash
            })
        })
    })
}

/// Provenance for a cache entry whose output is now complete inside staging.
fn staged_output(entry: &CacheEntry, configuration_hash: &str) -> StagedOutput {
    StagedOutput {
        schema_version: CONVERTER_SCHEMA_VERSION,
        configuration_hash: configuration_hash.to_owned(),
        source_hash: entry.source_hash.clone(),
        output_size: entry.output_size,
        output_hash: entry.output_hash.clone(),
    }
}

/// Publishes only runtime artifacts. Staging keeps `vfs/` and
/// `.ingestion-cache/` as build workspace; those never land in `output`.
fn publish_runtime_pack(
    staging: &Path,
    output: &Path,
    report: &PipelineReport,
    lock: &AssetLock,
) -> Result<()> {
    // Beside staging, not inside it: a resumed staging dir keeps the
    // previous pack's linked files, so the new pack must not share inodes
    // with anything a resumed run will later unlink and rewrite.
    let pack_staging = staging.with_extension(format!(
        "pack-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if pack_staging.exists() {
        fs::remove_dir_all(&pack_staging)?;
    }
    fs::create_dir_all(&pack_staging)?;
    let published = (|| -> Result<()> {
        for artifact in &report.artifacts {
            let source = staging.join(artifact);
            if !source.is_file() {
                continue;
            }
            let destination = pack_staging.join(artifact);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            link_or_copy(&source, &destination).wrap_err_with(|| {
                format!(
                    "failed to publish {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        }
        ensure!(
            pack_staging.join("conversion-manifest.json").is_file(),
            "runtime pack is missing conversion-manifest.json"
        );
        publish_directory_locked(&pack_staging, output, true, lock)
    })();
    if published.is_err() {
        let _ = fs::remove_dir_all(&pack_staging);
    }
    published
}

/// Copies new ingestion blobs into the persistent cache root outside the pack.
fn persist_ingestion_cache(staging_cache: &Path, cache_root: &Path) -> Result<()> {
    for entry in WalkDir::new(staging_cache).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        // A spill copy (`<hash>.N`) only stands in for a full blob while a run links files out of
        // it; restoring makes fresh ones in staging, so persisting them would just duplicate bytes.
        if is_spill_copy(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let relative = entry.path().strip_prefix(staging_cache)?;
        let destination = cache_root.join(relative);
        if destination.is_file() {
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        link_or_copy(entry.path(), &destination).wrap_err_with(|| {
            format!(
                "failed to persist cache blob {} to {}",
                entry.path().display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

/// Compiles Phase 1 terrain LOD chunks for every worldspace with a valid
/// origin, after the world database and cell cache exist and before
/// integration validation reads them.
///
/// Settings resolution per worldspace: an explicit `lod_origins` entry wins
/// (custom worlds), otherwise the `lodsettings/<WorldspaceEDID>.lod` sidecar
/// supplies its origin and Skyrim grid metadata (installed worlds). Missing or invalid
/// settings omit only that world's LOD; full-detail conversion remains usable.
///
/// The build identity is one hash over the ordered plugin bytes, every
/// compiled chunk's content hash, the configuration hash, and the converter
/// schema (BUILD-01/02). It is written to `lod_build` and to the published
/// manifest; the runtime refuses chunks whose manifest identity differs.
pub(crate) async fn compile_lod_chunks(
    config: &PipelineConfig,
    staging: &Path,
    plugins: &[PathBuf],
    plugin_hashes: &[String],
    progress_tx: &Sender<ProgressEvent>,
    report: &mut PipelineReport,
) -> Result<()> {
    compile_lod_chunks_with_cancel(
        config,
        staging,
        plugins,
        plugin_hashes,
        progress_tx,
        &Cancellation::new(),
        report,
    )
    .await
}

async fn compile_lod_chunks_with_cancel(
    config: &PipelineConfig,
    staging: &Path,
    plugins: &[PathBuf],
    plugin_hashes: &[String],
    progress_tx: &Sender<ProgressEvent>,
    cancellation: &Cancellation,
    report: &mut PipelineReport,
) -> Result<()> {
    let db_path = staging.join("skyrim_world.db");
    if plugins.is_empty() || !db_path.is_file() {
        return Ok(());
    }
    let connection = Connection::open(&db_path)?;
    let mut worlds: Vec<(u32, String)> = connection
        .prepare("SELECT id, editor_id FROM worldspaces ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    // Deterministic order regardless of rowid layout.
    worlds.sort();
    send(
        progress_tx,
        ProgressStage::LodChunks,
        0,
        worlds.len() as u64,
        None,
        "Compiling terrain LOD chunks",
    )
    .await;
    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut world_settings = Vec::with_capacity(worlds.len());
    let mut compiled_worlds = 0u64;
    let mut terrain_sources = BTreeMap::new();
    let compiler_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.cpu_jobs)
        .build()
        .wrap_err("failed to create terrain LOD compiler pool")?;
    for (index, (worldspace_id, editor_id)) in worlds.iter().enumerate() {
        interrupt(cancellation)?;
        let settings = match resolve_lod_settings(config, &staging.join("vfs"), editor_id) {
            Ok(Some(settings)) => settings,
            Ok(None) => {
                let message = format!(
                    "worldspace {editor_id} ({worldspace_id:08X}) has no LOD origin: no lod_origins entry and no lodsettings/{editor_id}.lod; its terrain LOD is skipped"
                );
                report.lod_warnings.push(message);
                world_settings.push(serde_json::json!({
                    "worldspace_id": worldspace_id,
                    "editor_id": editor_id,
                    "status": "missing",
                }));
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
            Err(error) => {
                report.lod_warnings.push(format!(
                    "worldspace {editor_id} ({worldspace_id:08X}) has invalid LOD settings: {error:#}; its terrain LOD is skipped"
                ));
                world_settings.push(serde_json::json!({
                    "worldspace_id": worldspace_id,
                    "editor_id": editor_id,
                    "status": "invalid",
                }));
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
        };
        let origin = settings.origin;
        world_settings.push(serde_json::json!({
            "worldspace_id": worldspace_id,
            "editor_id": editor_id,
            "status": "ready",
            "origin": [origin.grid_x, origin.grid_y],
            "sidecar": settings.sidecar.map(|sidecar| serde_json::json!({
                "stride": sidecar.stride,
                "min_level": sidecar.min_level,
                "max_level": sidecar.max_level,
            })),
        }));
        connection.execute(
            "UPDATE worldspaces SET lod_origin_x = ?1, lod_origin_y = ?2 WHERE id = ?3",
            rusqlite::params![origin.grid_x, origin.grid_y, worldspace_id],
        )?;
        let cells = exterior_terrain_cells(&connection, *worldspace_id)?;
        if cells.is_empty() {
            send(
                progress_tx,
                ProgressStage::LodChunks,
                (index + 1) as u64,
                worlds.len() as u64,
                None,
                "Compiling terrain LOD chunks",
            )
            .await;
            continue;
        }
        let lookup: std::collections::HashMap<u32, (i32, i32)> = cells
            .iter()
            .map(|(grid_x, grid_y, cell_id)| (*cell_id, (*grid_x, *grid_y)))
            .collect();
        let inputs = read_cached_heights(&staging.join("cell_cache.rkyv"), &lookup)?;
        let textures = match TerrainTextures::load(&connection, &staging.join("vfs"), &inputs) {
            Ok(textures) => textures,
            Err(error) => {
                report.lod_warnings.push(format!("worldspace {editor_id} ({worldspace_id:08X}) terrain LOD skipped: invalid material inputs: {error:#}"));
                if let Some(settings) = world_settings.last_mut() {
                    settings["status"] = serde_json::json!("invalid_material");
                    settings["material_error"] = serde_json::json!(format!("{error:#}"));
                }
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
        };
        let compiled = compiler_pool
            .install(|| compile_world_terrain(*worldspace_id, origin, &inputs, &textures));
        interrupt(cancellation)?;
        textures.verify_sources(&staging.join("vfs"))?;
        let chunks = match compiled {
            Ok(chunks) => chunks,
            Err(error) => {
                report.lod_warnings.push(format!("worldspace {editor_id} ({worldspace_id:08X}) terrain LOD skipped: invalid compiler content: {error:#}"));
                if let Some(settings) = world_settings.last_mut() {
                    settings["status"] = serde_json::json!("invalid_content");
                    settings["compiler_error"] = serde_json::json!(format!("{error:#}"));
                }
                send(
                    progress_tx,
                    ProgressStage::LodChunks,
                    (index + 1) as u64,
                    worlds.len() as u64,
                    None,
                    "Compiling terrain LOD chunks",
                )
                .await;
                continue;
            }
        };
        terrain_sources.extend(textures.source_hashes);
        for chunk in &chunks {
            chunk_hashes.push(format!(
                "{}:{}",
                shared::lod::chunk_payload_path(chunk.key),
                hash_bytes(&chunk.glb)
            ));
        }
        // Payloads and their index rows now; the single `lod_build` row
        // after every world is compiled, once the identity is final.
        publish_chunks(&connection, staging, None, &chunks)?;
        report.lod_chunks += chunks.len() as u64;
        report.artifacts.extend(
            chunks
                .iter()
                .map(|chunk| PathBuf::from(shared::lod::chunk_payload_path(chunk.key))),
        );
        compiled_worlds += 1;
        send(
            progress_tx,
            ProgressStage::LodChunks,
            (index + 1) as u64,
            worlds.len() as u64,
            None,
            "Compiling terrain LOD chunks",
        )
        .await;
    }
    chunk_hashes.sort();
    let current_plugin_hashes = plugins
        .iter()
        .map(|plugin| hash_file(plugin))
        .collect::<Result<Vec<_>>>()?;
    color_eyre::eyre::ensure!(
        current_plugin_hashes == plugin_hashes,
        "plugin inputs changed during conversion; refusing to publish an LOD build from a mixed input generation"
    );
    let identity = build_identity(
        plugin_hashes,
        &chunk_hashes,
        &configuration_hash(config)?,
        &world_settings,
        &terrain_sources,
    )?;
    connection.execute(
        "INSERT OR REPLACE INTO lod_build(id, build_identity) VALUES (1, ?1)",
        rusqlite::params![identity],
    )?;
    if compiled_worlds > 0 {
        report.artifacts.push(PathBuf::from("lod-manifest.json"));
        let manifest = LodManifest {
            build_identity: identity,
            converter_schema: CONVERTER_SCHEMA_VERSION,
            world_database_schema: shared::WORLD_DATABASE_SCHEMA_VERSION,
            chunks: report.lod_chunks,
            land_texture_repeats_per_cell: shared::LAND_TEXTURE_REPEATS_PER_CELL,
            terrain_sources,
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(staging.join("lod-manifest.json"), &bytes)?;
    }
    Ok(())
}

/// Whether a cache file name is a spill copy of a blob: 64 hex digits, a dot and a number.
fn is_spill_copy(name: &str) -> bool {
    name.split_once('.').is_some_and(|(hash, index)| {
        hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
            && !index.is_empty()
            && index.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Removes ingestion-cache blobs (and spill files) no longer referenced by
/// the published manifest. Without this, removed or replaced archives leave
/// orphaned blobs behind forever, since publication only adds.
fn prune_stale_ingestion_blobs(cache_root: &Path, manifest: &ConversionManifest) -> Result<()> {
    use std::collections::BTreeSet;
    if !cache_root.is_dir() {
        return Ok(());
    }
    let mut live = BTreeSet::new();
    for entry in manifest.archives.values() {
        for file in &entry.files {
            live.insert(file.hash.clone());
        }
    }
    for entry in WalkDir::new(cache_root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        // Blob names are 64 hex chars; spill files append `.N`. Anything
        // else (probes, partials from crashed runs) is left alone.
        let hash = name.split('.').next().unwrap_or("");
        let is_blob = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if is_blob && !live.contains(hash) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedLodSettings {
    origin: LodOrigin,
    sidecar: Option<LodSettings>,
}

/// Resolve against the effective staged VFS, after archive and loose overlays.
fn resolve_lod_settings(
    config: &PipelineConfig,
    vfs: &Path,
    editor_id: &str,
) -> Result<Option<ResolvedLodSettings>> {
    if let Some([x, y]) = config.lod_origins.get(editor_id) {
        return Ok(Some(ResolvedLodSettings {
            origin: LodOrigin::new(*x, *y),
            sidecar: None,
        }));
    }
    let path = sidecar_path(vfs, editor_id)?;
    match fs::metadata(&path) {
        Ok(_) => {
            let settings = LodSettings::read(&path)?;
            Ok(Some(ResolvedLodSettings {
                origin: settings.origin,
                sidecar: Some(settings),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .wrap_err_with(|| format!("failed to inspect LOD settings {}", path.display())),
    }
}

/// One build identity across ordered plugin bytes, compiled chunk payloads,
/// configuration, and converter schema (BUILD-01/02).
fn build_identity(
    plugin_hashes: &[String],
    chunk_hashes: &[String],
    configuration_hash: &str,
    world_settings: &[serde_json::Value],
    terrain_sources: &BTreeMap<String, String>,
) -> Result<String> {
    let canonical = serde_json::json!({
        "converter_schema": CONVERTER_SCHEMA_VERSION,
        "world_database_schema": shared::WORLD_DATABASE_SCHEMA_VERSION,
        "configuration": configuration_hash,
        "land_texture_repeats_per_cell": shared::LAND_TEXTURE_REPEATS_PER_CELL,
        "plugins": plugin_hashes,
        "chunks": chunk_hashes,
        "world_settings": world_settings,
        "terrain_sources": terrain_sources,
    });
    Ok(hash_bytes(&serde_json::to_vec(&canonical)?))
}

/// The published `lod-manifest.json`: the identity the runtime checks before
/// trusting any chunk row or payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LodManifest {
    build_identity: String,
    converter_schema: u32,
    world_database_schema: u32,
    chunks: u64,
    #[serde(default)]
    land_texture_repeats_per_cell: f32,
    terrain_sources: BTreeMap<String, String>,
}

#[cfg(test)]
fn publish_directory(staging: &Path, output: &Path) -> Result<()> {
    publish_directory_with_policy(staging, output, true)
}

pub(crate) fn publish_new_directory(staging: &Path, output: &Path, lock: &AssetLock) -> Result<()> {
    publish_directory_locked(staging, output, false, lock)
}

#[cfg(test)]
fn publish_directory_with_policy(staging: &Path, output: &Path, replace: bool) -> Result<()> {
    reject_symlink_output(output)?;
    let output = resolve_asset_path(output)?;
    let asset_lock = AssetLock::acquire_exclusive(&output).wrap_err_with(|| {
        format!(
            "failed to lock asset directory {} for publication",
            output.display()
        )
    })?;
    publish_directory_locked(staging, &output, replace, &asset_lock)
}

fn reject_symlink_output(output: &Path) -> Result<()> {
    match output.symlink_metadata() {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing symlinked asset output {}; use its real directory",
            output.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn publish_directory_locked(
    staging: &Path,
    output: &Path,
    replace: bool,
    lock: &AssetLock,
) -> Result<()> {
    reject_symlink_output(output)?;
    ensure!(
        lock.mode() == AssetLockMode::Exclusive && lock.asset_dir() == resolve_asset_path(output)?,
        "publication requires the exclusive destination lock"
    );
    // Checked again under the publication lock before the old output is moved.
    crate::config::check_output_dir(output)?;
    ensure!(
        replace
            || output
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "metadata rebuild requires a new output directory: {}",
        output.display()
    );
    if !replace {
        return fs::rename(staging, output).wrap_err("failed to publish new metadata output");
    }
    recover_interrupted_publication(output)?;
    let backup = output.with_file_name(format!(
        "{}{}",
        publication_backup_prefix(output)?,
        std::process::id()
    ));
    if backup.exists() {
        bail!("refusing to overwrite stale backup {}", backup.display());
    }
    let _backup_lock = AssetLock::acquire_exclusive(&backup)?;
    let record_path = publication_record_path(output)?;
    if output.exists() {
        write_publication_record(output, &backup, staging)?;
        fs::rename(output, &backup).wrap_err("failed to preserve previous asset output")?;
        // The folder is checked once more under its backup name: something written into it
        // between the check above and the rename would otherwise be deleted with it below.
        if let Err(error) = crate::config::check_output_dir(&backup) {
            if let Err(restore) = fs::rename(&backup, output) {
                bail!(
                    "{error}; the previous output could not be moved back from {} to {}: {restore}",
                    backup.display(),
                    output.display()
                );
            }
            fs::remove_file(&record_path)?;
            return Err(error.into());
        }
    }
    if let Err(error) = fs::rename(staging, output) {
        if backup.exists()
            && let Err(restore_error) = fs::rename(&backup, output)
        {
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to publish converted assets and failed to restore last-good output from {}: {restore_error}",
                    backup.display()
                )
            });
        }
        if record_path.is_file() {
            fs::remove_file(&record_path)?;
        }
        return Err(error).wrap_err("failed to publish converted assets");
    }
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    if record_path.is_file() {
        fs::remove_file(record_path)?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicationRecord {
    destination: PathBuf,
    backup_name: String,
    previous_manifest_hash: Option<String>,
    next_manifest_hash: Option<String>,
    generated_artifacts: BTreeMap<String, ArtifactSeal>,
    next_generated_artifacts: BTreeMap<String, ArtifactSeal>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactSeal {
    size: u64,
    sha256: String,
}

fn publication_record_path(output: &Path) -> Result<PathBuf> {
    let name = output
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("output has no name"))?;
    Ok(output.with_file_name(format!(".{}.publication.json", name.to_string_lossy())))
}

fn manifest_hash(root: &Path) -> Result<Option<String>> {
    let path = root.join("conversion-manifest.json");
    if path.is_file() {
        Ok(Some(hash_file(&path)?))
    } else {
        Ok(None)
    }
}

// Seal only generated files here; converted outputs already have manifest hashes.
fn seal_generated_artifacts(output: &Path) -> Result<BTreeMap<String, ArtifactSeal>> {
    let manifest_path = output.join("conversion-manifest.json");
    let converted: BTreeSet<String> = if manifest_path.is_file() {
        let manifest: ConversionManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest
            .entries
            .values()
            .map(|entry| entry.output.clone())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut generated_artifacts = BTreeMap::new();
    for entry in WalkDir::new(output)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(entry.file_name().to_str(), Some("vfs" | ".ingestion-cache"))
        })
    {
        let entry = entry?;
        ensure!(
            !entry.file_type().is_symlink(),
            "refusing symlink in publication backup: {}",
            entry.path().display()
        );
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(output)?
            .to_string_lossy()
            .replace('\\', "/");
        if converted.contains(&relative) || relative == "conversion-manifest.json" {
            continue;
        }
        generated_artifacts.insert(
            relative,
            ArtifactSeal {
                size: entry.metadata()?.len(),
                sha256: hash_file(entry.path())?,
            },
        );
    }
    Ok(generated_artifacts)
}

fn write_publication_record(output: &Path, backup: &Path, staging: &Path) -> Result<()> {
    use std::io::Write;
    let record = PublicationRecord {
        destination: resolve_asset_path(output)?,
        backup_name: backup
            .file_name()
            .ok_or_else(|| color_eyre::eyre::eyre!("backup has no name"))?
            .to_string_lossy()
            .into_owned(),
        previous_manifest_hash: manifest_hash(output)?,
        next_manifest_hash: manifest_hash(staging)?,
        generated_artifacts: seal_generated_artifacts(output)?,
        next_generated_artifacts: seal_generated_artifacts(staging)?,
    };
    let record_path = publication_record_path(output)?;
    let mut file = tempfile::NamedTempFile::new_in(record_path.parent().unwrap())?;
    file.write_all(&serde_json::to_vec_pretty(&record)?)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(record_path)?;
    Ok(())
}

/// The caller holds the destination's exclusive lock. Unowned backups are never adopted.
fn recover_interrupted_publication(output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let record_path = publication_record_path(output)?;
    let record = match record_path.symlink_metadata() {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "invalid publication ownership record"
            );
            serde_json::from_slice::<PublicationRecord>(&fs::read(&record_path)?)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !output.exists() {
                let prefix = publication_backup_prefix(output)?;
                for entry in fs::read_dir(parent)? {
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    ensure!(
                        !(kind.is_dir() || kind.is_symlink())
                            || !entry.file_name().to_string_lossy().starts_with(&prefix),
                        "unowned publication backup {}; manual recovery required",
                        entry.path().display()
                    );
                }
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        record.destination == resolve_asset_path(output)?,
        "publication record belongs to another destination"
    );
    ensure!(
        Path::new(&record.backup_name).components().count() == 1
            && !record.backup_name.contains(['/', '\\', ':'])
            && record
                .backup_name
                .starts_with(&publication_backup_prefix(output)?),
        "invalid owned backup name"
    );
    let backup = parent.join(&record.backup_name);
    let _backup_lock = AssetLock::acquire_exclusive(&backup)?;
    if output.exists() {
        let current = manifest_hash(output)?;
        if current == record.next_manifest_hash {
            if backup.exists() {
                verify_package_seal(
                    output,
                    &record.next_manifest_hash,
                    &record.next_generated_artifacts,
                )?;
                validate_publication_package(output)?;
                verify_publication_seal(&backup, &record)?;
                crate::config::check_output_dir(&backup)?;
                fs::remove_dir_all(&backup)?;
            }
        } else {
            ensure!(
                !backup.exists() && current == record.previous_manifest_hash,
                "ambiguous interrupted publication; outputs left untouched"
            );
        }
    } else {
        verify_publication_seal(&backup, &record)?;
        validate_publication_package(&backup)?;
        fs::rename(&backup, output).wrap_err("failed to restore owned last-good assets")?;
    }
    fs::remove_file(record_path)?;
    Ok(())
}

fn verify_publication_seal(backup: &Path, record: &PublicationRecord) -> Result<()> {
    verify_package_seal(
        backup,
        &record.previous_manifest_hash,
        &record.generated_artifacts,
    )
}

fn verify_package_seal(
    backup: &Path,
    expected_manifest_hash: &Option<String>,
    generated_artifacts: &BTreeMap<String, ArtifactSeal>,
) -> Result<()> {
    ensure!(
        backup.symlink_metadata()?.is_dir() && !backup.symlink_metadata()?.file_type().is_symlink(),
        "owned backup is not a real directory"
    );
    ensure!(
        manifest_hash(backup)? == *expected_manifest_hash,
        "backup manifest changed since publication began"
    );
    for (relative, seal) in generated_artifacts {
        let path = resolve_asset_uri(backup, &backup.join("conversion-manifest.json"), relative)?;
        ensure!(
            path.is_file()
                && fs::metadata(&path)?.len() == seal.size
                && hash_file(&path)? == seal.sha256,
            "generated backup artifact is missing or changed: {relative}"
        );
    }
    Ok(())
}

fn publication_backup_prefix(output: &Path) -> Result<String> {
    let name = output
        .file_name()
        .ok_or_else(|| color_eyre::eyre::eyre!("publication output has no directory name"))?;
    Ok(format!("{}.backup-", name.to_string_lossy()))
}

/// Checks structural integrity for record-owned recovery. A published pack may
/// be incomplete; recovery preserves that status rather than enforcing Play readiness.
fn validate_publication_package(backup: &Path) -> Result<()> {
    let root = fs::canonicalize(backup)?;
    for entry in WalkDir::new(&root).follow_links(false) {
        ensure!(
            !entry?.file_type().is_symlink(),
            "backup contains a symlink"
        );
    }
    let manifest: ConversionManifest =
        serde_json::from_slice(&fs::read(root.join("conversion-manifest.json"))?)?;
    ensure!(
        (shared::MIN_RUNTIME_CONVERTER_SCHEMA_VERSION..=CONVERTER_SCHEMA_VERSION)
            .contains(&manifest.schema_version),
        "backup has an unsupported conversion manifest"
    );
    ensure!(
        !manifest.entries.is_empty() || root.join("scripts/papyrus_runtime.luau").is_file(),
        "backup has no converted or generated runtime artifacts"
    );
    for entry in manifest.entries.values() {
        let path = resolve_asset_uri(&root, &root.join("conversion-manifest.json"), &entry.output)?;
        ensure!(
            path.is_file()
                && fs::metadata(&path)?.len() == entry.output_size
                && hash_file(&path)? == entry.output_hash,
            "backup output does not match its manifest: {}",
            entry.output
        );
    }
    let has_world = manifest.inputs_by_kind.get("esm").copied().unwrap_or(0) > 0
        || [
            "skyrim_world.db",
            "cell_cache.rkyv",
            "integration-report.json",
            "lod-manifest.json",
        ]
        .iter()
        .any(|name| root.join(name).exists());
    if has_world {
        let database = Connection::open_with_flags(
            root.join("skyrim_world.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let integrity: String =
            database.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        ensure!(integrity == "ok", "backup database integrity failed");
        let version: u32 =
            database.query_row("SELECT version FROM schema_info", [], |row| row.get(0))?;
        ensure!(
            shared::supports_runtime_world_database_schema(version),
            "backup database schema {version} is unsupported"
        );
        ensure!(
            version >= 5 || !root.join("lod-manifest.json").exists(),
            "legacy backup database cannot advertise LOD"
        );
        read_cached_heights(
            &root.join("cell_cache.rkyv"),
            &std::collections::HashMap::new(),
        )?;
        let integration: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("integration-report.json"))?)?;
        ensure!(
            integration["passed"].is_boolean() && integration["schema_version"] == version,
            "backup integration report is invalid"
        );
        if version >= 5 {
            let count: u64 =
                database.query_row("SELECT count(*) FROM lod_chunks", [], |row| row.get(0))?;
            if count > 0 || root.join("lod-manifest.json").exists() {
                let lod: LodManifest =
                    serde_json::from_slice(&fs::read(root.join("lod-manifest.json"))?)?;
                let identity: String = database.query_row(
                    "SELECT build_identity FROM lod_build WHERE id=1",
                    [],
                    |row| row.get(0),
                )?;
                ensure!(
                    identity == lod.build_identity
                        && lod.chunks == count
                        && lod.world_database_schema == version
                        && lod.converter_schema == manifest.schema_version,
                    "backup LOD identity/count/schema mismatch"
                );
                let rows = database
                    .prepare("SELECT payload_path, content_hash FROM lod_chunks")?
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (relative, expected) in rows {
                    let path =
                        resolve_asset_uri(&root, &root.join("lod-manifest.json"), &relative)?;
                    ensure!(
                        hash_file(&path)? == expected,
                        "backup LOD payload hash mismatch: {relative}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Stops the worker pool and keeps the first error. The batch still drains
/// its channel and awaits the pool before returning it, so no worker writes
/// into a staging directory the caller is about to remove.
fn stop_batch(
    cancelled: &AtomicBool,
    first_error: &mut Option<color_eyre::eyre::Error>,
    error: color_eyre::eyre::Error,
) {
    cancelled.store(true, Ordering::Relaxed);
    first_error.get_or_insert(error);
}

async fn send(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    completed: u64,
    total: u64,
    current_file: Option<PathBuf>,
    message: &str,
) {
    let _ = tx
        .send(ProgressEvent::new(
            stage,
            completed,
            total,
            current_file,
            message,
        ))
        .await;
}

/// Sends a one-off line for the person watching: the CLI ends any open status line before it, so a
/// warning never splices into a redrawn line.
async fn send_notice(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    current_file: Option<PathBuf>,
    message: &str,
) {
    let _ = tx
        .send(ProgressEvent::notice(stage, current_file, message))
        .await;
}

/// Sends a progress event that also carries byte counts, which the status line weighs a stage by.
#[allow(clippy::too_many_arguments)]
async fn send_with_bytes(
    tx: &Sender<ProgressEvent>,
    stage: ProgressStage,
    completed: u64,
    total: u64,
    current_file: Option<PathBuf>,
    message: &str,
    bytes: (u64, u64),
) {
    let _ = tx
        .send(
            ProgressEvent::new(stage, completed, total, current_file, message)
                .with_bytes(bytes.0, bytes.1),
        )
        .await;
}

/// Stops the run at the next safe point when the user interrupted it. The caller keeps the staging
/// folder, so the run can be resumed where it stopped.
fn interrupt(cancellation: &Cancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        return Err(Interrupted::new().into());
    }
    Ok(())
}

/// Test-only: presses Stop from inside the extractor after a given number of per-entry stop
/// checks of one output's run, so a test stops part way through an archive without racing it.
#[cfg(test)]
mod stop_hook {
    use super::Cancellation;
    use std::{
        path::{Path, PathBuf},
        sync::Mutex,
    };

    static CANCEL_AFTER: Mutex<Vec<(PathBuf, usize)>> = Mutex::new(Vec::new());

    pub(super) fn cancel_after(output: &Path, checks: usize) {
        CANCEL_AFTER
            .lock()
            .unwrap()
            .push((output.to_path_buf(), checks));
    }

    pub(super) fn tick(output: &Path, cancellation: &Cancellation) {
        let mut hooks = CANCEL_AFTER.lock().unwrap();
        if let Some(index) = hooks.iter().position(|(path, _)| path == output) {
            if hooks[index].1 == 0 {
                hooks.remove(index);
                cancellation.cancel();
            } else {
                hooks[index].1 -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_output_name_keeps_its_whole_name_in_the_staging_folder() {
        let staging = staging_path(Path::new("converted/Skyrim.Converted"));
        let name = staging.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Skyrim.Converted.staging-"), "{name}");
        assert_eq!(staging.parent(), Some(Path::new("converted")));
        // And the folder it names is the one a resume finds.
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Skyrim.Converted");
        let staging = staging_path(&output);
        fs::create_dir_all(&staging).unwrap();
        assert_eq!(crate::find_resumable_staging(&output), Some((staging, 0)));
    }
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn lod_settings_change_lod_identity_without_invalidating_asset_cache() {
        let config = PipelineConfig::new("/data", "/output");
        let cache_identity = configuration_hash(&config).unwrap();
        let mut changed = config;
        changed.lod_origins.insert("Tamriel".to_owned(), [-64, 32]);
        assert_eq!(configuration_hash(&changed).unwrap(), cache_identity);

        let first = build_identity(&[], &[], &cache_identity, &[], &BTreeMap::new()).unwrap();
        let second = build_identity(
            &[],
            &[],
            &cache_identity,
            &[serde_json::json!({
                "worldspace_id": 1,
                "editor_id": "Tamriel",
                "status": "ready",
                "origin": [-64, 32],
                "sidecar": null,
            })],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn explicit_lod_origin_precedes_invalid_staged_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let vfs = directory.path().join("vfs");
        fs::create_dir_all(vfs.join("lodsettings")).unwrap();
        fs::write(vfs.join("lodsettings/tamriel.lod"), b"invalid").unwrap();
        let mut config = PipelineConfig::new("/data", "/output");
        assert!(resolve_lod_settings(&config, &vfs, "Tamriel").is_err());
        config.lod_origins.insert("Tamriel".into(), [-2, 4]);
        assert_eq!(
            resolve_lod_settings(&config, &vfs, "Tamriel").unwrap(),
            Some(ResolvedLodSettings {
                origin: LodOrigin::new(-2, 4),
                sidecar: None,
            })
        );
    }

    #[test]
    fn lod_only_omissions_do_not_mark_full_detail_conversion_incomplete() {
        let report = PipelineReport {
            lod_warnings: vec!["worldspace without sidecar".to_owned()],
            ..Default::default()
        };
        assert!(conversion_is_complete(&report));
    }

    #[test]
    fn restores_last_good_output_after_interrupted_directory_swap() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("sentinel"), b"last good").unwrap();
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            entries: BTreeMap::from([(
                "sentinel".to_owned(),
                CacheEntry {
                    source_hash: "source".to_owned(),
                    output: "sentinel".to_owned(),
                    output_size: 9,
                    output_hash: hash_file(&backup.join("sentinel")).unwrap(),
                },
            )]),
            ..Default::default()
        };
        manifest
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        seal_test_backup(&output, &backup);

        recover_interrupted_publication(&output).unwrap();

        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"last good");
        assert!(!backup.exists());
    }

    fn seal_test_backup(output: &Path, backup: &Path) {
        fs::rename(backup, output).unwrap();
        write_publication_record(output, backup, output).unwrap();
        fs::rename(output, backup).unwrap();
    }

    fn write_test_runtime_pack(root: &Path, configuration: &str) {
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            configuration_hash: configuration.to_owned(),
            complete: true,
            ..Default::default()
        }
        .save(&root.join("conversion-manifest.json"))
        .unwrap();
    }

    #[test]
    fn recovery_handles_each_owned_publication_boundary() {
        for boundary in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let output = directory.path().join("assets");
            let backup = directory.path().join("assets.backup-123");
            let staging = directory.path().join("staging");
            write_test_runtime_pack(&output, "previous");
            write_test_runtime_pack(&staging, "next");
            write_publication_record(&output, &backup, &staging).unwrap();
            if boundary >= 1 {
                fs::rename(&output, &backup).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&staging, &output).unwrap();
            }
            if boundary >= 3 {
                fs::remove_dir_all(&backup).unwrap();
            }
            recover_interrupted_publication(&output).unwrap();
            assert!(!backup.exists());
            assert!(!publication_record_path(&output).unwrap().exists());
            assert_eq!(
                published_manifest(&output).configuration_hash,
                if boundary < 2 { "previous" } else { "next" }
            );
        }
    }

    #[tokio::test]
    async fn recovery_v86_handles_incomplete_owned_packages_at_each_boundary() {
        for boundary in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let data = directory.path().join("Data");
            let output = directory.path().join("assets");
            let backup = directory.path().join("assets.backup-123");
            let staging = directory.path().join("staging");
            dummy_content::layout::prepare_directory(&data, false).unwrap();
            dummy_content::layout::generate(
                &data,
                dummy_content::layout::DEFAULT_SEED,
                dummy_content::layout::Formats::parse("dds,nif,pex,esm,lodsettings").unwrap(),
            )
            .unwrap();
            let mut config = PipelineConfig::new(&data, &output);
            config.cpu_jobs = 2;
            let report = run_without_progress(config).await;
            assert!(report.complete);
            for root in [&output, &staging] {
                if root == &staging {
                    fs::create_dir_all(root).unwrap();
                    for entry in WalkDir::new(&output) {
                        let entry = entry.unwrap();
                        let destination = root.join(entry.path().strip_prefix(&output).unwrap());
                        if entry.file_type().is_dir() {
                            fs::create_dir_all(destination).unwrap();
                        } else {
                            fs::copy(entry.path(), destination).unwrap();
                        }
                    }
                }
                let mut manifest = published_manifest(root);
                manifest.configuration_hash =
                    if root == &output { "previous" } else { "next" }.into();
                manifest.complete = false;
                manifest
                    .failures
                    .insert("textures/missing.dds".into(), "missing texture".into());
                manifest
                    .save(&root.join("conversion-manifest.json"))
                    .unwrap();
                let report_path = root.join("integration-report.json");
                let mut report: serde_json::Value =
                    serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
                report["passed"] = serde_json::json!(false);
                fs::write(report_path, serde_json::to_vec(&report).unwrap()).unwrap();
            }
            write_publication_record(&output, &backup, &staging).unwrap();
            if boundary >= 1 {
                fs::rename(&output, &backup).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&staging, &output).unwrap();
            }
            if boundary >= 3 {
                fs::remove_dir_all(&backup).unwrap();
            }
            recover_interrupted_publication(&output).unwrap();
            assert!(output.is_dir());
            assert!(!published_manifest(&output).complete);
            assert_eq!(
                published_manifest(&output).configuration_hash,
                if boundary < 2 { "previous" } else { "next" }
            );
            assert_eq!(published_manifest(&output).failures.len(), 1);
            assert!(!backup.exists());
            assert!(!publication_record_path(&output).unwrap().exists());
        }
    }

    #[test]
    fn recovery_v85_preserves_backup_when_replacement_generated_file_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        let staging = directory.path().join("staging");
        write_test_runtime_pack(&output, "previous");
        write_test_runtime_pack(&staging, "next");
        write_publication_record(&output, &backup, &staging).unwrap();
        fs::rename(&output, &backup).unwrap();
        fs::rename(&staging, &output).unwrap();
        fs::remove_file(output.join("scripts/papyrus_runtime.luau")).unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert_eq!(published_manifest(&backup).configuration_hash, "previous");
        assert!(publication_record_path(&output).unwrap().is_file());
        fs::write(output.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(!backup.exists());
    }

    #[test]
    fn recovery_rejects_record_copied_from_another_destination() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        write_test_runtime_pack(&original, "original");
        write_test_runtime_pack(&backup, "unrelated");
        write_publication_record(&original, &backup, &original).unwrap();
        fs::rename(
            publication_record_path(&original).unwrap(),
            publication_record_path(&output).unwrap(),
        )
        .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert_eq!(published_manifest(&backup).configuration_hash, "unrelated");
        assert!(!output.exists());
    }

    #[test]
    fn complete_but_unowned_backup_is_never_adopted() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-other");
        fs::create_dir_all(backup.join("scripts")).unwrap();
        fs::write(backup.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        assert!(validate_publication_package(&backup).is_ok());
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(backup.is_dir());
        assert!(!output.exists());
    }

    #[test]
    fn recovery_refuses_an_owned_backup_held_by_a_reader() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("assets");
        let backup = directory.path().join("assets.backup-123");
        fs::create_dir_all(backup.join("scripts")).unwrap();
        fs::write(backup.join("scripts/papyrus_runtime.luau"), b"return {}").unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        seal_test_backup(&output, &backup);
        let reader = AssetLock::acquire_shared(&backup).unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(backup.is_dir());
        drop(reader);
        recover_interrupted_publication(&output).unwrap();
        assert!(output.is_dir());
    }

    #[tokio::test]
    async fn recovery_verifies_generated_database_cache_manifest_and_chunk_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.cpu_jobs = 2;
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert!(report.lod_chunks > 0);
        let chunk = report
            .artifacts
            .iter()
            .find(|path| path.starts_with("lod"))
            .unwrap()
            .clone();
        let backup = directory.path().join("assets.backup-123");
        fs::rename(&output, &backup).unwrap();
        seal_test_backup(&output, &backup);
        for relative in [
            PathBuf::from("skyrim_world.db"),
            PathBuf::from("cell_cache.rkyv"),
            PathBuf::from("integration-report.json"),
            PathBuf::from("lod-manifest.json"),
            chunk,
        ] {
            let path = backup.join(&relative);
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            assert!(
                recover_interrupted_publication(&output).is_err(),
                "accepted missing {}",
                relative.display()
            );
            assert!(!output.exists());
            let mut corrupt = bytes.clone();
            corrupt[0] ^= 1;
            fs::write(&path, &corrupt).unwrap();
            assert!(
                recover_interrupted_publication(&output).is_err(),
                "accepted changed {}",
                relative.display()
            );
            assert!(!output.exists());
            fs::write(&path, bytes).unwrap();
        }
        recover_interrupted_publication(&output).unwrap();
        assert!(output.join("skyrim_world.db").is_file());
        assert!(!backup.exists());

        // Supported schema-16/world-3 packs retain full-detail recovery.
        fs::remove_dir_all(output.join("lod")).unwrap();
        fs::remove_file(output.join("lod-manifest.json")).unwrap();
        let mut legacy = published_manifest(&output);
        legacy.schema_version = 16;
        legacy
            .save(&output.join("conversion-manifest.json"))
            .unwrap();
        let database = Connection::open(output.join("skyrim_world.db")).unwrap();
        database.execute_batch("UPDATE schema_info SET version=3; DELETE FROM lod_chunks; DELETE FROM lod_chunks_spatial; DELETE FROM lod_build;").unwrap();
        drop(database);
        let integration_path = output.join("integration-report.json");
        let mut integration: serde_json::Value =
            serde_json::from_slice(&fs::read(&integration_path).unwrap()).unwrap();
        integration["schema_version"] = serde_json::json!(3);
        fs::write(integration_path, serde_json::to_vec(&integration).unwrap()).unwrap();
        fs::rename(&output, &backup).unwrap();
        seal_test_backup(&output, &backup);
        recover_interrupted_publication(&output).unwrap();
        assert_eq!(published_manifest(&output).schema_version, 16);
        assert!(!output.join("lod-manifest.json").exists());
    }

    #[tokio::test]
    async fn schema16_meshes_reuse_but_changed_mesh_source_reconverts() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,pex,nif,esm,lodsettings").unwrap(),
        )
        .unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.cpu_jobs = 2;
        run_without_progress(config.clone()).await;
        let path = output.join("conversion-manifest.json");
        let mut old: ConversionManifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old.schema_version = 16;
        old.configuration_hash = configuration_hash_for_schema(&config, 16).unwrap();
        old.save(&path).unwrap();
        let meshes: BTreeMap<_, _> = old
            .entries
            .values()
            .filter(|entry| entry.output.ends_with(".glb"))
            .map(|entry| {
                (
                    entry.output.clone(),
                    fs::read(output.join(&entry.output)).unwrap(),
                )
            })
            .collect();
        assert!(!meshes.is_empty());
        let reused = run_without_progress(config.clone()).await;
        assert_eq!(reused.converted, 0);
        for (relative, bytes) in &meshes {
            assert_eq!(fs::read(output.join(relative)).unwrap(), *bytes);
        }
        fs::write(
            data.join("meshes/generated.nif"),
            dummy_content::nif::static_shape(&dummy_content::nif::StaticShape {
                name: "changed",
                positions: &[[0.0, 0.0, 0.0], [128.0, 0.0, 0.0], [0.0, 128.0, 0.0]],
                normals: &[[0.0, 0.0, 1.0]; 3],
                uvs: &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                indices: &[[0, 1, 2]],
                diffuse: "textures/generated_color.dds",
                normal_texture: "textures/generated_normal.dds",
            })
            .unwrap(),
        )
        .unwrap();
        let changed = run_without_progress(config).await;
        assert!(changed.converted > 0);
        assert_ne!(
            fs::read(output.join("meshes/generated.glb")).unwrap(),
            meshes["meshes/generated.glb"]
        );
    }

    #[tokio::test]
    async fn invalid_world_compiler_content_is_omitted_without_partial_publication() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let staging = directory.path().join("staging");
        dummy_content::layout::prepare_directory(&data, false).unwrap();
        dummy_content::layout::generate(
            &data,
            dummy_content::layout::DEFAULT_SEED,
            dummy_content::layout::Formats::parse("dds,esm,lodsettings").unwrap(),
        )
        .unwrap();
        fs::create_dir_all(staging.join("vfs")).unwrap();
        overlay_loose_assets(&data, &staging.join("vfs"), &discover(&data).unwrap()).unwrap();
        let plugins = vec![data.join("Skyrim.esm")];
        let records =
            EsmParser::convert_plugins_with_records(&plugins, &staging.join("skyrim_world.db"))
                .unwrap();
        write_cell_cache(&records, &staging.join("cell_cache.rkyv")).unwrap();
        let bytes = fs::read(staging.join("cell_cache.rkyv")).unwrap();
        let mut cache = rkyv::from_bytes::<shared::CellCache, rkyv::rancor::Error>(&bytes).unwrap();
        let mut good_cell = cache.cells[0].clone();
        cache.cells[0].heights[0] = f32::NAN;
        fs::write(
            staging.join("cell_cache.rkyv"),
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        let mut config = PipelineConfig::new(&data, directory.path().join("output"));
        config.cpu_jobs = 2;
        let (tx, _rx) = mpsc::channel(64);
        let mut report = PipelineReport::default();
        let hashes = plugins
            .iter()
            .map(|path| hash_file(path).unwrap())
            .collect::<Vec<_>>();
        compile_lod_chunks(&config, &staging, &plugins, &hashes, &tx, &mut report)
            .await
            .unwrap();
        assert_eq!(report.lod_chunks, 0);
        assert!(
            report
                .lod_warnings
                .iter()
                .any(|warning| warning.contains("invalid compiler content"))
        );
        assert!(!staging.join("lod-manifest.json").exists());
        assert!(!staging.join("lod").exists());
        let conn = Connection::open(staging.join("skyrim_world.db")).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM lod_chunks", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );

        let old_cell_id = good_cell.cell_id;
        good_cell.cell_id = 0x00ff0001;
        conn.execute(
            "INSERT INTO worldspaces(id,editor_id,flags) VALUES(2,'ValidWorld',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cells(id,worldspace_id,grid_x,grid_y,flags) SELECT ?1,2,grid_x,grid_y,flags FROM cells WHERE id=?2",
            rusqlite::params![good_cell.cell_id, old_cell_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO land(cell_id,heightmap,vtex,vclr,normals) SELECT ?1,heightmap,vtex,vclr,normals FROM land WHERE cell_id=?2",
            rusqlite::params![good_cell.cell_id, old_cell_id],
        ).unwrap();
        cache.cells.push(good_cell);
        fs::write(
            staging.join("cell_cache.rkyv"),
            rkyv::to_bytes::<rkyv::rancor::Error>(&cache).unwrap(),
        )
        .unwrap();
        config.lod_origins.insert("ValidWorld".into(), [0, 0]);
        let mut mixed = PipelineReport::default();
        compile_lod_chunks(&config, &staging, &plugins, &hashes, &tx, &mut mixed)
            .await
            .unwrap();
        assert!(mixed.lod_chunks > 0);
        assert!(
            mixed
                .lod_warnings
                .iter()
                .any(|warning| warning.contains("invalid compiler content"))
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM lod_chunks WHERE worldspace_id != 2",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert!(staging.join("lod-manifest.json").is_file());
    }

    #[tokio::test]
    async fn conversion_fails_before_staging_when_output_has_a_reader() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        let output = directory.path().join("assets");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&output).unwrap();
        let _reader = AssetLock::acquire_shared(&output).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let error = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("before conversion"));
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn publication_rejects_symlink_output_without_changing_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        let staging = directory.path().join("staging");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(real.join("sentinel"), b"original").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert!(publish_directory(&staging, &alias).is_err());
        assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(real.join("sentinel")).unwrap(), b"original");
        assert!(staging.is_dir());
    }

    #[test]
    fn recovery_preserves_unrelated_and_incomplete_backups() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("sentinel"), b"unrelated").unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.join("sentinel").exists());
        ConversionManifest::default()
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.exists());
    }

    #[test]
    fn dotted_output_does_not_adopt_another_outputs_backup() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("Skyrim.New");
        let backup = directory.path().join("Skyrim.Old.backup-12345");
        fs::create_dir(&backup).unwrap();
        ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            ..Default::default()
        }
        .save(&backup.join("conversion-manifest.json"))
        .unwrap();
        recover_interrupted_publication(&output).unwrap();
        assert!(!output.exists());
        assert!(backup.exists());
        assert_eq!(
            publication_backup_prefix(&output).unwrap(),
            "Skyrim.New.backup-"
        );
    }

    #[test]
    fn recovery_rejects_missing_or_changed_manifest_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let backup = output.with_extension("backup-12345");
        fs::create_dir(&backup).unwrap();
        let manifest = ConversionManifest {
            schema_version: CONVERTER_SCHEMA_VERSION,
            complete: true,
            entries: BTreeMap::from([(
                "asset".to_owned(),
                CacheEntry {
                    source_hash: "source".to_owned(),
                    output: "asset".to_owned(),
                    output_size: 4,
                    output_hash: hash_bytes(b"good"),
                },
            )]),
            ..Default::default()
        };
        manifest
            .save(&backup.join("conversion-manifest.json"))
            .unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        fs::write(backup.join("asset"), b"evil").unwrap();
        assert!(recover_interrupted_publication(&output).is_err());
        assert!(!output.exists());
        assert!(backup.exists());
    }

    #[test]
    fn publication_refuses_to_swap_assets_held_by_a_runtime_reader() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("modern_assets");
        let staging = directory.path().join("staging");
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(output.join("sentinel"), b"last good").unwrap();
        fs::write(staging.join("sentinel"), b"new build").unwrap();
        let _reader = AssetLock::acquire_shared(&output).unwrap();

        let error = publish_directory(&staging, &output).unwrap_err();

        assert!(
            format!("{error:?}").contains("another process holds an incompatible lock"),
            "unexpected publish error: {error:?}"
        );
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"last good");
        assert_eq!(fs::read(staging.join("sentinel")).unwrap(), b"new build");
    }

    #[test]
    fn maps_srgb_runtime_aliases_back_to_their_converted_source() {
        assert_eq!(
            source_texture_key("textures/effects/fire.opensky-srgb.ktx2").unwrap(),
            "textures/effects/fire.ktx2"
        );
        assert_eq!(
            source_texture_key("textures/effects/fire.ktx2").unwrap(),
            "textures/effects/fire.ktx2"
        );
    }

    #[test]
    fn preserves_staged_meshes_and_vfs_during_generated_output_invalidation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("vfs/meshes")).unwrap();
        fs::write(staging.join("meshes/resumable.glb"), b"mesh").unwrap();
        fs::write(staging.join("vfs/meshes/source.glb"), b"source").unwrap();

        invalidate_staged_generated_outputs(staging).unwrap();

        assert!(staging.join("meshes/resumable.glb").is_file());
        assert!(staging.join("vfs/meshes/source.glb").is_file());
    }

    #[test]
    fn removes_staged_assets_without_current_sources() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("vfs/meshes")).unwrap();
        fs::write(staging.join("meshes/orphan.glb"), b"mesh").unwrap();
        fs::write(staging.join("vfs/meshes/source.glb"), b"source").unwrap();

        remove_orphan_staged_assets(staging, &[]).unwrap();

        assert!(!staging.join("meshes/orphan.glb").exists());
        assert!(staging.join("vfs/meshes/source.glb").is_file());
    }

    #[test]
    fn keeps_a_provenance_verified_staged_mesh() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::write(staging.join("meshes/verified.glb"), b"mesh").unwrap();
        fs::write(staging.join("meshes/stale.glb"), b"mesh").unwrap();

        invalidate_staged_mesh_outputs(
            staging,
            &BTreeSet::from(["meshes/verified.glb".to_owned()]),
        )
        .unwrap();

        assert!(staging.join("meshes/verified.glb").is_file());
        assert!(!staging.join("meshes/stale.glb").exists());
    }

    #[test]
    fn removes_partial_asset_output_after_conversion_failure() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("scripts/partial.luau");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"partial output").unwrap();

        let (result, fatal) = remove_partial_output_after_failure(
            &target,
            Err(color_eyre::eyre::eyre!("conversion failed after writing")),
        );

        assert!(result.is_err());
        assert!(!fatal);
        assert!(!target.exists());
    }

    #[test]
    fn publishes_a_distinct_asset_path_for_srgb_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        fs::create_dir_all(staging.join("meshes")).unwrap();
        fs::create_dir_all(staging.join("textures/effects")).unwrap();
        fs::write(staging.join("textures/effects/fire.ktx2"), b"texture").unwrap();
        let mut json = serde_json::to_vec(&serde_json::json!({
            "asset": { "version": "2.0" },
            "images": [{ "uri": "../textures/effects/fire.opensky-srgb.ktx2" }],
            "textures": [{ "source": 0 }],
            "materials": [{
                "pbrMetallicRoughness": { "baseColorTexture": { "index": 0 } }
            }]
        }))
        .unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let mut glb = b"glTF".to_vec();
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&u32::try_from(20 + json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(&u32::try_from(json.len()).unwrap().to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json);
        fs::write(staging.join("meshes/fire.glb"), glb).unwrap();

        let aliases = publish_srgb_texture_aliases(staging).unwrap();

        assert_eq!(
            aliases,
            vec![PathBuf::from("textures/effects/fire.opensky-srgb.ktx2")]
        );
        assert_eq!(fs::read(staging.join(&aliases[0])).unwrap(), b"texture");
    }

    #[test]
    fn loose_asset_override_does_not_write_through_a_linked_vfs_entry() {
        let directory = tempfile::tempdir().unwrap();
        let previous = directory.path().join("previous");
        let previous_vfs = previous.join("vfs/textures/rock.dds");
        fs::create_dir_all(previous_vfs.parent().unwrap()).unwrap();
        fs::write(&previous_vfs, b"archive bytes").unwrap();

        // What a first conversion publishes: the extracted `vfs` file and its content-addressed
        // blob share one file. What a reconversion stages for the same entry is one file with
        // that published blob again.
        let hash = "ab".repeat(32);
        let previous_blob = previous.join(".ingestion-cache/sha256/ab").join(&hash);
        fs::create_dir_all(previous_blob.parent().unwrap()).unwrap();
        link_or_copy(&previous_vfs, &previous_blob).unwrap();
        let staging_vfs = directory.path().join("staging/vfs");
        let staged = staging_vfs.join("textures/rock.dds");
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        link_or_copy(&previous_blob, &staged).unwrap();

        let data = directory.path().join("Data");
        fs::create_dir_all(data.join("textures")).unwrap();
        let loose = data.join("textures/rock.dds");
        fs::write(&loose, b"loose override").unwrap();

        overlay_loose_assets(&data, &staging_vfs, &[loose]).unwrap();

        assert_eq!(fs::read(&staged).unwrap(), b"loose override");
        assert_eq!(
            fs::read(&previous_vfs).unwrap(),
            b"archive bytes",
            "the override wrote through the link into the previous output"
        );
        assert_eq!(fs::read(&previous_blob).unwrap(), b"archive bytes");
    }

    #[test]
    fn orders_archives_by_plugin_load_order() {
        let plugins = vec![
            PathBuf::from("Skyrim.esm"),
            PathBuf::from("Update.esm"),
            PathBuf::from("Example.esp"),
        ];
        let mut archives = vec![
            PathBuf::from("Example - Textures.bsa"),
            PathBuf::from("Skyrim - Textures.bsa"),
            PathBuf::from("Update.bsa"),
            PathBuf::from("Skyrim - Meshes.bsa"),
        ];
        sort_archives_by_load_order(&mut archives, &plugins);
        assert_eq!(
            archives,
            vec![
                PathBuf::from("Skyrim - Meshes.bsa"),
                PathBuf::from("Skyrim - Textures.bsa"),
                PathBuf::from("Update.bsa"),
                PathBuf::from("Example - Textures.bsa"),
            ]
        );
    }

    #[tokio::test]
    async fn converts_and_reuses_assets_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("scripts/two.pex"),
            dummy_content::pex::minimal("Two").unwrap(),
        )
        .unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.cpu_jobs = 2;
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let first = AssetPipeline::run_async(config.clone(), tx).await.unwrap();
        drain.await.unwrap();
        assert_eq!(first.converted, 2);
        assert!(output.join("scripts/one.luau").is_file());
        assert!(output.join("scripts/papyrus_runtime.luau").is_file());

        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let second = AssetPipeline::run_async(config, tx).await.unwrap();
        drain.await.unwrap();
        assert_eq!(second.cache_hits, 2);
        assert_eq!(second.skipped, 0);
        assert!(
            ConversionManifest::load(&output.join("conversion-manifest.json"))
                .unwrap()
                .complete
        );
    }

    #[tokio::test]
    async fn resume_does_not_publish_unverified_staged_meshes_for_any_manifest_schema() {
        let manifests = [
            ("absent", None),
            (
                "schema-14",
                Some(
                    br#"{"schema_version":14,"complete":true,"configuration_hash":"","entries":{}}"#
                        .as_slice(),
                ),
            ),
            (
                "schema-15",
                Some(
                    br#"{"schema_version":15,"complete":true,"configuration_hash":"","entries":{}}"#
                        .as_slice(),
                ),
            ),
        ];

        for (name, manifest) in manifests {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("Data");
            let output = temp.path().join("modern");
            let staging = temp.path().join("modern.staging-resume");
            fs::create_dir_all(&data).unwrap();
            fs::create_dir_all(&output).unwrap();
            fs::create_dir_all(staging.join("meshes")).unwrap();
            fs::write(staging.join("meshes/stale.glb"), b"unverified mesh").unwrap();
            if let Some(manifest) = manifest {
                fs::write(output.join("conversion-manifest.json"), manifest).unwrap();
            }

            let mut config = PipelineConfig::new(&data, &output);
            config.resume_staging = Some(staging);
            let report = run_without_progress(config).await;

            assert!(report.complete, "resume failed with {name} manifest");
            assert!(
                !output.join("meshes/stale.glb").exists(),
                "stale mesh published with {name} manifest"
            );
            assert_eq!(
                ConversionManifest::load(&output.join("conversion-manifest.json"))
                    .unwrap()
                    .schema_version,
                crate::cache::CONVERTER_SCHEMA_VERSION
            );
        }
    }

    #[tokio::test]
    async fn reuses_and_invalidates_archive_ingestion_cache_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(
                &[dummy_content::Entry::new(
                    "docs/readme.txt",
                    b"cached asset",
                )],
                dummy_content::ba2::Compression::None,
            )
            .unwrap(),
        )
        .unwrap();
        let config = PipelineConfig::new(&data, &output);

        let cache_root = config.ingestion_cache_dir();
        let first = run_without_progress(config.clone()).await;
        assert_eq!(first.converted, 1);
        assert_eq!(first.cache_hits, 0);
        assert!(!output.join("vfs").exists());
        assert!(!output.join(".ingestion-cache").exists());
        let blobs: Vec<_> = WalkDir::new(cache_root.join(".ingestion-cache"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .collect();
        assert_eq!(blobs.len(), 1);
        assert_eq!(fs::read(blobs[0].path()).unwrap(), b"cached asset");

        let second = run_without_progress(config.clone()).await;
        assert_eq!(second.converted, 0);
        assert_eq!(second.cache_hits, 1);

        let mut invalidated = config;
        invalidated.invalidate_cache = true;
        let third = run_without_progress(invalidated).await;
        assert_eq!(third.converted, 1);
        assert_eq!(third.cache_hits, 0);
    }

    #[tokio::test]
    async fn a_failed_batch_stops_and_keeps_staging_for_a_resume() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/bad.dds"), b"not a DDS").unwrap();
        fs::write(data.join("textures/also-bad.dds"), b"also not a DDS").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let mut config = PipelineConfig::new(&data, &output);
        config.fail_fast = true;
        let failure = AssetPipeline::run_async(config, tx).await.unwrap_err();
        let events = collect.await.unwrap();

        assert!(failure.to_string().contains("failed to convert"));
        assert!(!failure.cancelled);
        assert!(!output.exists());
        let staging = failure
            .staging
            .expect("a failed run keeps the work it has done");
        assert!(staging.is_dir());
        let last = events.last().unwrap();
        assert_eq!(last.stage, ProgressStage::Textures);
        assert_eq!(last.outcome, Some(AssetOutcome::Failed));
        assert_eq!(last.message, "Asset conversion failed");
        assert!(last.current_file.as_ref().is_some_and(|path| {
            path == Path::new("textures/bad.dds") || path == Path::new("textures/also-bad.dds")
        }));
    }

    fn staging_entries(parent: &Path) -> Vec<PathBuf> {
        staging_entries_named(parent, "modern")
    }

    fn staging_entries_named(parent: &Path, output_name: &str) -> Vec<PathBuf> {
        let prefix = format!("{output_name}.staging-");
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_journal_write_failure_stops_the_batch_before_removing_staging() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        // Enough work that workers are still converting when the first
        // journal write fails.
        for index in 0..400 {
            let name = format!("Script{index}");
            fs::write(
                data.join(format!("scripts/{name}.pex")),
                dummy_content::pex::minimal(&name).unwrap(),
            )
            .unwrap();
        }

        crate::cache::FAIL_JOURNAL_WRITES.with(|fail| fail.set(true));
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx).await;
        crate::cache::FAIL_JOURNAL_WRITES.with(|fail| fail.set(false));

        let error = result.unwrap_err();
        assert!(format!("{error:?}").contains("injected journal write failure"));
        let staging = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&staging).unwrap();
        // A worker left running would write into staging after it was removed.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!output.exists());
        assert_eq!(staging_entries(temp.path()), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn a_folder_that_is_not_an_earlier_output_is_refused_and_kept() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("Games");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/One.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        fs::create_dir_all(output.join("Skyrim")).unwrap();
        fs::write(output.join("Skyrim/save.ess"), b"keep me").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("conversion-manifest.json"));
        assert_eq!(
            fs::read(output.join("Skyrim/save.ess")).unwrap(),
            b"keep me"
        );
        assert_eq!(
            staging_entries_named(temp.path(), "Games"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn publishing_refuses_to_replace_a_folder_that_is_not_an_earlier_output() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("pack");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("conversion-manifest.json"), b"{}").unwrap();
        let output = temp.path().join("Games");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("keep.txt"), b"keep me").unwrap();

        assert!(publish_directory(&staging, &output).is_err());
        assert_eq!(fs::read(output.join("keep.txt")).unwrap(), b"keep me");
        assert!(staging.join("conversion-manifest.json").is_file());
    }

    #[tokio::test]
    async fn a_failed_publish_keeps_the_journal_in_staging() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/One.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        // A stale backup alongside a live output makes `publish_directory`
        // refuse to publish: without the output dir, a lone backup reads as
        // an interrupted swap and is restored instead.
        fs::create_dir_all(&output).unwrap();
        let backup = output.with_extension(format!("backup-{}", std::process::id()));
        fs::create_dir_all(&backup).unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap_err();

        assert!(format!("{error:?}").contains("stale backup"));
        let staging = staging_entries(temp.path());
        assert_eq!(
            staging.len(),
            1,
            "expected only the staging directory: {staging:?}"
        );
        assert!(staging[0].is_dir());
        assert!(StagingJournal::path_in(&staging[0]).is_file());
        assert!(!parked_journal_path(&staging[0]).exists());

        // Once the backup is gone the same staging directory publishes, and
        // neither the output nor its parent keeps the journal.
        fs::remove_dir_all(&backup).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging[0].clone());
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        AssetPipeline::run_async(config, tx).await.unwrap();
        assert!(output.join("conversion-manifest.json").is_file());
        assert!(!StagingJournal::path_in(&output).exists());
        assert_eq!(staging_entries(temp.path()), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn an_interrupted_run_keeps_staging_and_resumes_from_it() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();

        let cancellation = Cancellation::new();
        cancellation.cancel();
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let failure = AssetPipeline::run_async_with_cancel(
            PipelineConfig::new(&data, &output),
            tx,
            cancellation,
        )
        .await
        .unwrap_err();
        drain.await.unwrap();

        assert!(failure.cancelled);
        assert!(failure.to_string().contains("interrupted"));
        assert!(!output.exists(), "an interrupted run does not publish");
        let staging = failure
            .staging
            .expect("an interrupted run keeps its staging folder");
        assert!(staging.is_dir());

        // The kept folder is a working resume point: the run finishes from it.
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert!(output.join("scripts/one.luau").is_file());
    }

    #[tokio::test]
    async fn skips_failed_assets_and_records_redo_list() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("textures")).unwrap();
        fs::write(data.join("textures/bad.dds"), b"not a DDS").unwrap();
        fs::write(data.join("textures/also-bad.dds"), b"also not a DDS").unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let report = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap();
        let events = collect.await.unwrap();

        assert_eq!(report.skipped, 2);
        assert_eq!(report.warnings.len(), 2);
        assert!(!report.complete);
        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(!manifest.complete);
        assert_eq!(manifest.failures.len(), 2);
        assert!(manifest.failures.contains_key("textures/bad.dds"));
        assert!(manifest.failures.contains_key("textures/also-bad.dds"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.outcome == Some(AssetOutcome::Skipped))
                .count(),
            2
        );
    }

    #[test]
    fn pruned_texture_references_do_not_make_a_run_incomplete() {
        let pruned_only = PipelineReport {
            pruned_texture_references: 182,
            ..PipelineReport::default()
        };
        assert!(
            conversion_is_complete(&pruned_only),
            "a texture the game data never contained must not block a release"
        );

        let skipped = PipelineReport {
            skipped: 1,
            ..PipelineReport::default()
        };
        assert!(!conversion_is_complete(&skipped));

        let warned = PipelineReport {
            warnings: vec!["asset integration failed: 1 missing models".to_owned()],
            ..PipelineReport::default()
        };
        assert!(!conversion_is_complete(&warned));
    }

    /// Notices never hide failures or affect completeness, regardless of insertion order.
    #[test]
    fn notices_do_not_affect_completeness() {
        let mut report = PipelineReport::default();
        report.notices.push("nested plugins ignored".into());
        assert!(conversion_is_complete(&report));
        report.warnings.push("conversion failed".into());
        assert!(!conversion_is_complete(&report));
        report.notices.push("another advisory".into());
        assert!(!conversion_is_complete(&report));
        report.warnings.clear();
        report.skipped = 1;
        assert!(!conversion_is_complete(&report));
    }

    #[tokio::test]
    async fn publishes_meshes_with_missing_textures_and_stays_complete() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        // One mesh drops an auxiliary map, the other the mandatory base color.
        let shapes = [
            (
                "meshes/missing_normal.nif",
                dummy_content::nif::StaticShape {
                    name: "MissingNormalQuad",
                    positions: &positions,
                    normals: &normals,
                    uvs: &uvs,
                    indices: &indices,
                    diffuse: "textures/present.dds",
                    normal_texture: "textures/absent_n.dds",
                },
            ),
            (
                "meshes/missing_diffuse.nif",
                dummy_content::nif::StaticShape {
                    name: "MissingDiffuseQuad",
                    positions: &positions,
                    normals: &normals,
                    uvs: &uvs,
                    indices: &indices,
                    diffuse: "textures/absent.dds",
                    normal_texture: "textures/present_n.dds",
                },
            ),
        ];
        for (path, shape) in shapes {
            fs::write(
                data.join(path),
                dummy_content::nif::static_shape(&shape).unwrap(),
            )
            .unwrap();
        }
        for texture in ["textures/present.dds", "textures/present_n.dds"] {
            fs::write(
                data.join(texture),
                dummy_content::dds::generate(
                    &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                    &mut dummy_content::rng::Rng::new(7),
                )
                .unwrap(),
            )
            .unwrap();
        }

        let report = run_without_progress(PipelineConfig::new(&data, &output)).await;

        assert_eq!(report.skipped, 0);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(
            report.complete,
            "a texture the game data does not contain is not an incomplete conversion"
        );
        assert_eq!(report.pruned_texture_references, 2);

        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(manifest.complete);
        assert!(
            manifest.failures.is_empty(),
            "a pruned reference is not a failure: {:?}",
            manifest.failures
        );
        // Base color is published through an sRGB alias, so that is the URI the
        // mesh dropped.
        assert_eq!(
            manifest
                .pruned_texture_references
                .get("meshes/missing_diffuse.glb"),
            Some(&BTreeSet::from([
                "textures/absent.opensky-srgb.ktx2".to_owned()
            ]))
        );
        assert_eq!(
            manifest
                .pruned_texture_references
                .get("meshes/missing_normal.glb"),
            Some(&BTreeSet::from(["textures/absent_n.ktx2".to_owned()]))
        );
        for (glb, kept) in [
            ("meshes/missing_diffuse.glb", "present_n"),
            ("meshes/missing_normal.glb", "present.opensky-srgb"),
        ] {
            let source_key = canonical_asset_path(glb, AssetKind::Mesh, "nif").unwrap();
            let entry = &manifest.entries[&source_key];
            assert_eq!(entry.output_hash, hash_file(&output.join(glb)).unwrap());
            assert_eq!(
                entry.output_size,
                fs::metadata(output.join(glb)).unwrap().len()
            );
            let uris = MeshConverter::glb_texture_uris(&output.join(glb)).unwrap();
            assert!(
                !uris.iter().any(|uri| uri.contains("absent")),
                "the dangling reference is still in {glb}: {uris:?}"
            );
            assert!(
                uris.iter().any(|uri| uri.contains(kept)),
                "{glb} lost the texture that does exist: {uris:?}"
            );
        }
    }

    #[tokio::test]
    async fn failed_archives_still_skip_and_warn() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("broken.bsa"), b"not a BSA archive").unwrap();

        let report = run_without_progress(PipelineConfig::new(&data, &output)).await;

        assert_eq!(report.skipped, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("broken.bsa"));
        assert_eq!(report.pruned_texture_references, 0);
        assert!(!report.complete);

        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert!(!manifest.complete);
        assert_eq!(manifest.failures.len(), 1);
        assert!(manifest.pruned_texture_references.is_empty());
    }

    const PRUNED_MESH: &str = "meshes/dangling_normal.glb";
    const PRUNED_REFERENCE: &str = "textures/absent_n.ktx2";

    /// Writes one NIF whose normal map is absent from the game data, next to the
    /// base-color DDS the game data does contain.
    fn write_mesh_with_absent_normal(data: &Path) {
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let shape = dummy_content::nif::StaticShape {
            name: "DanglingNormalQuad",
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            indices: &indices,
            diffuse: "textures/present.dds",
            normal_texture: "textures/absent_n.dds",
        };
        fs::write(
            data.join("meshes/dangling_normal.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("textures/present.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(7),
            )
            .unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn resumed_runs_carry_reused_mesh_prunes_forward() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()]))
        );

        // Resume from a staging directory holding exactly what the first run
        // published: the mesh is reused instead of converted again, so the prune
        // pass has nothing left to remove from it.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);

        let (resumed, events) = run_collecting_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.message == "Texture reference pruned")
                .count(),
            0,
            "the resumed run reused the pruned mesh instead of pruning it again"
        );
        assert!(resumed.cache_hits > 0);
        assert_eq!(
            resumed.pruned_texture_references, 1,
            "the prune record of the reused mesh is carried forward"
        );
        let manifest = published_manifest(&output);
        assert!(manifest.complete);
        let entry = &manifest.entries["meshes/dangling_normal.nif"];
        assert_eq!(
            entry.output_hash,
            hash_file(&output.join(PRUNED_MESH)).unwrap()
        );
        assert_eq!(
            manifest.pruned_texture_references.get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()]))
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            !uris.iter().any(|uri| uri.contains("absent")),
            "the record describes the published mesh: {uris:?}"
        );
    }

    /// A run that prunes a mesh and stops before publishing must not lose the prune record when
    /// it is resumed: the staged mesh no longer holds the removed reference, so only converting
    /// it again lets the prune pass find and record it.
    #[tokio::test]
    async fn a_resumed_run_keeps_the_prune_record_of_a_mesh_pruned_before_the_stop() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        // A stale backup beside a live output makes the first run fail at publishing, after the
        // prune pass, and keep its staging directory and journal. Without the output, a lone
        // backup reads as an interrupted swap and is recovered instead.
        fs::create_dir_all(&output).unwrap();
        let backup = output.with_extension(format!("backup-{}", std::process::id()));
        fs::create_dir_all(&backup).unwrap();

        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let staging = error.staging.expect("staging directory was kept");
        let staged_uris = MeshConverter::glb_texture_uris(&staging.join(PRUNED_MESH)).unwrap();
        assert!(
            !staged_uris.iter().any(|uri| uri.contains("absent")),
            "the first run pruned the staged mesh before it stopped: {staged_uris:?}"
        );

        fs::remove_dir_all(&backup).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(resumed.pruned_texture_references, 1);
        let manifest = published_manifest(&output);
        assert_eq!(
            manifest.pruned_texture_references.get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the published manifest records the reference the published mesh omits"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");

        // A normal rerun still reuses the pruned mesh and keeps its record.
        let rerun = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(rerun.complete);
        assert_eq!(rerun.converted, 0);
        assert_eq!(rerun.pruned_texture_references, 1);
    }

    #[tokio::test]
    async fn a_tampered_staged_pruned_mesh_is_not_reused() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        run_without_progress(PipelineConfig::new(&data, &output)).await;
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        // The staged copy of the pruned mesh keeps its length but no longer matches the hash the
        // manifest recorded. The published copy is gone, so the cache-hit path cannot replace it
        // and only the reuse gate stands between the tampered bytes and the next publish.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut tampered = expected.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_ne!(tampered, expected);
        fs::write(staging.join(PRUNED_MESH), tampered).unwrap();
        fs::remove_file(output.join(PRUNED_MESH)).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the tampered staged mesh was converted again, not published"
        );
    }

    /// A resume whose staged pruned mesh still holds the bytes the previous manifest recorded
    /// keeps the mesh's prune record even when the published copy is gone: the staged mesh
    /// already omits the reference, so the prune pass cannot record it again.
    #[tokio::test]
    async fn a_verified_staged_pruned_mesh_keeps_its_record_without_a_published_copy() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        fs::remove_file(output.join(PRUNED_MESH)).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the verified staged mesh was published"
        );
        assert_eq!(resumed.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the published manifest records the reference the published mesh omits"
        );

        // The record is what brings the reference back once its texture exists.
        fs::write(
            data.join("textures/absent_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(9),
            )
            .unwrap(),
        )
        .unwrap();
        let restored = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(restored.complete);
        assert_eq!(restored.pruned_texture_references, 0);
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("absent_n")),
            "the restored texture is not referenced: {uris:?}"
        );
    }

    /// The published copy of the pruned mesh is not the only evidence for keeping its record:
    /// when it was replaced by different bytes but the staged mesh still matches the hash the
    /// previous manifest recorded for the same source, the record still describes what is
    /// about to be published again.
    #[tokio::test]
    async fn a_staged_mesh_matching_the_previous_entry_keeps_its_record_over_a_changed_published_copy()
     {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);
        let expected = fs::read(output.join(PRUNED_MESH)).unwrap();

        // The resume copies the published output into staging, then another build republishes
        // different bytes at the same path. The published copy no longer matches the staged
        // mesh, but the staged bytes still match the previous manifest's entry, so the record
        // describes them and must survive.
        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        let mut rebuilt = expected.clone();
        rebuilt.push(0);
        fs::write(output.join(PRUNED_MESH), rebuilt).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(
            fs::read(output.join(PRUNED_MESH)).unwrap(),
            expected,
            "the staged bytes the record describes were published again"
        );
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the staged mesh matched the previous entry, so its record was kept"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");
    }

    /// Stopping and resuming twice in a row must leave the prune record intact each time: the
    /// second stop happens after the first resume published the mesh and its record.
    #[tokio::test]
    async fn repeated_stops_and_resumes_keep_the_prune_record() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        // A stale backup beside a live output makes the run fail at publishing, after the prune
        // pass, and keep its staging directory and journal. Without the output, a lone backup
        // reads as an interrupted swap and is recovered instead.
        let backup = output.with_extension(format!("backup-{}", std::process::id()));

        // First cycle: stop before the first publish, then resume to completion.
        fs::create_dir_all(&output).unwrap();
        fs::create_dir_all(&backup).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let error = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let staging = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&backup).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let first_resume = run_without_progress(config).await;
        assert!(first_resume.complete);
        assert_eq!(first_resume.pruned_texture_references, 1);

        // Second cycle: stop once more, resuming this time from the published pack, then resume
        // again. The published manifest holds the record through both stops.
        let staging = temp.path().join("modern.staging-again");
        copy_tree(&output, &staging);
        fs::create_dir_all(&backup).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let error = AssetPipeline::run_async(config, tx).await.unwrap_err();
        assert!(format!("{error:?}").contains("stale backup"));
        let stopped = error.staging.expect("staging directory was kept");
        fs::remove_dir_all(&backup).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(stopped);
        let second_resume = run_without_progress(config).await;

        assert!(second_resume.complete);
        assert_eq!(second_resume.pruned_texture_references, 1);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([PRUNED_REFERENCE.to_owned()])),
            "the record survived two stop-and-resume cycles"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(!uris.iter().any(|uri| uri.contains("absent")), "{uris:?}");
    }

    #[tokio::test]
    async fn changed_meshes_do_not_keep_stale_prune_records() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);
        run_without_progress(PipelineConfig::new(&data, &output)).await;

        let staging = temp.path().join("modern.staging-resume");
        copy_tree(&output, &staging);
        // The mesh's source changed and now names a normal map the game data holds: the stored
        // record describes the old mesh, not the one this run converts and publishes.
        let positions = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0]];
        let shape = dummy_content::nif::StaticShape {
            name: "DanglingNormalQuad",
            positions: &positions,
            normals: &[[0.0, 0.0, 1.0]; 3],
            uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
            indices: &[[0, 1, 2]],
            diffuse: "textures/present.dds",
            normal_texture: "textures/present_n.dds",
        };
        fs::write(
            data.join("meshes/dangling_normal.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        fs::write(
            data.join("textures/present_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(11),
            )
            .unwrap(),
        )
        .unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);

        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert_eq!(resumed.pruned_texture_references, 0);
        assert!(
            published_manifest(&output)
                .pruned_texture_references
                .is_empty()
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("present_n")),
            "the changed mesh was converted again: {uris:?}"
        );
    }

    /// Pruning rewrites a mesh after its cache entry is written; the refreshed entry keeps the
    /// mesh a cache hit on the next run instead of converting it again.
    #[tokio::test]
    async fn pruned_meshes_remain_cache_hits() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        let second = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(
            second.converted, 0,
            "the pruned mesh was converted again instead of reusing its refreshed entry"
        );
        assert_eq!(second.pruned_texture_references, 1);
        let manifest = published_manifest(&output);
        let entry = manifest
            .entries
            .values()
            .find(|entry| entry.output == PRUNED_MESH)
            .expect("the pruned mesh has a cache entry");
        assert_eq!(
            entry.output_hash,
            hash_file(&output.join(PRUNED_MESH)).unwrap(),
            "the cache entry describes the pruned bytes that were published"
        );
    }

    /// A pruned reference whose source comes back forces the mesh to be converted again, so the
    /// reference is restored instead of staying missing behind a cached GLB.
    #[tokio::test]
    async fn restored_texture_sources_reconvert_the_mesh() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        // The normal map's source appears: the mesh must be converted again to regain it.
        fs::write(
            data.join("textures/absent_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(9),
            )
            .unwrap(),
        )
        .unwrap();

        let second = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(
            second.pruned_texture_references, 0,
            "the restored reference must not stay recorded as absent"
        );
        assert!(
            second.converted >= 1,
            "the mesh was reused without regaining its restored reference"
        );
        let uris = MeshConverter::glb_texture_uris(&output.join(PRUNED_MESH)).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("absent_n")),
            "the restored texture is not referenced: {uris:?}"
        );
    }

    /// A reused mesh that already omitted one texture and has a second pruned now reports both.
    #[tokio::test]
    async fn a_second_prune_keeps_the_first_record() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        write_mesh_with_absent_normal(&data);

        let first = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(first.complete);
        assert_eq!(first.pruned_texture_references, 1);

        // The base-color source disappears: its reference becomes dangling on the next run while
        // the mesh's own source is unchanged, so the mesh is reused.
        fs::remove_file(data.join("textures/present.dds")).unwrap();
        let second = run_without_progress(PipelineConfig::new(&data, &output)).await;
        assert!(second.complete);
        assert_eq!(second.pruned_texture_references, 2);
        assert_eq!(
            published_manifest(&output)
                .pruned_texture_references
                .get(PRUNED_MESH),
            Some(&BTreeSet::from([
                "textures/absent_n.ktx2".to_owned(),
                "textures/present.opensky-srgb.ktx2".to_owned(),
            ]))
        );
    }

    /// A source texture that exists but failed to publish is not pruned: the reference stays as
    /// the record of a failed publication, not of absent game data.
    #[tokio::test]
    async fn a_missing_artifact_with_a_present_source_is_not_pruned() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("meshes")).unwrap();
        fs::create_dir_all(data.join("textures")).unwrap();
        let positions = [
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let uvs = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
        let indices = [[0, 1, 2], [0, 2, 3]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let shape = dummy_content::nif::StaticShape {
            name: "FailedTextureQuad",
            positions: &positions,
            normals: &normals,
            uvs: &uvs,
            indices: &indices,
            diffuse: "textures/present.dds",
            normal_texture: "textures/present_n.dds",
        };
        fs::write(
            data.join("meshes/failed_texture.nif"),
            dummy_content::nif::static_shape(&shape).unwrap(),
        )
        .unwrap();
        // The base color exists under `vfs` but cannot be converted; the normal map publishes.
        fs::write(data.join("textures/present.dds"), b"not a DDS").unwrap();
        fs::write(
            data.join("textures/present_n.dds"),
            dummy_content::dds::generate(
                &dummy_content::dds::Spec::new(dummy_content::dds::Format::Bc1Unorm, 8, 8),
                &mut dummy_content::rng::Rng::new(7),
            )
            .unwrap(),
        )
        .unwrap();

        let report = run_without_progress(PipelineConfig::new(&data, &output)).await;

        assert!(!report.complete, "the failed texture still skips");
        assert_eq!(report.skipped, 1);
        assert_eq!(
            report.pruned_texture_references, 0,
            "a present source must not be recorded as absent game data"
        );
        let uris =
            MeshConverter::glb_texture_uris(&output.join("meshes/failed_texture.glb")).unwrap();
        assert!(
            uris.iter().any(|uri| uri.contains("present.")),
            "the failed texture's reference was pruned: {uris:?}"
        );
    }

    #[tokio::test]
    async fn an_interrupt_mid_batch_stops_at_the_next_asset_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        let assets = 300;
        for index in 0..assets {
            let script = dummy_content::pex::minimal("Script").unwrap();
            fs::write(data.join(format!("scripts/script{index:03}.pex")), script).unwrap();
        }

        let cancellation = Cancellation::new();
        let interrupt = cancellation.clone();
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(64);
        let watcher = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if event.message == "Converted asset" {
                    interrupt.cancel();
                }
            }
        });
        let mut config = PipelineConfig::new(&data, &output);
        config.cpu_jobs = 1;
        let failure = AssetPipeline::run_async_with_cancel(config, tx, cancellation)
            .await
            .unwrap_err();
        watcher.await.unwrap();

        assert!(failure.cancelled);
        let staging = failure
            .staging
            .clone()
            .expect("an interrupted run keeps staging");
        assert!(staging.is_dir());
        assert!(!output.exists(), "an interrupted run does not publish");
        // The manifest is written once, at the end of a run, so nothing in flight — or after it —
        // is recorded as converted; the resume re-checks the staged files instead.
        assert!(!staging.join("conversion-manifest.json").exists());
        let staged: Vec<_> = fs::read_dir(staging.join("scripts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "luau")
            })
            .collect();
        assert!(
            staged.len() < assets,
            "the run converted all {assets} assets despite the interrupt"
        );
        for file in &staged {
            assert!(
                !fs::read(file).unwrap().is_empty(),
                "{} was left half written",
                file.display()
            );
        }

        // The kept folder resumes the run to completion.
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let report = run_without_progress(config).await;
        assert!(report.complete);
        assert_eq!(report.skipped, 0);
        let converted = fs::read_dir(output.join("scripts"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "luau")
            })
            .count();
        assert!(converted >= assets, "resume left assets behind");
    }

    /// A stop pressed while one large archive is being extracted takes effect inside it, not
    /// when the whole archive is done, and ends the run as an interrupt rather than as a skipped
    /// archive. Nothing of the archive is cached, so the resume extracts it again in full.
    #[tokio::test]
    async fn an_interrupt_inside_an_archive_stops_its_extraction_and_resumes() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        let entries = 2048;
        let names: Vec<String> = (0..entries)
            .map(|index| format!("docs/file{index:04}.txt"))
            .collect();
        let contents: Vec<Vec<u8>> = (0..entries)
            .map(|index| format!("entry {index}").into_bytes())
            .collect();
        let archive_entries: Vec<_> = names
            .iter()
            .zip(&contents)
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(&archive_entries, dummy_content::ba2::Compression::None)
                .unwrap(),
        )
        .unwrap();

        // Stop from inside the extractor after its first entries: only the entries already in
        // flight on other threads can still finish, far fewer than the archive holds.
        stop_hook::cancel_after(&output, 16);
        let cancellation = Cancellation::new();
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(64);
        let watcher = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let failure = AssetPipeline::run_async_with_cancel(
            PipelineConfig::new(&data, &output),
            tx,
            cancellation,
        )
        .await
        .unwrap_err();
        watcher.await.unwrap();

        assert!(failure.cancelled, "a stop is reported as an interrupt");
        assert!(
            failure
                .error
                .downcast_ref::<Interrupted>()
                .is_some_and(|stop| stop.cause().is_none()),
            "a plain stop inside an archive has no cause to show: {failure}"
        );
        assert!(!output.exists(), "an interrupted run does not publish");
        let staging = failure
            .staging
            .clone()
            .expect("an interrupted run keeps staging");
        let written: Vec<_> = fs::read_dir(staging.join("vfs/docs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            written.len() < entries,
            "the whole archive was extracted despite the stop"
        );
        assert!(
            written.iter().all(|name| !name.ends_with(".partial")),
            "a stop left a half-written file behind"
        );
        // The archive's cache entry is only recorded once it is complete.
        assert!(!staging.join(".ingestion-cache/sha256").exists());
        assert!(!staging.join("conversion-manifest.json").exists());

        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging);
        let report = run_without_progress(config.clone()).await;
        assert!(report.complete);
        assert!(!output.join("vfs").exists());
        let manifest = ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap();
        assert_eq!(manifest.archives["assets.ba2"].files.len(), entries);
        let file_entry = manifest.archives["assets.ba2"]
            .files
            .iter()
            .find(|file| file.path == "docs/file2047.txt")
            .expect("file2047.txt not in archive manifest");
        let blob = config
            .ingestion_cache_dir()
            .join(".ingestion-cache/sha256")
            .join(&file_entry.hash[..2])
            .join(&file_entry.hash);
        assert_eq!(fs::read(blob).unwrap(), b"entry 2047");
    }

    /// A front end that stops reading progress must not stall extraction: the per-file updates
    /// find the channel full and are dropped, so the archive's threads run to the end. Only the
    /// events that matter on their own wait for room, and reading again lets the run finish.
    #[tokio::test]
    async fn extraction_does_not_wait_for_a_front_end_that_stopped_reading() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(&data).unwrap();
        // Enough entries for several per-file updates, one every 512 files.
        let entries = 2048;
        let names: Vec<String> = (0..entries)
            .map(|index| format!("docs/file{index:04}.txt"))
            .collect();
        let contents: Vec<Vec<u8>> = (0..entries)
            .map(|index| format!("entry {index}").into_bytes())
            .collect();
        let archive_entries: Vec<_> = names
            .iter()
            .zip(&contents)
            .map(|(name, data)| dummy_content::Entry::new(name, data))
            .collect();
        fs::write(
            data.join("assets.ba2"),
            dummy_content::ba2::general(&archive_entries, dummy_content::ba2::Compression::None)
                .unwrap(),
        )
        .unwrap();
        // The extractor caches the files only once every one of them is written, so the last
        // file's blob in the staging cache shows the extraction threads have finished.
        let last_blob = crate::cache::hash_bytes(b"entry 2047");

        // Room for one event: after the archive's first update, the channel stays full.
        let (tx, mut rx) = mpsc::channel::<ProgressEvent>(1);
        let front_end = async {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("the run reaches extraction")
                    .expect("the run is still sending");
                if event.stage == ProgressStage::Extracting && event.stage_fraction.is_none() {
                    break;
                }
            }
            // Stop reading until the archive is extracted.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let blob = crate::find_resumable_staging(&output).map(|(staging, _)| {
                    staging
                        .join(".ingestion-cache/sha256")
                        .join(&last_blob[..2])
                        .join(&last_blob)
                });
                if blob.is_some_and(|blob| blob.is_file()) {
                    break;
                }
                if Instant::now() > deadline {
                    // Close the channel first, so stalled threads end and the test fails
                    // instead of hanging.
                    drop(rx);
                    panic!("extraction stalled while nobody read its progress");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Reading again lets the waiting events through.
            while rx.recv().await.is_some() {}
        };
        let (report, ()) = tokio::join!(
            AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx),
            front_end
        );

        let report = report.unwrap();
        assert!(report.complete);
        let cache_root = PipelineConfig::new(&data, &output).ingestion_cache_dir();
        let blobs = WalkDir::new(cache_root.join(".ingestion-cache"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .count();
        assert_eq!(blobs, entries);
        assert_eq!(report.converted, entries as u64);
    }

    /// The window between the last stage and the publish rename cannot be hit from outside in a
    /// test without racing the run, so the gate itself is driven directly: a run that was
    /// interrupted while it was packing up must not publish, and must keep its staging folder.
    #[test]
    fn an_interrupt_before_the_publish_keeps_staging_and_leaves_the_output_alone() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("modern");

        let staging = temp.path().join("modern.staging-1-1");
        fs::create_dir_all(staging.join("scripts")).unwrap();
        fs::write(staging.join("scripts/one.luau"), b"return 1").unwrap();
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let failure = publish_if_not_interrupted(&staging, &output, &cancellation).unwrap_err();
        assert!(failure.cancelled);
        assert!(failure.error.downcast_ref::<Interrupted>().is_some());
        assert!(failure.to_string().contains("interrupted"));
        assert!(!output.exists(), "an interrupted run must not publish");
        assert_eq!(failure.staging.as_deref(), Some(staging.as_path()));
        assert!(staging.is_dir(), "the staging folder is kept for a resume");

        // Without the interrupt the same gate publishes, so the assertion above is about the
        // cancellation and not about publishing being broken.
        let staging = temp.path().join("modern.staging-2-2");
        fs::create_dir_all(staging.join("scripts")).unwrap();
        fs::write(staging.join("scripts/one.luau"), b"return 1").unwrap();
        publish_if_not_interrupted(&staging, &output, &Cancellation::new()).unwrap();
        assert_eq!(
            fs::read(output.join("scripts/one.luau")).unwrap(),
            b"return 1"
        );
    }

    async fn run_without_progress(config: PipelineConfig) -> PipelineReport {
        let (tx, mut rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let report = AssetPipeline::run_async(config, tx).await.unwrap();
        drain.await.unwrap();
        report
    }

    /// Runs the pipeline and returns its report together with every progress
    /// event it emitted.
    async fn run_collecting_progress(
        config: PipelineConfig,
    ) -> (PipelineReport, Vec<ProgressEvent>) {
        let (tx, mut rx) = mpsc::channel(64);
        let collect = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        let report = AssetPipeline::run_async(config, tx).await.unwrap();
        (report, collect.await.unwrap())
    }

    /// Loads the manifest published in an output directory.
    fn published_manifest(output: &Path) -> ConversionManifest {
        ConversionManifest::load(&output.join("conversion-manifest.json")).unwrap()
    }

    /// Copies a published asset tree into a staging directory, so a run can
    /// resume from it.
    fn copy_tree(source: &Path, destination: &Path) {
        for entry in WalkDir::new(source) {
            let entry = entry.unwrap();
            let relative = entry.path().strip_prefix(source).unwrap();
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target).unwrap();
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn published_pack_excludes_build_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        let config = PipelineConfig::new(&data, &output);
        let report = run_without_progress(config.clone()).await;

        assert!(report.complete);
        assert!(output.join("scripts/one.luau").is_file());
        assert!(output.join("conversion-manifest.json").is_file());
        assert!(!output.join("vfs").exists());
        assert!(!output.join(".ingestion-cache").exists());
    }

    #[test]
    fn persisting_the_ingestion_cache_skips_spill_copies() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging-cache");
        fs::create_dir_all(&staging).unwrap();
        let blob = "ab".repeat(32);
        fs::write(staging.join(&blob), b"blob").unwrap();
        fs::write(staging.join(format!("{blob}.1")), b"spill").unwrap();
        let root = directory.path().join("cache");

        persist_ingestion_cache(&staging, &root).unwrap();

        assert!(root.join(&blob).is_file());
        assert!(!root.join(format!("{blob}.1")).exists());
    }

    #[test]
    fn prune_removes_only_unreferenced_blobs_and_spills() {
        use crate::cache::{IngestedFile, IngestionCacheEntry};

        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join(".ingestion-cache/sha256/ab");
        fs::create_dir_all(&cache).unwrap();
        let live = "ab".repeat(32);
        let dead = "cd".repeat(32);
        fs::write(cache.join(&live), b"live").unwrap();
        fs::write(cache.join(format!("{live}.1")), b"live spill").unwrap();
        fs::write(cache.join(&dead), b"dead").unwrap();
        fs::write(cache.join(format!("{dead}.2")), b"dead spill").unwrap();
        fs::write(cache.join("probe.tmp"), b"not a blob").unwrap();

        let mut manifest = ConversionManifest::default();
        manifest.archives.insert(
            "assets.ba2".to_owned(),
            IngestionCacheEntry {
                source_hash: "00".repeat(32),
                files: vec![IngestedFile {
                    path: "textures/rock.dds".to_owned(),
                    size: 4,
                    hash: live.clone(),
                }],
            },
        );
        prune_stale_ingestion_blobs(
            directory.path().join(".ingestion-cache").as_path(),
            &manifest,
        )
        .unwrap();

        assert!(cache.join(&live).is_file());
        assert!(cache.join(format!("{live}.1")).is_file());
        assert!(!cache.join(&dead).exists());
        assert!(!cache.join(format!("{dead}.2")).exists());
        assert!(cache.join("probe.tmp").is_file());
    }

    #[tokio::test]
    async fn a_resumed_publish_removes_its_staging_and_keeps_the_output() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("Data");
        let output = temp.path().join("modern");
        fs::create_dir_all(data.join("scripts")).unwrap();
        fs::write(
            data.join("scripts/one.pex"),
            dummy_content::pex::minimal("One").unwrap(),
        )
        .unwrap();
        run_without_progress(PipelineConfig::new(&data, &output)).await;
        let expected = fs::read(output.join("scripts/one.luau")).unwrap();

        // The published pack shares files with staging through hard links, so removing the
        // resumed staging folder must leave every published file in place.
        let staging = temp.path().join("modern.staging-resume-links");
        fs::create_dir_all(&staging).unwrap();
        let mut config = PipelineConfig::new(&data, &output);
        config.resume_staging = Some(staging.clone());
        let resumed = run_without_progress(config).await;

        assert!(resumed.complete);
        assert!(
            !staging.exists(),
            "a resumed publish kept its staging folder, which would be offered for resume again"
        );
        assert_eq!(fs::read(output.join("scripts/one.luau")).unwrap(), expected);
        assert_eq!(crate::find_resumable_staging(&output), None);
    }

    #[test]
    fn staging_that_holds_the_data_folder_is_kept_after_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("modern.staging-1-1");
        let data = staging.join("Data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("Skyrim.esm"), b"game data").unwrap();

        assert!(!remove_staging(&staging, &data));
        assert!(!remove_staging(&staging, &staging));
        assert_eq!(fs::read(data.join("Skyrim.esm")).unwrap(), b"game data");

        // A staging folder beside the data, the normal case, is removed.
        let beside = temp.path().join("modern.staging-2-2");
        fs::create_dir_all(beside.join("vfs")).unwrap();
        assert!(remove_staging(&beside, &data));
        assert!(!beside.exists());
        assert!(data.join("Skyrim.esm").is_file());
    }

    /// A staging folder with `count` valid artifacts of each kind validation checks, the
    /// artifact list and the texture semantics the KTX2s were encoded for.
    fn staging_with_valid_artifacts(
        staging: &Path,
        count: usize,
    ) -> (Vec<PathBuf>, BTreeMap<String, BTreeSet<TextureSemantic>>) {
        let dds = dummy_content::dds::generate(
            &dummy_content::dds::Spec::new(dummy_content::dds::Format::X8R8G8B8, 4, 4),
            &mut dummy_content::rng::Rng::new(0),
        )
        .unwrap();
        let uses = BTreeSet::from([TextureSemantic::BaseColor]);
        let ktx2 = crate::texture::TextureConverter::convert(
            &dds,
            TextureEncoding::from_semantics(&uses).unwrap(),
        )
        .unwrap();
        for folder in ["textures", "meshes", "scripts"] {
            fs::create_dir_all(staging.join(folder)).unwrap();
        }
        let mut artifacts = Vec::new();
        let mut semantics = BTreeMap::new();
        for index in 0..count {
            let texture = PathBuf::from(format!("textures/t{index}.ktx2"));
            fs::write(staging.join(&texture), &ktx2).unwrap();
            semantics.insert(format!("textures/t{index}.ktx2"), uses.clone());
            let mesh = PathBuf::from(format!("meshes/m{index}.glb"));
            fs::write(staging.join(&mesh), b"glTF\x02\x00\x00\x00\x0c\x00\x00\x00").unwrap();
            let script = PathBuf::from(format!("scripts/s{index}.luau"));
            fs::write(staging.join(&script), format!("return {index}")).unwrap();
            artifacts.extend([texture, mesh, script]);
        }
        (artifacts, semantics)
    }

    #[test]
    fn validates_a_mix_of_good_artifacts_on_one_thread_and_many() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 16);
        for jobs in [1, 4] {
            validate_artifacts(staging, &artifacts, &semantics, jobs)
                .unwrap_or_else(|error| panic!("jobs {jobs}: {error:#}"));
        }
    }

    #[test]
    fn rejects_one_bad_artifact_of_each_kind_among_good_ones() {
        let cases: [(&str, &[u8], &str); 3] = [
            ("textures/bad.ktx2", b"not a texture", "invalid KTX2"),
            (
                "meshes/bad.glb",
                b"glTX\x02\x00\x00\x00\x0c\x00\x00\x00",
                "invalid GLB artifact",
            ),
            ("scripts/bad.luau", b"return (", "invalid Luau artifact"),
        ];
        for (relative, bytes, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path();
            let (mut artifacts, mut semantics) = staging_with_valid_artifacts(staging, 16);
            fs::write(staging.join(relative), bytes).unwrap();
            semantics.insert(
                "textures/bad.ktx2".to_owned(),
                BTreeSet::from([TextureSemantic::BaseColor]),
            );
            // In the middle of the list, so a parallel run meets good files on both sides.
            artifacts.insert(artifacts.len() / 2, PathBuf::from(relative));
            for jobs in [1, 4] {
                let error =
                    validate_artifacts(staging, &artifacts, &semantics, jobs).expect_err(relative);
                let chain = format!("{error:#}");
                assert!(
                    chain.contains(expected) && chain.contains("bad."),
                    "jobs {jobs}: {chain}"
                );
            }
        }
    }

    #[test]
    fn reports_the_first_bad_artifact_when_several_fail() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 16);
        // `m1` comes before `m10` in the artifact list; both fail the GLB check the same way.
        for relative in ["meshes/m1.glb", "meshes/m10.glb"] {
            fs::write(
                staging.join(relative),
                b"glTX\x02\x00\x00\x00\x0c\x00\x00\x00",
            )
            .unwrap();
        }
        let expected = staging.join("meshes/m1.glb").display().to_string();
        for jobs in [1, 4, 16] {
            let error = validate_artifacts(staging, &artifacts, &semantics, jobs)
                .expect_err("both meshes are invalid");
            let chain = format!("{error:#}");
            assert!(
                chain.contains(&expected) && !chain.contains("m10.glb"),
                "jobs {jobs}: {chain}"
            );
        }
    }

    #[test]
    fn a_glb_shorter_than_its_header_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path();
        let (artifacts, semantics) = staging_with_valid_artifacts(staging, 2);
        fs::write(staging.join("meshes/m1.glb"), b"glTF\x02\x00").unwrap();
        let error = validate_artifacts(staging, &artifacts, &semantics, 2)
            .expect_err("a 6-byte GLB has no full header");
        assert!(
            format!("{error:#}").contains("invalid GLB artifact"),
            "{error:#}"
        );
    }

    /// Times artifact validation on a real converted output, for before/after comparisons.
    ///
    /// `OPENSKYRIM_VALIDATE_OUTPUT` names a converted output folder, which is only read.
    /// `OPENSKYRIM_VALIDATE_LIMIT` caps the artifact count; the cap samples the manifest evenly so
    /// every kind is represented. `OPENSKYRIM_VALIDATE_JOBS` sets the thread count (default: every
    /// core, as a conversion does). Run with
    /// `cargo test --release -p converter validation_timing_on_a_real_output -- --ignored --nocapture`.
    ///
    /// The timed call is the pipeline's own `validate_artifacts`. If it fails (an output from an
    /// older converter, say), the failures are counted per kind and the passing artifacts are
    /// timed again; that second timing runs with the files already in the OS cache.
    #[test]
    #[ignore = "needs a converted output; set OPENSKYRIM_VALIDATE_OUTPUT"]
    fn validation_timing_on_a_real_output() {
        use std::time::Instant;

        let Some(output) = std::env::var_os("OPENSKYRIM_VALIDATE_OUTPUT").map(PathBuf::from) else {
            eprintln!("OPENSKYRIM_VALIDATE_OUTPUT is not set; nothing to time");
            return;
        };
        let limit = std::env::var("OPENSKYRIM_VALIDATE_LIMIT")
            .ok()
            .map(|value| value.parse::<usize>().expect("OPENSKYRIM_VALIDATE_LIMIT"));
        let jobs = std::env::var("OPENSKYRIM_VALIDATE_JOBS").map_or_else(
            |_| std::thread::available_parallelism().map_or(1, usize::from),
            |value| value.parse::<usize>().expect("OPENSKYRIM_VALIDATE_JOBS"),
        );

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(output.join("conversion-manifest.json")).expect("read manifest"),
        )
        .expect("parse manifest");
        let mut artifacts: Vec<PathBuf> = manifest["entries"]
            .as_object()
            .expect("manifest entries")
            .values()
            .filter_map(|entry| entry["output"].as_str())
            .map(PathBuf::from)
            .filter(|relative| output.join(relative).is_file())
            .collect();
        artifacts.sort();
        artifacts.dedup();
        if let Some(limit) = limit.filter(|&limit| limit > 0 && limit < artifacts.len()) {
            let total = artifacts.len();
            artifacts = (0..limit)
                .map(|index| {
                    // Widened to u64: `index * total` would overflow a 32-bit usize.
                    let sampled = index as u64 * total as u64 / limit as u64;
                    artifacts[sampled as usize].clone()
                })
                .collect();
        }
        let mut by_kind = BTreeMap::<String, usize>::new();
        for relative in &artifacts {
            *by_kind.entry(artifact_kind(relative)).or_default() += 1;
        }

        // Collecting semantics reads every GLB and takes minutes on a full install;
        // `OPENSKYRIM_VALIDATE_SEMANTICS=skip` validates textures without them instead.
        let started = Instant::now();
        let texture_semantics =
            if std::env::var("OPENSKYRIM_VALIDATE_SEMANTICS").is_ok_and(|value| value == "skip") {
                BTreeMap::new()
            } else {
                collect_texture_semantics(&output).unwrap_or_else(|error| {
                    eprintln!("texture semantics unavailable, validating without them: {error:#}");
                    BTreeMap::new()
                })
            };
        eprintln!(
            "texture semantics: {} textures in {:.2} s (not part of the timing)",
            texture_semantics.len(),
            started.elapsed().as_secs_f64()
        );

        eprintln!(
            "validating {} artifacts from {} on {jobs} threads ({by_kind:?})",
            artifacts.len(),
            output.display()
        );
        let started = Instant::now();
        let result = validate_artifacts(&output, &artifacts, &texture_semantics, jobs);
        let elapsed = started.elapsed().as_secs_f64();
        match result {
            Ok(()) => {
                eprintln!(
                    "RESULT artifacts={} seconds={elapsed:.2} errors=0",
                    artifacts.len()
                );
                return;
            }
            Err(error) => {
                eprintln!("validation failed after {elapsed:.2} s: {error:#}");
            }
        }

        let mut errors = BTreeMap::<String, (usize, String)>::new();
        let mut passing = Vec::new();
        let mut lua = None;
        for relative in &artifacts {
            match validate_artifact(&output, relative, &texture_semantics, &mut lua) {
                Ok(()) => passing.push(relative.clone()),
                Err(error) => {
                    let slot = errors
                        .entry(artifact_kind(relative))
                        .or_insert_with(|| (0, format!("{error:#}")));
                    slot.0 += 1;
                }
            }
        }
        for (kind, (count, first)) in &errors {
            eprintln!("errors[{kind}] = {count}; first: {first}");
        }
        let failed: usize = errors.values().map(|(count, _)| count).sum();
        let started = Instant::now();
        validate_artifacts(&output, &passing, &texture_semantics, jobs)
            .expect("the passing artifacts validate");
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "RESULT artifacts={} seconds={elapsed:.2} errors={failed} (passing set, warm cache)",
            passing.len()
        );
    }

    fn artifact_kind(relative: &Path) -> String {
        relative
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    }
}
