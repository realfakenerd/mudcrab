//! Collision data carried in converted GLB scene extras.

use serde::{Deserialize, Serialize};

/// Version 2 adds [`CollisionAsset::bodies`]; a version 1 asset has none and every shape is fixed.
pub const COLLISION_ASSET_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollisionAsset {
    pub version: u32,
    /// `true` distinguishes an authored absence from an older GLB with no collision metadata.
    pub authored: bool,
    pub shapes: Vec<CollisionShape>,
    /// Unsupported blocks are retained so coverage can identify missing collision.
    pub skipped: Vec<String>,
    /// Rigid-body dynamics per collision object (#104 phase a). Absent in a version 1 asset.
    /// Readers ignore unknown fields so later phases (constraints) can extend a body.
    #[serde(default)]
    pub bodies: Vec<CollisionBody>,
}

/// How the runtime should simulate a [`CollisionBody`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyKind {
    Fixed,
    Keyframed,
    Dynamic,
}

/// Raw Havok values, kept for faithfulness and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HavokBodyInfo {
    pub motion_system: u8,
    pub quality_type: u8,
    pub deactivator_type: u8,
    pub collision_layer: u8,
}

/// One NIF rigid body (`bhkRigidBody`/`bhkRigidBodyT`) and the shapes it owns.
///
/// `center_of_mass` and `inertia` are in the same frame as the listed shapes: the shapes'
/// runtime basis with the node and body transforms already applied. Lengths are in Creation
/// units (Havok x 70), inertia in kg x Creation units squared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollisionBody {
    /// glTF node index, in the same GLB, that the `bhkCollisionObject` targets.
    pub node: u32,
    /// NIF node name, for logs and debugging only.
    pub target: String,
    /// Indices into [`CollisionAsset::shapes`] that belong to this body.
    pub shapes: Vec<u32>,
    pub kind: BodyKind,
    pub havok: HavokBodyInfo,
    /// Kilograms, as stored.
    pub mass: f32,
    /// Symmetric 3x3 tensor, row-major.
    pub inertia: [f32; 9],
    pub center_of_mass: [f32; 3],
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub friction: f32,
    pub restitution: f32,
    pub max_linear_velocity: f32,
    pub max_angular_velocity: f32,
    /// `true` only when every shape is a box, capsule or convex-vertex hull (sphere shapes are
    /// unsupported and skipped).
    pub convex: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CollisionShape {
    Mesh {
        vertices: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    },
    Capsule {
        a: [f32; 3],
        b: [f32; 3],
        radius: f32,
    },
    Box {
        center: [f32; 3],
        half_extents: [f32; 3],
    },
    Hull {
        points: Vec<[f32; 3]>,
    },
}
