//! Going through a load door: look at one, press E, fade to black, arrive at the other side.
//!
//! One state machine per crossing, with a single crossing at a time:
//!
//! 1. **Fade out** (0.25 s) under a full-screen black UI node.
//! 2. At full black the space switches in one step: every cell of the space left behind unloads at
//!    once (unpaced), the [`ActiveSpace`] and `CameraSpace` change, an exterior landing sets the
//!    render origin to the arrival's grid square (an interior landing leaves it alone), and the
//!    player is teleported to the door's `XTEL` arrival point.
//! 3. **Hold black** until the landing cell is resident, nothing under its root is pending (models, surfaces) and
//!    every mesh, material and texture under the cell's root has finished loading, then two more
//!    frames so render extraction and preparation run, for at most 10 s. While the screen is black
//!    the per-frame upload budget is lifted (a stutter behind black is invisible) and the configured
//!    value comes back when the fade-in starts. A landing cell that fails to load puts the player
//!    back where they were, with a warning.
//! 4. **Fade in** (0.25 s).
//!
//! There is no preload: the fade simply holds until the destination is resident and uploaded.

use crate::{
    doors::LoadDoor,
    physics::{CursorCapture, MovementTuning, TeleportPlayer, body_and_camera_for_feet},
    render::{TerrainMaterial, WaterMaterial},
    sky::CameraSpace,
    streaming::{
        ActiveSpace, PendingUnder, RenderOrigin, StreamingWorld, creation_rotation_to_bevy,
        creation_to_bevy, streaming_center, unload_all_cells_now,
    },
    world::{
        components::{CELL_SIZE, ExpectedModelBounds, StreamingCamera, WorldPosition},
        database::CellKey,
    },
};
use bevy::{
    asset::{LoadState, UntypedAssetId},
    ecs::system::SystemParam,
    prelude::*,
    render::render_asset::RenderAssetBytesPerFrame,
};
use std::collections::HashSet;

/// How far from the camera a door can be and still be used, in Creation units.
pub const DOOR_REACH: f32 = 200.0;
/// The box a load door is activated through when its reference carries no model bounds, in the
/// door's local space: a door-sized box centred on the origin horizontally (160 wide and deep, so
/// a door hinged at the origin is still hit across its width) that starts at the floor and reaches
/// 200 up, a person's height and a little more.
pub const FALLBACK_DOOR_MIN: Vec3 = Vec3::new(-80.0, 0.0, -80.0);
pub const FALLBACK_DOOR_MAX: Vec3 = Vec3::new(80.0, 200.0, 80.0);
/// Seconds to fade out, and to fade in.
pub const FADE_SECONDS: f32 = 0.25;
/// Longest the screen stays black waiting for the destination.
pub const LANDING_TIMEOUT_SECONDS: f32 = 10.0;
/// Frames to keep waiting once everything under the landing cell has loaded.
const SETTLE_FRAMES: u32 = 2;

pub struct DoorCrossingPlugin;

impl Plugin for DoorCrossingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DoorCrossing>()
            .init_resource::<CameraSpace>()
            .add_message::<TeleportPlayer>()
            .add_systems(Startup, spawn_fade_overlay)
            // After the streaming chain in `Update`, so the pending counts are this frame's.
            .add_systems(PostUpdate, drive_door_crossing);
    }
}

/// The full-screen black node the crossing fades.
#[derive(Component)]
pub struct FadeOverlay;

/// Where a crossing puts the player.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Landing {
    space: ActiveSpace,
    /// The cell whose residency ends the black hold.
    key: CellKey,
    /// `Some(grid)` for an exterior landing; an interior landing leaves the origin alone.
    origin: Option<IVec2>,
    camera_space: CameraSpace,
    /// FEET position in render space (relative to `origin`, or to the unchanged origin), as
    /// [`TeleportPlayer`] takes it.
    position: Vec3,
    yaw: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    FadeOut {
        elapsed: f32,
    },
    /// Black; the switch is done and the landing cell is loading.
    Landing {
        waited: f32,
        restoring: bool,
        /// Consecutive frames the landing cell has been fully loaded; the fade-in starts after
        /// [`SETTLE_FRAMES`] more, so render extraction runs with the budget still lifted.
        ready_frames: u32,
    },
    FadeIn {
        elapsed: f32,
    },
}

/// The player's own place before a crossing.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Restore {
    space: ActiveSpace,
    camera_space: CameraSpace,
    configured_worldspace: u32,
    /// Feet position. For an exterior start: absolute render-space coordinates (the render
    /// position plus the render origin's offset), so independent of whichever origin is in force.
    /// For an interior start: the raw render position, interiors being placed absolutely with the
    /// origin unused.
    feet: Vec3,
    yaw: f32,
}

/// The absolute feet position for render-space `feet` under `origin`.
fn absolute_feet(feet: Vec3, origin: IVec2) -> Vec3 {
    feet + Vec3::new(
        origin.x as f32 * CELL_SIZE,
        0.0,
        -(origin.y as f32) * CELL_SIZE,
    )
}

impl Restore {
    fn landing(&self) -> Landing {
        match self.space.interior {
            Some(cell_id) => Landing {
                space: self.space,
                key: CellKey::Interior(cell_id),
                origin: None,
                camera_space: self.camera_space,
                position: self.feet,
                yaw: self.yaw,
            },
            None => {
                let grid = streaming_center(self.feet, IVec2::ZERO);
                Landing {
                    space: self.space,
                    key: CellKey::Exterior {
                        worldspace_id: self.space.exterior_worldspace(self.configured_worldspace),
                        grid_x: grid.x,
                        grid_y: grid.y,
                    },
                    origin: Some(grid),
                    camera_space: self.camera_space,
                    position: absolute_feet(self.feet, -grid),
                    yaw: self.yaw,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Crossing {
    door: LoadDoor,
    target: Landing,
    /// Where the player was, to put them back if the target cannot load. Kept in world terms (the
    /// absolute feet position, see [`absolute_feet`]) so a render-origin rebase before the switch
    /// cannot move it; it becomes a [`Landing`] only when it is used.
    restore: Restore,
    /// Seconds since E was pressed.
    since_press: f32,
    stage: Stage,
}

/// The crossing in progress, if any.
#[derive(Resource, Default, Debug)]
pub struct DoorCrossing {
    active: Option<Crossing>,
}

impl DoorCrossing {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

fn spawn_fade_overlay(mut commands: Commands) {
    commands.spawn((
        Name::new("Door fade"),
        FadeOverlay,
        Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.0)),
        GlobalZIndex(i32::MAX - 1),
    ));
}

fn fallback_bounds() -> ExpectedModelBounds {
    ExpectedModelBounds {
        min: FALLBACK_DOOR_MIN,
        max: FALLBACK_DOOR_MAX,
    }
}

/// The destination as the crossing's INFO line prints it: the interior cell id, or the
/// worldspace and grid square.
fn describe_destination(key: &CellKey) -> String {
    match key {
        CellKey::Interior(cell_id) => format!("interior cell {cell_id}"),
        CellKey::Exterior {
            worldspace_id,
            grid_x,
            grid_y,
        } => format!("worldspace {worldspace_id} grid ({grid_x}, {grid_y})"),
    }
}

/// The distance along a ray (unit `direction`) to a door's model bounds, if within `reach`.
///
/// The bounds are the converter's model-space box; the ray is taken into the door's local space
/// so a rotated or scaled door is tested as the oriented box it is.
fn ray_hits_door(
    origin: Vec3,
    direction: Vec3,
    door: &GlobalTransform,
    bounds: &ExpectedModelBounds,
    reach: f32,
) -> Option<f32> {
    let inverse = door.affine().inverse();
    let local_origin = inverse.transform_point3(origin);
    // Not renormalised, so the ray parameter stays a world distance.
    let local_direction = inverse.transform_vector3(direction);
    let mut near = 0.0f32;
    let mut far = reach;
    for axis in 0..3 {
        let (start, step) = (local_origin[axis], local_direction[axis]);
        if step.abs() < 1e-9 {
            if start < bounds.min[axis] || start > bounds.max[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = (
            (bounds.min[axis] - start) / step,
            (bounds.max[axis] - start) / step,
        );
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    Some(near)
}

/// The landing a door's destination describes, from the space the player is in now.
fn landing_for(door: &LoadDoor, current: &ActiveSpace, configured_worldspace: u32) -> Landing {
    let destination = &door.destination;
    let creation = Vec3::from_array(destination.arrival_position);
    let yaw = creation_rotation_to_bevy(destination.arrival_rotation)
        .to_euler(EulerRot::YXZ)
        .0;
    if let Some(cell_id) = destination.interior_cell_id {
        // Interiors are placed in absolute coordinates and the origin stays as it is.
        return Landing {
            space: ActiveSpace {
                worldspace_id: Some(current.exterior_worldspace(configured_worldspace)),
                interior: Some(cell_id),
            },
            key: CellKey::Interior(cell_id),
            origin: None,
            camera_space: CameraSpace::Interior,
            position: creation_to_bevy(creation),
            yaw,
        };
    }
    let worldspace_id = destination
        .worldspace_id
        .unwrap_or_else(|| current.exterior_worldspace(configured_worldspace));
    let at = WorldPosition::from_creation_units(creation);
    Landing {
        space: ActiveSpace {
            worldspace_id: Some(worldspace_id),
            interior: None,
        },
        key: CellKey::Exterior {
            worldspace_id,
            grid_x: at.grid.x,
            grid_y: at.grid.y,
        },
        origin: Some(at.grid),
        camera_space: CameraSpace::Exterior,
        position: creation_to_bevy(at.relative_to(at.grid)),
        yaw,
    }
}

/// Switches the world to `landing` in one step. Runs as a command, so it applies before the next
/// system that reads the cells and the old space is gone the same frame.
fn switch_space(world: &mut World, landing: Landing) {
    unload_all_cells_now(world);
    *world.resource_mut::<ActiveSpace>() = landing.space;
    if let Some(grid) = landing.origin {
        world.resource_mut::<RenderOrigin>().0 = grid;
    }
    *world.resource_mut::<CameraSpace>() = landing.camera_space;
    let (_, eye) = body_and_camera_for_feet(world.resource::<MovementTuning>(), landing.position);
    // Move the camera now as well as through the message: the planner runs before the teleport is
    // applied and must already see the player in the new space.
    let mut cameras = world.query_filtered::<&mut Transform, With<StreamingCamera>>();
    for mut camera in cameras.iter_mut(world) {
        camera.translation = eye;
    }
    world.write_message(TeleportPlayer {
        position: landing.position,
        yaw: landing.yaw,
    });
}

/// What a landing cell is made of, for the readiness check.
#[derive(SystemParam)]
#[allow(clippy::type_complexity)]
struct LandingAssets<'w, 's> {
    server: Option<Res<'w, AssetServer>>,
    children: Query<'w, 's, &'static Children>,
    parts: Query<
        'w,
        's,
        (
            Option<&'static Mesh3d>,
            Option<&'static MeshMaterial3d<StandardMaterial>>,
            Option<&'static MeshMaterial3d<TerrainMaterial>>,
            Option<&'static MeshMaterial3d<WaterMaterial>>,
        ),
    >,
    meshes: Option<Res<'w, Assets<Mesh>>>,
    images: Option<Res<'w, Assets<Image>>>,
    standard: Option<Res<'w, Assets<StandardMaterial>>>,
    terrain: Option<Res<'w, Assets<TerrainMaterial>>>,
    water: Option<Res<'w, Assets<WaterMaterial>>>,
}

/// What the readiness check found under a landing root.
#[derive(Debug, Default)]
struct LandingReport {
    /// Assets still loading.
    waiting: usize,
    /// The first of them, by asset path when the server knows one.
    first_waiting: Option<String>,
    /// Assets whose load failed. They count as done, so a missing texture cannot hold the screen.
    failed: HashSet<UntypedAssetId>,
}

impl LandingAssets<'_, '_> {
    /// Notes an asset that is still loading, or failed, in `report`. A finished asset is loaded,
    /// or (no load tracked, as for an asset added directly) present in its store.
    fn check(
        &self,
        id: UntypedAssetId,
        present: impl FnOnce() -> bool,
        report: &mut LandingReport,
    ) {
        let Some(server) = &self.server else {
            return;
        };
        let waiting = match server.get_load_state(id) {
            Some(LoadState::Loaded) => false,
            Some(LoadState::Failed(_)) => {
                report.failed.insert(id);
                false
            }
            Some(_) => true,
            None => !present(),
        };
        if waiting {
            report.waiting += 1;
            if report.first_waiting.is_none() {
                report.first_waiting = Some(
                    server
                        .get_path(id)
                        .map_or_else(|| format!("{id:?}"), |path| path.to_string()),
                );
            }
        }
    }

    fn check_image(&self, id: UntypedAssetId, report: &mut LandingReport) {
        self.check(
            id,
            || match (&self.images, id.try_typed::<Image>()) {
                (Some(images), Ok(typed)) => images.contains(typed),
                _ => true,
            },
            report,
        );
    }

    /// Checks every dependency (the textures) of a material.
    fn check_dependencies<A: Asset>(&self, material: &A, report: &mut LandingReport) {
        material.visit_dependencies(&mut |id| self.check_image(id, report));
    }

    /// Checks a material handle and its textures.
    fn check_material<A: Asset>(
        &self,
        handle: &Handle<A>,
        store: &Option<Res<Assets<A>>>,
        textures_of: impl FnOnce(&Self, &A, &mut LandingReport),
        report: &mut LandingReport,
    ) {
        let present = || {
            store
                .as_ref()
                .is_none_or(|assets| assets.contains(handle.id()))
        };
        let before = report.waiting;
        self.check(handle.id().untyped(), present, report);
        if report.waiting != before {
            return;
        }
        if let Some(material) = store.as_ref().and_then(|assets| assets.get(handle.id())) {
            textures_of(self, material, report);
        }
    }

    /// Inspects every mesh, material and texture under `root`. Covered: `Mesh3d`,
    /// `MeshMaterial3d<StandardMaterial>` with all its texture dependencies, and the terrain and
    /// water materials with their base-material textures. The terrain and water extensions' own
    /// layer and reflection images are private to `render.rs` and are not checked.
    fn inspect_under(&self, root: Entity) -> LandingReport {
        let mut report = LandingReport::default();
        for entity in std::iter::once(root).chain(self.children.iter_descendants(root)) {
            let Ok((mesh, standard, terrain, water)) = self.parts.get(entity) else {
                continue;
            };
            if let Some(Mesh3d(handle)) = mesh {
                self.check(
                    handle.id().untyped(),
                    || {
                        self.meshes
                            .as_ref()
                            .is_none_or(|meshes| meshes.contains(handle.id()))
                    },
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = standard {
                self.check_material(
                    handle,
                    &self.standard,
                    |this, m, report| this.check_dependencies(m, report),
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = terrain {
                self.check_material(
                    handle,
                    &self.terrain,
                    |this, m, report| this.check_dependencies(&m.base, report),
                    &mut report,
                );
            }
            if let Some(MeshMaterial3d(handle)) = water {
                self.check_material(
                    handle,
                    &self.water,
                    |this, m, report| this.check_dependencies(&m.base, report),
                    &mut report,
                );
            }
        }
        report
    }
}

/// Sets the per-frame render-asset upload budget, if the engine has one.
fn set_upload_budget(
    budget: &mut Option<ResMut<RenderAssetBytesPerFrame>>,
    max_bytes: Option<usize>,
) {
    if let Some(budget) = budget {
        budget.max_bytes = max_bytes;
    }
}

fn set_alpha(overlay: &mut Query<&mut BackgroundColor, With<FadeOverlay>>, alpha: f32) {
    for mut colour in overlay.iter_mut() {
        colour.0 = Color::srgba(0.0, 0.0, 0.0, alpha.clamp(0.0, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_door_crossing(
    keyboard: Res<ButtonInput<KeyCode>>,
    capture: Res<CursorCapture>,
    time: Res<Time>,
    config: Res<crate::config::EngineConfig>,
    tuning: Res<MovementTuning>,
    space: Res<ActiveSpace>,
    origin: Res<RenderOrigin>,
    camera_space: Res<CameraSpace>,
    streaming: Res<StreamingWorld>,
    camera: Query<&Transform, With<StreamingCamera>>,
    doors: Query<(&LoadDoor, &GlobalTransform, Option<&ExpectedModelBounds>)>,
    mut crossing: ResMut<DoorCrossing>,
    mut overlay: Query<&mut BackgroundColor, With<FadeOverlay>>,
    (mut budget, landing_assets, pending_under): (
        Option<ResMut<RenderAssetBytesPerFrame>>,
        LandingAssets,
        PendingUnder,
    ),
    mut commands: Commands,
) {
    let delta = time.delta_secs();
    let Some(active) = crossing.active.as_mut() else {
        // Idle: E at a door starts a crossing.
        if !keyboard.just_pressed(KeyCode::KeyE) || *capture != CursorCapture::Captured {
            return;
        }
        let Ok(view) = camera.single() else {
            return;
        };
        let direction = view.forward().as_vec3();
        let Some((door, _)) = doors
            .iter()
            .filter_map(|(door, transform, bounds)| {
                let bounds = bounds.copied().unwrap_or_else(fallback_bounds);
                ray_hits_door(view.translation, direction, transform, &bounds, DOOR_REACH)
                    .map(|distance| (door, distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
        else {
            return;
        };
        let (yaw, _, _) = view.rotation.to_euler(EulerRot::YXZ);
        // The camera sits an eye height above the body, and the body a capsule half-extent above
        // the feet.
        let (body_offset, _) = body_and_camera_for_feet(&tuning, Vec3::ZERO);
        let feet = view.translation - Vec3::Y * tuning.eye_height - body_offset;
        let restore = Restore {
            space: *space,
            camera_space: *camera_space,
            configured_worldspace: config.worldspace_id,
            feet: if space.interior.is_some() {
                feet
            } else {
                absolute_feet(feet, origin.0)
            },
            yaw,
        };
        let target = landing_for(door, &space, config.worldspace_id);
        debug!(
            door = format_args!("{:08X}", door.ref_id),
            "door crossing started"
        );
        crossing.active = Some(Crossing {
            door: door.clone(),
            target,
            restore,
            since_press: 0.0,
            stage: Stage::FadeOut { elapsed: 0.0 },
        });
        set_alpha(&mut overlay, 0.0);
        return;
    };
    active.since_press += delta;
    match active.stage {
        Stage::FadeOut { elapsed } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                let target = active.target;
                commands.queue(move |world: &mut World| switch_space(world, target));
                // Behind black a stutter is invisible: lift the upload budget until the fade-in.
                set_upload_budget(&mut budget, None);
                active.stage = Stage::Landing {
                    waited: 0.0,
                    restoring: false,
                    ready_frames: 0,
                };
            } else {
                active.stage = Stage::FadeOut { elapsed };
            }
        }
        Stage::Landing {
            waited,
            restoring,
            ready_frames,
        } => {
            set_alpha(&mut overlay, 1.0);
            let waited = waited + delta;
            let landing = if restoring {
                active.restore.landing()
            } else {
                active.target
            };
            if streaming.is_failed(landing.key) {
                if restoring {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        "door crossing: the player's own space could not be reloaded; fading in anyway"
                    );
                    info!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        destination = %describe_destination(&landing.key),
                        restored = true,
                        milliseconds = (active.since_press * 1000.0) as u32,
                        "door crossing: restored the player's own place (it failed to reload); fade-in starts"
                    );
                    set_upload_budget(&mut budget, config.max_upload_bytes_per_frame());
                    active.stage = Stage::FadeIn { elapsed: 0.0 };
                } else {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        destination = ?active.target.key,
                        "door crossing: the destination could not be loaded; putting the player back"
                    );
                    let restore = active.restore.landing();
                    commands.queue(move |world: &mut World| switch_space(world, restore));
                    active.stage = Stage::Landing {
                        waited: 0.0,
                        restoring: true,
                        ready_frames: 0,
                    };
                }
                return;
            }
            // Only the landing cell's own work counts: other cells of a stream window may stay
            // busy for a long time without affecting what the player sees.
            let root = streaming.resident_root(landing.key);
            let pending = root.map_or(0, |root| pending_under.count(root));
            let report = root.map(|root| landing_assets.inspect_under(root));
            let loaded = report
                .as_ref()
                .is_some_and(|report| pending == 0 && report.waiting == 0);
            // Once loaded, a few more frames let extraction and preparation upload with the budget
            // still lifted: the fade-in starts on the SETTLE_FRAMES-th loaded frame.
            let ready_frames = if loaded { ready_frames + 1 } else { 0 };
            let ready = ready_frames >= SETTLE_FRAMES;
            if ready || waited >= LANDING_TIMEOUT_SECONDS {
                if !ready {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        waited_seconds = waited,
                        resident = root.is_some(),
                        pending_components = pending,
                        assets_loading = report.as_ref().map_or(0, |report| report.waiting),
                        first_not_loaded = report
                            .as_ref()
                            .and_then(|report| report.first_waiting.as_deref())
                            .unwrap_or("none"),
                        "door crossing: the destination was not ready in time; fading in anyway"
                    );
                }
                if let Some(report) = report.as_ref().filter(|report| !report.failed.is_empty()) {
                    warn!(
                        door = format_args!("{:08X}", active.door.ref_id),
                        failed_assets = report.failed.len(),
                        "door crossing: some assets under the landing cell failed to load"
                    );
                }
                info!(
                    door = format_args!("{:08X}", active.door.ref_id),
                    destination = %describe_destination(&landing.key),
                    restored = restoring,
                    milliseconds = (active.since_press * 1000.0) as u32,
                    "door crossing: {} fade-in starts",
                    if restoring {
                        "restored the player's own place;"
                    } else {
                        "arrived;"
                    }
                );
                set_upload_budget(&mut budget, config.max_upload_bytes_per_frame());
                active.stage = Stage::FadeIn { elapsed: 0.0 };
            } else {
                active.stage = Stage::Landing {
                    waited,
                    restoring,
                    ready_frames,
                };
            }
        }
        Stage::FadeIn { elapsed } => {
            let elapsed = elapsed + delta;
            set_alpha(&mut overlay, 1.0 - elapsed / FADE_SECONDS);
            if elapsed >= FADE_SECONDS {
                crossing.active = None;
            } else {
                active.stage = Stage::FadeIn { elapsed };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        doors::DoorDestination,
        physics::MoveMode,
        profiling::ProfilingState,
        streaming::{StreamingMetrics, StreamingWorld, pending_marker_for_test},
    };
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    const DT: f32 = 0.05;

    #[derive(Resource, Default)]
    struct Teleports(Vec<TeleportPlayer>);

    fn collect(mut reader: MessageReader<TeleportPlayer>, mut out: ResMut<Teleports>) {
        out.0.extend(reader.read().copied());
    }

    fn door(destination: DoorDestination) -> LoadDoor {
        LoadDoor {
            ref_id: 0x10,
            destination,
        }
    }

    fn interior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x20,
            interior_cell_id: Some(77),
            worldspace_id: None,
            arrival_position: [100.0, 200.0, 300.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    fn exterior_door() -> LoadDoor {
        door(DoorDestination {
            destination_ref_id: 0x21,
            interior_cell_id: None,
            worldspace_id: Some(60),
            // Grid (3, -2) in Creation units, 100 into the cell on each axis.
            arrival_position: [3.0 * 4096.0 + 100.0, -2.0 * 4096.0 + 100.0, 50.0],
            arrival_rotation: [0.0, 0.0, 0.0],
        })
    }

    /// A headless world with the crossing registered, the camera 100 units in front of a door
    /// whose bounds are a 100-unit cube, and one resident old-space cell.
    fn app_with(door: LoadDoor) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin, AssetPlugin::default()));
        app.init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .insert_resource(RenderAssetBytesPerFrame {
                max_bytes: crate::config::EngineConfig::default().max_upload_bytes_per_frame(),
            });
        app.init_resource::<ProfilingState>()
            .init_resource::<StreamingMetrics>()
            .init_resource::<StreamingWorld>()
            .init_resource::<crate::streaming::TerrainContinuity>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ActiveSpace>()
            .init_resource::<MovementTuning>()
            .init_resource::<Teleports>()
            .insert_resource(MoveMode::Noclip)
            .insert_resource(CursorCapture::Captured)
            .insert_resource(RenderOrigin(IVec2::new(1, 1)))
            .insert_resource(crate::config::EngineConfig::default())
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
                DT,
            )))
            .add_plugins(DoorCrossingPlugin)
            .add_systems(Last, collect);
        app.world_mut().spawn((
            StreamingCamera,
            Transform::from_xyz(0.0, 0.0, 0.0).looking_to(Vec3::NEG_Z, Vec3::Y),
            GlobalTransform::default(),
        ));
        app.world_mut().spawn((
            door,
            ExpectedModelBounds::new(Vec3::splat(-50.0), Vec3::splat(50.0)).unwrap(),
            Transform::from_xyz(0.0, 0.0, -150.0),
            GlobalTransform::default(),
        ));
        let old_root = app.world_mut().spawn_empty().id();
        let old = CellKey::Exterior {
            worldspace_id: 1,
            grid_x: 1,
            grid_y: 1,
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(old, old_root);
        app.finish();
        app.update();
        (app, old_root)
    }

    fn press_e(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyE);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(KeyCode::KeyE);
    }

    fn alpha(app: &mut App) -> f32 {
        let mut query = app
            .world_mut()
            .query_filtered::<&BackgroundColor, With<FadeOverlay>>();
        query.single(app.world()).unwrap().0.to_srgba().alpha
    }

    fn run_until_black(app: &mut App) {
        for _ in 0..20 {
            if app.world().resource::<ActiveSpace>() != &ActiveSpace::default() {
                return;
            }
            app.update();
        }
        panic!("the space never switched");
    }

    #[test]
    fn e_at_a_door_starts_a_crossing_and_e_elsewhere_does_not() {
        let (mut app, _) = app_with(interior_door());
        // Turn away from the door: nothing starts.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.look_to(Vec3::Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        // Face it, but from beyond reach.
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            let mut camera = query.single_mut(app.world_mut()).unwrap();
            camera.translation = Vec3::new(0.0, 0.0, 400.0);
            camera.look_to(Vec3::NEG_Z, Vec3::Y);
        }
        press_e(&mut app);
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            query.single_mut(app.world_mut()).unwrap().translation = Vec3::ZERO;
        }
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
        // E while it runs does not start another.
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_space_switches_at_full_black_and_the_old_cells_go_the_same_frame() {
        let (mut app, old_root) = app_with(interior_door());
        press_e(&mut app);
        assert!(alpha(&mut app) < 1.0);
        run_until_black(&mut app);
        assert!(alpha(&mut app) >= 0.99, "the switch happens at full black");
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.interior, Some(77));
        assert!(app.world().get_entity(old_root).is_err());
        assert_eq!(
            app.world()
                .resource::<StreamingWorld>()
                .cell_count_for_test(),
            0
        );
        assert_eq!(
            *app.world().resource::<CameraSpace>(),
            CameraSpace::Interior
        );
        // An interior landing leaves the render origin alone.
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 1);
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 200.0, 300.0))
        );
    }

    #[test]
    fn an_exterior_landing_sets_the_render_origin_to_the_arrival_grid() {
        let (mut app, _) = app_with(exterior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(3, -2));
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.worldspace_id, Some(60));
        assert_eq!(space.interior, None);
        let teleports = &app.world().resource::<Teleports>().0;
        // 100 units into the cell on each axis, relative to the new origin: no cell offset.
        assert_eq!(
            teleports[0].position,
            creation_to_bevy(Vec3::new(100.0, 100.0, 50.0))
        );
    }

    #[test]
    fn the_fade_in_waits_for_the_landing_cell_and_starts_within_a_frame_of_it() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        for _ in 0..30 {
            app.update();
        }
        assert!(
            alpha(&mut app) >= 0.99,
            "still black while nothing is resident"
        );
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        // Pending work under the landing root keeps it black.
        let marker = app
            .world_mut()
            .spawn((pending_marker_for_test(), ChildOf(root)))
            .id();
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99, "pending work under the root holds");
        app.world_mut().entity_mut(marker).despawn();
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the fade-in starts once it is ready");
        for _ in 0..10 {
            app.update();
        }
        assert!(!app.world().resource::<DoorCrossing>().is_active());
        assert_eq!(alpha(&mut app), 0.0);
    }

    #[test]
    fn pending_work_in_another_cell_does_not_hold_the_fade_in() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        // The global counts and a marker under some other cell's root are not the landing cell's.
        {
            let mut metrics = app.world_mut().resource_mut::<StreamingMetrics>();
            metrics.pending_asset_instances = 50;
            metrics.pending_surface_instances = 50;
        }
        let other_root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .spawn((pending_marker_for_test(), ChildOf(other_root)));
        for _ in 0..4 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the fade-in started");
    }

    fn budget(app: &App) -> Option<usize> {
        app.world().resource::<RenderAssetBytesPerFrame>().max_bytes
    }

    fn configured_budget() -> Option<usize> {
        crate::config::EngineConfig::default().max_upload_bytes_per_frame()
    }

    #[test]
    fn the_upload_budget_is_lifted_at_the_switch_and_restored_at_the_fade_in() {
        let (mut app, _) = app_with(interior_door());
        assert!(configured_budget().is_some());
        press_e(&mut app);
        assert_eq!(budget(&app), configured_budget(), "kept while fading out");
        run_until_black(&mut app);
        assert_eq!(budget(&app), None, "lifted from the switch");
        let root = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        app.update();
        assert_eq!(budget(&app), None, "still lifted while the frames settle");
        for _ in 0..10 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0);
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn the_upload_budget_is_restored_when_the_destination_fails() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        assert_eq!(budget(&app), None);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        assert_eq!(
            budget(&app),
            None,
            "still black, still lifted while restoring"
        );
        // The player's own space fails too: the crossing fades in anyway.
        let own = CellKey::Exterior {
            worldspace_id: crate::config::EngineConfig::default().worldspace_id,
            grid_x: 1,
            grid_y: 1,
        };
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(own);
        app.update();
        app.update();
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn the_fade_in_waits_for_a_landing_texture_and_a_mesh_then_settles_two_frames() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let texture = app.world().resource::<Assets<Image>>().reserve_handle();
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial {
                base_color_texture: Some(texture.clone()),
                ..default()
            });
        let mesh = app.world().resource::<Assets<Mesh>>().reserve_handle();
        let root = app.world_mut().spawn_empty().id();
        app.world_mut().spawn((
            Mesh3d(mesh.clone()),
            MeshMaterial3d(material),
            ChildOf(root),
        ));
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_resident_for_test(CellKey::Interior(77), root);
        for _ in 0..20 {
            app.update();
        }
        assert!(
            alpha(&mut app) >= 0.99,
            "the texture and mesh are still loading"
        );
        assert_eq!(budget(&app), None);
        app.world_mut()
            .resource_mut::<Assets<Image>>()
            .insert(texture.id(), Image::default())
            .unwrap();
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99, "the mesh is still loading");
        app.world_mut()
            .resource_mut::<Assets<Mesh>>()
            .insert(
                mesh.id(),
                Mesh::new(
                    bevy::mesh::PrimitiveTopology::TriangleList,
                    bevy::asset::RenderAssetUsages::default(),
                ),
            )
            .unwrap();
        // Loaded on the first of these frames; the fade-in starts on the SETTLE_FRAMES-th loaded
        // frame (the second), and the fade itself shows from the frame after.
        app.update();
        assert!(alpha(&mut app) >= 0.99, "settling");
        assert_eq!(budget(&app), None);
        app.update();
        assert_eq!(budget(&app), configured_budget(), "the fade-in started");
        app.update();
        assert!(alpha(&mut app) < 1.0, "the fade shows");
    }

    #[test]
    fn the_timeout_fades_in_anyway() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let frames = (LANDING_TIMEOUT_SECONDS / DT) as usize;
        for _ in 0..frames - 5 {
            app.update();
        }
        assert!(alpha(&mut app) >= 0.99);
        for _ in 0..20 {
            app.update();
        }
        assert!(alpha(&mut app) < 1.0, "the timeout started the fade-in");
        assert_eq!(budget(&app), configured_budget());
    }

    #[test]
    fn a_destination_that_fails_puts_the_player_back_where_they_were() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space, ActiveSpace::default());
        assert_eq!(app.world().resource::<RenderOrigin>().0, IVec2::new(1, 1));
        let teleports = &app.world().resource::<Teleports>().0;
        assert_eq!(teleports.len(), 2);
        let back = teleports[1].position;
        assert!(back.x.abs() < 0.01 && back.z.abs() < 0.01);
    }

    #[test]
    fn the_restore_pose_survives_a_render_origin_rebase_during_the_fade_out() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        // A rebase while fading out: the origin moves, the stored pose must not.
        app.world_mut().resource_mut::<RenderOrigin>().0 = IVec2::new(5, 5);
        run_until_black(&mut app);
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(CellKey::Interior(77));
        app.update();
        app.update();
        let origin = app.world().resource::<RenderOrigin>().0;
        let teleports = &app.world().resource::<Teleports>().0;
        let back = teleports.last().unwrap().position;
        // The player stood at render (0, _, 0) under origin (1, 1): the same world place now.
        let world_x = back.x + origin.x as f32 * CELL_SIZE;
        let world_z = back.z - origin.y as f32 * CELL_SIZE;
        assert!((world_x - CELL_SIZE).abs() < 0.01, "x {world_x}");
        assert!((world_z + CELL_SIZE).abs() < 0.01, "z {world_z}");
        let tuning = MovementTuning::default();
        let feet_y = -tuning.eye_height - (tuning.capsule_half_height() + tuning.capsule_radius);
        assert!((back.y - feet_y).abs() < 0.01);
    }

    #[test]
    fn a_failed_crossing_from_an_interior_lands_where_the_player_stood() {
        // Start inside interior 5 with a non-zero render origin that interiors do not use.
        let (mut app, _) = app_with(exterior_door());
        *app.world_mut().resource_mut::<ActiveSpace>() = ActiveSpace {
            worldspace_id: Some(60),
            interior: Some(5),
        };
        *app.world_mut().resource_mut::<CameraSpace>() = CameraSpace::Interior;
        app.world_mut().resource_mut::<RenderOrigin>().0 = IVec2::new(7, -3);
        {
            let mut query = app
                .world_mut()
                .query_filtered::<&mut Transform, With<StreamingCamera>>();
            query.single_mut(app.world_mut()).unwrap().translation = Vec3::new(10.0, 0.0, 0.0);
        }
        press_e(&mut app);
        let door_target = CellKey::Exterior {
            worldspace_id: 60,
            grid_x: 3,
            grid_y: -2,
        };
        for _ in 0..20 {
            if app.world().resource::<ActiveSpace>().interior.is_none() {
                break;
            }
            app.update();
        }
        app.world_mut()
            .resource_mut::<StreamingWorld>()
            .set_failed_for_test(door_target);
        app.update();
        app.update();
        let space = *app.world().resource::<ActiveSpace>();
        assert_eq!(space.interior, Some(5), "back in the interior");
        let tuning = MovementTuning::default();
        let (body_offset, _) = body_and_camera_for_feet(&tuning, Vec3::ZERO);
        let expected = Vec3::new(10.0, 0.0, 0.0) - Vec3::Y * tuning.eye_height - body_offset;
        let back = app
            .world()
            .resource::<Teleports>()
            .0
            .last()
            .unwrap()
            .position;
        assert!(
            (back - expected).length() < 0.01,
            "restored to {back}, expected {expected}"
        );
    }

    #[test]
    fn a_door_without_model_bounds_can_still_be_activated() {
        let (mut app, _) = app_with(interior_door());
        let mut doors = app.world_mut().query_filtered::<Entity, With<LoadDoor>>();
        let door = doors.single(app.world()).unwrap();
        app.world_mut()
            .entity_mut(door)
            .remove::<ExpectedModelBounds>();
        press_e(&mut app);
        assert!(app.world().resource::<DoorCrossing>().is_active());
    }

    #[test]
    fn the_teleport_target_is_the_arrival_feet_position() {
        let (mut app, _) = app_with(interior_door());
        press_e(&mut app);
        run_until_black(&mut app);
        let tuning = MovementTuning::default();
        let (_, eye) =
            body_and_camera_for_feet(&tuning, creation_to_bevy(Vec3::new(100.0, 200.0, 300.0)));
        let mut view = app
            .world_mut()
            .query_filtered::<&Transform, With<StreamingCamera>>();
        assert!((view.single(app.world()).unwrap().translation - eye).length() < 1e-3);
    }

    #[test]
    fn a_ray_tests_the_oriented_bounds() {
        let bounds =
            ExpectedModelBounds::new(Vec3::new(-10.0, 0.0, -2.0), Vec3::new(10.0, 40.0, 2.0))
                .unwrap();
        let door = GlobalTransform::from(
            Transform::from_xyz(0.0, 0.0, -100.0).with_rotation(Quat::from_rotation_y(0.5)),
        );
        let hit = ray_hits_door(
            Vec3::new(0.0, 20.0, 0.0),
            Vec3::NEG_Z,
            &door,
            &bounds,
            200.0,
        );
        assert!(hit.is_some_and(|distance| (distance - 98.0).abs() < 5.0));
        assert!(ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::Z, &door, &bounds, 200.0).is_none());
        assert!(
            ray_hits_door(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Z, &door, &bounds, 50.0).is_none()
        );
    }
}
