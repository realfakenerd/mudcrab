use crate::{
    config::EngineConfig,
    metrics::AcceptanceMetricsPlugin,
    physics::{MovementTuning, PhysicsFixturePlugin, WorldPlayerPlugin},
    profiling::{ProfilingPlugin, ProfilingState},
    render::{
        LIGHT_LAYERS, MAIN_VIEW_LAYERS, RendererMetrics, TerrainExtension, TerrainMaterial,
        VercidiumRendererPlugin, WATER_LAYER, WaterExtension, WaterMaterial,
        WaterReflectionTexture, terrain_layer_sampler,
    },
    sky::{FogCamera, SkyCamera, SkyPlugin},
    streaming::{
        AssetFailure, RenderOrigin, StreamingMetrics, StreamingPlugin, StreamingWorld,
        build_terrain_quadrant_mesh, streaming_center, validate_standard_material,
    },
    world::{
        cache::{CellCache, TerrainLayerSnapshot, TerrainSnapshot},
        components::{
            CellRef, ExpectedModelBounds, FormId, InstanceBounds, StreamedCellRoot, StreamingCamera,
        },
        database::{
            AssetCatalog, CellKey, MAX_RUNTIME_DATABASE_SCHEMA_VERSION,
            MIN_RUNTIME_DATABASE_SCHEMA_VERSION, WorldDatabase, supports_runtime_database_schema,
        },
    },
};
use bevy::{
    asset::{AssetPlugin, RenderAssetUsages},
    camera::primitives::MeshAabb,
    camera::visibility::RenderLayers,
    core_pipeline::prepass::DepthPrepass,
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    light::{CascadeShadowConfig, CascadeShadowConfigBuilder, DirectionalLightShadowMap},
    log::{Level, LogPlugin},
    prelude::*,
    render::diagnostic::RenderDiagnosticsPlugin,
    render::occlusion_culling::OcclusionCulling,
    render::render_asset::RenderAssetBytesPerFrame,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    render::view::screenshot::{Screenshot, save_to_disk},
    tasks::{IoTaskPool, TaskPoolBuilder},
    window::{MonitorSelection, PresentMode, WindowPlugin, WindowPosition},
    winit::WinitSettings,
};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Resource)]
struct InitialCameraGroundHeight(f32);

pub fn run(mut config: EngineConfig) -> Result<()> {
    validate_fixture_selection(&config)?;
    configure_io_task_pool();
    let interactive_world_physics = config.interactive_world_physics();
    let streaming_fixture_dir = if config.streaming_fixture {
        let fixture = StreamingFixtureDirectory::create(config.worldspace_id, config.start_grid)?;
        config.assets_dir = fixture.path.clone();
        Some(fixture)
    } else {
        None
    };
    let runtime_data = if config.streaming_fixture {
        let database_path = config.assets_dir.join("skyrim_world.db");
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?,
            InitialCameraGroundHeight(0.0),
        ))
    } else if config.benchmark_only
        || config.material_fixture
        || config.terrain_water_fixture
        || config.transform_bounds_fixture
        || config.renderer_fixture
        || config.physics_fixture
    {
        None
    } else {
        validate_runtime_assets(&config)?;
        let database_path = config.assets_dir.join("skyrim_world.db");
        let cache = CellCache::open(&config.assets_dir.join("cell_cache.rkyv"))?;
        let ground_height = initial_camera_ground_height(&config, &database_path, &cache)?;
        Some((
            WorldDatabase::open(&database_path)?,
            AssetCatalog::open(&database_path)?,
            cache,
            InitialCameraGroundHeight(ground_height),
        ))
    };
    let movement_tuning = (interactive_world_physics
        && runtime_data.is_some()
        && !config.streaming_fixture)
        .then(|| MovementTuning::from_world_database(&config.assets_dir.join("skyrim_world.db")))
        .transpose()?;
    let asset_path = config.assets_dir.to_string_lossy().into_owned();
    let benchmark_active =
        config.benchmark_frames.is_some() || config.benchmark_duration_secs.is_some();
    configure_benchmark_priority(benchmark_active)?;
    let window = (!config.headless).then(|| Window {
        title: config.window_title(),
        resolution: (1600, 900).into(),
        // A timing run opens on screen, in the middle, so whoever is at the machine can see what
        // is measuring and not disturb it.
        position: if benchmark_active {
            WindowPosition::Centered(MonitorSelection::Primary)
        } else {
            WindowPosition::Automatic
        },
        present_mode: if benchmark_active {
            PresentMode::AutoNoVsync
        } else {
            PresentMode::AutoVsync
        },
        ..default()
    });
    let origin = RenderOrigin(IVec2::new(config.start_grid.0, config.start_grid.1));
    let mut app = App::new();
    if let Some(tuning) = movement_tuning {
        app.insert_resource(tuning);
    }
    if benchmark_active {
        // Acceptance runs are commonly left unfocused while the campaign driver
        // advances through its scenarios. Bevy's game default throttles an
        // unfocused window to 60 Hz, which makes a 16.67 ms P95 gate measure the
        // event-loop sleep instead of renderer performance.
        app.insert_resource(WinitSettings::continuous());
    }
    let render_asset_budget = upload_budget(&config);
    app.insert_resource(config)
        .insert_resource(origin)
        .insert_resource(render_asset_budget)
        .init_resource::<StreamingMetrics>()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin {
                    file_path: asset_path,
                    ..default()
                })
                .set(WindowPlugin {
                    primary_window: window,
                    ..default()
                })
                .set(LogPlugin {
                    level: Level::INFO,
                    ..default()
                }),
        )
        .add_plugins((
            FrameTimeDiagnosticsPlugin::default(),
            LogDiagnosticsPlugin {
                debug: true,
                ..default()
            },
            AcceptanceMetricsPlugin,
            ProfilingPlugin,
            RenderDiagnosticsPlugin,
        ))
        .add_plugins((VercidiumRendererPlugin, SkyPlugin))
        // Registered for every run, lights or not: the plugin owns the budget, not the spawning,
        // and `--lights` is what `streaming::spawn_cell` reads to place anything for it to budget.
        .add_plugins(crate::lights::LightsPlugin)
        .add_systems(Update, (fly_camera, capture_acceptance_screenshot));
    if let Some((database, catalog, cache, ground_height)) = runtime_data {
        app.insert_resource(database)
            .insert_resource(catalog)
            .insert_resource(cache)
            .insert_resource(ground_height)
            .add_plugins(StreamingPlugin);
        app.add_systems(Startup, setup_world);
        if interactive_world_physics {
            app.add_plugins((WorldPlayerPlugin, crate::door_crossing::DoorCrossingPlugin));
        }
        if app.world().resource::<EngineConfig>().streaming_fixture {
            app.init_resource::<StreamingFixtureState>()
                .add_systems(Startup, setup_streaming_fixture_visual)
                .add_systems(
                    PreUpdate,
                    (drive_streaming_fixture, cross_streaming_fixture_interior).chain(),
                )
                .add_systems(PostUpdate, validate_streaming_fixture);
        }
    } else if app.world().resource::<EngineConfig>().material_fixture {
        app.add_systems(Startup, setup_material_fixture)
            .add_systems(Update, validate_material_fixture);
    } else if app.world().resource::<EngineConfig>().terrain_water_fixture {
        app.add_systems(PostStartup, setup_terrain_water_fixture)
            .add_systems(Update, validate_terrain_water_fixture);
    } else if app
        .world()
        .resource::<EngineConfig>()
        .transform_bounds_fixture
    {
        app.add_systems(Startup, setup_transform_bounds_fixture)
            .add_systems(Update, validate_transform_bounds_fixture);
    } else if app.world().resource::<EngineConfig>().renderer_fixture {
        app.add_systems(Startup, setup_renderer_fixture)
            .add_systems(Update, validate_renderer_fixture);
    } else if app.world().resource::<EngineConfig>().physics_fixture {
        app.add_plugins(PhysicsFixturePlugin);
    } else {
        app.add_systems(Startup, setup_world);
        app.add_systems(Startup, setup_synthetic_benchmark);
    }
    app.run();
    drop(app);
    drop(streaming_fixture_dir);
    Ok(())
}

/// Bevy's per-frame render-asset byte budget, seeded from the run's option.
///
/// Textures and meshes over the budget wait for a later frame instead of being
/// prepared the moment they load, so a cell's new models arrive over a few
/// frames rather than in one upload burst. Deferred assets are never dropped,
/// and a single asset larger than the whole budget is still prepared. Images
/// are prepared before meshes and share the one budget, so while new images
/// use it up, new meshes wait for a later frame.
fn upload_budget(config: &EngineConfig) -> RenderAssetBytesPerFrame {
    RenderAssetBytesPerFrame {
        max_bytes: config.max_upload_bytes_per_frame(),
    }
}

fn validate_fixture_selection(config: &EngineConfig) -> Result<()> {
    let selected = [
        config.material_fixture,
        config.terrain_water_fixture,
        config.transform_bounds_fixture,
        config.renderer_fixture,
        config.streaming_fixture,
        config.physics_fixture,
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count();
    color_eyre::eyre::ensure!(selected <= 1, "select only one fixture mode");
    Ok(())
}

#[cfg(windows)]
fn configure_benchmark_priority(benchmark_active: bool) -> Result<()> {
    if benchmark_active {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle,
        // which is valid for SetPriorityClass and must not be closed.
        let configured =
            unsafe { SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) };
        if configured == 0 {
            return Err(std::io::Error::last_os_error())
                .wrap_err("failed to set benchmark process priority");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn configure_benchmark_priority(_benchmark_active: bool) -> Result<()> {
    Ok(())
}

/// The stack each IO task pool thread reserves.
///
/// Asset loads nest on these stacks. bevy_asset runs every load as a task on the IO pool, and
/// bevy_gltf's loader loads a file's textures inside `IoTaskPool::scope`, whose `block_on` ticks
/// the pool's shared executor on the calling thread while it waits. So a glTF load waiting for its
/// textures picks up the next queued glTF load and runs it on the same stack, that one does the
/// same, and so on: the nesting is as deep as the queue of model loads. Measured while
/// streaming Markarth's dense city interiors: each nested load costs about 85 KiB, and one thread
/// reached 8,074 KiB (about 95 loads deep) and overflowed the 8 MiB this used to be. A dense cell
/// queues hundreds of distinct models at once (the largest interior has 522), plus its
/// neighbours, so the reservation has to cover the whole queue, not a typical load.
///
/// 128 MiB is room for about 1,500 nested loads. It is address space, not memory: the thread's stack is
/// reserved, and pages are committed only as deep as the thread reaches.
const IO_TASK_STACK_BYTES: usize = 128 * 1024 * 1024;

fn io_task_pool_builder(threads: usize) -> TaskPoolBuilder {
    TaskPoolBuilder::new()
        .num_threads(threads)
        .thread_name("IO Task Pool".to_owned())
        .stack_size(IO_TASK_STACK_BYTES)
}

fn configure_io_task_pool() {
    let threads = std::thread::available_parallelism()
        .map(|count| count.get().div_ceil(4).clamp(1, 4))
        .unwrap_or(1);
    IoTaskPool::get_or_init(|| io_task_pool_builder(threads).build());
}

/// The interior cell the streaming fixture loads by id. An interior carries no grid square and
/// belongs to no worldspace, and its id sits past the block the exterior cells are numbered in.
const STREAMING_FIXTURE_INTERIOR_CELL_ID: u32 = 0x0001_0000;
/// The reference placed inside that interior cell. It has no model, so the fixture still needs no
/// converted assets; the crossing is observed through the root and this reference.
const STREAMING_FIXTURE_INTERIOR_REFERENCE_ID: u32 = STREAMING_FIXTURE_INTERIOR_CELL_ID + 1;
/// The frame the fixture loads the interior on, after the first teleport has moved the camera
/// several cells away from the grid it starts on.
const STREAMING_FIXTURE_INTERIOR_FRAME: u32 = 8;

struct StreamingFixtureDirectory {
    path: PathBuf,
}

impl StreamingFixtureDirectory {
    fn create(worldspace_id: u32, start_grid: (i32, i32)) -> Result<Self> {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "openskyrim-streaming-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir(&path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        let fixture = Self { path };
        fixture.populate(worldspace_id, start_grid)?;
        Ok(fixture)
    }

    fn populate(&self, worldspace_id: u32, start_grid: (i32, i32)) -> Result<()> {
        let database_path = self.path.join("skyrim_world.db");
        let connection = Connection::open(&database_path)?;
        connection.execute_batch(
            r#"CREATE TABLE schema_info(version INTEGER NOT NULL);
            INSERT INTO schema_info VALUES(4);
            CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER,interior_name TEXT);
            CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
            CREATE TABLE statics(id INTEGER PRIMARY KEY,model_path TEXT,bounds_min_x REAL,bounds_min_y REAL,bounds_min_z REAL,bounds_max_x REAL,bounds_max_y REAL,bounds_max_z REAL,bounds_valid INTEGER NOT NULL);
            CREATE TABLE "references"(id INTEGER PRIMARY KEY,cell_id INTEGER,base_form_id INTEGER,pos_x REAL,pos_y REAL,pos_z REAL,rot_x REAL,rot_y REAL,rot_z REAL,scale REAL);
            CREATE VIRTUAL TABLE exterior_spatial USING rtree(id,minX,maxX,minY,maxY,minZ,maxZ,+cell_id,+worldspace_id);
            CREATE TABLE texture_sets(id INTEGER PRIMARY KEY,diffuse_path TEXT);
            CREATE TABLE landscape_textures(id INTEGER PRIMARY KEY,texture_set_id INTEGER);
            CREATE TABLE waters(id INTEGER PRIMARY KEY,flow_normal_path TEXT);"#,
        )?;
        let mut insert = connection
            .prepare("INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(?1,?2,?3,?4)")?;
        let mut cell_id = 1u32;
        for grid_y in start_grid.1.saturating_sub(48)..=start_grid.1.saturating_add(48) {
            for grid_x in start_grid.0.saturating_sub(48)..=start_grid.0.saturating_add(48) {
                insert.execute(params![cell_id, worldspace_id, grid_x, grid_y])?;
                cell_id += 1;
            }
        }
        drop(insert);
        connection.execute(
            "INSERT INTO cells(id,worldspace_id,grid_x,grid_y,interior_name) VALUES(?1,NULL,NULL,NULL,'Fixture Hall')",
            params![STREAMING_FIXTURE_INTERIOR_CELL_ID],
        )?;
        connection.execute(
            "INSERT INTO \"references\"(id,cell_id,base_form_id,pos_x,pos_y,pos_z,rot_x,rot_y,rot_z,scale)
             VALUES(?1,?2,?3,256.0,0.0,192.0,0.0,0.0,0.0,1.0)",
            params![
                STREAMING_FIXTURE_INTERIOR_REFERENCE_ID,
                STREAMING_FIXTURE_INTERIOR_CELL_ID,
                STREAMING_FIXTURE_INTERIOR_REFERENCE_ID
            ],
        )?;
        drop(connection);
        let cache = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: Vec::new(),
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache)
            .wrap_err("failed to archive streaming fixture cache")?;
        fs::write(self.path.join("cell_cache.rkyv"), bytes)?;
        Ok(())
    }
}

impl Drop for StreamingFixtureDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            warn!(%error, path = %self.path.display(), "failed to remove streaming fixture directory");
        }
    }
}

#[derive(Resource, Default)]
struct StreamingFixtureState {
    frames: u32,
    total_x: i32,
    total_y: i32,
    finished: bool,
    interior: InteriorCrossing,
    /// The exterior grid the camera stood on when the interior was loaded: the place it has to
    /// leave for the interior to be observed from far outside the unload radius.
    interior_center: IVec2,
}

/// What the fixture observed of the exterior/interior crossing. The interior is loaded by id the
/// way a door crossing will load one; the camera then carries on over exteriors far outside the
/// unload radius and comes back. The contract is that the interior never exists twice and its
/// references match its root. Today it also stays loaded throughout (an interior has no grid
/// square, so [`cell_within_unload_radius`](crate::streaming) keeps it, and only a space switch
/// unloads one); that is current behaviour, not part of the contract.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct InteriorCrossing {
    requested_frame: Option<u32>,
    resident_frame: Option<u32>,
    /// Frames on which the camera stood more than `unload_radius` cells from the grid the interior
    /// was loaded from, and the interior root counts seen on them.
    away_samples: u32,
    min_away_roots: usize,
    max_away_roots: usize,
}

impl InteriorCrossing {
    fn observe(&mut self, away: bool, roots: usize) {
        if !away {
            return;
        }
        self.away_samples = self.away_samples.saturating_add(1);
        if self.away_samples == 1 {
            self.min_away_roots = roots;
            self.max_away_roots = roots;
        } else {
            self.min_away_roots = self.min_away_roots.min(roots);
            self.max_away_roots = self.max_away_roots.max(roots);
        }
    }
}

/// The crossing contract, as the fixture's own observations and the final root and reference
/// counts express it: the interior was loaded, the camera was observed far away from it, it never
/// had two roots, and its references match its root (one each, or none if it was unloaded).
/// Orphaned and missing roots are the lifecycle validator's, which the run also requires at zero.
fn interior_crossing_valid(crossing: &InteriorCrossing, roots: usize, references: usize) -> bool {
    crossing.requested_frame.is_some()
        && crossing.resident_frame.is_some()
        && crossing.away_samples > 0
        && crossing.max_away_roots <= 1
        && roots <= 1
        && references == roots
}

fn setup_streaming_fixture_visual(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<TerrainMaterial>>,
) {
    let mesh = Mesh3d(meshes.add(Cuboid::new(180.0, 480.0, 180.0)));
    let material = MeshMaterial3d(materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.22, 0.48, 0.18),
            perceptual_roughness: 0.88,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    commands.spawn_batch((0..64).map(move |index| {
        let x = index % 8;
        let z = index / 8;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(700.0 + x as f32 * 360.0, 240.0, -700.0 - z as f32 * 360.0),
        )
    }));
}

fn drive_streaming_fixture(
    mut state: ResMut<StreamingFixtureState>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let Some((x, y, label)) = (match state.frames {
        4 => Some((6, 0, "rapid_traversal")),
        5 => Some((0, -7, "rapid_traversal")),
        6 => Some((18, 12, "teleport")),
        14 => Some((-30, -9, "teleport")),
        22 => Some((9, 5, "rapid_traversal")),
        30 => Some((-state.total_x, -state.total_y, "return_to_origin")),
        _ => None,
    }) else {
        return;
    };
    let Ok(mut camera) = camera.single_mut() else {
        return;
    };
    camera.translation.x += x as f32 * crate::world::components::CELL_SIZE;
    camera.translation.z -= y as f32 * crate::world::components::CELL_SIZE;
    state.total_x += x;
    state.total_y += y;
    profiler.event("streaming-fixture", label, None);
}

/// Loads the fixture's interior by id, through the loader path the camera planner uses, and
/// watches it while the camera keeps crossing exteriors around it.
#[allow(clippy::too_many_arguments)]
fn cross_streaming_fixture_interior(
    config: Res<EngineConfig>,
    mut state: ResMut<StreamingFixtureState>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    mut streaming: ResMut<StreamingWorld>,
    mut metrics: ResMut<StreamingMetrics>,
    camera: Query<&Transform, With<StreamingCamera>>,
    roots: Query<&CellRef, With<StreamedCellRoot>>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    let Ok(camera) = camera.single() else {
        return;
    };
    let center = streaming_center(camera.translation, origin.0);
    if state.interior.requested_frame.is_none() {
        if state.frames < STREAMING_FIXTURE_INTERIOR_FRAME {
            return;
        }
        streaming.request_cell(
            &database,
            CellKey::Interior(STREAMING_FIXTURE_INTERIOR_CELL_ID),
            &mut metrics,
            &mut profiler,
        );
        state.interior.requested_frame = Some(state.frames);
        state.interior_center = center;
        profiler.event("streaming-fixture", "interior_requested", None);
        return;
    }
    let roots = roots
        .iter()
        .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
        .count();
    // Only a loaded interior can be unloaded against the rule: the frames between the request and
    // the commit have no root to count yet.
    if state.interior.resident_frame.is_none() {
        if roots > 0 {
            state.interior.resident_frame = Some(state.frames);
            profiler.event("streaming-fixture", "interior_resident", None);
        }
        return;
    }
    let away = (center.x - state.interior_center.x).abs() > config.unload_radius
        || (center.y - state.interior_center.y).abs() > config.unload_radius;
    state.interior.observe(away, roots);
}

fn validate_streaming_fixture(
    config: Res<EngineConfig>,
    mut state: ResMut<StreamingFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    roots: Query<&CellRef, With<StreamedCellRoot>>,
    references: Query<&FormId>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || state.frames < 90 {
        return;
    }
    let expected_resident = ((config.stream_radius * 2 + 1).max(0) as usize).pow(2);
    let maximum_resident = ((config.unload_radius * 2 + 1).max(0) as usize).pow(2);
    let interior_roots = roots
        .iter()
        .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
        .count();
    let interior_references = references
        .iter()
        .filter(|form_id| form_id.0 == STREAMING_FIXTURE_INTERIOR_REFERENCE_ID)
        .count();
    let settled = metrics.active_requests == 0 && metrics.loading_cells == 0;
    let valid = settled
        && metrics.requests_submitted > expected_resident as u64
        && metrics.responses_received > 0
        && metrics.stale_responses > 0
        && metrics.unloaded_cells > 0
        && metrics.origin_rebases >= 6
        && metrics.resident_cells >= expected_resident
        && metrics.resident_cells <= maximum_resident
        && metrics.resident_roots == metrics.resident_cells
        && metrics.out_of_range_cell_roots == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.commit_frames > 0
        && interior_crossing_valid(&state.interior, interior_roots, interior_references);
    if valid {
        metrics.streaming_fixture_validated = true;
        profiler.event("streaming-fixture", "validated", None);
        state.finished = true;
    } else if state.frames >= 300 {
        metrics.streaming_fixture_failures = metrics.streaming_fixture_failures.saturating_add(1);
        error!(
            ?metrics,
            "streaming fixture did not settle or violated its lifecycle contract"
        );
        profiler.event("streaming-fixture", "failed", None);
        state.finished = true;
    }
}

#[derive(Component, Debug, Clone, Copy)]
enum CanonicalMaterialKind {
    Opaque,
    Cutout,
    Blend,
    Emissive,
    DoubleSided,
    NormalMapped,
}

#[derive(Resource, Default)]
struct CanonicalMaterialFixtureState {
    finished: bool,
}

fn fixture_image(data: Vec<u8>, srgb: bool) -> Image {
    let mut image = Image::new(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        if srgb {
            TextureFormat::Rgba8UnormSrgb
        } else {
            TextureFormat::Rgba8Unorm
        },
        RenderAssetUsages::default(),
    );
    image.sampler = bevy::image::ImageSampler::linear();
    image
}

/// A terrain layer image of the synthetic fixture: one flat colour, carried by a sampler that
/// repeats like the one a streamed layer gets, since the shader tiles every layer across a
/// cell. Bevy's default sampler clamps to the edge, which stretches the outermost texels over the
/// rest of the tiles.
fn terrain_fixture_image(pixel: [u8; 4]) -> Image {
    let mut image = fixture_image((0..16).flat_map(|_| pixel).collect(), true);
    image.sampler = bevy::image::ImageSampler::Descriptor(terrain_layer_sampler());
    image
}

fn setup_material_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<CanonicalMaterialFixtureState>();
    let checker = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                let alpha = if (index + index / 4) % 2 == 0 { 255 } else { 0 };
                [78, 166, 88, alpha]
            })
            .collect(),
        true,
    ));
    let normal = images.add(fixture_image(
        (0..16).flat_map(|_| [128, 128, 255, 255]).collect(),
        false,
    ));
    let definitions = [
        (
            CanonicalMaterialKind::Opaque,
            StandardMaterial {
                base_color: Color::srgb(0.55, 0.42, 0.25),
                perceptual_roughness: 0.75,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Cutout,
            StandardMaterial {
                base_color_texture: Some(checker),
                alpha_mode: AlphaMode::Mask(0.5),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Blend,
            StandardMaterial {
                base_color: Color::srgba(0.15, 0.45, 0.9, 0.45),
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::Emissive,
            StandardMaterial {
                base_color: Color::srgb(0.08, 0.08, 0.08),
                emissive: LinearRgba::new(6.0, 1.2, 0.15, 1.0),
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::DoubleSided,
            StandardMaterial {
                base_color: Color::srgb(0.75, 0.2, 0.18),
                double_sided: true,
                cull_mode: None,
                ..default()
            },
        ),
        (
            CanonicalMaterialKind::NormalMapped,
            StandardMaterial {
                base_color: Color::srgb(0.45, 0.48, 0.52),
                normal_map_texture: Some(normal),
                ..default()
            },
        ),
    ];
    let mesh = meshes.add(Cuboid::new(2.2, 2.2, 2.2));
    for (index, (kind, material)) in definitions.into_iter().enumerate() {
        commands.spawn((
            Name::new(format!("Canonical {kind:?}")),
            kind,
            Mesh3d(mesh.clone()),
            MeshMaterial3d(materials.add(material)),
            Transform::from_xyz((index as f32 - 2.5) * 2.8, 0.0, 0.0),
        ));
    }
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 5.0, 18.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 120.0,
        ..default()
    });
}

fn validate_material_fixture(
    query: Query<(&CanonicalMaterialKind, &MeshMaterial3d<StandardMaterial>)>,
    materials: Res<Assets<StandardMaterial>>,
    images: Res<Assets<Image>>,
    mut state: ResMut<CanonicalMaterialFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished || query.iter().count() != 6 {
        return;
    }
    let mut validated_images = 0usize;
    for (kind, handle) in &query {
        let result = materials
            .get(handle)
            .ok_or_else(|| "material is not loaded".to_owned())
            .and_then(|material| {
                match kind {
                    CanonicalMaterialKind::Opaque if material.alpha_mode != AlphaMode::Opaque => {
                        Err("opaque mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Cutout
                        if !matches!(material.alpha_mode, AlphaMode::Mask(_)) =>
                    {
                        Err("mask mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Blend if material.alpha_mode != AlphaMode::Blend => {
                        Err("blend mode was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::Emissive if material.emissive.red <= 0.0 => {
                        Err("emissive intensity was lost".to_owned())
                    }
                    CanonicalMaterialKind::DoubleSided
                        if !material.double_sided || material.cull_mode.is_some() =>
                    {
                        Err("double-sided culling was not preserved".to_owned())
                    }
                    CanonicalMaterialKind::NormalMapped
                        if material.normal_map_texture.is_none() =>
                    {
                        Err("normal map was not preserved".to_owned())
                    }
                    _ => Ok(()),
                }?;
                validate_standard_material(material, &images)
            });
        match result {
            Ok(count) => validated_images += count,
            Err(reason) => {
                metrics.asset_load_failures += 1;
                metrics.material_validation_failures += 1;
                metrics.asset_failures.push(AssetFailure {
                    model_path: format!("canonical-material-fixture/{kind:?}"),
                    reference_form_id: 0,
                    base_form_id: 0,
                    cell_id: 0,
                    dependency_chain: vec![reason],
                });
                profiler.increment("assets/load_failures", 1);
            }
        }
    }
    metrics.materials_validated += 6;
    metrics.images_validated += validated_images as u64;
    metrics.canonical_fixture_validated = metrics.material_validation_failures == 0;
    state.finished = true;
}

#[derive(Component)]
struct TerrainWaterFixtureTerrain;

#[derive(Component)]
struct TerrainWaterFixtureWater;

#[derive(Resource, Default)]
struct TerrainWaterFixtureState {
    finished: bool,
}

/// The synthetic LAND snapshot the terrain/water fixture draws: a rolling height field whose four
/// quadrants each carry a full base-and-five-overlays stack, every overlay strongest around its own
/// centre so the weight field is visible in the scene. Built without game data, like every fixture.
fn terrain_water_fixture_snapshot() -> TerrainSnapshot {
    let mut layers = Vec::new();
    for quadrant in 0..4 {
        layers.push(TerrainLayerSnapshot {
            texture_form_id: 1,
            quadrant,
            layer: 0,
            is_base: true,
            weights: Vec::new(),
        });
        for layer in 1..=5u16 {
            let weights = (0usize..17 * 17)
                .filter_map(|vertex| {
                    let x = vertex % 17;
                    let y = vertex / 17;
                    let center = (layer as usize * 3).min(16);
                    let distance = x.abs_diff(center).min(y.abs_diff(center));
                    (distance < 3).then(|| (vertex as u16, (3 - distance) as f32 * 0.12))
                })
                .collect();
            layers.push(TerrainLayerSnapshot {
                texture_form_id: u32::from(layer) + 1,
                quadrant,
                layer,
                is_base: false,
                weights,
            });
        }
    }
    TerrainSnapshot {
        cell_id: 0xF170_0001,
        width: 33,
        height: 33,
        heights: (0..33 * 33)
            .map(|index| {
                let x = (index % 33) as f32 - 16.0;
                let y = (index / 33) as f32 - 16.0;
                45.0 * (x * 0.22).sin() + 35.0 * (y * 0.18).cos()
            })
            .collect(),
        normals: (0..33 * 33).flat_map(|_| [0, 0, 127]).collect(),
        vertex_colors: (0..33 * 33)
            .flat_map(|index| {
                let shade = 190 + (index % 33) as u8;
                [shade, shade, shade]
            })
            .collect(),
        layers,
        water_height: Some(12.0),
        water_type_form_id: Some(1),
    }
}

fn setup_terrain_water_fixture(
    mut commands: Commands,
    reflection: Res<WaterReflectionTexture>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut water_materials: ResMut<Assets<WaterMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    commands.init_resource::<TerrainWaterFixtureState>();
    let palette = [
        [82, 116, 58, 255],
        [122, 101, 70, 255],
        [83, 92, 102, 255],
        [146, 138, 103, 255],
        [60, 91, 54, 255],
        [113, 82, 62, 255],
    ];
    let texture_handles: [Handle<Image>; 6] =
        palette.map(|pixel| images.add(terrain_fixture_image(pixel)));
    let flow_normal = images.add(fixture_image(
        (0..16)
            .flat_map(|index| {
                if index % 2 == 0 {
                    [150, 110, 255, 255]
                } else {
                    [110, 150, 255, 255]
                }
            })
            .collect(),
        false,
    ));
    let terrain = terrain_water_fixture_snapshot();
    for quadrant in 0..4 {
        commands.spawn((
            Name::new(format!("Terrain/water fixture quadrant {quadrant}")),
            Mesh3d(
                meshes.add(
                    build_terrain_quadrant_mesh(&terrain, quadrant)
                        .expect("canonical terrain fixture must build"),
                ),
            ),
            MeshMaterial3d(
                terrain_materials.add(TerrainMaterial {
                    base: StandardMaterial {
                        base_color: Color::WHITE,
                        perceptual_roughness: 0.92,
                        cull_mode: None,
                        double_sided: true,
                        ..default()
                    },
                    extension: TerrainExtension::fixture(
                        &terrain,
                        quadrant,
                        texture_handles.clone(),
                    )
                    .expect("canonical terrain fixture must build"),
                }),
            ),
            TerrainWaterFixtureTerrain,
        ));
    }
    commands.spawn((
        Name::new("Terrain/water fixture water"),
        Mesh3d(meshes.add(Plane3d::default().mesh().size(2200.0, 2200.0))),
        MeshMaterial3d(water_materials.add(WaterMaterial {
            base: StandardMaterial {
                base_color: Color::srgba(0.04, 0.2, 0.32, 0.7),
                metallic: 0.15,
                perceptual_roughness: 0.06,
                reflectance: 0.9,
                alpha_mode: AlphaMode::Blend,
                ..default()
            },
            extension: WaterExtension::with_reflection(reflection.0.clone(), Some(flow_normal)),
        })),
        Transform::from_xyz(CELL_SIZE_HALF, 12.0, -CELL_SIZE_HALF),
        crate::world::components::WaterSurface,
        TerrainWaterFixtureWater,
        RenderLayers::layer(WATER_LAYER),
    ));
    let target = Vec3::new(CELL_SIZE_HALF, 0.0, -CELL_SIZE_HALF);
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(CELL_SIZE_HALF, 1800.0, 2600.0).looking_at(target, Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(MAIN_VIEW_LAYERS),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        brightness: 160.0,
        ..default()
    });
}

fn validate_terrain_water_fixture(
    terrain: Query<(&Mesh3d, &MeshMaterial3d<TerrainMaterial>), With<TerrainWaterFixtureTerrain>>,
    water: Query<&MeshMaterial3d<WaterMaterial>, With<TerrainWaterFixtureWater>>,
    meshes: Res<Assets<Mesh>>,
    terrain_materials: Res<Assets<TerrainMaterial>>,
    water_materials: Res<Assets<WaterMaterial>>,
    mut state: ResMut<TerrainWaterFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
) {
    if state.finished || terrain.iter().count() != 4 || water.iter().count() != 1 {
        return;
    }
    // The fixture exists to show terrain without game data, so it is only valid if its materials
    // render the overlay weight field the streamed path uses - the point of the scene.
    let valid_terrain = terrain.iter().all(|(mesh, handle)| {
        let Some(material) = terrain_materials.get(handle) else {
            return false;
        };
        meshes.get(mesh).is_some() && material.extension.reads_weight_field()
    });
    let valid_water = water
        .single()
        .ok()
        .and_then(|material| water_materials.get(material))
        .is_some();
    if valid_terrain && valid_water {
        metrics.terrain_patches_validated += 4;
        metrics.water_surfaces_validated += 1;
        metrics.materials_validated += 5;
        metrics.images_validated += 7;
        metrics.terrain_water_fixture_validated = true;
    } else {
        metrics.terrain_validation_failures += (!valid_terrain) as u64;
        metrics.water_validation_failures += (!valid_water) as u64;
    }
    state.finished = true;
}

#[derive(Component)]
struct TransformBoundsFixtureRoot;

#[derive(Resource, Default)]
struct TransformBoundsFixtureState {
    frames: u8,
    finished: bool,
}

fn setup_transform_bounds_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<TransformBoundsFixtureState>();
    let beam_mesh = meshes.add(Cuboid::new(2.0, 4.0, 1.5));
    let cube_mesh = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let cap_mesh = meshes.add(Cuboid::new(6.5, 0.7, 1.2));
    let stone = materials.add(StandardMaterial {
        base_color: Color::srgb(0.38, 0.46, 0.58),
        perceptual_roughness: 0.72,
        ..default()
    });
    let bronze = materials.add(StandardMaterial {
        base_color: Color::srgb(0.72, 0.39, 0.12),
        metallic: 0.45,
        perceptual_roughness: 0.42,
        ..default()
    });
    let moss = materials.add(StandardMaterial {
        base_color: Color::srgb(0.22, 0.48, 0.24),
        perceptual_roughness: 0.86,
        ..default()
    });

    let left = Transform::from_xyz(-2.5, 0.0, 0.0)
        .with_rotation(Quat::from_rotation_z(0.28))
        .with_scale(Vec3::new(1.0, 1.35, 0.75));
    let group = Transform::from_xyz(2.0, 0.5, 0.0)
        .with_rotation(Quat::from_rotation_y(-0.42))
        .with_scale(Vec3::new(0.8, 1.3, 0.65));
    let nested = Transform::from_xyz(1.0, 1.0, 0.0)
        .with_rotation(Quat::from_rotation_x(0.31))
        .with_scale(Vec3::new(1.2, 0.5, 1.7));
    let cap = Transform::from_xyz(0.0, 3.8, 0.0)
        .with_rotation(Quat::from_euler(EulerRot::YXZ, 0.18, -0.12, 0.08))
        .with_scale(Vec3::new(1.05, 0.8, 1.25));

    let mut expected_min = Vec3::splat(f32::INFINITY);
    let mut expected_max = Vec3::splat(f32::NEG_INFINITY);
    for bounds in [
        InstanceBounds::transformed(
            Vec3::new(-1.0, -2.0, -0.75),
            Vec3::new(1.0, 2.0, 0.75),
            left.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::splat(-1.0),
            Vec3::splat(1.0),
            group.to_matrix() * nested.to_matrix(),
        ),
        InstanceBounds::transformed(
            Vec3::new(-3.25, -0.35, -0.6),
            Vec3::new(3.25, 0.35, 0.6),
            cap.to_matrix(),
        ),
    ] {
        expected_min = expected_min.min(bounds.min);
        expected_max = expected_max.max(bounds.max);
    }

    commands
        .spawn((
            Name::new("Canonical transform/bounds assembly"),
            TransformBoundsFixtureRoot,
            ExpectedModelBounds {
                min: expected_min,
                max: expected_max,
            },
            Transform::from_xyz(0.0, -1.0, 0.0)
                .with_rotation(Quat::from_rotation_y(0.48))
                .with_scale(Vec3::new(1.1, 0.9, 1.2)),
            Visibility::default(),
        ))
        .with_children(|parent| {
            parent.spawn((
                Name::new("Rotated left support"),
                Mesh3d(beam_mesh),
                MeshMaterial3d(stone),
                left,
            ));
            parent
                .spawn((
                    Name::new("Non-uniform hierarchy pivot"),
                    group,
                    Visibility::default(),
                ))
                .with_child((
                    Name::new("Nested rotated support"),
                    Mesh3d(cube_mesh),
                    MeshMaterial3d(bronze),
                    nested,
                ));
            parent.spawn((
                Name::new("Rotated top cap"),
                Mesh3d(cap_mesh),
                MeshMaterial3d(moss),
                cap,
            ));
        });
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(2.0, 5.5, 16.0).looking_at(Vec3::new(0.0, 1.0, 0.0), Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.7, -0.55, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 140.0,
        ..default()
    });
}

#[allow(clippy::too_many_arguments)]
fn validate_transform_bounds_fixture(
    roots: Query<
        (Entity, &ExpectedModelBounds, &GlobalTransform),
        With<TransformBoundsFixtureRoot>,
    >,
    children: Query<&Children>,
    nodes: Query<(&Transform, &GlobalTransform, Option<&Mesh3d>)>,
    meshes: Res<Assets<Mesh>>,
    mut state: ResMut<TransformBoundsFixtureState>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    if state.frames < 3 {
        return;
    }
    let result = (|| -> Result<(usize, usize), String> {
        let (root, expected, root_global) = roots
            .single()
            .map_err(|_| "canonical transform fixture root is missing".to_owned())?;
        let root_inverse = root_global.affine().inverse();
        let mut actual_min = Vec3::splat(f32::INFINITY);
        let mut actual_max = Vec3::splat(f32::NEG_INFINITY);
        let mut node_count = 0usize;
        let mut mesh_count = 0usize;
        for descendant in children.iter_descendants(root) {
            let (local, global, mesh) = nodes
                .get(descendant)
                .map_err(|_| format!("fixture node {descendant:?} has no transform"))?;
            if !local.to_matrix().is_finite()
                || !global.to_matrix().is_finite()
                || local.scale.abs().min_element() <= 1.0e-6
            {
                return Err(format!(
                    "fixture node {descendant:?} has an invalid transform"
                ));
            }
            node_count += 1;
            let Some(mesh) = mesh else { continue };
            let aabb = meshes
                .get(mesh)
                .and_then(MeshAabb::compute_aabb)
                .ok_or_else(|| format!("fixture mesh {:?} has no bounds", mesh.id()))?;
            let center = Vec3::from(aabb.center);
            let half = Vec3::from(aabb.half_extents);
            let bounds = InstanceBounds::transformed(
                center - half,
                center + half,
                Mat4::from(root_inverse * global.affine()),
            );
            actual_min = actual_min.min(bounds.min);
            actual_max = actual_max.max(bounds.max);
            mesh_count += 1;
        }
        let error = (actual_min - expected.min)
            .abs()
            .max((actual_max - expected.max).abs())
            .max_element();
        (mesh_count == 3 && error <= 1.0e-4)
            .then_some((node_count, mesh_count))
            .ok_or_else(|| {
                format!(
                    "hierarchy bounds mismatch: expected {:?}..{:?}, actual {:?}..{:?}",
                    expected.min, expected.max, actual_min, actual_max
                )
            })
    })();
    match result {
        Ok((nodes, meshes)) => {
            metrics.transform_instances_validated += 1;
            metrics.transform_nodes_validated += nodes as u64;
            metrics.bounds_validated += meshes as u64;
            metrics.transform_bounds_fixture_validated = true;
            profiler.increment("transforms/fixture_validated", 1);
        }
        Err(reason) => {
            metrics.asset_load_failures += 1;
            metrics.transform_bounds_validation_failures += 1;
            metrics.asset_failures.push(AssetFailure {
                model_path: "fixtures/transform-bounds-assembly".to_owned(),
                reference_form_id: 0,
                base_form_id: 0,
                cell_id: 0,
                dependency_chain: vec![reason],
            });
            profiler.increment("transforms/validation_failures", 1);
        }
    }
    state.finished = true;
}

#[derive(Component)]
struct RendererFixtureCenterVisible;

#[derive(Component)]
struct RendererFixtureRightVisible;

#[derive(Component)]
struct RendererFixtureLeftVisible;

#[derive(Resource, Default)]
struct RendererFixtureState {
    frames: u16,
    phase_started: u16,
    phase: u8,
    center_seen: bool,
    right_seen: bool,
    left_seen: bool,
    finished: bool,
}

fn setup_renderer_fixture(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.init_resource::<RendererFixtureState>();
    let cube = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let wall = meshes.add(Cuboid::new(12.0, 10.0, 1.0));
    let opaque = materials.add(StandardMaterial {
        base_color: Color::srgb(0.28, 0.3, 0.34),
        perceptual_roughness: 0.9,
        ..default()
    });
    let green = materials.add(StandardMaterial {
        base_color: Color::srgb(0.12, 0.8, 0.2),
        ..default()
    });
    let red = materials.add(StandardMaterial {
        base_color: Color::srgb(0.85, 0.08, 0.05),
        ..default()
    });
    let blue = materials.add(StandardMaterial {
        base_color: Color::srgb(0.08, 0.35, 0.9),
        ..default()
    });
    let gold = materials.add(StandardMaterial {
        base_color: Color::srgb(0.9, 0.55, 0.08),
        metallic: 0.25,
        ..default()
    });
    commands.spawn((
        Name::new("Renderer fixture occluder"),
        Mesh3d(wall),
        MeshMaterial3d(opaque),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture front visible"),
        RendererFixtureCenterVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(green),
        Transform::from_xyz(0.0, 0.0, 5.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture fully occluded"),
        Mesh3d(cube.clone()),
        MeshMaterial3d(red),
        Transform::from_xyz(0.0, 0.0, -4.0),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after right turn"),
        RendererFixtureRightVisible,
        Mesh3d(cube.clone()),
        MeshMaterial3d(blue),
        Transform::from_xyz(10.0, 0.0, -2.0)
            .with_rotation(Quat::from_rotation_y(0.45))
            .with_scale(Vec3::new(1.8, 0.7, 1.2)),
    ));
    commands.spawn((
        Name::new("Renderer fixture visible after left turn"),
        RendererFixtureLeftVisible,
        Mesh3d(cube),
        MeshMaterial3d(gold),
        Transform::from_xyz(-10.0, 0.0, -2.0)
            .with_rotation(Quat::from_euler(EulerRot::XYZ, 0.25, -0.5, 0.18))
            .with_scale(Vec3::new(0.65, 2.1, 1.4)),
    ));
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y),
        StreamingCamera,
        FogCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.65, -0.45, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::WHITE,
        brightness: 130.0,
        ..default()
    });
}

fn validate_renderer_fixture(
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    center: Query<&ViewVisibility, With<RendererFixtureCenterVisible>>,
    right: Query<&ViewVisibility, With<RendererFixtureRightVisible>>,
    left: Query<&ViewVisibility, With<RendererFixtureLeftVisible>>,
    mut state: ResMut<RendererFixtureState>,
    mut renderer: ResMut<RendererMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    if state.finished {
        return;
    }
    state.frames = state.frames.saturating_add(1);
    let phase_frames = state.frames.saturating_sub(state.phase_started);
    let center_visible = center.single().is_ok_and(|visibility| visibility.get());
    let right_visible = right.single().is_ok_and(|visibility| visibility.get());
    let left_visible = left.single().is_ok_and(|visibility| visibility.get());
    match state.phase {
        0 if phase_frames >= 10 && renderer.final_path_active() && center_visible => {
            state.center_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 1;
            state.phase_started = state.frames;
        }
        1 if phase_frames >= 8 && right_visible => {
            state.right_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0)
                    .looking_at(Vec3::new(-10.0, 0.0, -2.0), Vec3::Y);
            }
            state.phase = 2;
            state.phase_started = state.frames;
        }
        2 if phase_frames >= 8 && left_visible => {
            state.left_seen = true;
            if let Ok(mut camera) = camera.single_mut() {
                *camera = Transform::from_xyz(0.0, 1.5, 16.0).looking_at(Vec3::ZERO, Vec3::Y);
            }
            state.phase = 3;
            state.phase_started = state.frames;
        }
        3 if phase_frames >= 8 && center_visible && renderer.final_path_active() => {
            renderer.renderer_fixture_validated =
                state.center_seen && state.right_seen && state.left_seen;
            renderer.renderer_validation_failures += (!renderer.renderer_fixture_validated) as u64;
            profiler.increment("renderer/fixture_validated", 1);
            state.finished = true;
        }
        _ if state.frames >= 180 => {
            renderer.renderer_validation_failures =
                renderer.renderer_validation_failures.saturating_add(1);
            profiler.increment("renderer/validation_failures", 1);
            state.finished = true;
        }
        _ => {}
    }
}

#[derive(Deserialize)]
struct RuntimeManifest {
    schema_version: u32,
    complete: bool,
}

#[derive(Deserialize)]
struct RuntimeIntegrationReport {
    schema_version: u32,
    passed: bool,
}

fn validate_runtime_assets(config: &EngineConfig) -> Result<()> {
    for required in ["skyrim_world.db", "cell_cache.rkyv"] {
        color_eyre::eyre::ensure!(
            config.assets_dir.join(required).is_file(),
            "converted asset set is missing {required}: {}",
            config.assets_dir.display()
        );
    }
    if config.allow_incomplete_assets {
        return Ok(());
    }
    let manifest_path = config.assets_dir.join("conversion-manifest.json");
    let manifest: RuntimeManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?,
    )
    .wrap_err("invalid conversion manifest")?;
    if !(MIN_RUNTIME_CONVERTER_SCHEMA_VERSION..=converter_schema_version())
        .contains(&manifest.schema_version)
    {
        let rejection = if manifest.schema_version < MIN_RUNTIME_CONVERTER_SCHEMA_VERSION {
            AssetSetRejection::ConverterSchemaOlder {
                found: manifest.schema_version,
            }
        } else {
            AssetSetRejection::ConverterSchemaNewer {
                found: manifest.schema_version,
            }
        };
        color_eyre::eyre::bail!(
            "{}",
            asset_set_rejection_message(&config.assets_dir, rejection)
        );
    }
    color_eyre::eyre::ensure!(
        manifest.complete,
        "{}",
        asset_set_rejection_message(
            &config.assets_dir,
            AssetSetRejection::IncompleteConversion {
                schema: manifest.schema_version
            }
        )
    );
    let report_path = config.assets_dir.join("integration-report.json");
    let report: RuntimeIntegrationReport = serde_json::from_slice(
        &std::fs::read(&report_path)
            .wrap_err_with(|| format!("failed to read {}", report_path.display()))?,
    )
    .wrap_err("invalid integration report")?;
    if !supports_runtime_database_schema(report.schema_version) {
        color_eyre::eyre::bail!(
            "{}",
            asset_set_rejection_message(
                &config.assets_dir,
                AssetSetRejection::WorldDatabaseSchema {
                    found: report.schema_version,
                }
            )
        );
    }
    color_eyre::eyre::ensure!(
        report.passed,
        "{}",
        asset_set_rejection_message(
            &config.assets_dir,
            AssetSetRejection::IntegrationReportFailed
        )
    );
    Ok(())
}

/// Why the runtime refused a converted asset set.
///
/// Each situation needs a different fix, so each gets its own message instead
/// of one "incomplete or stale" error covering all of them.
#[derive(Clone, Copy, Debug)]
enum AssetSetRejection {
    /// The converter that wrote the set is older than every schema this engine accepts.
    ConverterSchemaOlder { found: u32 },
    /// The converter that wrote the set is newer than every schema this engine accepts.
    ConverterSchemaNewer { found: u32 },
    /// The converter stopped early or skipped inputs (`complete: false`); `schema` is the one the
    /// manifest names.
    IncompleteConversion { schema: u32 },
    /// The integration report names a world database schema outside the accepted range.
    WorldDatabaseSchema { found: u32 },
    /// The integration report ran and reported failures.
    IntegrationReportFailed,
}

/// Names the failed check, what it found, the range it accepts, and the
/// command that fixes it.
fn asset_set_rejection_message(assets_dir: &Path, rejection: AssetSetRejection) -> String {
    let converter_min = MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
    let converter_max = converter_schema_version();
    let database_min = MIN_RUNTIME_DATABASE_SCHEMA_VERSION;
    let database_max = MAX_RUNTIME_DATABASE_SCHEMA_VERSION;
    let manifest = assets_dir.join("conversion-manifest.json");
    let report = assets_dir.join("integration-report.json");
    // The converter's usage string takes the Skyrim Data folder first and the
    // output directory second. The engine knows only the directory it was
    // given, so the Data folder stays a placeholder.
    let reconvert = format!(
        "cargo run --release -p converter -- <Skyrim Data folder> \"{}\"",
        assets_dir.display()
    );
    match rejection {
        AssetSetRejection::ConverterSchemaOlder { found } => format!(
            "converted assets are stale: {} was written by converter schema {found}, but this \
             engine accepts converter schemas {converter_min} through {converter_max}; reconvert \
             with `{reconvert}` (the converter reuses what it can from the previous conversion)",
            manifest.display()
        ),
        AssetSetRejection::ConverterSchemaNewer { found } => format!(
            "converted assets are newer than this engine: {} was written by converter schema \
             {found}, but this engine understands only converter schemas {converter_min} through \
             {converter_max}; update the engine and rebuild it (`cargo build --release -p \
             engine`), or reconvert with a converter at schema {converter_max}",
            manifest.display()
        ),
        AssetSetRejection::IncompleteConversion { schema } => format!(
            "the asset conversion did not finish: {} reports complete=false at converter schema \
             {schema}, so the converter stopped early or skipped inputs; rerun `{reconvert}` (it \
             reuses unchanged work), or start the engine with --allow-incomplete-assets to use \
             what is there",
            manifest.display()
        ),
        AssetSetRejection::WorldDatabaseSchema { found } => format!(
            "the converted assets use an unsupported world database schema: {} reports world \
             database schema {found} is unsupported; this engine accepts world database schemas \
             {database_min} through {database_max}; reconvert with a converter built from the \
             same revision as this engine: `{reconvert}`",
            report.display()
        ),
        AssetSetRejection::IntegrationReportFailed => format!(
            "the asset integration report did not pass: {} reports passed=false; read that report \
             for the failing check, then reconvert with `{reconvert}`",
            report.display()
        ),
    }
}

/// The oldest converter manifest schema the runtime accepts. Schema 15 sets were written before
/// the merge that brought converter schema 16 and world database schema 4, and still load.
const MIN_RUNTIME_CONVERTER_SCHEMA_VERSION: u32 = 15;

const fn converter_schema_version() -> u32 {
    // Kept in sync with converter::cache::CONVERTER_SCHEMA_VERSION without
    // linking the heavy converter crate into the runtime binary.
    16
}

fn setup_synthetic_benchmark(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut terrain_materials: ResMut<Assets<TerrainMaterial>>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = std::time::Instant::now();
    let mesh = Mesh3d(meshes.add(Cuboid::new(18.0, 60.0, 18.0)));
    let material = MeshMaterial3d(terrain_materials.add(TerrainMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.16, 0.36, 0.12),
            perceptual_roughness: 0.9,
            ..default()
        },
        extension: TerrainExtension::default(),
    }));
    let side = (config.synthetic_instances as f64).sqrt().ceil() as usize;
    commands.spawn_batch((0..config.synthetic_instances).map(move |index| {
        let x = index % side;
        let z = index / side;
        (
            mesh.clone(),
            material.clone(),
            Transform::from_xyz(x as f32 * 32.0, 30.0, -(z as f32 * 32.0)),
        )
    }));
    info!(
        instances = config.synthetic_instances,
        "synthetic indirect-render benchmark initialized"
    );
    profiler.increment("synthetic/instances", config.synthetic_instances as u64);
    profiler.record_elapsed("startup/synthetic_scene", started);
}

/// Creation units in a metre. A Creation unit is about 1.43 cm, and this engine renders one
/// Creation unit as one Bevy world unit, so a distance a renderer's default expresses in metres
/// is this many times larger here.
const CREATION_UNITS_PER_METRE: f32 = 70.0;

/// The number of cascades the sun's shadow map is split into. Four is Bevy's default and its
/// per-light maximum on desktop (`MAX_CASCADES_PER_LIGHT`, `bevy_pbr-0.19.0`
/// `src/render/light.rs:228`), and with [`sun_shadow_cascades`] reaching the far corner of the
/// drawn grid, fewer would mean larger cascades and coarser shadows everywhere the player looks.
const SUN_SHADOW_CASCADES: usize = 4;

/// The size of each of the sun's cascades, in texels a side. This is Bevy's own default, which
/// `DirectionalLightShadowMap` documents as having to be a power of two, and it is stated here
/// rather than left implicit so that the resolution the sun's shadows are drawn at is this
/// engine's decision instead of a Bevy default that can move under it.
const SUN_SHADOW_MAP_SIZE: usize = 2048;

/// The most cells, from the camera's cell to the far edge of the drawn grid, that
/// [`sun_shadow_cascades`] fits the sun's shadow range to. `--stream-radius` accepts any integer,
/// and a radius of 100000 would ask for a range of hundreds of millions of units, in which the
/// outermost cascade's texels are wider than the cells they are meant to shadow. The engine's own
/// worlds are a handful of cells across, so the cap is far past anything that streams at a usable
/// frame rate: it only keeps a nonsense radius from asking for gigametre cascades.
const SUN_SHADOW_MAX_GRID_CELLS: i32 = 256;

/// The sun's shadow cascades, fitted to a world whose unit is about 1.43 cm.
///
/// # Why the sun cast no shadows
///
/// `DirectionalLight::shadow_maps_enabled` is already true, but without a [`CascadeShadowConfig`]
/// of its own the sun gets Bevy's default, which is built for a metre-scale world: four cascades
/// with a first far bound of 10 and a maximum distance of 150, split geometrically into the far
/// bounds 10 / 24.7 / 60.8 / 150. Read in Creation units that is a shadow map spent on the two
/// metres of ground around the camera, with every house, tree and cell of the streamed world
/// outside the last cascade - the sun lights the scene and none of it falls in shadow.
///
/// # The distances
///
/// The first two are Bevy's defaults read in metres and converted at [`CREATION_UNITS_PER_METRE`]:
/// a first cascade far bound of 10 m (700 units), and a near clamp of 0.1 m (7 units) below which
/// no shadow is drawn, which is the same 10 cm Bevy's default works at. The overlap between
/// cascades is left at the builder's default; only the distances are fitted to this world.
///
/// [`maximum_distance`] is not a conversion but a property of the drawn world:
/// `streaming::plan_cells` requests `config.stream_radius` cells around the camera's cell and keeps
/// them until they leave `config.unload_radius`, which the command line sets one ring wider, so
/// cells - and the geometry they carry - keep drawing out to the unload ring rather than to the
/// requested radius. The camera stands somewhere inside its own cell, so that grid is at most
/// `unload_radius + 1` cells from it to the edge and [sqrt(2)] times that to the far corner, and
/// the range is the distance from the camera to that corner, the camera being [`camera_offset`]
/// above the point it looks at. It stops there: nothing is drawn past the corner, so a wider range
/// would only spend the same shadow map on coarser cascades. Terrain standing above the ground
/// plane at the corner is further from the camera than the corner and is not accounted for.
/// [SUN_SHADOW_MAX_GRID_CELLS] caps the fit for radii the command line accepts but nothing could
/// stream. At the default `unload_radius` of 3 the last bound is about 23,200 units, a third of a
/// kilometre.
///
/// # The cost
///
/// This is the first configuration in which the shadow pass draws the streamed world rather than a
/// patch of ground in front of the camera: it draws the union of the cascades, which is the
/// camera's view out to `maximum_distance`, so it draws on the order of what the main pass draws
/// again. A frame rate that suffers is turned back up by, in order of how much they give,
/// `maximum_distance`, which is the geometry the pass draws at all, [`SUN_SHADOW_CASCADES`], which
/// is how many passes it is split over, and [`SUN_SHADOW_MAP_SIZE`], which is fill rate rather than
/// geometry. The biases are left at Bevy's defaults. Measure that cost rather than assume it: the
/// synthetic scenario (`scripts/phase2-profile.ps1 -Scenario synthetic`) runs this setup without
/// game data, and `docs/roadmap/02-profiling.md` holds the campaign and its regression thresholds.
///
/// [sqrt(2)]: std::f32::consts::SQRT_2
/// [`maximum_distance`]: CascadeShadowConfigBuilder::maximum_distance
fn sun_shadow_cascades(config: &EngineConfig) -> CascadeShadowConfig {
    // Cells draw out to `unload_radius`, one ring past the requested `stream_radius`, and the
    // `+ 1` is the camera's own cell: the grid is `unload_radius` cells around that cell rather
    // than around the camera, which may stand at the far edge of its own. The radius is clamped
    // at both ends: a negative one, which the command line allows, streams nothing and the range
    // derived from it would fall under the first cascade's far bound, which
    // `CascadeShadowConfigBuilder::build` rejects by panic, while one past
    // [SUN_SHADOW_MAX_GRID_CELLS] is more grid than the shadow map can usefully cover.
    let unload_cells = config.unload_radius.saturating_add(1);
    let cells = unload_cells.clamp(1, SUN_SHADOW_MAX_GRID_CELLS);
    let corner = crate::world::components::CELL_SIZE * cells as f32 * std::f32::consts::SQRT_2;
    CascadeShadowConfigBuilder {
        minimum_distance: 0.1 * CREATION_UNITS_PER_METRE,
        maximum_distance: corner.hypot(camera_offset(config).y),
        first_cascade_far_bound: 10.0 * CREATION_UNITS_PER_METRE,
        num_cascades: SUN_SHADOW_CASCADES,
        ..default()
    }
    .into()
}

/// Where `setup_world` stands the camera relative to the point it looks at, the centre of the cell
/// at the origin of the world grid: the acceptance screenshot is taken from far above it, the
/// walk-around view from behind and above it. [`sun_shadow_cascades`] measures its range from the
/// camera, so the offsets live here rather than in two places.
fn camera_offset(config: &EngineConfig) -> Vec3 {
    if config.acceptance_screenshot.is_some() {
        config
            .screenshot_camera_offset
            .map(Vec3::from)
            .unwrap_or(Vec3::new(0.0, 20_000.0, 1000.0))
    } else {
        Vec3::new(0.0, 1200.0, 2500.0)
    }
}

fn setup_world(
    mut commands: Commands,
    config: Res<EngineConfig>,
    ground_height: Option<Res<InitialCameraGroundHeight>>,
    tuning: Option<Res<MovementTuning>>,
) {
    let ground_height = ground_height.as_deref().map_or(0.0, |height| height.0);
    let target = Vec3::new(CELL_SIZE_HALF, ground_height, -CELL_SIZE_HALF);
    let camera_offset = if config.interactive_world_physics() {
        let tuning = tuning.as_deref().cloned().unwrap_or_default();
        Vec3::Y * (tuning.eye_height + tuning.capsule_standing_height * 0.5 + 16.0)
    } else {
        camera_offset(&config)
    };
    let camera_position = target + camera_offset;
    let far = crate::world::components::CELL_SIZE * (config.stream_radius.max(1) + 2) as f32 * 2.0;
    let camera_transform = if config.interactive_world_physics() {
        Transform::from_translation(camera_position)
            .looking_at(camera_position + Vec3::NEG_Z, Vec3::Y)
    } else {
        Transform::from_translation(camera_position).looking_at(target, Vec3::Y)
    };
    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection { far, ..default() }),
        camera_transform,
        StreamingCamera,
        // This camera draws the streamed world, so the weather's distance fog covers it.
        FogCamera,
        // The sky draws a dome around this camera and clears it to the weather's fog colour.
        SkyCamera,
        Msaa::Off,
        DepthPrepass,
        OcclusionCulling,
        RenderLayers::from_layers(MAIN_VIEW_LAYERS),
    ));
    // The sun's shadows. `shadow_maps_enabled` was never the missing piece - the cascades were:
    // without a configuration of its own the sun gets Bevy's, which reaches 150 metres of a world
    // whose unit is 1.43 cm, and a shadow map spent on that patch is a world lit flat
    // (`sun_shadow_cascades`).
    commands.insert_resource(DirectionalLightShadowMap {
        size: SUN_SHADOW_MAP_SIZE,
    });
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        sun_shadow_cascades(&config),
        RenderLayers::from_layers(LIGHT_LAYERS),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, -0.5, 0.0)),
    ));
    commands.insert_resource(GlobalAmbientLight {
        color: Color::srgb(0.48, 0.55, 0.7),
        // The one definition of the ambient this world path applies: the converted lights are
        // scaled against it (`crate::lights`).
        brightness: crate::lights::AMBIENT_ILLUMINANCE,
        ..default()
    });
    info!(
        assets = %config.assets_dir.display(),
        worldspace = format_args!("{:08X}", config.worldspace_id),
        ground_height,
        camera = ?camera_position,
        target = ?target,
        "OpenSkyrim runtime initialized"
    );
}

fn initial_camera_ground_height(
    config: &EngineConfig,
    database_path: &std::path::Path,
    cache: &CellCache,
) -> Result<f32> {
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open {}", database_path.display()))?;
    let cell_id = connection
        .query_row(
            crate::world::database::EXTERIOR_CELL_ID_SQL,
            params![
                config.worldspace_id,
                config.start_grid.0,
                config.start_grid.1
            ],
            |row| row.get::<_, u32>(0),
        )
        .optional()?;
    let Some(terrain) = cell_id.and_then(|cell_id| cache.terrain(cell_id)) else {
        return Ok(0.0);
    };
    let width = usize::from(terrain.width);
    let height = usize::from(terrain.height);
    let center = (height / 2)
        .checked_mul(width)
        .and_then(|row| row.checked_add(width / 2));
    Ok(center
        .and_then(|index| terrain.heights.get(index))
        .copied()
        .unwrap_or(0.0))
}

const CELL_SIZE_HALF: f32 = crate::world::components::CELL_SIZE * 0.5;
const AUTO_FLIGHT_HALF_SPAN: f32 = crate::world::components::CELL_SIZE * 4.0;

#[derive(Default)]
struct AutoFlightState {
    initialized: bool,
    axis: Vec3,
    sign: f32,
    offset: f32,
}

fn bounded_auto_flight_direction(
    forward: Vec3,
    step_distance: f32,
    state: &mut AutoFlightState,
) -> Vec3 {
    if !state.initialized {
        state.initialized = true;
        state.axis = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::NEG_Z);
        state.sign = 1.0;
    }
    let next_offset = state.offset + state.sign * step_distance.max(0.0);
    if next_offset >= AUTO_FLIGHT_HALF_SPAN {
        state.sign = -1.0;
    } else if next_offset <= -AUTO_FLIGHT_HALF_SPAN {
        state.sign = 1.0;
    }
    state.offset += state.sign * step_distance.max(0.0);
    state.axis * state.sign
}

fn fly_camera(
    time: Res<Time>,
    config: Res<EngineConfig>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mut camera: Query<&mut Transform, With<StreamingCamera>>,
    mut profiler: ResMut<ProfilingState>,
    mut auto_flight: Local<AutoFlightState>,
) {
    // Interactive player paths own the camera; automated camera paths keep legacy controls.
    if config.physics_fixture || config.interactive_world_physics() {
        return;
    }
    let started = std::time::Instant::now();
    let Ok(mut transform) = camera.single_mut() else {
        return;
    };
    let mut direction = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        direction += *transform.forward();
    }
    if keyboard.pressed(KeyCode::KeyS) {
        direction += *transform.back();
    }
    if keyboard.pressed(KeyCode::KeyA) {
        direction += *transform.left();
    }
    if keyboard.pressed(KeyCode::KeyD) {
        direction += *transform.right();
    }
    if keyboard.pressed(KeyCode::Space) {
        direction += Vec3::Y;
    }
    if keyboard.pressed(KeyCode::ShiftLeft) {
        direction -= Vec3::Y;
    }
    let acceptance_capture_pending = config
        .acceptance_screenshot
        .as_ref()
        .is_some_and(|path| !path.is_file());
    let speed = if config.auto_fly_speed > 0.0 {
        config.auto_fly_speed
    } else if keyboard.pressed(KeyCode::ControlLeft) {
        4000.0
    } else {
        900.0
    };
    if config.auto_fly_speed > 0.0 && !acceptance_capture_pending {
        direction += bounded_auto_flight_direction(
            *transform.forward(),
            speed * time.delta_secs(),
            &mut auto_flight,
        );
    }
    transform.translation += direction.normalize_or_zero() * speed * time.delta_secs();
    profiler.record_elapsed("world/fly_camera", started);
}

fn capture_acceptance_screenshot(
    mut commands: Commands,
    config: Res<EngineConfig>,
    mut state: Local<ScreenshotCaptureState>,
    streaming: Option<Res<StreamingMetrics>>,
    world_database: Option<Res<WorldDatabase>>,
    renderer: Res<RendererMetrics>,
    windows: Query<(), With<Window>>,
) {
    let Some(path) = &config.acceptance_screenshot else {
        return;
    };
    state.frames = state.frames.saturating_add(1);
    let gpu_warmed_up = state
        .started
        .get_or_insert_with(std::time::Instant::now)
        .elapsed()
        >= std::time::Duration::from_secs(2);
    if state.captured
        || state.frames < config.benchmark_warmup_frames.saturating_add(10)
        || !gpu_warmed_up
        || windows.is_empty()
    {
        return;
    }
    let assets_ready = streaming
        .as_deref()
        .is_none_or(|metrics| screenshot_assets_ready(metrics, world_database.is_some(), &config));
    let renderer_ready = renderer.final_path_active()
        && (!config.renderer_fixture || renderer.renderer_fixture_validated);
    if !assets_ready || !renderer_ready {
        return;
    }
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        error!(%error, path = %path.display(), "failed to create screenshot directory");
        return;
    }
    commands
        .spawn(Screenshot::primary_window())
        .observe(save_to_disk(path.clone()));
    state.captured = true;
}

fn screenshot_assets_ready(
    metrics: &StreamingMetrics,
    world_streaming_active: bool,
    config: &EngineConfig,
) -> bool {
    metrics.pending_asset_instances == 0
        && metrics.pending_surface_instances == 0
        && metrics.loading_cells == 0
        && metrics.failed_cells == 0
        && (!world_streaming_active || metrics.resident_cells > 0)
        && metrics.asset_load_failures == 0
        && metrics.material_validation_failures == 0
        && metrics.transform_bounds_validation_failures == 0
        && metrics.diagnostic_fallbacks == 0
        && metrics.streaming_invariant_failures == 0
        && metrics.streaming_fixture_failures == 0
        && (!config.material_fixture || metrics.canonical_fixture_validated)
        && (!config.terrain_water_fixture || metrics.terrain_water_fixture_validated)
        && (!config.transform_bounds_fixture || metrics.transform_bounds_fixture_validated)
        && (!config.streaming_fixture || metrics.streaming_fixture_validated)
        && (!config.physics_fixture || metrics.physics_fixture_validated)
}

#[derive(Default)]
struct ScreenshotCaptureState {
    frames: u32,
    captured: bool,
    started: Option<std::time::Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::asset::{AssetApp, AssetPlugin};
    use bevy::world_serialization::WorldSerializationPlugin;

    /// The streaming fixture's own systems over its own fixture database, with no window, GPU or
    /// game data: the camera crosses exteriors, the fixture loads its interior by id, and the
    /// camera finishes on a fully streamed exterior ring around the grid it started on.
    fn streaming_fixture_app() -> (App, StreamingFixtureDirectory) {
        let mut config = EngineConfig {
            streaming_fixture: true,
            ..default()
        };
        let directory =
            StreamingFixtureDirectory::create(config.worldspace_id, config.start_grid).unwrap();
        config.assets_dir = directory.path.clone();
        let database_path = config.assets_dir.join("skyrim_world.db");
        let cache_path = config.assets_dir.join("cell_cache.rkyv");
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin {
                watch_for_changes_override: Some(false),
                ..default()
            },
            WorldSerializationPlugin,
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .init_asset::<TerrainMaterial>()
        .init_asset::<WaterMaterial>()
        .insert_resource(config)
        .insert_resource(WorldDatabase::open(&database_path).unwrap())
        .insert_resource(AssetCatalog::open(&database_path).unwrap())
        .insert_resource(CellCache::open(&cache_path).unwrap())
        .insert_resource(RenderOrigin(IVec2::ZERO))
        .insert_resource(WaterReflectionTexture(Handle::<Image>::default()))
        .init_resource::<ProfilingState>()
        .init_resource::<StreamingFixtureState>()
        .add_plugins(StreamingPlugin)
        .add_systems(
            PreUpdate,
            (drive_streaming_fixture, cross_streaming_fixture_interior).chain(),
        )
        .add_systems(PostUpdate, validate_streaming_fixture);
        app.world_mut()
            .spawn((Transform::default(), StreamingCamera));
        (app, directory)
    }

    #[test]
    fn streaming_fixture_loads_its_interior_by_id_and_keeps_it_while_the_camera_crosses_exteriors()
    {
        let (mut app, _directory) = streaming_fixture_app();
        let expected_resident = {
            let config = app.world().resource::<EngineConfig>();
            ((config.stream_radius * 2 + 1).max(0) as usize).pow(2)
        };
        let mut settled_frames = 0;
        let mut updates = 0;
        while updates < 1_200 {
            app.update();
            // The fixture's frames are vsynced in the acceptance run, and its frame budget assumes
            // that pacing. Pace the headless loop like a 60 Hz frame, so a loaded test machine
            // cannot make the fixture give up before the database worker has answered its cells.
            std::thread::sleep(std::time::Duration::from_millis(16));
            updates += 1;
            let metrics = app.world().resource::<StreamingMetrics>();
            let state = app.world().resource::<StreamingFixtureState>();
            // Settling alone can come before the validator's frame 90 on a fast database, so also
            // wait for the validator's verdict, pass or fail.
            let judged =
                metrics.streaming_fixture_validated || metrics.streaming_fixture_failures > 0;
            let crossed_back = judged
                && state.interior.resident_frame.is_some()
                && metrics.resident_cells >= expected_resident
                && metrics.active_requests == 0
                && metrics.loading_cells == 0;
            settled_frames = if crossed_back { settled_frames + 1 } else { 0 };
            if settled_frames >= 5 {
                break;
            }
        }
        assert!(settled_frames >= 5, "the fixture never settled");

        let interior = app.world().resource::<StreamingFixtureState>().interior;
        assert_eq!(
            interior.requested_frame,
            Some(STREAMING_FIXTURE_INTERIOR_FRAME)
        );
        assert!(interior.resident_frame.is_some());
        assert!(interior.away_samples > 0, "camera never left the grid");
        assert!(
            interior.max_away_roots <= 1,
            "the interior must never have two roots"
        );
        // Current behaviour, not the contract: nothing unloads an interior on this tree, so it is
        // still one root when the camera comes back.
        assert_eq!(interior.min_away_roots, 1);

        let world = app.world_mut();
        let mut roots = world.query_filtered::<&CellRef, With<StreamedCellRoot>>();
        let interior_roots = roots
            .iter(world)
            .filter(|cell| cell.0 == STREAMING_FIXTURE_INTERIOR_CELL_ID)
            .count();
        let mut form_ids = world.query::<&FormId>();
        let interior_references = form_ids
            .iter(world)
            .filter(|form_id| form_id.0 == STREAMING_FIXTURE_INTERIOR_REFERENCE_ID)
            .count();
        assert_eq!(interior_roots, 1);
        assert_eq!(interior_references, 1);

        let metrics = app.world().resource::<StreamingMetrics>();
        assert!(
            metrics.streaming_fixture_validated,
            "the fixture's own validation did not accept the run"
        );
        assert_eq!(metrics.duplicate_cell_roots, 0);
        assert_eq!(metrics.orphaned_cell_roots, 0);
        assert_eq!(metrics.missing_cell_roots, 0);
        assert_eq!(metrics.streaming_invariant_failures, 0);
        assert!(metrics.unloaded_cells > 0);
        assert!(metrics.commit_frames > 0);
    }

    #[test]
    fn interior_crossing_fails_on_a_duplicated_or_mismatched_interior() {
        let crossed = InteriorCrossing {
            requested_frame: Some(STREAMING_FIXTURE_INTERIOR_FRAME),
            resident_frame: Some(40),
            away_samples: 12,
            min_away_roots: 1,
            max_away_roots: 1,
        };
        assert!(interior_crossing_valid(&crossed, 1, 1));
        // A second root for the same cell, or a second copy of its reference.
        assert!(!interior_crossing_valid(&crossed, 2, 1));
        assert!(!interior_crossing_valid(&crossed, 1, 2));
        // Never loaded.
        assert!(!interior_crossing_valid(&InteriorCrossing::default(), 1, 1));
        // Unloaded again is allowed: the contract does not require an interior to stay loaded.
        assert!(interior_crossing_valid(&crossed, 0, 0));
        // A reference left behind without its root, or a root without its reference.
        assert!(!interior_crossing_valid(&crossed, 0, 1));
        assert!(!interior_crossing_valid(&crossed, 1, 0));
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                requested_frame: Some(STREAMING_FIXTURE_INTERIOR_FRAME),
                ..default()
            },
            1,
            1
        ));
        // Gone on some frames while the camera was away is allowed too.
        assert!(interior_crossing_valid(
            &InteriorCrossing {
                min_away_roots: 0,
                ..crossed
            },
            1,
            1
        ));
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                max_away_roots: 2,
                ..crossed
            },
            1,
            1
        ));
        // No observation from outside the unload radius at all: the final count alone would pass
        // for an interior that was loaded and dropped again.
        assert!(!interior_crossing_valid(
            &InteriorCrossing {
                away_samples: 0,
                min_away_roots: 0,
                max_away_roots: 0,
                ..crossed
            },
            1,
            1
        ));
    }

    /// The IO pool's stack has to hold a whole queue of loads nested inside one another, because a
    /// scope opened on a pool thread (bevy_gltf's texture scope) runs the other queued tasks on its
    /// own stack while it waits. This queues `LOADS` tasks behind a blocked single-thread pool,
    /// each of which takes a 64 KiB frame and then opens a scope, the shape of a glTF load. They
    /// nest far past the 8 MiB the pool used to have (the overflow seen streaming a dense city), and must finish
    /// on [`IO_TASK_STACK_BYTES`].
    #[test]
    fn the_io_pool_stack_holds_a_queue_of_loads_nested_in_scopes() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        const LOADS: usize = 300;
        const FRAME_BYTES: usize = 64 * 1024;

        fn load(pool: &bevy::tasks::TaskPool, depth: &AtomicUsize, deepest: &AtomicUsize) {
            let frame = [0u8; FRAME_BYTES];
            std::hint::black_box(&frame);
            let now = depth.fetch_add(1, Ordering::SeqCst) + 1;
            deepest.fetch_max(now, Ordering::SeqCst);
            pool.scope(|scope| scope.spawn(async {}));
            depth.fetch_sub(1, Ordering::SeqCst);
            std::hint::black_box(&frame);
        }

        let pool = Arc::new(io_task_pool_builder(1).build());
        let depth = Arc::new(AtomicUsize::new(0));
        let deepest = Arc::new(AtomicUsize::new(0));

        // Hold the only thread until every load is queued, so the nesting does not depend on
        // how fast this thread can spawn.
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocker = pool.spawn(async move {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();

        let tasks: Vec<_> = (0..LOADS)
            .map(|_| {
                let (pool_ref, depth, deepest) = (pool.clone(), depth.clone(), deepest.clone());
                pool.spawn(async move { load(&pool_ref, &depth, &deepest) })
            })
            .collect();
        release_tx.send(()).unwrap();
        bevy::tasks::block_on(blocker);
        for task in tasks {
            bevy::tasks::block_on(task);
        }

        let deepest = deepest.load(Ordering::SeqCst);
        assert!(
            deepest * FRAME_BYTES > 8 * 1024 * 1024,
            "the loads nested only {deepest} deep, too shallow to test the stack"
        );
    }

    #[test]
    fn rejects_conflicting_fixture_modes() {
        let mut config = EngineConfig {
            physics_fixture: true,
            ..Default::default()
        };
        assert!(validate_fixture_selection(&config).is_ok());
        config.streaming_fixture = true;
        assert!(validate_fixture_selection(&config).is_err());
    }

    #[test]
    fn screenshot_readiness_requires_resident_cells_only_with_world_streaming() {
        let metrics = StreamingMetrics::default();
        let config = EngineConfig::default();

        assert!(screenshot_assets_ready(&metrics, false, &config));
        assert!(!screenshot_assets_ready(&metrics, true, &config));

        let mut settled_metrics = metrics;
        settled_metrics.resident_cells = 1;
        assert!(screenshot_assets_ready(&settled_metrics, true, &config));

        let mut failed_metrics = settled_metrics;
        failed_metrics.failed_cells = 1;
        assert!(!screenshot_assets_ready(&failed_metrics, true, &config));
    }
    /// The sun's shadows reach the grid the streamer draws, whichever way the camera faces, and the
    /// shader's view of them is usable: far bounds that increase (the shader takes the first bound a
    /// fragment is inside, so a bound out of order hands everything beyond it to a cascade that
    /// cannot see it) and a near clamp below the first far bound. The two near distances are Bevy's
    /// defaults read in metres; the last is this engine's drawn grid.
    #[test]
    fn the_sun_shadows_cover_the_drawn_grid() {
        let config = EngineConfig::default();
        let cascades = sun_shadow_cascades(&config);
        let cell = crate::world::components::CELL_SIZE;
        assert_eq!(config.stream_radius, 2);
        // Cells are requested inside the stream radius and kept until they leave the unload ring,
        // one wider, so the unload ring is the grid that keeps drawing.
        assert_eq!(config.unload_radius, 3);
        assert_eq!(cascades.bounds.len(), SUN_SHADOW_CASCADES);

        assert_eq!(
            cascades.minimum_distance,
            0.1 * CREATION_UNITS_PER_METRE,
            "Bevy's 10 cm near clamp, in Creation units"
        );
        assert_eq!(
            cascades.bounds[0],
            10.0 * CREATION_UNITS_PER_METRE,
            "and its 10 m first cascade, which is 700 units here"
        );
        assert_eq!(
            cascades.overlap_proportion,
            CascadeShadowConfigBuilder::default().overlap_proportion,
            "the cascade overlap is the builder's default, not a number of this engine's"
        );

        // The camera stands somewhere inside its own cell and cells stay resident - and keep
        // drawing - out to `unload_radius` cells around that cell, so the grid is
        // `unload_radius + 1` cells from the camera to its edge and sqrt(2) times that to its far
        // corner.
        let axis = cell * (config.unload_radius as f32 + 1.0);
        let corner = axis * std::f32::consts::SQRT_2;
        let reach = corner.hypot(camera_offset(&config).y);
        assert!(
            cascades.bounds[3] > axis,
            "the grid straight ahead is shadowed: {} covers {axis}",
            cascades.bounds[3]
        );
        // `calculate_cascade_bounds` reaches the maximum distance geometrically, so the last bound
        // is the requested distance to within a rounding error rather than to the bit.
        assert!(
            (cascades.bounds[3] - reach).abs() < 1.0,
            "and the far corner is inside the last cascade too: {} against {reach}",
            cascades.bounds[3]
        );
        // The requested radius is not the drawn grid: a range fitted to `stream_radius` alone would
        // leave the ring the streamer keeps beyond it lit flat.
        let requested = cell * (config.stream_radius as f32 + 1.0) * std::f32::consts::SQRT_2;
        assert!(
            cascades.bounds[3] > requested,
            "the unload ring is covered as well: {} against {requested}",
            cascades.bounds[3]
        );

        assert!(
            cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]),
            "the far bounds increase: {:?}",
            cascades.bounds
        );
        assert!(cascades.minimum_distance < cascades.bounds[0]);

        // "Not metre-scale" is what this fix is for: the first cascade on its own reaches further
        // than everything Bevy's default covered, 150 of its world units away.
        let metre_scale = CascadeShadowConfig::default();
        assert!(
            cascades.bounds[0] > *metre_scale.bounds.last().unwrap(),
            "a first cascade of {} units against Bevy's whole default range of {:?}",
            cascades.bounds[0],
            metre_scale.bounds
        );
    }

    /// The range is the camera's own distance to the far corner of the drawn grid, not the corner's
    /// distance from the grid centre: the acceptance screenshot camera is 20,000 units above the
    /// grid it looks at, and its shadows have to reach as far down and out as it looks.
    #[test]
    fn the_shadow_range_counts_the_camera_height() {
        let overhead = EngineConfig {
            acceptance_screenshot: Some(std::path::PathBuf::from("acceptance.png")),
            ..EngineConfig::default()
        };
        let cell = crate::world::components::CELL_SIZE;
        let corner = cell * (overhead.unload_radius as f32 + 1.0) * std::f32::consts::SQRT_2;
        let walk_around = corner.hypot(camera_offset(&EngineConfig::default()).y);
        let above = corner.hypot(camera_offset(&overhead).y);
        assert!(above > walk_around, "the camera's height counts");

        let cascades = sun_shadow_cascades(&overhead);
        assert!(
            (cascades.bounds[3] - above).abs() < 1.0,
            "the overhead camera's range reaches its own corner: {} against {above}",
            cascades.bounds[3]
        );

        let walk_around_cascades = sun_shadow_cascades(&EngineConfig::default());
        assert!(
            (walk_around_cascades.bounds[3] - walk_around).abs() < 1.0,
            "and the walk-around camera's reaches its: {} against {walk_around}",
            walk_around_cascades.bounds[3]
        );
    }

    /// The range follows the grid the streamer draws: at each stream radius the command line takes,
    /// the far corner of the unload ring is inside the last cascade, while the two near distances
    /// stay Bevy's defaults in Creation units rather than following the grid.
    #[test]
    fn the_shadow_range_follows_the_drawn_grid() {
        let cell = crate::world::components::CELL_SIZE;
        let mut previous = 0.0;
        for radius in [0, 1, 2, 4, 8, 16] {
            // `--stream-radius` takes one radius and keeps cells a ring wider than it.
            let args = ["--stream-radius".to_owned(), radius.to_string()];
            let config = EngineConfig::from_args(args);
            assert_eq!(
                (config.stream_radius, config.unload_radius),
                (radius, radius + 1)
            );
            let cascades = sun_shadow_cascades(&config);
            assert_eq!(cascades.bounds.len(), SUN_SHADOW_CASCADES);
            assert_eq!(
                cascades.bounds[0],
                10.0 * CREATION_UNITS_PER_METRE,
                "the first cascade is set in metres, not by the grid, at radius {radius}"
            );
            assert!(cascades.minimum_distance < cascades.bounds[0]);
            assert!(
                cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]),
                "the far bounds increase at radius {radius}: {:?}",
                cascades.bounds
            );

            // The unload ring is what is still drawn: one ring wider than `stream_radius`.
            let axis = cell * (config.unload_radius as f32 + 1.0);
            let corner = axis * std::f32::consts::SQRT_2;
            let reach = corner.hypot(camera_offset(&config).y);
            assert!(
                cascades.bounds[3] > axis,
                "radius {radius} shadows the grid straight ahead: {} covers {axis}",
                cascades.bounds[3]
            );
            assert!(
                (cascades.bounds[3] - reach).abs() < 1.0,
                "radius {radius} reaches the far corner: {} against {reach}",
                cascades.bounds[3]
            );
            assert!(
                cascades.bounds[3] > previous,
                "a wider grid is more world to shadow"
            );
            previous = cascades.bounds[3];
        }

        // A stream radius as negative as the command line allows streams nothing, and the range
        // derived from it would fall under the first cascade's far bound - which
        // `CascadeShadowConfigBuilder::build` rejects by panic. The engine clamps it and starts.
        let args = ["--stream-radius".to_owned(), "-4".to_owned()];
        let nothing = EngineConfig::from_args(args);
        assert!(nothing.unload_radius < 0);
        let cascades = sun_shadow_cascades(&nothing);
        assert!(cascades.bounds[3] > 10.0 * CREATION_UNITS_PER_METRE);
        assert!(cascades.bounds.windows(2).all(|pair| pair[0] < pair[1]));

        // A radius no engine could stream is capped rather than asked for: the range is fitted to
        // the widest grid `sun_shadow_cascades` will fit one to.
        let args = ["--stream-radius".to_owned(), "100000".to_owned()];
        let gigametres = EngineConfig::from_args(args);
        let widest = cell * SUN_SHADOW_MAX_GRID_CELLS as f32 * std::f32::consts::SQRT_2;
        let reach = widest.hypot(camera_offset(&gigametres).y);
        let cascades = sun_shadow_cascades(&gigametres);
        assert!(
            (cascades.bounds[3] - reach).abs() < 1.0,
            "a radius of 100000 cells is capped at {SUN_SHADOW_MAX_GRID_CELLS}: {} against {reach}",
            cascades.bounds[3]
        );
    }

    /// The engine's startup path is `setup_world`, not the helper above, so the sun it spawns is
    /// what has to carry the cascades - along with the shadow map size they are drawn at.
    #[test]
    fn setup_world_spawns_the_sun_with_its_cascades() {
        let config = EngineConfig::default();
        let mut app = App::new();
        app.insert_resource(config.clone())
            .add_systems(Startup, setup_world);
        app.update();

        let world = app.world_mut();
        let mut suns = world.query::<(&DirectionalLight, &CascadeShadowConfig)>();
        let Ok((light, cascades)) = suns.single(world) else {
            panic!("setup_world spawns one shadow-casting directional light");
        };
        assert!(light.shadow_maps_enabled);

        let expected = sun_shadow_cascades(&config);
        assert_eq!(cascades.bounds, expected.bounds);
        assert_eq!(cascades.minimum_distance, expected.minimum_distance);

        // `resource` panics if `setup_world` left the shadow map size unset, which is one of the
        // two ways this test can fail.
        let shadow_map = world.resource::<DirectionalLightShadowMap>();
        assert_eq!(shadow_map.size, SUN_SHADOW_MAP_SIZE);
    }

    /// A placed object is kept out of the reflection pass by its layer, so the streaming camera has
    /// to keep drawing that layer or the objects would vanish from the player's own view.
    #[test]
    fn the_streaming_camera_renders_every_layer_the_world_uses() {
        let mut app = App::new();
        app.insert_resource(EngineConfig::default())
            .add_systems(Startup, setup_world);
        app.update();

        let mut cameras = app
            .world_mut()
            .query_filtered::<&RenderLayers, With<StreamingCamera>>();
        let layers = cameras
            .single(app.world())
            .expect("the streaming camera must be spawned");
        for layer in [
            crate::render::WORLD_LAYER,
            WATER_LAYER,
            crate::render::PLACED_OBJECT_LAYER,
        ] {
            assert!(
                layers.intersects(&RenderLayers::layer(layer)),
                "the streaming camera must render layer {layer}"
            );
        }
    }

    #[test]
    fn automatic_flight_reverses_before_leaving_the_representative_world_area() {
        let mut state = AutoFlightState::default();
        let direction = bounded_auto_flight_direction(Vec3::new(0.0, -1.0, -1.0), 1.0, &mut state);
        assert_eq!(direction, Vec3::NEG_Z);

        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, AUTO_FLIGHT_HALF_SPAN, &mut state),
            Vec3::Z
        );
        assert_eq!(
            bounded_auto_flight_direction(Vec3::NEG_Z, 2.0, &mut state),
            Vec3::NEG_Z
        );
        assert!(state.offset.abs() <= AUTO_FLIGHT_HALF_SPAN);
    }

    #[test]
    fn the_upload_budget_resource_carries_the_configured_option() {
        let mut app = App::new();
        app.insert_resource(upload_budget(&EngineConfig::default()));
        assert_eq!(
            app.world().resource::<RenderAssetBytesPerFrame>().max_bytes,
            Some(16 * 1024 * 1024)
        );

        let unlimited = EngineConfig {
            max_upload_mib_per_frame: 0,
            ..default()
        };
        app.insert_resource(upload_budget(&unlimited));
        assert_eq!(
            app.world().resource::<RenderAssetBytesPerFrame>().max_bytes,
            None
        );
    }

    /// An assets directory the engine was given.
    const EXAMPLE_ASSETS: &str = "converted-assets";

    fn asset_set_message(rejection: AssetSetRejection) -> String {
        asset_set_rejection_message(Path::new(EXAMPLE_ASSETS), rejection)
    }

    /// Runs the real gate against a temporary asset set and returns its error.
    fn runtime_asset_error(manifest: &str, report: &str) -> String {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            manifest.as_bytes(),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            report.as_bytes(),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        format!("{:#}", validate_runtime_assets(&config).unwrap_err())
    }

    #[test]
    fn rejects_stale_or_incomplete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"schema_version":3,"complete":true}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":3,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        assert!(validate_runtime_assets(&config).is_err());
    }

    #[test]
    fn older_converter_schema_names_the_accepted_range_and_the_reconvert_command() {
        let oldest = MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        let newest = converter_schema_version();
        let message =
            asset_set_message(AssetSetRejection::ConverterSchemaOlder { found: oldest - 1 });
        assert!(message.contains("are stale"), "{message}");
        assert!(
            message.contains(&format!("was written by converter schema {}", oldest - 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "accepts converter schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
        assert!(message.contains("reuses what it can"), "{message}");
    }

    #[test]
    fn newer_converter_schema_names_the_accepted_range_and_the_engine_rebuild() {
        let oldest = MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        let newest = converter_schema_version();
        let message =
            asset_set_message(AssetSetRejection::ConverterSchemaNewer { found: newest + 1 });
        assert!(message.contains("newer than this engine"), "{message}");
        assert!(
            message.contains(&format!("was written by converter schema {}", newest + 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "understands only converter schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains("cargo build --release -p engine"),
            "{message}"
        );
    }

    #[test]
    fn incomplete_conversion_names_the_rerun_and_the_incomplete_assets_option() {
        let engine = converter_schema_version();
        let message = asset_set_message(AssetSetRejection::IncompleteConversion { schema: engine });
        assert!(message.contains("did not finish"), "{message}");
        assert!(message.contains("complete=false"), "{message}");
        assert!(
            message.contains(&format!("at converter schema {engine}")),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
        assert!(message.contains("--allow-incomplete-assets"), "{message}");
    }

    #[test]
    fn incomplete_conversion_names_the_schema_the_manifest_reports() {
        let oldest = MIN_RUNTIME_CONVERTER_SCHEMA_VERSION;
        assert_ne!(oldest, converter_schema_version());
        let message = runtime_asset_error(
            &format!(r#"{{"schema_version":{oldest},"complete":false}}"#),
            &format!(r#"{{"schema_version":{MAX_RUNTIME_DATABASE_SCHEMA_VERSION},"passed":true}}"#),
        );
        assert!(
            message.contains(&format!("complete=false at converter schema {oldest},")),
            "{message}"
        );
    }

    #[test]
    fn world_database_schema_mismatch_names_the_accepted_range_and_the_reconvert_command() {
        let oldest = MIN_RUNTIME_DATABASE_SCHEMA_VERSION;
        let newest = MAX_RUNTIME_DATABASE_SCHEMA_VERSION;
        let message =
            asset_set_message(AssetSetRejection::WorldDatabaseSchema { found: oldest - 1 });
        assert!(message.contains("world database schema"), "{message}");
        assert!(
            message.contains(&format!("world database schema {}", oldest - 1)),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "accepts world database schemas {oldest} through {newest}"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
    }

    #[test]
    fn failed_integration_report_points_at_the_report_file() {
        let message = asset_set_message(AssetSetRejection::IntegrationReportFailed);
        assert!(
            message.contains("integration report did not pass"),
            "{message}"
        );
        assert!(message.contains("passed=false"), "{message}");
        assert!(message.contains("integration-report.json"), "{message}");
        assert!(message.contains(EXAMPLE_ASSETS), "{message}");
        assert!(
            message.contains(&format!(
                "cargo run --release -p converter -- <Skyrim Data folder> \"{EXAMPLE_ASSETS}\""
            )),
            "{message}"
        );
    }

    #[test]
    fn stale_incomplete_and_failed_runtime_assets_report_distinct_reasons() {
        let engine = converter_schema_version();
        let world = shared::WORLD_DATABASE_SCHEMA_VERSION;
        let passing_report = format!(r#"{{"schema_version":{world},"passed":true}}"#);
        let stale =
            runtime_asset_error(r#"{"schema_version":14,"complete":true}"#, &passing_report);
        let incomplete = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":false}}"#),
            &passing_report,
        );
        let failed_report = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":true}}"#),
            &format!(r#"{{"schema_version":{world},"passed":false}}"#),
        );
        let schema_report = runtime_asset_error(
            &format!(r#"{{"schema_version":{engine},"complete":true}}"#),
            &format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                MIN_RUNTIME_DATABASE_SCHEMA_VERSION - 1
            ),
        );

        assert!(stale.contains("converted assets are stale"), "{stale}");
        assert!(
            incomplete.contains("--allow-incomplete-assets"),
            "{incomplete}"
        );
        assert!(failed_report.contains("passed=false"), "{failed_report}");
        assert!(
            schema_report.contains("world database schema"),
            "{schema_report}"
        );
        for (left, right) in [
            (&stale, &incomplete),
            (&stale, &failed_report),
            (&stale, &schema_report),
            (&incomplete, &failed_report),
            (&incomplete, &schema_report),
            (&failed_report, &schema_report),
        ] {
            assert_ne!(left, right);
        }
    }

    #[test]
    fn accepts_current_complete_runtime_assets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            format!(
                r#"{{"schema_version":{},"complete":true}}"#,
                converter_schema_version()
            ),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            format!(
                r#"{{"schema_version":{},"passed":true}}"#,
                shared::WORLD_DATABASE_SCHEMA_VERSION
            ),
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":5,"passed":true}"#,
        )
        .unwrap();
        assert!(
            validate_runtime_assets(&config)
                .unwrap_err()
                .to_string()
                .contains("schema 5 is unsupported")
        );
    }

    #[test]
    fn accepts_passing_schema_four_integration_report() {
        let directory = tempfile::tempdir().unwrap();
        for required in ["skyrim_world.db", "cell_cache.rkyv"] {
            std::fs::write(directory.path().join(required), []).unwrap();
        }
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"schema_version":15,"complete":true}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":4,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
    }

    #[test]
    fn accepts_legacy_schema_15_assets_with_runtime_proxy_fallback() {
        // A real pre-merge conversion: converter schema 15 wrote world database schema 3.
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
        std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
        std::fs::write(
            directory.path().join("conversion-manifest.json"),
            br#"{"schema_version":15,"complete":true}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("integration-report.json"),
            br#"{"schema_version":3,"passed":true}"#,
        )
        .unwrap();
        let config = EngineConfig {
            assets_dir: directory.path().to_owned(),
            ..default()
        };
        validate_runtime_assets(&config).unwrap();
    }

    #[test]
    fn rejects_truncated_manifest_and_integration_report() {
        for truncated_file in ["conversion-manifest.json", "integration-report.json"] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("skyrim_world.db"), []).unwrap();
            std::fs::write(directory.path().join("cell_cache.rkyv"), []).unwrap();
            std::fs::write(
                directory.path().join("conversion-manifest.json"),
                format!(
                    r#"{{"schema_version":{},"complete":true}}"#,
                    converter_schema_version()
                ),
            )
            .unwrap();
            std::fs::write(
                directory.path().join("integration-report.json"),
                format!(
                    r#"{{"schema_version":{},"passed":true}}"#,
                    shared::WORLD_DATABASE_SCHEMA_VERSION
                ),
            )
            .unwrap();
            std::fs::write(directory.path().join(truncated_file), b"{").unwrap();
            let config = EngineConfig {
                assets_dir: directory.path().to_owned(),
                ..default()
            };
            assert!(
                validate_runtime_assets(&config).is_err(),
                "{truncated_file}"
            );
        }
    }

    #[test]
    fn ground_height_query_reads_the_right_cell() {
        let database_directory = tempfile::tempdir().unwrap();
        let database_path = database_directory.path().join("skyrim_world.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE cells(id INTEGER PRIMARY KEY,worldspace_id INTEGER,grid_x INTEGER,grid_y INTEGER);
                 CREATE TABLE land(cell_id INTEGER PRIMARY KEY);
                 INSERT INTO cells(id,worldspace_id,grid_x,grid_y) VALUES(42,7,0,0);
                 INSERT INTO land(cell_id) VALUES(42);",
            )
            .unwrap();
        drop(connection);

        let cache_directory = tempfile::tempdir().unwrap();
        let cache_path = cache_directory.path().join("cell_cache.rkyv");
        let source = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id: 42,
                width: 2,
                height: 2,
                heights: vec![1.0, 2.0, 3.0, 4.0],
                normals: vec![0; 12],
                vertex_colors: vec![255; 12],
                layers: vec![],
                water_height: None,
                water_type_form_id: None,
            }],
        };
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&source).unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        let cache = CellCache::open(&cache_path).unwrap();

        let config = EngineConfig {
            worldspace_id: 7,
            start_grid: (0, 0),
            ..default()
        };

        let ground_height = initial_camera_ground_height(&config, &database_path, &cache).unwrap();
        assert_eq!(ground_height, 4.0);
    }

    #[test]
    fn ground_height_query_does_not_create_a_missing_database() {
        // The old code opened with `Connection::open`, which is
        // read-write-and-create-if-missing: querying a database that does not
        // exist yet silently created an empty one as a side effect. An
        // explicit read-only open must instead fail without creating
        // anything.
        let database_directory = tempfile::tempdir().unwrap();
        let database_path = database_directory.path().join("skyrim_world.db");
        assert!(!database_path.exists());

        let cache_directory = tempfile::tempdir().unwrap();
        let cache_path = cache_directory.path().join("cell_cache.rkyv");
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![],
        })
        .unwrap();
        std::fs::write(&cache_path, bytes).unwrap();
        let cache = CellCache::open(&cache_path).unwrap();

        let config = EngineConfig {
            worldspace_id: 7,
            start_grid: (0, 0),
            ..default()
        };

        assert!(initial_camera_ground_height(&config, &database_path, &cache).is_err());
        assert!(
            !database_path.exists(),
            "a read-only ground-height query must not create a missing database file"
        );
    }

    /// The terrain/water fixture is the only scene that draws terrain without game data, so its
    /// materials have to carry the overlay weight field the streamed path reads: on the packed
    /// vertex attributes it would show the sharpened carrier instead, and the field would go
    /// unexercised outside a converted asset set.
    #[test]
    fn terrain_water_fixture_quadrants_carry_their_weight_field() {
        let terrain = terrain_water_fixture_snapshot();
        for quadrant in 0..4 {
            let extension = TerrainExtension::fixture(
                &terrain,
                quadrant,
                std::array::from_fn(|_| Handle::<Image>::default()),
            )
            .expect("the canonical terrain fixture must build");
            assert!(
                extension.reads_weight_field(),
                "quadrant {quadrant} must render its overlays through the weight field"
            );
        }
    }

    /// The shader tiles every terrain layer across a cell, so the fixture's layer images
    /// need the sampler a streamed layer is loaded with. Bevy's default clamps to the edge, which
    /// stretches the outermost texels over the rest of the tiles.
    #[test]
    fn terrain_water_fixture_layers_are_sampled_with_a_repeating_sampler() {
        let expected = bevy::image::ImageSampler::Descriptor(terrain_layer_sampler());
        assert_eq!(terrain_fixture_image([82, 116, 58, 255]).sampler, expected);
    }
}
