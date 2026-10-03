//! #104 phase (a): rigid-body dynamics extracted from NIFs into the GLB collision extras.

use converter::mesh::MeshConverter;
use dummy_content::nif::{BoxBody, StaticShape, static_shape_with_bodies};
use shared::collision::{BodyKind, COLLISION_ASSET_VERSION, CollisionAsset, CollisionShape};
use std::{fs, path::Path};

const QUAD: StaticShape<'static> = StaticShape {
    name: "PhysicsQuad",
    positions: &[
        [-1.0, -1.0, 0.0],
        [1.0, -1.0, 0.0],
        [1.0, 1.0, 0.0],
        [-1.0, 1.0, 0.0],
    ],
    normals: &[[0.0, 0.0, 1.0]; 4],
    uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
    indices: &[[0, 1, 2], [0, 2, 3]],
    diffuse: "textures/generated_color.dds",
    normal_texture: "textures/generated_normal.dds",
};

fn crate_body() -> BoxBody<'static> {
    BoxBody {
        node_name: "Crate",
        half_extents: [0.1, 0.2, 0.3],
        transform: None,
        collision_layer: 4,
        motion_system: 4, // MO_SYS_BOX_INERTIA
        deactivator_type: 1,
        quality_type: 4, // MO_QUAL_MOVING
        mass: 2.5,
        inertia: [0.1, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0, 0.3],
        center_of_mass: [0.01, 0.02, 0.03],
        linear_damping: 0.1,
        angular_damping: 0.05,
        friction: 0.5,
        restitution: 0.4,
        max_linear_velocity: 104.4,
        max_angular_velocity: 31.57,
    }
}

fn wall_body() -> BoxBody<'static> {
    BoxBody {
        node_name: "Wall",
        half_extents: [1.0, 1.0, 1.0],
        transform: None,
        collision_layer: 1,
        motion_system: 7, // MO_SYS_FIXED
        deactivator_type: 1,
        quality_type: 1, // MO_QUAL_FIXED
        mass: 0.0,
        inertia: [0.0; 9],
        center_of_mass: [0.0; 3],
        linear_damping: 0.1,
        angular_damping: 0.05,
        friction: 0.5,
        restitution: 0.4,
        max_linear_velocity: 104.4,
        max_angular_velocity: 31.57,
    }
}

/// Converts a NIF with the given bodies and returns the GLB's glTF JSON and collision extras.
fn convert(bodies: &[BoxBody<'_>]) -> (serde_json::Value, CollisionAsset) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("bodies.nif");
    let output = directory.path().join("bodies.glb");
    fs::write(&input, static_shape_with_bodies(&QUAD, bodies).unwrap()).unwrap();
    MeshConverter::convert_nif_to_glb(&input, &output).unwrap();
    read_glb(&output)
}

fn read_glb(path: &Path) -> (serde_json::Value, CollisionAsset) {
    let bytes = fs::read(path).unwrap();
    let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let json: serde_json::Value = serde_json::from_slice(&bytes[20..20 + json_len]).unwrap();
    let asset =
        serde_json::from_value(json["scenes"][0]["extras"]["openSkyrimCollision"].clone()).unwrap();
    (json, asset)
}

fn assert_close(actual: &[f32], expected: &[f32], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}");
    for (a, e) in actual.iter().zip(expected) {
        assert!(
            (a - e).abs() <= 1.0e-3 * e.abs().max(1.0),
            "{what}: {actual:?} != {expected:?}"
        );
    }
}

#[test]
fn dynamic_and_fixed_box_bodies_round_trip_with_units() {
    let (gltf, asset) = convert(&[crate_body(), wall_body()]);
    assert_eq!(asset.version, COLLISION_ASSET_VERSION);
    assert!(asset.skipped.is_empty(), "{:?}", asset.skipped);
    assert_eq!(asset.shapes.len(), 2);
    let [dynamic, fixed] = asset.bodies.as_slice() else {
        panic!("expected two bodies, got {:?}", asset.bodies);
    };

    // glTF nodes: root, shape, then one node per body, in order.
    assert_eq!((dynamic.node, fixed.node), (2, 3));
    assert_eq!(gltf["nodes"][2]["name"], "Crate");
    assert_eq!(gltf["nodes"][3]["name"], "Wall");
    assert_eq!(
        (dynamic.target.as_str(), fixed.target.as_str()),
        ("Crate", "Wall")
    );
    assert_eq!(
        (dynamic.shapes.as_slice(), fixed.shapes.as_slice()),
        (&[0][..], &[1][..])
    );

    assert_eq!(dynamic.kind, BodyKind::Dynamic);
    assert_eq!(
        (
            dynamic.havok.motion_system,
            dynamic.havok.quality_type,
            dynamic.havok.deactivator_type,
            dynamic.havok.collision_layer
        ),
        (4, 4, 1, 4)
    );
    assert_eq!(dynamic.mass, 2.5);
    // Diagonal Havok tensor (0.1, 0.2, 0.3) x 70^2, with Creation y and z swapped into the
    // runtime basis (runtime y = Creation z, runtime z = -Creation y).
    assert_close(
        &dynamic.inertia,
        &[490.0, 0.0, 0.0, 0.0, 1470.0, 0.0, 0.0, 0.0, 980.0],
        "inertia",
    );
    assert_close(&dynamic.center_of_mass, &[0.7, 2.1, -1.4], "center of mass");
    assert_close(
        &[dynamic.linear_damping, dynamic.angular_damping],
        &[0.1, 0.05],
        "damping",
    );
    assert_close(
        &[dynamic.friction, dynamic.restitution],
        &[0.5, 0.4],
        "surface",
    );
    assert_close(
        &[dynamic.max_linear_velocity],
        &[104.4 * 70.0],
        "max linear velocity",
    );
    assert_close(
        &[dynamic.max_angular_velocity],
        &[31.57],
        "max angular velocity",
    );
    assert!(dynamic.convex);

    assert_eq!(fixed.kind, BodyKind::Fixed);
    assert_eq!(
        (
            fixed.havok.motion_system,
            fixed.havok.quality_type,
            fixed.havok.collision_layer
        ),
        (7, 1, 1)
    );
    assert_eq!(fixed.mass, 0.0);
    assert!(fixed.convex);

    // The same extras survive a JSON round trip unchanged.
    let json = serde_json::to_value(&asset).unwrap();
    assert_eq!(json["bodies"][0]["kind"], "dynamic");
    assert_eq!(json["bodies"][1]["kind"], "fixed");
}

#[test]
fn rotated_rigid_body_t_rotates_the_tensor_and_moves_the_center_into_the_shape_frame() {
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let mut body = crate_body();
    body.transform = Some(([1.0, 0.0, 0.0], [0.0, 0.0, half, half])); // 90 degrees about Z
    body.half_extents = [0.1, 0.1, 0.1];
    body.inertia = [1.0, 0.5, 0.0, 0.5, 2.0, 0.0, 0.0, 0.0, 3.0];
    body.center_of_mass = [1.0, 0.0, 0.0];
    let (_, asset) = convert(&[body]);
    assert!(asset.skipped.is_empty(), "{:?}", asset.skipped);
    let [body] = asset.bodies.as_slice() else {
        panic!("expected one body");
    };
    // Rz(90) maps x -> y and y -> -x, so R I R^T = [[2, -0.5, 0], [-0.5, 1, 0], [0, 0, 3]] in
    // Creation axes. The runtime basis then maps (x, y, z) to (x, z, -y), giving
    // [[2, 0, 0.5], [0, 3, 0], [0.5, 0, 1]]; all scaled by 70^2.
    assert_close(
        &body.inertia,
        &[9800.0, 0.0, 2450.0, 0.0, 14700.0, 0.0, 2450.0, 0.0, 4900.0],
        "rotated inertia",
    );
    // Local centre (70, 0, 0) turns to (0, 70, 0), then the body translation adds (70, 0, 0).
    assert_close(
        &body.center_of_mass,
        &[70.0, 0.0, -70.0],
        "moved center of mass",
    );
    // The box hull sits at the same body origin, (70, 0, 0) in Creation axes.
    let CollisionShape::Hull { points } = &asset.shapes[0] else {
        panic!("expected a hull");
    };
    let centroid: Vec<f32> = (0..3)
        .map(|axis| points.iter().map(|p| p[axis]).sum::<f32>() / points.len() as f32)
        .collect();
    assert_close(&centroid, &[70.0, 0.0, 0.0], "shape centroid");
}

#[test]
fn bodies_on_nodes_sharing_a_name_keep_their_own_node() {
    // Both body nodes are named "Crate" in the written GLB (vanilla meshes reuse names). The
    // GLB was built from this NIF, so each body stays on the node at its own index.
    let (gltf, asset) = convert(&[crate_body(), crate_body()]);
    assert_eq!(gltf["nodes"][2]["name"], "Crate");
    assert_eq!(gltf["nodes"][3]["name"], "Crate");
    assert!(asset.skipped.is_empty(), "{:?}", asset.skipped);
    let nodes: Vec<u32> = asset.bodies.iter().map(|body| body.node).collect();
    assert_eq!(nodes, [2, 3]);
}

#[test]
fn version_one_extras_without_bodies_still_deserialise() {
    let asset: CollisionAsset = serde_json::from_str(
        r#"{"version":1,"authored":true,"shapes":[],"skipped":["block 3: unsupported"]}"#,
    )
    .unwrap();
    assert_eq!(asset.version, 1);
    assert!(asset.bodies.is_empty());
}

#[test]
fn unknown_body_fields_are_ignored() {
    let asset: CollisionAsset = serde_json::from_str(
        r#"{"version":2,"authored":true,"shapes":[],"skipped":[],"bodies":[{
            "node":7,"target":"Box01","shapes":[0,1],"kind":"dynamic",
            "havok":{"motion_system":3,"quality_type":4,"deactivator_type":1,
                     "collision_layer":4,"future":9},
            "mass":2.5,"inertia":[1,0,0,0,1,0,0,0,1],"center_of_mass":[0,1,2],
            "linear_damping":0.1,"angular_damping":0.05,"friction":0.5,"restitution":0.4,
            "max_linear_velocity":7000.0,"max_angular_velocity":31.57,"convex":true,
            "constraints":[{"kind":"hinge"}]
        }]}"#,
    )
    .unwrap();
    let [body] = asset.bodies.as_slice() else {
        panic!("expected one body");
    };
    assert_eq!((body.kind, body.node), (BodyKind::Dynamic, 7));
    assert_eq!(body.shapes, [0, 1]);
}
