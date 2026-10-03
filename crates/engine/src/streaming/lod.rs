use super::{
    RenderOrigin, StreamingCommitBudget, StreamingMetrics, TerrainCoverage, TerrainSurfaceReady,
};
use crate::{
    config::{EngineConfig, TerrainLodDistances},
    profiling::ProfilingState,
    world::{
        components::{CELL_SIZE, StreamingCamera},
        database::{LodChunkMetadata, LodChunkQuery, WorldDatabase},
    },
};
use bevy::{
    asset::{AssetLoadError, LoadState, RecursiveDependencyLoadState, io::AssetReaderError},
    gltf::GltfAssetLabel,
    mesh::{Indices, PrimitiveTopology, VertexAttributeValues},
    prelude::*,
    tasks::{IoTaskPool, Task, block_on},
    world_serialization::{WorldAsset, WorldAssetRoot, WorldInstanceReady},
};
use sha2::{Digest, Sha256};
use shared::lod::{
    ChunkKey, LodOrigin, LodTier, TERRAIN_QUADRANT_INDEX_COUNT, TERRAIN_QUADRANT_VERTEX_COUNT,
    nodes,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

const LOD_UNLOAD_MARGIN_CELLS: i32 = 2;
const LOD_RESPONSE_SCAN_LIMIT: usize = 32;
const LOD_CHUNK_MAX_RETRIES: u8 = 3;
const LOD_CHUNK_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const LOD_CHUNK_RETRY_MAX_DELAY: Duration = Duration::from_secs(4);

/// A far plane that keeps every cell within `reach_cells` of the camera's cell.
fn lod_camera_far(reach_cells: i32) -> f32 {
    CELL_SIZE * (reach_cells as f32 + 0.5) * std::f32::consts::SQRT_2
}

#[derive(Resource, Default)]
pub(super) struct LodStreaming {
    generation: u64,
    center: Option<IVec2>,
    requested_queries: HashSet<LodTier>,
    pending_queries: HashSet<LodTier>,
    query_retries: HashMap<LodTier, (u8, Option<Instant>)>,
    chunks: HashMap<ChunkKey, LodChunkStatus>,
    pending_chunks: VecDeque<(u64, LodChunkMetadata, Option<u8>)>,
    queued_retry_chunks: HashSet<ChunkKey>,
    build_identity: Option<String>,
    pub(super) visibility_dirty: bool,
    visibility_camera_grid: Option<IVec2>,
    visibility_stream_radius: Option<i32>,
}

impl LodStreaming {
    fn query_ready(&self, tier: LodTier, now: Instant) -> bool {
        !self.requested_queries.contains(&tier)
            && self
                .query_retries
                .get(&tier)
                .is_none_or(|(_, retry_at)| retry_at.is_some_and(|deadline| now >= deadline))
    }

    fn query_submitted(&mut self, tier: LodTier) {
        self.requested_queries.insert(tier);
        self.pending_queries.insert(tier);
        if let Some((count, retry_at)) = self.query_retries.get_mut(&tier) {
            *count = count.saturating_add(1);
            *retry_at = None;
        }
    }

    fn query_failed(&mut self, tier: LodTier, class: LodChunkFailureClass, now: Instant) {
        self.requested_queries.remove(&tier);
        self.pending_queries.remove(&tier);
        let count = self.query_retries.get(&tier).map_or(0, |(count, _)| *count);
        let retry_at = lod_retry_deadline(class, count, now);
        self.query_retries.insert(tier, (count, retry_at));
    }

    fn has_queued_chunks(&self) -> bool {
        self.pending_chunks
            .iter()
            .any(|(generation, metadata, retry)| {
                *generation == self.generation
                    && (retry.is_some() || !self.chunks.contains_key(&metadata.key))
            })
    }

    fn move_center(&mut self, center: IVec2, worldspace_id: u32, distances: &TerrainLodDistances) {
        self.generation = self.generation.wrapping_add(1);
        self.requested_queries.clear();
        self.pending_queries.clear();
        self.query_retries.clear();
        self.center = Some(center);
        self.pending_chunks.retain_mut(|(generation, metadata, _)| {
            let keep = metadata.key.worldspace_id == worldspace_id
                && chunk_within_radius(
                    metadata.key,
                    metadata.origin,
                    (i64::from(center.x), i64::from(center.y)),
                    query_unload_radius(distances.reach_cells(metadata.key.tier)),
                );
            if keep {
                *generation = self.generation;
            }
            keep
        });
        self.queued_retry_chunks = self
            .pending_chunks
            .iter()
            .filter_map(|(_, metadata, retry)| retry.map(|_| metadata.key))
            .collect();
    }
}

#[derive(Clone)]
enum LodChunkStatus {
    Loading {
        root: Entity,
        generation: u64,
        origin: LodOrigin,
    },
    Ready {
        root: Entity,
        origin: LodOrigin,
    },
    Failed {
        origin: LodOrigin,
        metadata: LodChunkMetadata,
        retry_count: u8,
        retry_at: Option<Instant>,
    },
}

impl LodChunkStatus {
    fn origin(&self) -> LodOrigin {
        match self {
            Self::Loading { origin, .. }
            | Self::Ready { origin, .. }
            | Self::Failed { origin, .. } => *origin,
        }
    }

    fn root(&self) -> Option<Entity> {
        match self {
            Self::Loading { root, .. } | Self::Ready { root, .. } => Some(*root),
            Self::Failed { .. } => None,
        }
    }
}

#[derive(Component)]
pub(super) struct LodChunkRoot {
    key: ChunkKey,
    generation: u64,
    origin: LodOrigin,
    retry_count: u8,
}

#[derive(Component)]
pub(super) struct LodChunkGridOrigin {
    pub(super) grid_x: i64,
    pub(super) grid_y: i64,
}

#[derive(Component)]
pub(super) struct PendingLodChunk {
    metadata: LodChunkMetadata,
    asset: Handle<WorldAsset>,
    scene_spawned: bool,
    hash_task: Option<Task<Result<(), LodChunkFailure>>>,
    hash_verified: bool,
    started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LodChunkFailureClass {
    Transient,
    Terminal,
}

#[derive(Debug)]
struct LodChunkFailure {
    class: LodChunkFailureClass,
    reason: String,
}

impl LodChunkFailure {
    fn transient(reason: impl Into<String>) -> Self {
        Self {
            class: LodChunkFailureClass::Transient,
            reason: reason.into(),
        }
    }

    fn terminal(reason: impl Into<String>) -> Self {
        Self {
            class: LodChunkFailureClass::Terminal,
            reason: reason.into(),
        }
    }
}

pub(super) fn mark_lod_world_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingLodChunk>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_lod_chunks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut streaming: ResMut<LodStreaming>,
    mut budget: ResMut<StreamingCommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    if config.stream_radius < 0 {
        if streaming.center.take().is_some() {
            streaming.generation = streaming.generation.wrapping_add(1);
        }
        streaming.requested_queries.clear();
        streaming.pending_queries.clear();
        streaming.query_retries.clear();
        streaming.pending_chunks.clear();
        streaming.queued_retry_chunks.clear();
        let previous_chunks = streaming.chunks.len();
        streaming.chunks.retain(|key, status| {
            if let Some(root) = status.root() {
                commands.entity(root).try_despawn();
            }
            profiler.event(format!("{key:?}"), "unloaded", None);
            false
        });
        if streaming.chunks.len() != previous_chunks {
            streaming.visibility_dirty = true;
        }
        update_counts(&streaming, &mut metrics, &mut profiler);
        profiler.record_elapsed("lod/plan", started);
        return;
    }

    let center = streaming_center(config.acceptance_screenshot.is_some(), origin.0, camera);
    if streaming.center != Some(center) {
        streaming.move_center(center, config.worldspace_id, &config.terrain_lod);
    }
    let generation = streaming.generation;
    for tier in LodTier::ALL {
        if !streaming.query_ready(tier, Instant::now()) {
            continue;
        }
        let query = query_for(
            config.worldspace_id,
            tier,
            center,
            query_unload_radius(config.terrain_lod.reach_cells(tier)),
        );
        match database.request_lod_chunks(generation, query) {
            Ok(()) => {
                streaming.query_submitted(tier);
                metrics.lod_queries_submitted = metrics.lod_queries_submitted.saturating_add(1);
                profiler.increment("lod/queries_submitted", 1);
            }
            Err(error) => {
                metrics.lod_query_submission_failures =
                    metrics.lod_query_submission_failures.saturating_add(1);
                debug!(?tier, %error, "terrain LOD query will be retried");
            }
        }
    }
    metrics.pending_lod_queries = streaming.pending_queries.len();

    let center64 = (i64::from(center.x), i64::from(center.y));
    let previous_chunks = streaming.chunks.len();
    streaming.chunks.retain(|key, status| {
        let keep = chunk_within_radius(
            *key,
            status.origin(),
            center64,
            query_unload_radius(config.terrain_lod.reach_cells(key.tier)),
        );
        if !keep {
            if let Some(root) = status.root() {
                commands.entity(root).try_despawn();
            }
            profiler.event(format!("{key:?}"), "unloaded", None);
        }
        keep
    });
    if streaming.chunks.len() != previous_chunks {
        streaming.visibility_dirty = true;
    }
    let generation = streaming.generation;
    streaming
        .pending_chunks
        .retain(|(queued_generation, _, _)| *queued_generation == generation);
    let resident_keys: HashSet<_> = streaming.chunks.keys().copied().collect();
    streaming
        .queued_retry_chunks
        .retain(|key| resident_keys.contains(key));
    enqueue_due_lod_retries(&mut streaming, Instant::now());
    budget.reserve_for_lod(
        streaming.has_queued_chunks(),
        config.max_cell_commits_per_frame,
    );
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/plan", started);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_lod_chunks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    asset_server: Res<AssetServer>,
    origin: Res<RenderOrigin>,
    mut camera_projection: Query<&mut Projection, With<StreamingCamera>>,
    mut streaming: ResMut<LodStreaming>,
    mut budget: ResMut<StreamingCommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    for _ in 0..LOD_RESPONSE_SCAN_LIMIT {
        let Some(response) = database.try_lod_response() else {
            break;
        };
        if response.generation == streaming.generation {
            streaming.pending_queries.remove(&response.query.tier);
        }
        metrics.lod_query_responses = metrics.lod_query_responses.saturating_add(1);
        metrics.total_lod_query_micros = metrics
            .total_lod_query_micros
            .saturating_add(response.query_micros);
        metrics.max_lod_query_micros = metrics.max_lod_query_micros.max(response.query_micros);
        profiler.record_micros("lod/db_queue_wait", response.queue_wait_micros);
        profiler.record_micros("lod/db_query", response.query_micros);
        profiler.record_micros("lod/db_request_total", response.total_request_micros);

        if !is_current_query(&streaming, response.generation) {
            metrics.stale_lod_query_responses = metrics.stale_lod_query_responses.saturating_add(1);
            profiler.increment("lod/stale_query_responses", 1);
            continue;
        }
        match response.result {
            Err(error) => {
                metrics.failed_lod_queries = metrics.failed_lod_queries.saturating_add(1);
                let class = if error.transient {
                    LodChunkFailureClass::Transient
                } else {
                    LodChunkFailureClass::Terminal
                };
                streaming.query_failed(response.query.tier, class, Instant::now());
                warn!(?response.query.tier, %error, "terrain LOD query failed");
            }
            Ok(chunks) => {
                streaming.query_retries.remove(&response.query.tier);
                if !chunks.is_empty()
                    && let Ok(mut projection) = camera_projection.single_mut()
                    && let Projection::Perspective(perspective) = &mut *projection
                {
                    perspective.far = perspective
                        .far
                        .max(lod_camera_far(config.terrain_lod.max_reach_cells()));
                }
                streaming.pending_chunks.extend(
                    chunks
                        .into_iter()
                        .map(|metadata| (response.generation, metadata, None)),
                );
            }
        }
    }

    while budget.remaining > 0 {
        let Some((generation, metadata, retry_attempt)) = streaming.pending_chunks.pop_front()
        else {
            break;
        };
        if retry_attempt.is_some() {
            streaming.queued_retry_chunks.remove(&metadata.key);
        }
        if generation != streaming.generation || streaming.center.is_none() {
            metrics.stale_lod_query_responses = metrics.stale_lod_query_responses.saturating_add(1);
            continue;
        }
        let center = streaming.center.unwrap();
        if !chunk_within_radius(
            metadata.key,
            metadata.origin,
            (i64::from(center.x), i64::from(center.y)),
            query_unload_radius(config.terrain_lod.reach_cells(metadata.key.tier)),
        ) {
            continue;
        }
        let retry_count = match retry_attempt {
            Some(retry_attempt) => {
                let Some(LodChunkStatus::Failed {
                    retry_count,
                    retry_at: Some(retry_at),
                    ..
                }) = streaming.chunks.get(&metadata.key)
                else {
                    continue;
                };
                if retry_count.saturating_add(1) != retry_attempt || Instant::now() < *retry_at {
                    continue;
                }
                retry_attempt
            }
            None => {
                if streaming.chunks.contains_key(&metadata.key) {
                    continue;
                }
                0
            }
        };
        if let Some(identity) = &streaming.build_identity {
            if identity != &metadata.build_identity {
                metrics.failed_lod_chunks = metrics.failed_lod_chunks.saturating_add(1);
                error!(
                    ?metadata.key,
                    expected = identity,
                    actual = metadata.build_identity,
                    "terrain LOD chunk belongs to a different asset build"
                );
                streaming.chunks.insert(
                    metadata.key,
                    LodChunkStatus::Failed {
                        origin: metadata.origin,
                        metadata,
                        retry_count: 0,
                        retry_at: None,
                    },
                );
                continue;
            }
        } else {
            streaming.build_identity = Some(metadata.build_identity.clone());
        }

        let key = metadata.key;
        let chunk_origin = metadata.origin;
        let min_grid = chunk_min_grid(key, metadata.origin);
        let scene_path = GltfAssetLabel::Scene(0).from_asset(metadata.payload_path.clone());
        // Bevy's load request restarts an asset whose cached load state is Failed.
        let asset = asset_server.load(scene_path);
        let expected_hash = metadata.content_hash.clone();
        let hash_path = config.assets_dir.join(&metadata.payload_path);
        let hash_task =
            IoTaskPool::get().spawn(async move { verify_payload_hash(hash_path, expected_hash) });
        let local_grid_x = min_grid.0 - i64::from(origin.0.x);
        let local_grid_y = min_grid.1 - i64::from(origin.0.y);
        let root = commands
            .spawn((
                Name::new(format!(
                    "LOD {:?} chunk {},{}",
                    key.tier, key.anchor.x, key.anchor.y
                )),
                LodChunkRoot {
                    key,
                    generation,
                    origin: metadata.origin,
                    retry_count,
                },
                LodChunkGridOrigin {
                    grid_x: min_grid.0,
                    grid_y: min_grid.1,
                },
                WorldAssetRoot(asset.clone()),
                Transform::from_xyz(
                    local_grid_x as f32 * CELL_SIZE,
                    0.0,
                    -(local_grid_y as f32) * CELL_SIZE,
                ),
                Visibility::Hidden,
                PendingLodChunk {
                    metadata,
                    asset,
                    scene_spawned: false,
                    hash_task: Some(hash_task),
                    hash_verified: false,
                    started: Instant::now(),
                },
            ))
            .id();
        streaming.chunks.insert(
            key,
            LodChunkStatus::Loading {
                root,
                generation,
                origin: chunk_origin,
            },
        );
        budget.remaining -= 1;
        budget.commits = budget.commits.saturating_add(1);
        metrics.lod_chunks_requested = metrics.lod_chunks_requested.saturating_add(1);
        profiler.increment("lod/chunks_requested", 1);
        profiler.event(format!("{key:?}"), "requested", None);
    }
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/collect", started);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn track_lod_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    world_assets: Res<Assets<WorldAsset>>,
    meshes: Res<Assets<Mesh>>,
    children: Query<&Children>,
    names: Query<&Name>,
    mesh_handles: Query<&Mesh3d>,
    mut pending_chunks: Query<(Entity, &LodChunkRoot, &mut PendingLodChunk)>,
    mut streaming: ResMut<LodStreaming>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    for (entity, root, mut pending) in &mut pending_chunks {
        let load_failure = asset_server.get_load_states(pending.asset.id()).and_then(
            |(load, _, recursive)| match (load, recursive) {
                (LoadState::Failed(error), _) => Some(classify_asset_load_failure(&error)),
                (_, RecursiveDependencyLoadState::Failed(error)) => {
                    Some(classify_asset_load_failure(&error))
                }
                _ => None,
            },
        );
        if let Some(failure) = load_failure {
            fail_chunk(
                (entity, root),
                &pending.metadata,
                failure,
                &mut commands,
                &mut streaming,
                &mut metrics,
                &mut profiler,
            );
            continue;
        }

        if !pending.hash_verified {
            let Some(task) = pending.hash_task.as_ref() else {
                continue;
            };
            if !task.is_finished() {
                continue;
            }
            let task = pending.hash_task.take().expect("finished hash task exists");
            match block_on(task) {
                Ok(()) => pending.hash_verified = true,
                Err(reason) => {
                    fail_chunk(
                        (entity, root),
                        &pending.metadata,
                        reason,
                        &mut commands,
                        &mut streaming,
                        &mut metrics,
                        &mut profiler,
                    );
                    continue;
                }
            }
        }

        if !pending.scene_spawned
            || !asset_server.is_loaded_with_dependencies(pending.asset.id())
            || world_assets.get(&pending.asset).is_none()
        {
            continue;
        }

        let patches = match validate_lod_scene(
            entity,
            &pending.metadata,
            &children,
            &names,
            &mesh_handles,
            &meshes,
        ) {
            Ok(patches) => patches,
            Err(reason) => {
                fail_chunk(
                    (entity, root),
                    &pending.metadata,
                    LodChunkFailure::terminal(reason),
                    &mut commands,
                    &mut streaming,
                    &mut metrics,
                    &mut profiler,
                );
                continue;
            }
        };
        for (patch, coverage) in &patches {
            commands
                .entity(*patch)
                .insert((*coverage, TerrainSurfaceReady, Visibility::Hidden));
        }
        if !patches.is_empty() {
            streaming.visibility_dirty = true;
        }
        commands.entity(entity).insert(Visibility::Inherited);
        commands.entity(entity).remove::<PendingLodChunk>();
        if let Some(LodChunkStatus::Loading {
            root: expected_root,
            generation,
            origin,
        }) = streaming.chunks.get(&root.key).cloned()
            && expected_root == entity
            && generation == root.generation
        {
            streaming.chunks.insert(
                root.key,
                LodChunkStatus::Ready {
                    root: entity,
                    origin,
                },
            );
        }
        metrics.lod_chunks_ready = metrics.lod_chunks_ready.saturating_add(1);
        metrics.ready_lod_terrain_patches = metrics
            .ready_lod_terrain_patches
            .saturating_add(patches.len() as u64);
        profiler.increment("lod/chunks_ready", 1);
        profiler.increment("lod/terrain_patches_ready", patches.len() as u64);
        profiler.record_elapsed("lod/chunk_ready", pending.started);
        profiler.event(format!("{:?}", root.key), "ready", None);
    }
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/readiness", started);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_terrain_lod_visibility(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut removed_coverage: RemovedComponents<TerrainCoverage>,
    mut removed_readiness: RemovedComponents<TerrainSurfaceReady>,
    mut streaming: ParamSet<(Res<LodStreaming>, ResMut<LodStreaming>)>,
    full_detail: Query<&TerrainCoverage, (With<super::TerrainPatch>, With<TerrainSurfaceReady>)>,
    mut lod_patches: Query<
        (
            &TerrainCoverage,
            Option<&TerrainSurfaceReady>,
            &mut Visibility,
        ),
        Without<super::TerrainPatch>,
    >,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let camera_grid = streaming_center(config.acceptance_screenshot.is_some(), origin.0, camera);
    let coverage_removed = removed_coverage.read().count() != 0;
    let readiness_removed = removed_readiness.read().count() != 0;
    let refresh = {
        let state = streaming.p0();
        state.visibility_dirty
            || coverage_removed
            || readiness_removed
            || state.visibility_camera_grid != Some(camera_grid)
            || state.visibility_stream_radius != Some(config.stream_radius)
    };
    if !refresh {
        return;
    }
    {
        let mut state = streaming.p1();
        state.visibility_dirty = false;
        state.visibility_camera_grid = Some(camera_grid);
        state.visibility_stream_radius = Some(config.stream_radius);
    }

    let full_ready: HashSet<_> = full_detail
        .iter()
        .map(|coverage| (coverage.grid, coverage.quadrant))
        .collect();
    let mut available = HashMap::<(IVec2, u8), HashSet<LodTier>>::new();
    let mut ready_patches = 0usize;
    for (coverage, ready, _) in &mut lod_patches {
        if let (Some(tier), Some(_)) = (coverage.tier, ready) {
            available
                .entry((coverage.grid, coverage.quadrant))
                .or_default()
                .insert(tier);
            ready_patches += 1;
        }
    }
    let selected_tiers: HashMap<_, _> = available
        .iter()
        .map(|(&(grid, quadrant), candidates)| {
            let distance = chebyshev_grid_distance(grid, camera_grid);
            (
                (grid, quadrant),
                select_terrain_lod_tier(
                    distance,
                    full_ready.contains(&(grid, quadrant)),
                    candidates,
                    &config.terrain_lod,
                ),
            )
        })
        .collect();
    let mut visible_patches = 0usize;
    for (coverage, ready, mut visibility) in &mut lod_patches {
        let selected = coverage.tier.is_some_and(|tier| {
            ready.is_some()
                && selected_tiers.get(&(coverage.grid, coverage.quadrant)) == Some(&Some(tier))
        });
        let next = if selected {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if selected {
            visible_patches += 1;
        }
        if *visibility != next {
            *visibility = next;
        }
    }
    metrics.visible_lod_terrain_patches = visible_patches;
    profiler.set_gauge("lod/ready_terrain_patches", ready_patches as f64);
    profiler.set_gauge("lod/visible_terrain_patches", visible_patches as f64);
    profiler.record_elapsed("lod/visibility", started);
}

fn fail_chunk(
    chunk: (Entity, &LodChunkRoot),
    metadata: &LodChunkMetadata,
    failure: LodChunkFailure,
    commands: &mut Commands,
    streaming: &mut LodStreaming,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) {
    let (entity, root) = chunk;
    let retry_at = lod_retry_deadline(failure.class, root.retry_count, Instant::now());
    warn!(
        ?root.key,
        class = ?failure.class,
        reason = %failure.reason,
        retrying = retry_at.is_some(),
        "terrain LOD chunk failed validation or loading"
    );
    metrics.failed_lod_chunks = metrics.failed_lod_chunks.saturating_add(1);
    profiler.increment("lod/chunks_failed", 1);
    profiler.event(format!("{:?}", root.key), "failed", None);
    if let Some(LodChunkStatus::Loading {
        root: expected_root,
        generation,
        ..
    }) = streaming.chunks.get(&root.key).cloned()
        && expected_root == entity
        && generation == root.generation
    {
        streaming.chunks.insert(
            root.key,
            LodChunkStatus::Failed {
                origin: root.origin,
                metadata: metadata.clone(),
                retry_count: root.retry_count,
                retry_at,
            },
        );
    }
    commands.entity(entity).try_despawn();
}

fn validate_lod_scene(
    root: Entity,
    metadata: &LodChunkMetadata,
    children: &Query<&Children>,
    names: &Query<&Name>,
    mesh_handles: &Query<&Mesh3d>,
    meshes: &Assets<Mesh>,
) -> Result<Vec<(Entity, TerrainCoverage)>, String> {
    let key = metadata.key;
    let source_cells: HashSet<_> = metadata
        .source_cells
        .iter()
        .map(|[x, y]| IVec2::new(*x, *y))
        .collect();
    if source_cells.is_empty() || source_cells.len() != metadata.source_cells.len() {
        return Err(format!(
            "{key:?} has empty or duplicate source-cell metadata"
        ));
    }
    if source_cells
        .iter()
        .any(|cell| !chunk_contains_cell(key, metadata.origin, *cell))
    {
        return Err(format!(
            "{key:?} source-cell metadata lies outside its chunk"
        ));
    }

    let mut stack = vec![(root, None)];
    let mut seen = HashSet::new();
    let mut patches = Vec::with_capacity(source_cells.len() * 4);
    while let Some((entity, inherited_grid)) = stack.pop() {
        let name = names.get(entity).ok().map(Name::as_str);
        let source_grid = match name.filter(|name| name.starts_with("cell_")) {
            Some(name) => {
                let Some((x, y)) = nodes::parse_source_cell(name) else {
                    return Err(format!("{key:?} has malformed source-cell node {name:?}"));
                };
                let grid = IVec2::new(x, y);
                if !source_cells.contains(&grid) {
                    return Err(format!("{key:?} has unexpected source cell {grid:?}"));
                }
                Some(grid)
            }
            None => inherited_grid,
        };
        if let Some(name) = name.filter(|name| name.starts_with("terrain_quadrant_")) {
            let quadrant = quadrant_from_node_name(name)
                .ok_or_else(|| format!("{key:?} has unknown terrain quadrant node {name:?}"))?;
            let grid = source_grid
                .ok_or_else(|| format!("{key:?} quadrant node has no source-cell parent"))?;
            // Bevy's glTF loader spawns each mesh primitive as a child of the
            // glTF node entity, not on the node itself: the quadrant node
            // carries the name, its single primitive child carries the Mesh3d.
            let primitive_children: Vec<Entity> = children
                .get(entity)
                .map(|direct| {
                    direct
                        .iter()
                        .filter(|child| mesh_handles.contains(*child))
                        .collect()
                })
                .unwrap_or_default();
            let [primitive] = primitive_children.as_slice() else {
                return Err(format!(
                    "{key:?} quadrant node {name:?} has {} mesh primitives, expected 1",
                    primitive_children.len()
                ));
            };
            let mesh_handle = mesh_handles
                .get(*primitive)
                .map_err(|_| format!("{key:?} quadrant node {name:?} has no mesh"))?;
            let mesh = meshes
                .get(&mesh_handle.0)
                .ok_or_else(|| format!("{key:?} quadrant node {name:?} mesh is missing"))?;
            validate_lod_quadrant_mesh(mesh)
                .map_err(|reason| format!("{key:?} {grid:?} quadrant {quadrant}: {reason}"))?;
            if !seen.insert((grid, quadrant)) {
                return Err(format!("{key:?} duplicates {grid:?} quadrant {quadrant}"));
            }
            patches.push((
                entity,
                TerrainCoverage {
                    grid,
                    quadrant,
                    tier: Some(key.tier),
                },
            ));
        }
        if let Ok(direct_children) = children.get(entity) {
            for child in direct_children.iter() {
                stack.push((child, source_grid));
            }
        }
    }

    if seen.len() != source_cells.len() * 4 {
        return Err(format!(
            "{key:?} contains {} terrain quadrants for {} source cells; expected {}",
            seen.len(),
            source_cells.len(),
            source_cells.len() * 4
        ));
    }
    Ok(patches)
}

fn validate_lod_quadrant_mesh(mesh: &Mesh) -> Result<(), String> {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
        return Err("mesh is not a triangle list".to_owned());
    }
    let positions = match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(VertexAttributeValues::Float32x3(values))
            if values.len() == TERRAIN_QUADRANT_VERTEX_COUNT =>
        {
            values
        }
        _ => return Err("mesh has an invalid Float32x3 position count".to_owned()),
    };
    if !positions.iter().flatten().all(|value| value.is_finite()) {
        return Err("mesh positions contain non-finite values".to_owned());
    }
    match mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
        Some(VertexAttributeValues::Float32x3(values))
            if values.len() == positions.len()
                && values.iter().flatten().all(|value| value.is_finite()) => {}
        _ => return Err("mesh normals do not match its positions".to_owned()),
    }
    match mesh.attribute(Mesh::ATTRIBUTE_UV_0) {
        Some(VertexAttributeValues::Float32x2(values))
            if values.len() == positions.len()
                && values
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) => {}
        _ => return Err("mesh UVs are missing, non-finite, or outside 0..1".to_owned()),
    }
    match mesh.attribute(Mesh::ATTRIBUTE_COLOR) {
        Some(VertexAttributeValues::Float32x4(values))
            if values.len() == positions.len()
                && values
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) => {}
        _ => return Err("mesh colors are missing, non-finite, or outside 0..1".to_owned()),
    }
    let index_count = match mesh.indices() {
        Some(Indices::U16(indices)) if indices.len() == TERRAIN_QUADRANT_INDEX_COUNT => {
            if indices
                .iter()
                .any(|index| usize::from(*index) >= positions.len())
            {
                return Err("mesh has an out-of-range index".to_owned());
            }
            indices.len()
        }
        Some(Indices::U32(indices)) if indices.len() == TERRAIN_QUADRANT_INDEX_COUNT => {
            if indices
                .iter()
                .any(|index| *index as usize >= positions.len())
            {
                return Err("mesh has an out-of-range index".to_owned());
            }
            indices.len()
        }
        _ => return Err("mesh has an invalid triangle index count".to_owned()),
    };
    if index_count % 3 != 0 {
        return Err("mesh indices do not form complete triangles".to_owned());
    }
    Ok(())
}

/// The finest ready tier whose configured reach covers the cell, or `None` once
/// full detail is drawable or the cell is beyond every reach.
fn select_terrain_lod_tier(
    grid_distance: i32,
    full_detail_ready: bool,
    available: &HashSet<LodTier>,
    distances: &TerrainLodDistances,
) -> Option<LodTier> {
    if full_detail_ready {
        return None;
    }
    LodTier::ALL
        .into_iter()
        .find(|tier| grid_distance <= distances.reach_cells(*tier) && available.contains(tier))
}

fn chebyshev_grid_distance(left: IVec2, right: IVec2) -> i32 {
    left.x
        .saturating_sub(right.x)
        .saturating_abs()
        .max(left.y.saturating_sub(right.y).saturating_abs())
}

fn quadrant_from_node_name(name: &str) -> Option<u8> {
    let direction = name.strip_prefix("terrain_quadrant_")?;
    ["sw", "se", "nw", "ne"]
        .iter()
        .position(|candidate| *candidate == direction)
        .map(|index| index as u8)
}

fn streaming_center(screenshot: bool, origin: IVec2, camera: &Transform) -> IVec2 {
    if screenshot {
        return origin;
    }
    let global_x = camera.translation.x + origin.x as f32 * CELL_SIZE;
    let global_y = -camera.translation.z + origin.y as f32 * CELL_SIZE;
    IVec2::new(
        (global_x / CELL_SIZE).floor() as i32,
        (global_y / CELL_SIZE).floor() as i32,
    )
}

fn query_for(worldspace_id: u32, tier: LodTier, center: IVec2, radius: i32) -> LodChunkQuery {
    let min_x = (i64::from(center.x) - i64::from(radius)) as f64 * f64::from(CELL_SIZE);
    let min_y = (i64::from(center.y) - i64::from(radius)) as f64 * f64::from(CELL_SIZE);
    let max_x = (i64::from(center.x) + i64::from(radius) + 1) as f64 * f64::from(CELL_SIZE);
    let max_y = (i64::from(center.y) + i64::from(radius) + 1) as f64 * f64::from(CELL_SIZE);
    LodChunkQuery {
        worldspace_id,
        tier,
        bounds_min: [min_x, min_y],
        bounds_max: [max_x, max_y],
    }
}

fn is_current_query(streaming: &LodStreaming, generation: u64) -> bool {
    streaming.center.is_some() && generation == streaming.generation
}

fn query_unload_radius(reach_cells: i32) -> i32 {
    reach_cells.saturating_add(LOD_UNLOAD_MARGIN_CELLS)
}

fn chunk_min_grid(key: ChunkKey, origin: LodOrigin) -> (i64, i64) {
    let side = i64::from(key.tier.side_cells());
    (
        i64::from(origin.grid_x) + i64::from(key.anchor.x) * side,
        i64::from(origin.grid_y) + i64::from(key.anchor.y) * side,
    )
}

fn chunk_contains_cell(key: ChunkKey, origin: LodOrigin, cell: IVec2) -> bool {
    let side = i64::from(key.tier.side_cells());
    let min = chunk_min_grid(key, origin);
    let x = i64::from(cell.x);
    let y = i64::from(cell.y);
    x >= min.0 && x < min.0 + side && y >= min.1 && y < min.1 + side
}

fn chunk_within_radius(key: ChunkKey, origin: LodOrigin, center: (i64, i64), radius: i32) -> bool {
    let side = i64::from(key.tier.side_cells());
    let min = chunk_min_grid(key, origin);
    let max = (min.0 + side - 1, min.1 + side - 1);
    let distance = |value: i64, low: i64, high: i64| {
        if value < low {
            low - value
        } else if value > high {
            value - high
        } else {
            0
        }
    };
    distance(center.0, min.0, max.0).max(distance(center.1, min.1, max.1)) <= i64::from(radius)
}

fn enqueue_due_lod_retries(streaming: &mut LodStreaming, now: Instant) {
    let generation = streaming.generation;
    let due: Vec<_> = streaming
        .chunks
        .iter()
        .filter_map(|(&key, status)| match status {
            LodChunkStatus::Failed {
                metadata,
                retry_count,
                retry_at: Some(retry_at),
                ..
            } if *retry_at <= now && !streaming.queued_retry_chunks.contains(&key) => {
                Some((key, metadata.clone(), retry_count.saturating_add(1)))
            }
            _ => None,
        })
        .collect();
    for (key, metadata, retry_attempt) in due {
        if streaming.queued_retry_chunks.insert(key) {
            streaming
                .pending_chunks
                .push_back((generation, metadata, Some(retry_attempt)));
        }
    }
}

fn lod_retry_deadline(
    class: LodChunkFailureClass,
    retry_count: u8,
    now: Instant,
) -> Option<Instant> {
    if class != LodChunkFailureClass::Transient || retry_count >= LOD_CHUNK_MAX_RETRIES {
        return None;
    }
    let multiplier = 1_u32 << u32::from(retry_count.min(2));
    Some(
        now + LOD_CHUNK_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(LOD_CHUNK_RETRY_MAX_DELAY),
    )
}

fn classify_asset_load_failure(error: &AssetLoadError) -> LodChunkFailure {
    let class = match error {
        AssetLoadError::AssetReaderError(AssetReaderError::Io(error))
            if error.kind() != std::io::ErrorKind::NotFound =>
        {
            LodChunkFailureClass::Transient
        }
        AssetLoadError::AssetReaderError(AssetReaderError::HttpError(408 | 429 | 500..=599)) => {
            LodChunkFailureClass::Transient
        }
        _ => LodChunkFailureClass::Terminal,
    };
    let reason = error.to_string();
    match class {
        LodChunkFailureClass::Transient => LodChunkFailure::transient(reason),
        LodChunkFailureClass::Terminal => LodChunkFailure::terminal(reason),
    }
}

fn verify_payload_hash(path: PathBuf, expected: String) -> Result<(), LodChunkFailure> {
    let bytes = std::fs::read(&path).map_err(|error| {
        let reason = format!("failed to read LOD payload {}: {error}", path.display());
        if error.kind() == std::io::ErrorKind::NotFound {
            LodChunkFailure::terminal(reason)
        } else {
            LodChunkFailure::transient(reason)
        }
    })?;
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if actual == expected {
        Ok(())
    } else {
        Err(LodChunkFailure::terminal(format!(
            "LOD payload {} has SHA-256 {actual}, expected {expected}",
            path.display()
        )))
    }
}

fn update_counts(
    streaming: &LodStreaming,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) {
    metrics.pending_lod_queries = streaming.pending_queries.len()
        + if streaming.center.is_some() {
            LodTier::ALL
                .into_iter()
                .filter(|tier| {
                    !streaming.requested_queries.contains(tier)
                        && streaming
                            .query_retries
                            .get(tier)
                            .is_none_or(|(_, retry_at)| retry_at.is_some())
                })
                .count()
        } else {
            0
        };
    metrics.unrecovered_lod_queries = streaming
        .query_retries
        .iter()
        .filter(|(tier, (_, retry_at))| {
            retry_at.is_none() && !streaming.pending_queries.contains(tier)
        })
        .count();
    metrics.unrecovered_lod_chunks = streaming
        .chunks
        .values()
        .filter(|status| matches!(status, LodChunkStatus::Failed { retry_at: None, .. }))
        .count();
    metrics.resident_lod_chunks = streaming
        .chunks
        .values()
        .filter(|status| matches!(status, LodChunkStatus::Ready { .. }))
        .count();
    let mut pending_keys: HashSet<_> = streaming
        .chunks
        .iter()
        .filter_map(|(key, status)| {
            matches!(
                status,
                LodChunkStatus::Loading { .. }
                    | LodChunkStatus::Failed {
                        retry_at: Some(_),
                        ..
                    }
            )
            .then_some(*key)
        })
        .collect();
    pending_keys.extend(
        streaming
            .pending_chunks
            .iter()
            .filter(|(generation, metadata, _)| {
                *generation == streaming.generation && !streaming.chunks.contains_key(&metadata.key)
            })
            .map(|(_, metadata, _)| metadata.key),
    );
    metrics.pending_lod_chunks = pending_keys.len();
    profiler.set_gauge("lod/resident_chunks", metrics.resident_lod_chunks as f64);
    profiler.set_gauge("lod/pending_chunks", metrics.pending_lod_chunks as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::database::LodChunkBounds;
    use bevy::ecs::system::RunSystemOnce;

    /// A reach equal to each tier's chunk side (4/8/16 cells), the smallest
    /// nesting the residency tests below exercise.
    fn side_reach() -> TerrainLodDistances {
        TerrainLodDistances {
            block_level0_distance: 4.0 * CELL_SIZE,
            block_level1_distance: 8.0 * CELL_SIZE,
            block_maximum_distance: 16.0 * CELL_SIZE,
            split_distance_mult: 1.0,
        }
    }

    fn default_unload_radius(tier: LodTier) -> i32 {
        query_unload_radius(side_reach().reach_cells(tier))
    }

    /// `SkyrimPrefs.ini` distances move each tier's reach, not the chunk sizes.
    /// The defaults are Skyrim's own, so level 4 reaches 12 cells, not 4.
    #[test]
    fn configured_skyrim_distances_set_each_tier_reach() {
        let skyrim = TerrainLodDistances::default();
        assert_eq!(
            LodTier::ALL.map(|tier| skyrim.reach_cells(tier)),
            [12, 25, 91]
        );
        let short = side_reach();
        assert_eq!(LodTier::ALL.map(|tier| short.reach_cells(tier)), [4, 8, 16]);
        let available = HashSet::from(LodTier::ALL);
        assert_eq!(
            select_terrain_lod_tier(12, false, &available, &skyrim),
            Some(LodTier::Tier4)
        );
        assert_eq!(
            select_terrain_lod_tier(12, false, &available, &short),
            Some(LodTier::Tier16)
        );
        assert_eq!(
            select_terrain_lod_tier(91, false, &available, &skyrim),
            Some(LodTier::Tier16)
        );
        assert_eq!(
            select_terrain_lod_tier(92, false, &available, &skyrim),
            None
        );
        assert_eq!(query_unload_radius(skyrim.reach_cells(LodTier::Tier4)), 14);
        assert!(lod_camera_far(skyrim.max_reach_cells()) > lod_camera_far(16));
        let unbounded = TerrainLodDistances {
            block_maximum_distance: f32::MAX,
            split_distance_mult: f32::MAX,
            ..skyrim
        };
        assert_eq!(unbounded.reach_cells(LodTier::Tier16), i32::MAX);
        assert_eq!(query_unload_radius(i32::MAX), i32::MAX);
    }

    #[test]
    fn failed_queries_retry_with_bounded_backoff_in_the_same_generation() {
        let mut streaming = LodStreaming::default();
        let tier = LodTier::Tier4;
        let mut now = Instant::now();
        assert!(streaming.query_ready(tier, now));
        for delay in [1, 2, 4] {
            streaming.query_submitted(tier);
            assert!(!streaming.query_ready(tier, now));
            streaming.query_failed(tier, LodChunkFailureClass::Transient, now);
            assert!(!streaming.query_ready(tier, now));
            now += Duration::from_secs(delay);
            assert!(streaming.query_ready(tier, now));
        }
        streaming.query_submitted(tier);
        streaming.query_failed(tier, LodChunkFailureClass::Transient, now);
        assert!(!streaming.query_ready(tier, now + Duration::from_secs(100)));
        assert!(streaming.pending_queries.is_empty());
        streaming.move_center(IVec2::ZERO, 1, &TerrainLodDistances::default());
        assert!(streaming.query_ready(tier, now));
    }

    #[test]
    fn terminal_query_failure_never_schedules_a_retry() {
        let mut streaming = LodStreaming::default();
        let now = Instant::now();
        streaming.query_submitted(LodTier::Tier4);
        streaming.query_failed(LodTier::Tier4, LodChunkFailureClass::Terminal, now);
        assert!(!streaming.query_ready(LodTier::Tier4, now + Duration::from_secs(100)));
        assert!(streaming.pending_queries.is_empty());
        assert_eq!(streaming.query_retries[&LodTier::Tier4].1, None);
    }

    #[test]
    fn tier_residency_bounds_near_chunks_and_preserves_coarse_inner_fallback() {
        assert_eq!(LodTier::ALL.map(default_unload_radius), [6, 10, 18]);
        for tier in LodTier::ALL {
            let origin = LodOrigin::new(-4, -4);
            let inner = ChunkKey::new(1, tier, origin.chunk_for_cell(tier, -1, -1));
            assert!(chunk_within_radius(
                inner,
                origin,
                (-1, -1),
                default_unload_radius(tier)
            ));
            let outer = ChunkKey::new(1, tier, origin.chunk_for_cell(tier, 30, 30));
            assert!(!chunk_within_radius(
                outer,
                origin,
                (-1, -1),
                default_unload_radius(tier)
            ));
        }
    }

    #[test]
    fn tier_residency_covers_moving_positive_and_negative_boundaries_with_less_work() {
        let origin = LodOrigin::new(-4, 3);
        for x in [-33, -17, -16, -5, -4, -1, 0, 3, 4, 15, 16, 31, 32] {
            let center = IVec2::new(x, -x);
            for tier in LodTier::ALL {
                let radius = default_unload_radius(tier);
                let query = query_for(1, tier, center, radius);
                let mut bounded = 0;
                let mut previous = 0;
                for ax in -20..=20 {
                    for ay in -20..=20 {
                        let key = ChunkKey::new(1, tier, shared::lod::ChunkAnchor::new(ax, ay));
                        let within = chunk_within_radius(
                            key,
                            origin,
                            (i64::from(center.x), i64::from(center.y)),
                            radius,
                        );
                        bounded += usize::from(within);
                        previous += usize::from(chunk_within_radius(
                            key,
                            origin,
                            (i64::from(center.x), i64::from(center.y)),
                            18,
                        ));
                        if within {
                            let min = chunk_min_grid(key, origin);
                            let side = i64::from(tier.side_cells());
                            assert!((min.0 as f64 * f64::from(CELL_SIZE)) < query.bounds_max[0]);
                            assert!(
                                ((min.0 + side) as f64 * f64::from(CELL_SIZE))
                                    > query.bounds_min[0]
                            );
                            assert!((min.1 as f64 * f64::from(CELL_SIZE)) < query.bounds_max[1]);
                            assert!(
                                ((min.1 + side) as f64 * f64::from(CELL_SIZE))
                                    > query.bounds_min[1]
                            );
                        }
                    }
                }
                if tier != LodTier::Tier16 {
                    assert!(bounded < previous);
                }
                for dx in -tier.side_cells()..=tier.side_cells() {
                    for dy in -tier.side_cells()..=tier.side_cells() {
                        let cell = center + IVec2::new(dx, dy);
                        let key =
                            ChunkKey::new(1, tier, origin.chunk_for_cell(tier, cell.x, cell.y));
                        assert!(chunk_contains_cell(key, origin, cell));
                        assert!(chunk_within_radius(
                            key,
                            origin,
                            (i64::from(center.x), i64::from(center.y)),
                            radius
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn movement_retains_in_range_queued_metadata_but_discards_teleport_work() {
        let metadata = retry_test_metadata();
        let mut streaming = LodStreaming {
            generation: 7,
            center: Some(IVec2::ZERO),
            pending_chunks: VecDeque::from([(7, metadata.clone(), None)]),
            ..default()
        };
        streaming.move_center(
            IVec2::new(1, 0),
            metadata.key.worldspace_id,
            &TerrainLodDistances::default(),
        );
        assert_eq!(
            streaming.pending_chunks.front(),
            Some(&(8, metadata.clone(), None))
        );
        assert!(!is_current_query(&streaming, 7));
        streaming.move_center(
            IVec2::new(1000, 1000),
            metadata.key.worldspace_id,
            &TerrainLodDistances::default(),
        );
        assert!(streaming.pending_chunks.is_empty());
    }

    #[test]
    fn pending_counts_include_queued_and_retry_work_without_duplicates() {
        let metadata = retry_test_metadata();
        let key = metadata.key;
        let mut streaming = LodStreaming {
            generation: 7,
            center: Some(IVec2::ZERO),
            requested_queries: LodTier::ALL.into_iter().collect(),
            pending_chunks: VecDeque::from([
                (7, metadata.clone(), None),
                (7, metadata.clone(), None),
            ]),
            ..default()
        };
        let mut metrics = StreamingMetrics::default();
        let mut profiler = ProfilingState::default();
        update_counts(&streaming, &mut metrics, &mut profiler);
        assert_eq!(metrics.pending_lod_chunks, 1);
        streaming.chunks.insert(
            key,
            LodChunkStatus::Failed {
                origin: metadata.origin,
                metadata,
                retry_count: 0,
                retry_at: Some(Instant::now() + Duration::from_secs(1)),
            },
        );
        update_counts(&streaming, &mut metrics, &mut profiler);
        assert_eq!(metrics.pending_lod_chunks, 1);
        streaming.pending_chunks.clear();
        streaming.query_failed(
            LodTier::Tier4,
            LodChunkFailureClass::Transient,
            Instant::now(),
        );
        update_counts(&streaming, &mut metrics, &mut profiler);
        assert_eq!(metrics.pending_lod_queries, 1);
        assert_eq!(metrics.pending_lod_chunks, 1);
        if let Some(LodChunkStatus::Failed { retry_at, .. }) = streaming.chunks.get_mut(&key) {
            *retry_at = None;
        }
        update_counts(&streaming, &mut metrics, &mut profiler);
        assert_eq!(metrics.pending_lod_chunks, 0);
        assert_eq!(metrics.unrecovered_lod_chunks, 1);
        streaming.chunks.remove(&key);
        update_counts(&streaming, &mut metrics, &mut profiler);
        assert_eq!(metrics.unrecovered_lod_chunks, 0);
    }

    #[test]
    fn lod_payload_hash_checks_keep_bad_and_missing_content_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.glb");
        let bytes = b"fixture payload";
        std::fs::write(&path, bytes).unwrap();
        let digest = Sha256::digest(bytes);
        let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert!(verify_payload_hash(path.clone(), expected.clone()).is_ok());
        std::fs::write(&path, b"changed payload").unwrap();
        assert_eq!(
            verify_payload_hash(path.clone(), expected.clone())
                .unwrap_err()
                .class,
            LodChunkFailureClass::Terminal
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            verify_payload_hash(path, expected).unwrap_err().class,
            LodChunkFailureClass::Terminal
        );
    }

    fn retry_test_metadata() -> LodChunkMetadata {
        LodChunkMetadata {
            key: ChunkKey::new(0x3c, LodTier::Tier16, shared::lod::ChunkAnchor::new(0, 0)),
            payload_path: "worlds/test/lod/tier16_0_0.glb".to_owned(),
            content_hash: "expected-hash".to_owned(),
            bounds: LodChunkBounds {
                min: [0.0; 3],
                max: [1.0; 3],
            },
            source_cells: vec![[0, 0]],
            build_identity: "test-build".to_owned(),
            origin: LodOrigin::new(0, 0),
        }
    }

    #[test]
    fn lod_asset_failures_separate_missing_payloads_from_transient_io() {
        let missing = AssetLoadError::AssetReaderError(AssetReaderError::NotFound(PathBuf::from(
            "missing.glb",
        )));
        assert_eq!(
            classify_asset_load_failure(&missing).class,
            LodChunkFailureClass::Terminal
        );

        let interrupted =
            AssetLoadError::AssetReaderError(AssetReaderError::Io(std::sync::Arc::new(
                std::io::Error::new(std::io::ErrorKind::TimedOut, "temporary read failure"),
            )));
        assert_eq!(
            classify_asset_load_failure(&interrupted).class,
            LodChunkFailureClass::Transient
        );
    }

    #[test]
    fn transient_lod_failures_back_off_and_requeue_saved_metadata() {
        let metadata = retry_test_metadata();
        let key = metadata.key;
        let failed_at = Instant::now();
        let first_retry_at =
            lod_retry_deadline(LodChunkFailureClass::Transient, 0, failed_at).unwrap();
        assert_eq!(first_retry_at, failed_at + Duration::from_secs(1));

        let mut streaming = LodStreaming {
            generation: 7,
            center: Some(IVec2::new(16, 0)),
            chunks: HashMap::from([(
                key,
                LodChunkStatus::Failed {
                    origin: metadata.origin,
                    metadata: metadata.clone(),
                    retry_count: 0,
                    retry_at: Some(first_retry_at),
                },
            )]),
            ..default()
        };

        enqueue_due_lod_retries(&mut streaming, first_retry_at - Duration::from_millis(1));
        assert!(streaming.pending_chunks.is_empty());
        enqueue_due_lod_retries(&mut streaming, first_retry_at);
        assert_eq!(
            streaming.pending_chunks.front(),
            Some(&(7, metadata.clone(), Some(1)))
        );
        assert!(streaming.queued_retry_chunks.contains(&key));
        enqueue_due_lod_retries(&mut streaming, first_retry_at);
        assert_eq!(streaming.pending_chunks.len(), 1, "a due retry queues once");

        let second_failed_at = first_retry_at + Duration::from_millis(200);
        let second_retry_at =
            lod_retry_deadline(LodChunkFailureClass::Transient, 1, second_failed_at).unwrap();
        assert_eq!(second_retry_at, second_failed_at + Duration::from_secs(2));
        streaming.pending_chunks.clear();
        streaming.queued_retry_chunks.clear();
        streaming.chunks.insert(
            key,
            LodChunkStatus::Failed {
                origin: metadata.origin,
                metadata: metadata.clone(),
                retry_count: 1,
                retry_at: Some(second_retry_at),
            },
        );
        enqueue_due_lod_retries(&mut streaming, second_retry_at - Duration::from_millis(1));
        assert!(streaming.pending_chunks.is_empty());
        enqueue_due_lod_retries(&mut streaming, second_retry_at);
        assert_eq!(
            streaming.pending_chunks.front(),
            Some(&(7, metadata.clone(), Some(2)))
        );

        assert_eq!(
            lod_retry_deadline(LodChunkFailureClass::Transient, 2, failed_at),
            Some(failed_at + Duration::from_secs(4))
        );
        assert_eq!(
            lod_retry_deadline(
                LodChunkFailureClass::Transient,
                LOD_CHUNK_MAX_RETRIES,
                failed_at
            ),
            None,
            "retry attempts are bounded"
        );
        assert_eq!(
            lod_retry_deadline(LodChunkFailureClass::Terminal, 0, failed_at),
            None,
            "terminal payload failures are not retried"
        );
    }

    #[test]
    fn tier_handoff_is_per_quadrant_and_falls_back_to_ready_coarser_data() {
        let defaults = side_reach();
        let available = HashSet::from([LodTier::Tier4, LodTier::Tier8, LodTier::Tier16]);
        assert_eq!(
            select_terrain_lod_tier(2, false, &available, &defaults),
            Some(LodTier::Tier4)
        );
        assert_eq!(
            select_terrain_lod_tier(6, false, &available, &defaults),
            Some(LodTier::Tier8)
        );
        assert_eq!(
            select_terrain_lod_tier(12, false, &available, &defaults),
            Some(LodTier::Tier16)
        );
        assert_eq!(
            select_terrain_lod_tier(2, true, &available, &defaults),
            None
        );

        let coarse_only = HashSet::from([LodTier::Tier16]);
        assert_eq!(
            select_terrain_lod_tier(2, false, &coarse_only, &defaults),
            Some(LodTier::Tier16)
        );
        assert_eq!(
            select_terrain_lod_tier(17, false, &coarse_only, &defaults),
            None
        );
        assert_eq!(
            select_terrain_lod_tier(2, false, &HashSet::new(), &defaults),
            None
        );
    }

    #[test]
    fn terrain_lod_visibility_skips_unchanged_frames() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(StreamingMetrics::default())
            .insert_resource(ProfilingState::default())
            .insert_resource(LodStreaming::default())
            .add_systems(Update, update_terrain_lod_visibility);
        let world = app.world_mut();
        world.spawn((
            StreamingCamera,
            Transform::from_xyz(CELL_SIZE * 0.5, 0.0, -CELL_SIZE * 0.5),
        ));
        let patches: Vec<_> = (0..22_000)
            .map(|_| {
                world
                    .spawn((
                        TerrainCoverage {
                            grid: IVec2::new(3, 0),
                            quadrant: 0,
                            tier: Some(LodTier::Tier4),
                        },
                        TerrainSurfaceReady,
                        Visibility::Hidden,
                    ))
                    .id()
            })
            .collect();
        let patch = patches[0];

        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(patch).unwrap(),
            Visibility::Inherited
        );
        app.world_mut().entity_mut(patch).insert(Visibility::Hidden);
        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(patch).unwrap(),
            Visibility::Hidden,
            "unchanged coverage leaves visibility untouched"
        );
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .visible_lod_terrain_patches,
            22_000,
            "cached handoff metrics remain unchanged on skipped frames"
        );

        let started = Instant::now();
        for _ in 0..32 {
            app.update();
        }
        eprintln!(
            "terrain_lod_visibility_noop_22k_mean_us={}",
            started.elapsed().as_micros() / 32
        );

        app.world_mut().resource_mut::<EngineConfig>().stream_radius += 1;
        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(patch).unwrap(),
            Visibility::Inherited,
            "stream-radius changes trigger a visibility refresh"
        );
        app.world_mut().entity_mut(patch).despawn();
        app.update();
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .visible_lod_terrain_patches,
            21_999,
            "removing a patch refreshes visibility metrics"
        );
    }

    #[test]
    fn terrain_readiness_removal_releases_lod_on_scheduled_update() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .insert_resource(RenderOrigin(IVec2::ZERO))
            .insert_resource(StreamingMetrics::default())
            .insert_resource(ProfilingState::default())
            .insert_resource(LodStreaming::default())
            .add_systems(Update, update_terrain_lod_visibility);
        let world = app.world_mut();
        world.spawn((
            StreamingCamera,
            Transform::from_xyz(CELL_SIZE * 0.5, 0.0, -CELL_SIZE * 0.5),
        ));
        let full_patch = world
            .spawn((
                super::super::TerrainPatch,
                TerrainCoverage {
                    grid: IVec2::ZERO,
                    quadrant: 0,
                    tier: None,
                },
                TerrainSurfaceReady,
                Visibility::Inherited,
            ))
            .id();
        let lod_patch = world
            .spawn((
                TerrainCoverage {
                    grid: IVec2::ZERO,
                    quadrant: 0,
                    tier: Some(LodTier::Tier4),
                },
                TerrainSurfaceReady,
                Visibility::Hidden,
            ))
            .id();

        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(lod_patch).unwrap(),
            Visibility::Hidden
        );
        app.world_mut()
            .entity_mut(full_patch)
            .remove::<TerrainSurfaceReady>();
        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(lod_patch).unwrap(),
            Visibility::Inherited,
            "readiness removal event restores LOD coverage"
        );
        assert_eq!(
            app.world()
                .resource::<StreamingMetrics>()
                .visible_lod_terrain_patches,
            1
        );
    }

    #[test]
    fn lod_grid_rebase_and_unload_bounds_handle_negative_anchors() {
        let origin = LodOrigin::new(8, -8);
        let key = ChunkKey::new(0x3c, LodTier::Tier4, shared::lod::ChunkAnchor::new(-1, 2));
        assert_eq!(chunk_min_grid(key, origin), (4, 0));
        assert!(chunk_contains_cell(key, origin, IVec2::new(7, 3)));
        assert!(!chunk_contains_cell(key, origin, IVec2::new(8, 3)));
        assert!(chunk_within_radius(key, origin, (3, 1), 1));
        assert!(!chunk_within_radius(key, origin, (10, 1), 1));
    }

    #[test]
    fn two_cell_handoff_is_independent_and_falls_back_after_a_tier_failure() {
        let mut world = World::new();
        // The tier boundaries below are written for each tier reaching its chunk side.
        world.insert_resource(EngineConfig {
            terrain_lod: side_reach(),
            ..EngineConfig::default()
        });
        world.insert_resource(RenderOrigin(IVec2::ZERO));
        world.insert_resource(StreamingMetrics::default());
        world.insert_resource(ProfilingState::default());
        world.insert_resource(LodStreaming::default());
        let camera = world
            .spawn((
                StreamingCamera,
                Transform::from_xyz(CELL_SIZE * 0.5, 0.0, -CELL_SIZE * 0.5),
            ))
            .id();

        let full_patches: Vec<_> = (0..4)
            .map(|quadrant| {
                world
                    .spawn((
                        super::super::TerrainPatch,
                        TerrainCoverage {
                            grid: IVec2::ZERO,
                            quadrant,
                            tier: None,
                        },
                        TerrainSurfaceReady,
                        Visibility::Inherited,
                    ))
                    .id()
            })
            .collect();
        let mut lod_patches = HashMap::new();
        for quadrant in 0..4 {
            for tier in LodTier::ALL {
                let zero = world
                    .spawn((
                        TerrainCoverage {
                            grid: IVec2::ZERO,
                            quadrant,
                            tier: Some(tier),
                        },
                        TerrainSurfaceReady,
                        Visibility::Hidden,
                    ))
                    .id();
                let three = world
                    .spawn((
                        TerrainCoverage {
                            grid: IVec2::new(3, 0),
                            quadrant,
                            tier: Some(tier),
                        },
                        TerrainSurfaceReady,
                        Visibility::Hidden,
                    ))
                    .id();
                lod_patches.insert((IVec2::ZERO, tier, quadrant), zero);
                lod_patches.insert((IVec2::new(3, 0), tier, quadrant), three);
            }
        }

        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert!(
            full_patches.iter().all(|entity| {
                *world.get::<Visibility>(*entity).unwrap() == Visibility::Inherited
            })
        );
        for quadrant in 0..4 {
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier4, quadrant)])
                    .unwrap(),
                Visibility::Hidden,
                "full-detail cell zero owns quadrant {quadrant}"
            );
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier4, quadrant)])
                    .unwrap(),
                Visibility::Inherited,
                "cell three independently selects tier 4 for quadrant {quadrant}"
            );
        }

        world
            .entity_mut(full_patches[1])
            .remove::<TerrainSurfaceReady>();
        for quadrant in 0..4 {
            world
                .entity_mut(lod_patches[&(IVec2::new(3, 0), LodTier::Tier4, quadrant)])
                .remove::<TerrainSurfaceReady>();
        }
        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier4, 1)])
                .unwrap(),
            Visibility::Inherited,
            "a single unready full-detail quadrant transfers to LOD"
        );
        for quadrant in 0..4 {
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier8, quadrant)])
                    .unwrap(),
                Visibility::Inherited,
                "ready tier 8 covers failed tier 4 in quadrant {quadrant}"
            );
        }

        world.get_mut::<Transform>(camera).unwrap().translation.x = CELL_SIZE * 10.5;
        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier16, 1)])
                .unwrap(),
            Visibility::Inherited,
            "teleporting outward selects tier 16 for cell zero"
        );
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier8, 0)])
                .unwrap(),
            Visibility::Inherited,
            "cell three retains its independent tier selection"
        );

        let visible_before_removal = world
            .resource::<StreamingMetrics>()
            .visible_lod_terrain_patches;
        world
            .entity_mut(lod_patches[&(IVec2::ZERO, LodTier::Tier16, 1)])
            .despawn();
        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert_eq!(
            world
                .resource::<StreamingMetrics>()
                .visible_lod_terrain_patches,
            visible_before_removal - 1,
            "removing a selected patch updates visibility metrics"
        );

        let current = LodStreaming {
            generation: 2,
            center: Some(IVec2::new(10, 0)),
            ..default()
        };
        assert!(
            !is_current_query(&current, 1),
            "pre-teleport results are stale"
        );
        assert!(is_current_query(&current, 2));

        let fine_chunk = ChunkKey::new(0x3c, LodTier::Tier4, shared::lod::ChunkAnchor::new(0, 0));
        assert!(!chunk_within_radius(
            fine_chunk,
            LodOrigin::new(0, 0),
            (22, 0),
            side_reach().max_reach_cells() + LOD_UNLOAD_MARGIN_CELLS,
        ));
    }
}
