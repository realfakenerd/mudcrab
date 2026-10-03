//! Shared scene-linear composition for world views and diagnostic fixtures.
//!
//! These fixed settings preserve the previous Bevy defaults; they are not a recovered Skyrim
//! image space. HDR moves the display transform after sky, fog and transparent composition.

use bevy::{
    camera::{Exposure, Hdr},
    core_pipeline::tonemapping::Tonemapping,
    prelude::*,
};

/// Pinned Bevy exposure baseline; not a calibrated Skyrim image-space value.
pub const DEFAULT_SCENE_EV100: f32 = 9.7;

/// Explicit camera settings for scene composition. A reflection uses the same exposure but no
/// display transform: the receiving water surface joins it to the main view before tone mapping.
#[derive(Bundle)]
pub struct SceneColorPipeline {
    pub hdr: Hdr,
    pub exposure: Exposure,
    pub tonemapping: Tonemapping,
}

impl Default for SceneColorPipeline {
    fn default() -> Self {
        Self {
            hdr: Hdr,
            exposure: Exposure {
                ev100: DEFAULT_SCENE_EV100,
            },
            tonemapping: Tonemapping::TonyMcMapface,
        }
    }
}

impl SceneColorPipeline {
    /// Compose reflected radiance without applying the main view's display transform.
    pub fn reflection() -> Self {
        Self {
            tonemapping: Tonemapping::None,
            ..default()
        }
    }
}

#[cfg(test)]
pub(crate) fn assert_scene_camera_output(world: &mut World, expected_count: usize) {
    let mut cameras = world
        .query_filtered::<(Option<&Hdr>, Option<&Exposure>, Option<&Tonemapping>), With<Camera3d>>(
        );
    let mut count = 0;
    for (hdr, exposure, tonemapping) in cameras.iter(world) {
        assert!(hdr.is_some(), "scene camera must compose in HDR");
        assert_eq!(exposure.unwrap().ev100, DEFAULT_SCENE_EV100);
        assert_eq!(*tonemapping.unwrap(), Tonemapping::TonyMcMapface);
        count += 1;
    }
    assert_eq!(count, expected_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_views_compose_in_hdr_with_explicit_output_settings() {
        let mut world = World::new();
        let entity = world.spawn(SceneColorPipeline::default()).id();
        assert!(world.get::<Hdr>(entity).is_some());
        assert_eq!(world.get::<Exposure>(entity).unwrap().ev100, 9.7);
        assert_eq!(
            *world.get::<Tonemapping>(entity).unwrap(),
            Tonemapping::TonyMcMapface
        );
    }

    #[test]
    fn reflection_keeps_scene_exposure_without_a_display_transform() {
        let scene = SceneColorPipeline::default();
        let reflection = SceneColorPipeline::reflection();
        assert_eq!(reflection.exposure.ev100, scene.exposure.ev100);
        assert_eq!(reflection.tonemapping, Tonemapping::None);
    }
}
