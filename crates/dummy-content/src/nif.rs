//! Deterministic NIF static-shape fixtures for Skyrim SE (`20.2.0.7`).
//!
//! The writer emits the minimal block set the converter renders:
//! `BSFadeNode` → `BSTriShape` → `BSLightingShaderProperty` → `BSShaderTextureSet`.
//! Geometry is validated before serialization and the output is byte-stable
//! for identical input.

use crate::bytes::{push_u16, push_u32, push_u64};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

const NIF_VERSION: u32 = 0x1402_0007;
const USER_VERSION: u32 = 12;
const BETHESDA_VERSION: u32 = 100;
const NULL_REF: u32 = u32::MAX;
const SHADER_TYPE_DEFAULT: u32 = 0;
const VERTEX_FLAGS: u16 = 0x0001 | 0x0002 | 0x0008;
const VERTEX_STRIDE: u8 = 6;

/// A triangle mesh rendered as a single static shape.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticShape<'a> {
    /// Shape name stored in the NIF string table.
    pub name: &'a str,
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Per-vertex normals; must match `positions`.
    pub normals: &'a [[f32; 3]],
    /// Per-vertex UVs; must match `positions`.
    pub uvs: &'a [[f32; 2]],
    /// Triangle indices into `positions`.
    pub indices: &'a [[u16; 3]],
    /// Diffuse texture path, for example `textures/generated_color.dds`.
    pub diffuse: &'a str,
    /// Normal texture path.
    pub normal_texture: &'a str,
}

/// A Havok box rigid body attached to its own `NiNode` under the root, written as a
/// `bhkCollisionObject` -> `bhkRigidBody` (or `bhkRigidBodyT`) -> `bhkBoxShape` chain.
/// Every value is stored raw, in Havok units (a Creation unit is 1/70 of one).
#[derive(Debug, Clone, PartialEq)]
pub struct BoxBody<'a> {
    /// Name of the `NiNode` the collision object targets.
    pub node_name: &'a str,
    /// Box half extents (`bhkBoxShape` dimensions).
    pub half_extents: [f32; 3],
    /// `Some((translation, rotation xyzw))` writes a `bhkRigidBodyT`, else a `bhkRigidBody`.
    pub transform: Option<([f32; 3], [f32; 4])>,
    pub collision_layer: u8,
    /// Raw `hkMotionType`, `hkDeactivatorType` and `hkQualityType` values (nif.xml).
    pub motion_system: u8,
    pub deactivator_type: u8,
    pub quality_type: u8,
    pub mass: f32,
    /// Row-major inertia tensor.
    pub inertia: [f32; 9],
    pub center_of_mass: [f32; 3],
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub friction: f32,
    pub restitution: f32,
    pub max_linear_velocity: f32,
    pub max_angular_velocity: f32,
}

/// Generates a minimal static NIF containing one triangle mesh.
pub fn static_shape(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    static_shape_with_bodies(shape, &[])
}

/// Like [`static_shape`], plus one collision body per entry. Each body hangs off its own
/// `NiNode` child of the root, so the converter's glTF node order is root, shape, then the
/// body nodes in order.
pub fn static_shape_with_bodies(
    shape: &StaticShape<'_>,
    bodies: &[BoxBody<'_>],
) -> Result<Vec<u8>> {
    validate(shape)?;
    for body in bodies {
        validate_body(body)?;
    }
    let mut strings = vec![shape.name];
    strings.extend(bodies.iter().map(|body| body.node_name));
    let first_body_block = 4_u32;
    let mut children = vec![1_u32];
    children.extend((0..bodies.len()).map(|i| first_body_block + 4 * i as u32));
    let mut blocks = vec![
        fade_node(&children),
        triangle_shape(shape)?,
        lighting_shader_property(),
        texture_set(shape)?,
    ];
    let mut block_types = vec![
        "BSFadeNode",
        "BSTriShape",
        "BSLightingShaderProperty",
        "BSShaderTextureSet",
    ];
    for (index, body) in bodies.iter().enumerate() {
        let node_block = first_body_block + 4 * index as u32;
        let mut node = Vec::new();
        push_av_object_with(&mut node, 1 + index as u32, node_block + 1);
        push_u32(&mut node, 0); // children
        push_u32(&mut node, 0); // effects
        blocks.push(node);
        block_types.push("NiNode");
        let mut object = Vec::new();
        push_u32(&mut object, node_block); // target
        push_u16(&mut object, 1); // flags
        push_u32(&mut object, node_block + 2); // body
        blocks.push(object);
        block_types.push("bhkCollisionObject");
        blocks.push(rigid_body(body, node_block + 3));
        block_types.push(if body.transform.is_some() {
            "bhkRigidBodyT"
        } else {
            "bhkRigidBody"
        });
        blocks.push(box_shape(body));
        block_types.push("bhkBoxShape");
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"Gamebryo File Format, Version 20.2.0.7\n");
    push_u32(&mut bytes, NIF_VERSION);
    bytes.push(1);
    push_u32(&mut bytes, USER_VERSION);
    push_u32(
        &mut bytes,
        u32::try_from(blocks.len()).map_err(|_| eyre!("NIF block count overflow"))?,
    );
    push_u32(&mut bytes, BETHESDA_VERSION);
    push_string8(&mut bytes, "OpenSkyrim dummy-content");
    push_string8(&mut bytes, "");
    push_string8(&mut bytes, "");
    push_u16(
        &mut bytes,
        u16::try_from(block_types.len()).map_err(|_| eyre!("NIF block type overflow"))?,
    );
    for block_type in block_types {
        push_u32(
            &mut bytes,
            u32::try_from(block_type.len()).map_err(|_| eyre!("NIF block type overflow"))?,
        );
        bytes.extend_from_slice(block_type.as_bytes());
    }
    for index in 0..blocks.len() {
        push_u16(
            &mut bytes,
            u16::try_from(index).map_err(|_| eyre!("NIF block index overflow"))?,
        );
    }
    for block in &blocks {
        push_u32(
            &mut bytes,
            u32::try_from(block.len()).map_err(|_| eyre!("NIF block size overflow"))?,
        );
    }
    push_u32(
        &mut bytes,
        u32::try_from(strings.len()).map_err(|_| eyre!("NIF string count overflow"))?,
    );
    push_u32(&mut bytes, max_string_length(&strings));
    for value in strings {
        push_u32(
            &mut bytes,
            u32::try_from(value.len()).map_err(|_| eyre!("NIF string overflow"))?,
        );
        bytes.extend_from_slice(value.as_bytes());
    }
    push_u32(&mut bytes, 0);
    for block in blocks {
        bytes.extend_from_slice(&block);
    }
    Ok(bytes)
}

fn validate(shape: &StaticShape<'_>) -> Result<()> {
    ensure!(!shape.name.is_empty(), "NIF shape name is empty");
    ensure!(
        shape.name.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "NIF shape name is not printable ASCII: {:?}",
        shape.name
    );
    ensure!(!shape.positions.is_empty(), "NIF shape has no positions");
    ensure!(
        shape.positions.len() == shape.normals.len(),
        "NIF shape has {} positions and {} normals",
        shape.positions.len(),
        shape.normals.len()
    );
    ensure!(
        shape.positions.len() == shape.uvs.len(),
        "NIF shape has {} positions and {} UVs",
        shape.positions.len(),
        shape.uvs.len()
    );
    ensure!(
        shape.positions.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 vertices"
    );
    ensure!(
        shape.indices.len() <= u16::MAX as usize,
        "NIF shape exceeds 65535 triangles"
    );
    ensure!(!shape.indices.is_empty(), "NIF shape has no triangles");
    ensure!(
        shape
            .positions
            .iter()
            .all(|position| position.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite position"
    );
    ensure!(
        shape
            .uvs
            .iter()
            .all(|uv| uv.iter().all(|value| value.is_finite())),
        "NIF shape contains a non-finite UV"
    );
    let vertex_count = shape.positions.len();
    ensure!(
        shape
            .indices
            .iter()
            .flatten()
            .all(|index| { usize::from(*index) < vertex_count }),
        "NIF shape contains an out-of-range triangle index"
    );
    ensure!(
        !shape.diffuse.is_empty(),
        "NIF shape needs a diffuse texture"
    );
    for texture in [shape.diffuse, shape.normal_texture] {
        ensure!(
            texture.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
            "NIF texture path is not printable ASCII: {texture:?}"
        );
    }
    Ok(())
}

fn fade_node(children: &[u32]) -> Vec<u8> {
    let mut block = Vec::with_capacity(84);
    push_av_object(&mut block, NULL_REF);
    push_u32(&mut block, children.len() as u32);
    for child in children {
        push_u32(&mut block, *child);
    }
    push_u32(&mut block, 0);
    block
}

fn validate_body(body: &BoxBody<'_>) -> Result<()> {
    ensure!(
        !body.node_name.is_empty()
            && body
                .node_name
                .bytes()
                .all(|byte| (0x20..0x7f).contains(&byte)),
        "NIF body node name must be printable ASCII"
    );
    ensure!(
        body.half_extents.iter().all(|v| v.is_finite() && *v > 0.0),
        "NIF box body needs positive half extents"
    );
    let finite = body
        .inertia
        .iter()
        .chain(&body.center_of_mass)
        .all(|v| v.is_finite())
        && [
            body.mass,
            body.linear_damping,
            body.angular_damping,
            body.friction,
            body.restitution,
            body.max_linear_velocity,
            body.max_angular_velocity,
        ]
        .iter()
        .all(|v| v.is_finite());
    ensure!(finite, "NIF body contains a non-finite value");
    Ok(())
}

/// `bhkRigidBody` block: `bhkWorldObject`, `bhkEntityCInfo`, then `bhkRigidBodyCInfo2010`
/// (nif.xml, Skyrim), then constraints and body flags.
fn rigid_body(body: &BoxBody<'_>, shape: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(250);
    push_u32(&mut b, shape);
    b.extend_from_slice(&[body.collision_layer, 0, 0, 0]); // havok filter
    b.extend_from_slice(&[0; 20]); // world object info
    b.extend_from_slice(&[1, 0, 0xff, 0xff]); // entity info
    b.extend_from_slice(&[0; 4]); // unused
    b.extend_from_slice(&[body.collision_layer, 0, 0, 0]); // havok filter
    b.extend_from_slice(&[0; 4]); // unused
    push_u32(&mut b, 0); // unknown int
    b.extend_from_slice(&[1, 0, 0xff, 0xff]); // response, unused, callback delay
    let (translation, rotation) = body.transform.unwrap_or(([0.0; 3], [0.0, 0.0, 0.0, 1.0]));
    for value in translation.into_iter().chain([0.0]) {
        push_f32(&mut b, value);
    }
    for value in rotation {
        push_f32(&mut b, value);
    }
    b.extend_from_slice(&[0; 32]); // linear and angular velocity
    for row in body.inertia.chunks(3) {
        for value in row.iter().copied().chain([0.0]) {
            push_f32(&mut b, value);
        }
    }
    for value in body.center_of_mass.into_iter().chain([0.0]) {
        push_f32(&mut b, value);
    }
    for value in [
        body.mass,
        body.linear_damping,
        body.angular_damping,
        1.0, // time factor
        1.0, // gravity factor
        body.friction,
        0.0, // rolling friction multiplier
        body.restitution,
        body.max_linear_velocity,
        body.max_angular_velocity,
        0.15, // penetration depth
    ] {
        push_f32(&mut b, value);
    }
    b.extend_from_slice(&[
        body.motion_system,
        body.deactivator_type,
        1, // solver deactivation off
        body.quality_type,
        0, // auto remove level
        0, // response modifier flags
        3, // shape keys in contact point
        0, // force collided onto PPU
    ]);
    b.extend_from_slice(&[0; 12]);
    push_u32(&mut b, 0); // constraints
    push_u16(&mut b, 0); // body flags
    b
}

fn box_shape(body: &BoxBody<'_>) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    push_u32(&mut b, 0); // material
    push_f32(&mut b, 0.0); // radius
    b.extend_from_slice(&[0; 8]);
    for value in body.half_extents {
        push_f32(&mut b, value);
    }
    push_f32(&mut b, 0.0);
    b
}

fn triangle_shape(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    let vertex_stride = usize::from(VERTEX_STRIDE) * 4;
    let vertex_bytes = shape
        .positions
        .len()
        .checked_mul(vertex_stride)
        .ok_or_else(|| eyre!("NIF vertex data overflow"))?;
    let triangle_bytes = shape
        .indices
        .len()
        .checked_mul(6)
        .ok_or_else(|| eyre!("NIF triangle data overflow"))?;
    let data_size = vertex_bytes
        .checked_add(triangle_bytes)
        .ok_or_else(|| eyre!("NIF geometry size overflow"))?;

    let (center, radius) = bounds(shape.positions);
    let mut block = Vec::with_capacity(72 + 16 + 24 + data_size);
    push_av_object(&mut block, 0);
    for value in center {
        block.extend_from_slice(&value.to_le_bytes());
    }
    block.extend_from_slice(&radius.to_le_bytes());
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 2);
    push_u32(&mut block, NULL_REF);
    let descriptor =
        u64::from(VERTEX_STRIDE) | (4 << 8) | (5 << 16) | (u64::from(VERTEX_FLAGS) << 44);
    push_u64(&mut block, descriptor);
    push_u16(
        &mut block,
        u16::try_from(shape.indices.len()).map_err(|_| eyre!("NIF triangle count overflow"))?,
    );
    push_u16(
        &mut block,
        u16::try_from(shape.positions.len()).map_err(|_| eyre!("NIF vertex count overflow"))?,
    );
    push_u32(
        &mut block,
        u32::try_from(data_size).map_err(|_| eyre!("NIF geometry size overflow"))?,
    );
    for (position, (normal, uv)) in shape
        .positions
        .iter()
        .zip(shape.normals.iter().zip(shape.uvs.iter()))
    {
        for value in position {
            block.extend_from_slice(&value.to_le_bytes());
        }
        block.extend_from_slice(&0.0f32.to_le_bytes());
        block.extend_from_slice(&encode_half(uv[0]).to_le_bytes());
        block.extend_from_slice(&encode_half(uv[1]).to_le_bytes());
        block.push(pack_normal(normal[0]));
        block.push(pack_normal(normal[1]));
        block.push(pack_normal(normal[2]));
        block.push(0);
    }
    for triangle in shape.indices {
        for index in triangle {
            block.extend_from_slice(&index.to_le_bytes());
        }
    }
    Ok(block)
}

fn lighting_shader_property() -> Vec<u8> {
    let mut block = Vec::with_capacity(100);
    push_u32(&mut block, SHADER_TYPE_DEFAULT);
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 0);
    push_u32(&mut block, NULL_REF);
    push_u32(&mut block, 0);
    push_u32(&mut block, 0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_u32(&mut block, 3);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 1.0);
    push_u32(&mut block, 0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 0.0);
    push_f32(&mut block, 80.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 1.0);
    push_f32(&mut block, 0.3);
    push_f32(&mut block, 2.0);
    debug_assert_eq!(block.len(), 100);
    block
}

fn texture_set(shape: &StaticShape<'_>) -> Result<Vec<u8>> {
    let slots = [
        shape.diffuse,
        shape.normal_texture,
        "",
        "",
        "",
        "",
        "",
        "",
        "",
    ];
    let mut block = Vec::new();
    push_u32(
        &mut block,
        u32::try_from(slots.len()).map_err(|_| eyre!("NIF texture slot overflow"))?,
    );
    for slot in slots {
        push_u32(
            &mut block,
            u32::try_from(slot.len()).map_err(|_| eyre!("NIF texture path overflow"))?,
        );
        block.extend_from_slice(slot.as_bytes());
    }
    Ok(block)
}

fn push_av_object(out: &mut Vec<u8>, name: u32) {
    push_av_object_with(out, name, NULL_REF);
}

fn push_av_object_with(out: &mut Vec<u8>, name: u32, collision_object: u32) {
    push_u32(out, name);
    push_u32(out, NULL_REF);
    push_u32(out, NULL_REF);
    push_u32(out, 0);
    for value in [0.0f32, 0.0, 0.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&1.0f32.to_le_bytes());
    push_u32(out, collision_object);
}

fn push_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_string8(out: &mut Vec<u8>, value: &str) {
    out.push((value.len() + 1) as u8);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

fn max_string_length(strings: &[&str]) -> u32 {
    strings
        .iter()
        .map(|value| u32::try_from(value.len()).unwrap_or(u32::MAX))
        .max()
        .unwrap_or(0)
}

fn pack_normal(value: f32) -> u8 {
    let scaled = ((value + 1.0) * 0.5 * 255.0).round();
    scaled.clamp(0.0, 255.0) as u8
}

/// Encodes an `f32` as an IEEE 754 half-precision value (round to nearest).
fn encode_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;
    if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        let mantissa = (mantissa | 0x0080_0000) >> (1 - exponent + 13);
        return sign | mantissa as u16;
    }
    if exponent >= 31 {
        return sign | 0x7c00 | u16::from(mantissa != 0) << 9;
    }
    sign | ((exponent as u16) << 10) | ((mantissa >> 13) as u16)
}

fn bounds(positions: &[[f32; 3]]) -> ([f32; 3], f32) {
    let count = positions.len() as f32;
    let mut center = [0.0f32; 3];
    for position in positions {
        for (axis, value) in position.iter().enumerate() {
            center[axis] += value / count;
        }
    }
    let radius = positions
        .iter()
        .map(|position| {
            let dx = position[0] - center[0];
            let dy = position[1] - center[1];
            let dz = position[2] - center[2];
            (dx * dx + dy * dy + dz * dz).sqrt()
        })
        .fold(0.0f32, f32::max);
    (center, radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad() -> StaticShape<'static> {
        StaticShape {
            name: "GeneratedQuad",
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
        }
    }

    #[test]
    fn writes_the_expected_header() {
        let bytes = static_shape(&quad()).unwrap();
        let line = b"Gamebryo File Format, Version 20.2.0.7\n";
        assert!(bytes.starts_with(line));
        let version = u32::from_le_bytes(bytes[line.len()..line.len() + 4].try_into().unwrap());
        assert_eq!(version, NIF_VERSION);
        assert_eq!(bytes[line.len() + 4], 1);
        let user = u32::from_le_bytes(bytes[line.len() + 5..line.len() + 9].try_into().unwrap());
        assert_eq!(user, USER_VERSION);
        let blocks = u32::from_le_bytes(bytes[line.len() + 9..line.len() + 13].try_into().unwrap());
        assert_eq!(blocks, 4);
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(
            static_shape(&quad()).unwrap(),
            static_shape(&quad()).unwrap()
        );
    }

    #[test]
    fn rejects_invalid_geometry() {
        let mut shape = quad();
        shape.normals = &[];
        assert!(static_shape(&shape).is_err());
        let mut shape = quad();
        shape.indices = &[[0, 1, 9]];
        assert!(static_shape(&shape).is_err());
        let mut shape = quad();
        shape.diffuse = "";
        assert!(static_shape(&shape).is_err());
    }
}
