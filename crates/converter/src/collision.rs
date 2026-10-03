//! Extract fixed collision authored in Skyrim NIFs for the runtime GLB.
//!
//! The render parser deliberately ignores Havok blocks. This reader follows only
//! collision-object -> rigid-body -> shape references. Unsupported shape families
//! remain explicit in the contract; they never silently become render geometry.

use color_eyre::{
    Result,
    eyre::{bail, ensure},
};
use nom_derive::Parse;
use project_wormhole_nif::{
    nif_block::{NiAVObject, NifBlock},
    nif_file::NifFile,
};
use project_wormhole_shared::glam::{Mat3, Mat4, Quat, Vec3};
use shared::{
    collision::{
        BodyKind, COLLISION_ASSET_VERSION, CollisionAsset, CollisionBody, CollisionShape,
        HavokBodyInfo,
    },
    coordinates::creation_to_runtime_vector,
};
use std::{fs, path::Path};

const HAVOK_TO_CREATION: f32 = 70.0;
const MAX_VERTICES: usize = 1_000_000;
const MAX_SHAPES: usize = 256;

struct Block<'a> {
    kind: &'a str,
    bytes: &'a [u8],
}

pub fn from_nif(path: &Path, nif: &NifFile) -> Result<CollisionAsset> {
    let bytes = fs::read(path)?;
    let (mut data, header) = crate::mesh::parse_skyrim_header(&bytes, path)?;
    ensure!(
        header.block_count as usize == nif.blocks.len(),
        "NIF block table changed during collision extraction"
    );
    let mut blocks = Vec::with_capacity(nif.blocks.len());
    for index in 0..nif.blocks.len() {
        let size = usize::try_from(header.block_size_index[index])?;
        let Some((raw, remaining)) = data.split_at_checked(size) else {
            bail!("truncated NIF collision block {index}");
        };
        data = remaining;
        blocks.push(Block {
            kind: header
                .get_block_type(index)
                .map_err(|_| color_eyre::eyre::eyre!("invalid NIF block type"))?,
            bytes: raw,
        });
    }
    let mut asset = CollisionAsset {
        version: COLLISION_ASSET_VERSION,
        authored: true,
        shapes: Vec::new(),
        skipped: Vec::new(),
        bodies: Vec::new(),
    };
    for (index, block) in blocks.iter().enumerate() {
        if block.kind != "bhkCollisionObject" {
            continue;
        }
        let outcome = (|| -> Result<()> {
            ensure!(block.bytes.len() >= 10, "short collision object");
            let target = usize::try_from(u32_at(block.bytes, 0)?)?;
            let body = usize::try_from(u32_at(block.bytes, 6)?)?;
            let body_block = blocks
                .get(body)
                .ok_or_else(|| color_eyre::eyre::eyre!("body reference out of range"))?;
            ensure!(
                matches!(body_block.kind, "bhkRigidBody" | "bhkRigidBodyT"),
                "unsupported body {}",
                body_block.kind
            );
            let layer = *body_block
                .bytes
                .get(4)
                .ok_or_else(|| color_eyre::eyre::eyre!("short rigid body"))?;
            // Skyrim uses layer 15 for harvestable flora and other nonphysical objects.
            if layer == 15 {
                return Ok(());
            }
            ensure!(
                matches!(layer, 1 | 2 | 3 | 4 | 9 | 10 | 13 | 17 | 27 | 31 | 35),
                "unsupported collision layer {layer}"
            );
            let node = target_transform(nif, &blocks, target)?;
            let shape = usize::try_from(u32_at(body_block.bytes, 0)?)?;
            let body_transform = if body_block.kind == "bhkRigidBodyT" {
                let translation = vec3_at(body_block.bytes, 52)? * HAVOK_TO_CREATION;
                let rotation = quat_at(body_block.bytes, 68)?;
                Mat4::from_rotation_translation(rotation, translation)
            } else {
                Mat4::IDENTITY
            };
            let first_shape = asset.shapes.len();
            let frame = node * body_transform;
            extract_shape(&blocks, shape, frame, &mut asset.shapes, 0)?;
            // The shapes stay even when the dynamics cannot be read: a fixed collider is
            // still correct, only the physics body is lost (and reported).
            let body = rigid_body(
                nif,
                target,
                body_block.bytes,
                frame,
                u32::try_from(first_shape)?..u32::try_from(asset.shapes.len())?,
                &asset.shapes,
            );
            match body {
                Ok(body) => asset.bodies.push(body),
                Err(error) => asset.skipped.push(format!(
                    "block {index}: rigid body {body}: {error:#}",
                    body = body_block.kind
                )),
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            asset.skipped.push(format!("block {index}: {error:#}"));
        }
    }
    Ok(asset)
}

// Havok enum values of `bhkRigidBodyCInfo2010`, from niftools nif.xml.
//
// hkMotionType (`Motion System`): 0 MO_SYS_INVALID, 1 MO_SYS_DYNAMIC, 2 MO_SYS_SPHERE_INERTIA,
// 3 MO_SYS_SPHERE_STABILIZED, 4 MO_SYS_BOX_INERTIA, 5 MO_SYS_BOX_STABILIZED, 6 MO_SYS_KEYFRAMED,
// 7 MO_SYS_FIXED, 8 MO_SYS_THIN_BOX, 9 MO_SYS_CHARACTER.
// hkQualityType (`Quality Type`): 0 MO_QUAL_INVALID, 1 MO_QUAL_FIXED, 2 MO_QUAL_KEYFRAMED,
// 3 MO_QUAL_DEBRIS, 4 MO_QUAL_MOVING, 5 MO_QUAL_CRITICAL, 6 MO_QUAL_BULLET, 7 MO_QUAL_USER,
// 8 MO_QUAL_CHARACTER, 9 MO_QUAL_KEYFRAMED_REPORT.
// hkDeactivatorType (`Deactivator Type`): 0 DEACTIVATOR_INVALID, 1 DEACTIVATOR_NEVER,
// 2 DEACTIVATOR_SPATIAL. It is kept raw and does not take part in `kind`.

/// Motion systems that integrate a body under forces: dynamic, sphere/box inertia (plain and
/// stabilized) and thin box. Keyframed, fixed, character and invalid are not simulated.
const SIMULATED_MOTION_SYSTEMS: [u8; 6] = [1, 2, 3, 4, 5, 8];
const MOTION_SYSTEM_KEYFRAMED: u8 = 6;
/// Qualities of a body that moves and collides: debris, moving, critical, bullet.
const MOVING_QUALITY_TYPES: [u8; 4] = [3, 4, 5, 6];

// Byte offsets inside a `bhkRigidBody`/`bhkRigidBodyT` block (Skyrim SE, BS version 100):
// `bhkWorldObject` (shape 0, filter 4, world-object info 8..28), `bhkEntityCInfo` (28..32),
// then `bhkRigidBodyCInfo2010` from 32. The translation at 52 anchors the whole layout.
const BODY_LAYER: usize = 4;
const BODY_INERTIA: usize = 116; // hkMatrix3: three rows of four floats, the fourth unused
const BODY_CENTER: usize = 164; // Vector4, w unused
const BODY_MASS: usize = 180;
const BODY_LINEAR_DAMPING: usize = 184;
const BODY_ANGULAR_DAMPING: usize = 188;
const BODY_FRICTION: usize = 200;
const BODY_RESTITUTION: usize = 208;
const BODY_MAX_LINEAR_VELOCITY: usize = 212;
const BODY_MAX_ANGULAR_VELOCITY: usize = 216;
const BODY_MOTION_SYSTEM: usize = 224;
const BODY_DEACTIVATOR_TYPE: usize = 225;
const BODY_QUALITY_TYPE: usize = 227;
const BODY_MIN_LEN: usize = 228;

/// Classifies a body from its raw Havok values (#104: motion system AND quality AND mass).
fn classify(motion_system: u8, quality_type: u8, mass: f32) -> BodyKind {
    if SIMULATED_MOTION_SYSTEMS.contains(&motion_system)
        && MOVING_QUALITY_TYPES.contains(&quality_type)
        && mass.is_finite()
        && mass > 0.0
    {
        BodyKind::Dynamic
    } else if motion_system == MOTION_SYSTEM_KEYFRAMED && mass == 0.0 {
        BodyKind::Keyframed
    } else {
        BodyKind::Fixed
    }
}

/// glTF node index the exporter gives a NIF block: the static scene writes one node per
/// `NiNode`, `BSFadeNode` and geometry block, in block order. The converter re-checks this
/// against the model it exported (`mesh.rs`).
fn gltf_node_index(nif: &NifFile, target: usize) -> Result<u32> {
    fn exported(block: &NifBlock) -> bool {
        matches!(
            block,
            NifBlock::NiNode(_)
                | NifBlock::BSFadeNode(_)
                | NifBlock::BSTriShape(_)
                | NifBlock::BSDynamicTriShape(_)
                | NifBlock::BSSubIndexTriShape(_)
                | NifBlock::BSLODTriShape(_)
                | NifBlock::NiTriShape(_)
        )
    }
    ensure!(
        nif.blocks.get(target).is_some_and(exported),
        "collision target {target} is not exported as a glTF node"
    );
    Ok(u32::try_from(
        nif.blocks[..target]
            .iter()
            .filter(|block| exported(block))
            .count(),
    )?)
}

fn node_name(nif: &NifFile, target: usize) -> String {
    let name = match nif.blocks.get(target) {
        Some(NifBlock::NiNode(node) | NifBlock::BSFadeNode(node)) => node.av.object.name,
        _ => return String::new(),
    };
    nif.header
        .get_string(name as usize)
        .map(str::to_owned)
        .unwrap_or_default()
}

/// Reads one rigid body's dynamics into the frame of the shapes it owns. `frame` is the
/// Creation-space transform applied to the shapes (target node x `bhkRigidBodyT` transform);
/// the tensor is rotated by its rotation, the centre of mass moved by all of it, and both then
/// take the same Creation-to-runtime basis the shapes get.
fn rigid_body(
    nif: &NifFile,
    target: usize,
    bytes: &[u8],
    frame: Mat4,
    shape_range: std::ops::Range<u32>,
    shapes: &[CollisionShape],
) -> Result<CollisionBody> {
    ensure!(!shape_range.is_empty(), "rigid body owns no shapes");
    let owned = shapes
        .get(shape_range.start as usize..shape_range.end as usize)
        .ok_or_else(|| color_eyre::eyre::eyre!("rigid body shape range out of bounds"))?;
    let mut body = read_dynamics(bytes, frame, owned)?;
    body.node = gltf_node_index(nif, target)?;
    body.target = node_name(nif, target);
    body.shapes = shape_range.collect();
    Ok(body)
}

/// The dynamics fields of a rigid-body block, converted into the shapes' frame. `node`,
/// `target` and `shapes` are filled in by [`rigid_body`]. Every converted value is checked
/// for finiteness after the unit conversion, since a large finite Havok value overflows
/// when multiplied by 70.
fn read_dynamics(bytes: &[u8], frame: Mat4, owned: &[CollisionShape]) -> Result<CollisionBody> {
    ensure!(bytes.len() >= BODY_MIN_LEN, "short rigid body");
    let (motion_system, deactivator_type, quality_type) = (
        bytes[BODY_MOTION_SYSTEM],
        bytes[BODY_DEACTIVATOR_TYPE],
        bytes[BODY_QUALITY_TYPE],
    );
    let scalar = |offset: usize| -> Result<f32> {
        let value = f32_at(bytes, offset)?;
        ensure!(value.is_finite(), "non-finite rigid body value");
        Ok(value)
    };
    let mass = scalar(BODY_MASS)?;
    ensure!(mass >= 0.0, "negative rigid body mass");
    let mut local = [0.0_f32; 9];
    for row in 0..3 {
        for column in 0..3 {
            local[row * 3 + column] = scalar(BODY_INERTIA + row * 16 + column * 4)?;
        }
    }
    let center = vec3_at(bytes, BODY_CENTER)? * HAVOK_TO_CREATION;
    ensure!(
        center.is_finite(),
        "rigid body centre of mass overflows after unit conversion"
    );

    let scale = frame.x_axis.truncate().length();
    ensure!(
        scale.is_finite() && scale > 0.0,
        "invalid rigid body transform scale"
    );
    let rotation = Mat3::from_cols(
        frame.x_axis.truncate() / scale,
        frame.y_axis.truncate() / scale,
        frame.z_axis.truncate() / scale,
    );
    let runtime_basis = Mat3::from_cols(
        Vec3::from(creation_to_runtime_vector([1.0, 0.0, 0.0])),
        Vec3::from(creation_to_runtime_vector([0.0, 1.0, 0.0])),
        Vec3::from(creation_to_runtime_vector([0.0, 0.0, 1.0])),
    );
    // `from_cols_array` takes the row-major values as columns, so transpose to get the tensor.
    let tensor = Mat3::from_cols_array(&local).transpose();
    let basis = runtime_basis * rotation;
    let tensor = basis
        * (tensor * (HAVOK_TO_CREATION * HAVOK_TO_CREATION * scale * scale))
        * basis.transpose();
    let mut inertia = [0.0_f32; 9];
    for row in 0..3 {
        for column in 0..3 {
            // Symmetrised: the contract is a symmetric tensor even if the file is not.
            inertia[row * 3 + column] = 0.5 * (tensor.col(column)[row] + tensor.col(row)[column]);
        }
    }
    ensure!(
        inertia.iter().all(|value| value.is_finite()),
        "rigid body inertia overflows after unit conversion"
    );
    let center_of_mass = point(frame, center)?;
    let max_linear_velocity = scalar(BODY_MAX_LINEAR_VELOCITY)? * HAVOK_TO_CREATION;
    ensure!(
        max_linear_velocity.is_finite(),
        "rigid body max linear velocity overflows after unit conversion"
    );
    let convex = !owned.is_empty()
        && owned
            .iter()
            .all(|shape| !matches!(shape, CollisionShape::Mesh { .. }));
    Ok(CollisionBody {
        node: 0,
        target: String::new(),
        shapes: Vec::new(),
        kind: classify(motion_system, quality_type, mass),
        havok: HavokBodyInfo {
            motion_system,
            quality_type,
            deactivator_type,
            collision_layer: bytes[BODY_LAYER],
        },
        mass,
        inertia,
        center_of_mass,
        linear_damping: scalar(BODY_LINEAR_DAMPING)?,
        angular_damping: scalar(BODY_ANGULAR_DAMPING)?,
        friction: scalar(BODY_FRICTION)?,
        restitution: scalar(BODY_RESTITUTION)?,
        max_linear_velocity,
        max_angular_velocity: scalar(BODY_MAX_ANGULAR_VELOCITY)?,
        convex,
    })
}

fn target_transform(nif: &NifFile, blocks: &[Block<'_>], target: usize) -> Result<Mat4> {
    fn visit(nif: &NifFile, blocks: &[Block<'_>], index: usize, depth: usize) -> Result<Mat4> {
        ensure!(depth < 64, "NIF node hierarchy is too deep");
        let raw_av;
        let av = match nif.blocks.get(index) {
            Some(NifBlock::NiNode(node) | NifBlock::BSFadeNode(node)) => &node.av,
            _ => {
                let block = blocks
                    .get(index)
                    .ok_or_else(|| color_eyre::eyre::eyre!("collision target out of range"))?;
                ensure!(
                    matches!(block.kind, "BSLeafAnimNode" | "BSTreeNode"),
                    "collision target {index} is not a scene node"
                );
                raw_av = NiAVObject::parse(block.bytes)
                    .map_err(|_| color_eyre::eyre::eyre!("unsupported node header at {index}"))?
                    .1;
                &raw_av
            }
        };
        let local = Mat4::from_scale_rotation_translation(
            Vec3::splat(av.scale.0),
            Quat::from_mat3(&av.rotation.0.0),
            av.translation.0.0,
        );
        let mut parent = None;
        for (candidate, block) in nif.blocks.iter().enumerate() {
            if let NifBlock::NiNode(node) | NifBlock::BSFadeNode(node) = block
                && node.children.contains(&(index as u32))
            {
                ensure!(
                    parent.replace(candidate).is_none(),
                    "collision node has multiple parents"
                );
            }
        }
        Ok(match parent {
            Some(parent) => visit(nif, blocks, parent, depth + 1)? * local,
            None => local,
        })
    }
    visit(nif, blocks, target, 0)
}

fn extract_shape(
    blocks: &[Block<'_>],
    index: usize,
    transform: Mat4,
    out: &mut Vec<CollisionShape>,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth < 16 && out.len() < MAX_SHAPES,
        "collision shape recursion/size limit"
    );
    let block = blocks
        .get(index)
        .ok_or_else(|| color_eyre::eyre::eyre!("shape reference out of range"))?;
    match block.kind {
        "bhkMoppBvTreeShape" => extract_shape(
            blocks,
            usize::try_from(u32_at(block.bytes, 0)?)?,
            transform,
            out,
            depth + 1,
        ),
        "bhkTransformShape" | "bhkConvexTransformShape" => {
            let child = usize::try_from(u32_at(block.bytes, 0)?)?;
            let local = havok_matrix(block.bytes, 20)?;
            extract_shape(blocks, child, transform * local, out, depth + 1)
        }
        "bhkListShape" => {
            let count = usize::try_from(u32_at(block.bytes, 0)?)?;
            ensure!(count <= MAX_SHAPES, "too many list shapes");
            for child in 0..count {
                extract_shape(
                    blocks,
                    usize::try_from(u32_at(block.bytes, 4 + child * 4)?)?,
                    transform,
                    out,
                    depth + 1,
                )?;
            }
            Ok(())
        }
        "bhkCompressedMeshShape" => {
            ensure!(block.bytes.len() >= 56, "short compressed mesh shape");
            let scale = vec3_at(block.bytes, 16)?;
            ensure!(
                scale.is_finite() && scale.min_element() > 0.0,
                "invalid compressed mesh scale"
            );
            let data = usize::try_from(u32_at(block.bytes, 52)?)?;
            let mesh = blocks
                .get(data)
                .ok_or_else(|| color_eyre::eyre::eyre!("compressed data reference out of range"))?;
            ensure!(
                mesh.kind == "bhkCompressedMeshShapeData",
                "compressed shape references {}",
                mesh.kind
            );
            out.push(decode_compressed_mesh(
                mesh.bytes,
                transform * Mat4::from_scale(scale),
            )?);
            Ok(())
        }
        "bhkNiTriStripsShape" => {
            let scale = vec3_at(block.bytes, 32)?;
            ensure!(
                scale.min_element() > 0.0,
                "invalid NiTriStrips collision scale"
            );
            let count = usize::try_from(u32_at(block.bytes, 48)?)?;
            ensure!(count <= MAX_SHAPES, "too many NiTriStrips data blocks");
            for item in 0..count {
                ensure!(out.len() < MAX_SHAPES, "collision shape size limit");
                let data = usize::try_from(u32_at(block.bytes, 52 + item * 4)?)?;
                let data_block = blocks.get(data).ok_or_else(|| {
                    color_eyre::eyre::eyre!("NiTriStrips data reference out of range")
                })?;
                ensure!(
                    data_block.kind == "NiTriStripsData",
                    "NiTriStrips shape references {}",
                    data_block.kind
                );
                out.push(decode_ni_tri_strips(
                    data_block.bytes,
                    transform * Mat4::from_scale(scale),
                )?);
            }
            Ok(())
        }
        "bhkCapsuleShape" => {
            ensure!(block.bytes.len() >= 48, "short capsule");
            let a = vec3_at(block.bytes, 16)?;
            let b = vec3_at(block.bytes, 32)?;
            let radius = f32_at(block.bytes, 28)?.max(f32_at(block.bytes, 44)?);
            ensure!(radius.is_finite() && radius > 0.0, "invalid capsule radius");
            let scale = transform.transform_vector3(Vec3::X).length();
            ensure!(
                scale.is_finite() && scale > 0.0,
                "invalid capsule transform scale"
            );
            out.push(CollisionShape::Capsule {
                a: point(transform, a * HAVOK_TO_CREATION)?,
                b: point(transform, b * HAVOK_TO_CREATION)?,
                radius: radius * HAVOK_TO_CREATION * scale,
            });
            Ok(())
        }
        "bhkBoxShape" => {
            ensure!(block.bytes.len() >= 32, "short box");
            let half = vec3_at(block.bytes, 16)? * HAVOK_TO_CREATION;
            ensure!(
                half.is_finite() && half.min_element() > 0.0,
                "invalid box dimensions"
            );
            // A rotated box is represented by its eight transformed corners as a hull.
            let mut points = Vec::with_capacity(8);
            for x in [-1.0, 1.0] {
                for y in [-1.0, 1.0] {
                    for z in [-1.0, 1.0] {
                        points.push(point(transform, half * Vec3::new(x, y, z))?);
                    }
                }
            }
            out.push(CollisionShape::Hull { points });
            Ok(())
        }
        "bhkConvexVerticesShape" => {
            let count = usize::try_from(u32_at(block.bytes, 32)?)?;
            ensure!(
                (4..=MAX_VERTICES).contains(&count),
                "invalid convex vertex count"
            );
            let mut points = Vec::with_capacity(count);
            for i in 0..count {
                points.push(point(
                    transform,
                    vec3_at(block.bytes, 36 + i * 16)? * HAVOK_TO_CREATION,
                )?);
            }
            out.push(CollisionShape::Hull { points });
            Ok(())
        }
        other => bail!("unsupported shape {other}"),
    }
}

fn havok_matrix(bytes: &[u8], offset: usize) -> Result<Mat4> {
    let mut columns = [0.0_f32; 16];
    for (index, value) in columns.iter_mut().enumerate() {
        *value = f32_at(bytes, offset + index * 4)?;
    }
    ensure!(
        columns.iter().all(|value| value.is_finite()),
        "non-finite shape transform"
    );
    ensure!(
        columns[3] == 0.0 && columns[7] == 0.0 && columns[11] == 0.0,
        "unsupported shape transform perspective"
    );
    for value in &mut columns[12..15] {
        *value *= HAVOK_TO_CREATION;
    }
    // Havok NIFs conventionally serialize zero in this otherwise unused slot.
    columns[15] = 1.0;
    let matrix = Mat4::from_cols_array(&columns);
    ensure!(
        matrix.determinant().is_finite() && matrix.determinant().abs() > 1.0e-6,
        "singular shape transform"
    );
    Ok(matrix)
}

fn decode_compressed_mesh(bytes: &[u8], transform: Mat4) -> Result<CollisionShape> {
    let mut cursor = Cursor::new(bytes);
    let bits = cursor.u32()?;
    let winding_bits = cursor.u32()?;
    ensure!(
        bits == 17 && winding_bits == 18,
        "unsupported compressed mesh index format"
    );
    cursor.skip(4 * 2)?; // masks
    let quantization_error = cursor.f32()?;
    ensure!(
        quantization_error.is_finite() && quantization_error > 0.0,
        "invalid compressed mesh quantization error"
    );
    cursor.skip(32 + 2)?; // AABB, welding/material mode
    for width in [4, 2, 1] {
        let count = cursor.count(MAX_VERTICES)?;
        cursor.skip(count * width)?;
    }
    let materials = cursor.count(MAX_SHAPES)?;
    let mut layers = Vec::with_capacity(materials);
    for _ in 0..materials {
        cursor.skip(4)?;
        layers.push(cursor.u8()?);
        cursor.skip(3)?;
    }
    let named = cursor.count(MAX_SHAPES)?;
    ensure!(
        named == 0,
        "named compressed mesh materials are unsupported"
    );
    let transforms = cursor.count(MAX_SHAPES)?;
    let mut chunk_transforms = Vec::with_capacity(transforms);
    for _ in 0..transforms {
        let translation = cursor.vec3()? * HAVOK_TO_CREATION;
        cursor.skip(4)?;
        let rotation = cursor.quat()?;
        chunk_transforms.push(Mat4::from_rotation_translation(rotation, translation));
    }
    let big_vertices = cursor.count(MAX_VERTICES)?;
    let mut vertices = Vec::with_capacity(big_vertices);
    for _ in 0..big_vertices {
        let p = cursor.vec3()? * HAVOK_TO_CREATION;
        cursor.skip(4)?;
        vertices.push(point(transform, p)?);
    }
    let big_triangles = cursor.count(MAX_VERTICES)?;
    let mut triangles = Vec::new();
    for _ in 0..big_triangles {
        let tri = [
            u32::from(cursor.u16()?),
            u32::from(cursor.u16()?),
            u32::from(cursor.u16()?),
        ];
        let material = usize::try_from(cursor.u32()?)?;
        cursor.skip(2)?;
        ensure!(
            material < layers.len(),
            "big triangle material out of range"
        );
        ensure!(
            tri.iter().all(|&index| (index as usize) < big_vertices),
            "big triangle index out of range"
        );
        if layers[material] != 15 && tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2] {
            triangles.push(tri);
        }
    }
    let chunks = cursor.count(MAX_SHAPES)?;
    for _ in 0..chunks {
        let chunk_translation = cursor.vec3()? * HAVOK_TO_CREATION;
        cursor.skip(4)?;
        let material = usize::try_from(cursor.u32()?)?;
        let reference = cursor.u16()?;
        let transform_index = usize::from(cursor.u16()?);
        ensure!(
            reference == u16::MAX,
            "referenced compressed chunk is unsupported"
        );
        ensure!(material < layers.len(), "chunk material out of range");
        let chunk_transform = *chunk_transforms
            .get(transform_index)
            .ok_or_else(|| color_eyre::eyre::eyre!("chunk transform out of range"))?;
        let coordinate_count = cursor.count(MAX_VERTICES * 3)?;
        ensure!(coordinate_count % 3 == 0, "incomplete compressed vertices");
        let count = coordinate_count / 3;
        ensure!(
            vertices.len() + count <= MAX_VERTICES,
            "compressed mesh vertex limit"
        );
        let offset = u32::try_from(vertices.len())?;
        for _ in 0..count {
            let quantized = Vec3::new(
                f32::from(cursor.u16()?),
                f32::from(cursor.u16()?),
                f32::from(cursor.u16()?),
            ) * (HAVOK_TO_CREATION * quantization_error);
            vertices.push(point(
                transform * chunk_transform,
                chunk_translation + quantized,
            )?);
        }
        let index_count = cursor.count(MAX_VERTICES * 3)?;
        let mut indices = Vec::with_capacity(index_count);
        for _ in 0..index_count {
            let index = cursor.u16()?;
            ensure!(
                usize::from(index) < count,
                "compressed triangle index out of range"
            );
            indices.push(u32::from(index) + offset);
        }
        let strips = cursor.count(MAX_VERTICES)?;
        let mut lengths = Vec::with_capacity(strips);
        for _ in 0..strips {
            lengths.push(usize::from(cursor.u16()?));
        }
        let weld = cursor.count(MAX_VERTICES * 3)?;
        cursor.skip(weld * 2)?;
        let mut used = 0;
        for length in lengths {
            ensure!(
                length >= 3 && used + length <= indices.len(),
                "invalid compressed strip length"
            );
            if layers[material] != 15 {
                for i in 0..length - 2 {
                    let tri = if i % 2 == 0 {
                        [
                            indices[used + i],
                            indices[used + i + 1],
                            indices[used + i + 2],
                        ]
                    } else {
                        [
                            indices[used + i + 1],
                            indices[used + i],
                            indices[used + i + 2],
                        ]
                    };
                    if tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2] {
                        triangles.push(tri);
                    }
                }
            }
            used += length;
        }
        ensure!(
            (indices.len() - used) % 3 == 0,
            "incomplete compressed triangle tail"
        );
        if layers[material] != 15 {
            for tail in indices[used..].as_chunks::<3>().0 {
                if tail[0] != tail[1] && tail[1] != tail[2] && tail[0] != tail[2] {
                    triangles.push([tail[0], tail[1], tail[2]]);
                }
            }
        }
    }
    let convex_pieces = cursor.count(MAX_SHAPES)?;
    ensure!(
        convex_pieces == 0 && cursor.remaining() == 0,
        "unsupported compressed mesh tail"
    );
    ensure!(
        !triangles.is_empty(),
        "compressed mesh has no physical triangles"
    );
    Ok(CollisionShape::Mesh {
        vertices,
        triangles,
    })
}

fn decode_ni_tri_strips(bytes: &[u8], transform: Mat4) -> Result<CollisionShape> {
    let mut cursor = Cursor::new(bytes);
    cursor.skip(4)?; // geometry group
    let vertex_count = usize::from(cursor.u16()?);
    ensure!(
        (3..=MAX_VERTICES).contains(&vertex_count),
        "invalid NiTriStrips vertex count"
    );
    cursor.skip(2)?; // Bethesda maximum vertex count
    ensure!(cursor.u8()? == 1, "NiTriStrips has no vertices");
    let mut vertices = Vec::with_capacity(vertex_count);
    for _ in 0..vertex_count {
        // NiGeometryData positions are already in Creation units.
        vertices.push(point(transform, cursor.vec3()?)?);
    }
    let data_flags = cursor.u16()?;
    cursor.skip(4)?; // Bethesda vector flags
    if cursor.u8()? != 0 {
        cursor.skip(vertex_count * 12)?; // normals
        if data_flags & 0x1000 != 0 {
            cursor.skip(vertex_count * 24)?; // tangent and bitangent
        }
    }
    cursor.skip(16)?; // bounds center and radius
    if cursor.u8()? != 0 {
        cursor.skip(vertex_count * 16)?; // colors
    }
    cursor.skip(vertex_count * usize::from(data_flags & 0x3f) * 8)?; // UV sets
    cursor.skip(6)?; // consistency flags and additional data reference
    let triangle_count = usize::from(cursor.u16()?);
    let strip_count = usize::from(cursor.u16()?);
    ensure!(strip_count <= MAX_SHAPES, "too many NiTriStrips strips");
    let mut lengths = Vec::with_capacity(strip_count);
    let mut index_count = 0usize;
    let mut declared_triangles = 0usize;
    for _ in 0..strip_count {
        let length = usize::from(cursor.u16()?);
        ensure!(length >= 3, "short NiTriStrips strip");
        index_count = index_count
            .checked_add(length)
            .ok_or_else(|| color_eyre::eyre::eyre!("NiTriStrips index count overflow"))?;
        declared_triangles += length - 2;
        lengths.push(length);
    }
    ensure!(index_count <= MAX_VERTICES * 3, "NiTriStrips index limit");
    ensure!(
        declared_triangles == triangle_count,
        "NiTriStrips triangle count mismatch"
    );
    ensure!(cursor.u8()? == 1, "NiTriStrips has no index data");
    let mut triangles = Vec::with_capacity(triangle_count);
    for length in lengths {
        let mut strip = Vec::with_capacity(length);
        for _ in 0..length {
            let index = usize::from(cursor.u16()?);
            ensure!(index < vertex_count, "NiTriStrips index out of range");
            strip.push(u32::try_from(index)?);
        }
        for i in 0..length - 2 {
            let triangle = if i % 2 == 0 {
                [strip[i], strip[i + 1], strip[i + 2]]
            } else {
                [strip[i + 1], strip[i], strip[i + 2]]
            };
            if triangle[0] != triangle[1]
                && triangle[1] != triangle[2]
                && triangle[0] != triangle[2]
            {
                triangles.push(triangle);
            }
        }
    }
    ensure!(
        !triangles.is_empty(),
        "NiTriStrips has no physical triangles"
    );
    ensure!(cursor.remaining() == 0, "NiTriStrips tail mismatch");
    Ok(CollisionShape::Mesh {
        vertices,
        triangles,
    })
}

fn point(transform: Mat4, p: Vec3) -> Result<[f32; 3]> {
    let p = transform.transform_point3(p);
    ensure!(p.is_finite(), "non-finite collision point");
    Ok(creation_to_runtime_vector(p.to_array()))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated u32"))?
            .try_into()?,
    ))
}
fn f32_at(bytes: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated f32"))?
            .try_into()?,
    ))
}
fn vec3_at(bytes: &[u8], offset: usize) -> Result<Vec3> {
    let v = Vec3::new(
        f32_at(bytes, offset)?,
        f32_at(bytes, offset + 4)?,
        f32_at(bytes, offset + 8)?,
    );
    ensure!(v.is_finite(), "non-finite vector");
    Ok(v)
}
fn quat_at(bytes: &[u8], offset: usize) -> Result<Quat> {
    let q = Quat::from_xyzw(
        f32_at(bytes, offset)?,
        f32_at(bytes, offset + 4)?,
        f32_at(bytes, offset + 8)?,
        f32_at(bytes, offset + 12)?,
    );
    ensure!(
        q.is_finite() && q.length_squared() > 0.5 && q.length_squared() < 1.5,
        "invalid quaternion"
    );
    Ok(q.normalize())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| color_eyre::eyre::eyre!("collision data offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated collision data"))?;
        self.offset = end;
        Ok(bytes)
    }
    fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn vec3(&mut self) -> Result<Vec3> {
        let v = Vec3::new(self.f32()?, self.f32()?, self.f32()?);
        ensure!(v.is_finite(), "non-finite collision vector");
        Ok(v)
    }
    fn quat(&mut self) -> Result<Quat> {
        let q = Quat::from_xyzw(self.f32()?, self.f32()?, self.f32()?, self.f32()?);
        ensure!(
            q.is_finite() && q.length_squared() > 0.5 && q.length_squared() < 1.5,
            "invalid chunk rotation"
        );
        Ok(q.normalize())
    }
    fn count(&mut self, max: usize) -> Result<usize> {
        let n = usize::try_from(self.u32()?)?;
        ensure!(n <= max, "collision array exceeds limit");
        Ok(n)
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_strategies::{config, corrupted};
    use proptest::prelude::*;

    #[test]
    fn classifies_bodies_by_motion_system_quality_and_mass() {
        // Simulated systems (nif.xml hkMotionType) with a moving quality and mass.
        for motion in [1, 2, 3, 4, 5, 8] {
            for quality in [3, 4, 5, 6] {
                assert_eq!(classify(motion, quality, 1.0), BodyKind::Dynamic);
            }
        }
        // Vanilla's exporter default: box stabilized, invalid quality, mass 0.
        assert_eq!(classify(5, 0, 0.0), BodyKind::Fixed);
        assert_eq!(classify(5, 4, 0.0), BodyKind::Fixed);
        assert_eq!(classify(5, 4, f32::NAN), BodyKind::Fixed);
        assert_eq!(classify(5, 1, 3.0), BodyKind::Fixed);
        assert_eq!(classify(7, 4, 3.0), BodyKind::Fixed); // MO_SYS_FIXED
        assert_eq!(classify(9, 8, 3.0), BodyKind::Fixed); // MO_SYS_CHARACTER
        assert_eq!(classify(6, 2, 0.0), BodyKind::Keyframed);
        assert_eq!(classify(6, 2, 5.0), BodyKind::Fixed);
    }

    /// A `bhkRigidBody` block written offset by offset, independently of the dummy-content
    /// writer, with a distinct value at every offset the reader uses.
    fn hand_built_body() -> Vec<u8> {
        let mut b = vec![0_u8; 250];
        let put = |b: &mut Vec<u8>, at: usize, v: f32| {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        b[4] = 17; // collision layer
        // Inertia rows at 116, 132, 148 (the fourth float of each row is unused padding).
        for (i, at) in [116, 120, 124, 132, 136, 140, 148, 152, 156]
            .into_iter()
            .enumerate()
        {
            put(&mut b, at, 0.5 + i as f32);
        }
        for at in [128, 144, 160] {
            put(&mut b, at, 99.0);
        }
        put(&mut b, 164, 0.25);
        put(&mut b, 168, 0.5);
        put(&mut b, 172, 0.75);
        put(&mut b, 176, 99.0); // centre w
        put(&mut b, 180, 3.5); // mass
        put(&mut b, 184, 0.11); // linear damping
        put(&mut b, 188, 0.22); // angular damping
        put(&mut b, 192, 77.0); // time factor, unused
        put(&mut b, 196, 78.0); // gravity factor, unused
        put(&mut b, 200, 0.33); // friction
        put(&mut b, 204, 79.0); // rolling friction, unused
        put(&mut b, 208, 0.44); // restitution
        put(&mut b, 212, 12.0); // max linear velocity
        put(&mut b, 216, 5.5); // max angular velocity
        put(&mut b, 220, 80.0); // penetration depth, unused
        b[224] = 4; // motion system
        b[225] = 2; // deactivator
        b[226] = 9; // solver deactivation, unused
        b[227] = 5; // quality
        b
    }

    #[test]
    fn hand_built_body_block_pins_every_offset() {
        let shapes = [CollisionShape::Hull { points: Vec::new() }];
        let body = read_dynamics(&hand_built_body(), Mat4::IDENTITY, &shapes).unwrap();
        assert_eq!(
            (
                body.havok.motion_system,
                body.havok.deactivator_type,
                body.havok.quality_type,
                body.havok.collision_layer
            ),
            (4, 2, 5, 17)
        );
        assert_eq!(body.kind, BodyKind::Dynamic);
        assert_eq!(body.mass, 3.5);
        assert_eq!((body.linear_damping, body.angular_damping), (0.11, 0.22));
        assert_eq!((body.friction, body.restitution), (0.33, 0.44));
        assert_eq!(body.max_linear_velocity, 12.0 * HAVOK_TO_CREATION);
        assert_eq!(body.max_angular_velocity, 5.5);
        assert!(body.convex);
        // Havok centre (0.25, 0.5, 0.75) x 70 in Creation axes, then (x, z, -y) for runtime.
        let near =
            |a: f32, e: f32| assert!((a - e).abs() < 1.0e-3 * e.abs().max(1.0), "{a} != {e}");
        for (a, e) in body.center_of_mass.into_iter().zip([17.5, 52.5, -35.0]) {
            near(a, e);
        }
        // Havok rows (0.5 1.5 2.5 / 3.5 4.5 5.5 / 6.5 7.5 8.5) x 70^2, symmetrised (creation
        // xy 2.5, xz 4.5, yz 6.5), then runtime (x, y, z) = (x, z, -y).
        let scale = HAVOK_TO_CREATION * HAVOK_TO_CREATION;
        let expected = [
            0.5, 4.5, -2.5, //
            4.5, 8.5, -6.5, //
            -2.5, -6.5, 4.5,
        ];
        for (a, e) in body.inertia.into_iter().zip(expected) {
            near(a, e * scale);
        }
    }

    #[test]
    fn overflow_after_unit_conversion_skips_the_body() {
        let shapes = [CollisionShape::Hull { points: Vec::new() }];
        for offset in [212, 164, 116] {
            let mut bytes = hand_built_body();
            bytes[offset..offset + 4].copy_from_slice(&f32::MAX.to_le_bytes());
            assert!(
                read_dynamics(&bytes, Mat4::IDENTITY, &shapes).is_err(),
                "offset {offset}"
            );
        }
    }

    fn body_nif() -> Vec<u8> {
        use dummy_content::nif::{BoxBody, StaticShape, static_shape_with_bodies};
        let body = |name, transform| BoxBody {
            node_name: name,
            half_extents: [0.1, 0.2, 0.3],
            transform,
            collision_layer: 4,
            motion_system: 4,
            deactivator_type: 1,
            quality_type: 4,
            mass: 2.0,
            inertia: [0.1, 0.0, 0.0, 0.0, 0.2, 0.0, 0.0, 0.0, 0.3],
            center_of_mass: [0.0; 3],
            linear_damping: 0.1,
            angular_damping: 0.05,
            friction: 0.5,
            restitution: 0.4,
            max_linear_velocity: 104.4,
            max_angular_velocity: 31.57,
        };
        static_shape_with_bodies(
            &StaticShape {
                name: "Quad",
                positions: &[[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [1.0, 1.0, 0.0]],
                normals: &[[0.0, 0.0, 1.0]; 3],
                uvs: &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0]],
                indices: &[[0, 1, 2]],
                diffuse: "textures/a.dds",
                normal_texture: "textures/b.dds",
            },
            &[
                body("A", None),
                body("B", Some(([1.0, 2.0, 3.0], [0.0, 0.0, 0.0, 1.0]))),
            ],
        )
        .unwrap()
    }

    fn extract(bytes: &[u8]) -> Result<CollisionAsset> {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("body.nif");
        fs::write(&input, bytes).unwrap();
        crate::mesh::MeshConverter::extract_collision(&input)
    }

    #[test]
    fn body_fixture_extracts_both_bodies() {
        let asset = extract(&body_nif()).unwrap();
        assert_eq!(asset.bodies.len(), 2, "{:?}", asset.skipped);
    }

    #[test]
    fn truncated_body_nif_never_panics() {
        let bytes = body_nif();
        // The body blocks are the tail of the file, so every cut there lands inside one.
        for len in (bytes.len().saturating_sub(700)..bytes.len()).step_by(3) {
            let _ = extract(&bytes[..len]);
        }
    }

    proptest! {
        #![proptest_config(config(128))]

        #[test]
        fn corrupted_body_nif_never_panics(bytes in corrupted(body_nif())) {
            let _ = extract(&bytes);
        }
    }

    #[test]
    fn ni_tri_strips_collision_uses_creation_units_and_shape_reference() {
        let mut data = Vec::new();
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&3_u16.to_le_bytes());
        data.extend_from_slice(&0_u16.to_le_bytes());
        data.push(1); // vertices present
        for position in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for value in position {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
        data.extend_from_slice(&0_u16.to_le_bytes()); // no UV sets
        data.extend_from_slice(&0_u32.to_le_bytes()); // Bethesda vector flags
        data.push(0); // normals absent
        data.extend_from_slice(&[0; 16]); // bounds
        data.push(0); // colors absent
        data.extend_from_slice(&0_u16.to_le_bytes()); // consistency flags
        data.extend_from_slice(&u32::MAX.to_le_bytes()); // no additional data
        data.extend_from_slice(&1_u16.to_le_bytes()); // triangle count
        data.extend_from_slice(&1_u16.to_le_bytes()); // strip count
        data.extend_from_slice(&3_u16.to_le_bytes()); // strip length
        data.push(1); // indices present
        for index in [0_u16, 1, 2] {
            data.extend_from_slice(&index.to_le_bytes());
        }
        let mut shape = vec![0; 56];
        for axis in 0..3 {
            shape[32 + axis * 4..36 + axis * 4].copy_from_slice(&1_f32.to_le_bytes());
        }
        shape[48..52].copy_from_slice(&1_u32.to_le_bytes());
        let blocks = [
            Block {
                kind: "NiTriStripsData",
                bytes: &data,
            },
            Block {
                kind: "bhkNiTriStripsShape",
                bytes: &shape,
            },
        ];
        let mut shapes = Vec::new();
        extract_shape(
            &blocks,
            1,
            Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            &mut shapes,
            0,
        )
        .unwrap();
        let [
            CollisionShape::Mesh {
                vertices,
                triangles,
            },
        ] = shapes.as_slice()
        else {
            panic!("expected one strip mesh");
        };
        assert_eq!(
            vertices,
            &vec![[10.0, 0.0, 0.0], [11.0, 0.0, 0.0], [10.0, 0.0, -1.0]]
        );
        assert_eq!(triangles, &vec![[0, 1, 2]]);
    }

    #[test]
    fn transform_wrappers_apply_havok_translation_to_child_shape() {
        for kind in ["bhkTransformShape", "bhkConvexTransformShape"] {
            let mut box_bytes = vec![0; 32];
            for (axis, half_extent) in [1.0_f32, 1.0, 1.0].into_iter().enumerate() {
                box_bytes[16 + axis * 4..20 + axis * 4].copy_from_slice(&half_extent.to_le_bytes());
            }
            let mut wrapper_bytes = vec![0; 84];
            let matrix = Mat4::from_translation(Vec3::new(2.0, 3.0, 4.0));
            for (axis, value) in matrix.to_cols_array().into_iter().enumerate() {
                wrapper_bytes[20 + axis * 4..24 + axis * 4].copy_from_slice(&value.to_le_bytes());
            }
            let blocks = [
                Block {
                    kind: "bhkBoxShape",
                    bytes: &box_bytes,
                },
                Block {
                    kind,
                    bytes: &wrapper_bytes,
                },
            ];
            let mut shapes = Vec::new();
            extract_shape(&blocks, 1, Mat4::IDENTITY, &mut shapes, 0).unwrap();
            let [CollisionShape::Hull { points }] = shapes.as_slice() else {
                panic!("expected one transformed box hull");
            };
            assert_eq!(points.len(), 8);
            for axis in 0..3 {
                let minimum = points
                    .iter()
                    .map(|point| point[axis])
                    .fold(f32::INFINITY, f32::min);
                let maximum = points
                    .iter()
                    .map(|point| point[axis])
                    .fold(f32::NEG_INFINITY, f32::max);
                let (expected_min, expected_max) =
                    [(70.0, 210.0), (210.0, 350.0), (-280.0, -140.0)][axis];
                assert_eq!(
                    (minimum, maximum),
                    (expected_min, expected_max),
                    "{kind} axis {axis}"
                );
            }
        }
    }

    #[test]
    fn compressed_strip_decodes_two_walkable_triangles() {
        let mut data = Vec::new();
        let mut u32v = |v: u32| data.extend_from_slice(&v.to_le_bytes());
        u32v(17);
        u32v(18);
        u32v(0x3ffff);
        u32v(0x1ffff);
        data.extend_from_slice(&0.002_f32.to_le_bytes());
        data.extend_from_slice(&[0; 32]); // AABB
        data.extend_from_slice(&[0, 1]); // welding, material mode
        for _ in 0..3 {
            data.extend_from_slice(&0_u32.to_le_bytes());
        }
        data.extend_from_slice(&1_u32.to_le_bytes()); // one material
        data.extend_from_slice(&0_u32.to_le_bytes());
        data.extend_from_slice(&[1, 0, 0, 0]); // STATIC layer
        data.extend_from_slice(&0_u32.to_le_bytes()); // no named materials
        data.extend_from_slice(&1_u32.to_le_bytes()); // identity transform
        for f in [0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            data.extend_from_slice(&f.to_le_bytes());
        }
        data.extend_from_slice(&0_u32.to_le_bytes()); // big vertices
        data.extend_from_slice(&0_u32.to_le_bytes()); // big triangles
        data.extend_from_slice(&1_u32.to_le_bytes()); // chunks
        data.extend_from_slice(&[0; 16]); // translation
        data.extend_from_slice(&0_u32.to_le_bytes()); // material
        data.extend_from_slice(&u16::MAX.to_le_bytes());
        data.extend_from_slice(&0_u16.to_le_bytes()); // transform index
        data.extend_from_slice(&12_u32.to_le_bytes()); // four xyz vertices
        for xyz in [[0_u16, 0, 0], [1000, 0, 0], [0, 1000, 0], [1000, 1000, 0]] {
            for value in xyz {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
        data.extend_from_slice(&4_u32.to_le_bytes());
        for index in 0_u16..4 {
            data.extend_from_slice(&index.to_le_bytes());
        }
        data.extend_from_slice(&1_u32.to_le_bytes()); // one strip
        data.extend_from_slice(&4_u16.to_le_bytes());
        data.extend_from_slice(&0_u32.to_le_bytes()); // welding info
        data.extend_from_slice(&0_u32.to_le_bytes()); // convex pieces
        let CollisionShape::Mesh {
            vertices,
            triangles,
        } = decode_compressed_mesh(&data, Mat4::IDENTITY).unwrap()
        else {
            panic!("expected mesh");
        };
        assert_eq!(
            vertices,
            vec![
                [0.0, 0.0, 0.0],
                [140.0, 0.0, 0.0],
                [0.0, 0.0, -140.0],
                [140.0, 0.0, -140.0]
            ]
        );
        assert_eq!(triangles, vec![[0, 1, 2], [2, 1, 3]]);
    }
}
