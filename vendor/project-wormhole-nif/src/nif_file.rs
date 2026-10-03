use std::collections::BTreeSet;

use gltf::json::accessor::ComponentType;
use gltf::json::accessor::GenericComponentType;
use gltf::json::buffer::View;
use gltf::json::extensions::material::PbrDiffuseFactor;
use gltf::json::extensions::material::PbrSpecularFactor;
use gltf::json::extensions::material::PbrSpecularGlossiness;
use gltf::json::material::EmissiveFactor;
use gltf::json::material::NormalTexture;
use gltf::json::material::PbrBaseColorFactor;
use gltf::json::material::PbrMetallicRoughness;
use gltf::json::material::StrengthFactor;
use gltf::json::mesh::Primitive;
use gltf::json::scene::UnitQuaternion;
use gltf::json::texture::Info;
use gltf::json::validation::{Checked, USize64};
use gltf::{json::*, Semantic};
use project_wormhole_ba2::dev::ensure_texture_parent;
use project_wormhole_ba2::dev::normalize_esm_path;
use project_wormhole_esm::structs::geometry::Quaternion;
use project_wormhole_shared::glam;
use project_wormhole_shared::glam::{Mat4, Vec3};
use zerocopy::IntoBytes;

use crate::model::all::*;

use super::dev::*;

#[derive(Debug)]
pub struct NifFile {
    pub header: NifHeader,
    pub blocks: Vec<NifBlock>,
}

impl NifFile {
    pub fn open(path: &str) -> Result<Self, std::io::Error> {
        // Open file for read
        let mut file = std::fs::File::open(path)?;

        // Init buffer
        let mut buf = Vec::new();

        // Read file
        file.read_to_end(&mut buf)?;

        // Parse nif file
        let (_, nif) = NifFile::parse(&buf).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid or unsupported NIF",
            )
        })?;

        Ok(nif)
    }

    pub fn get_nodes(&self) -> Vec<&NiNode> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::NiNode(node) | NifBlock::BSFadeNode(node) => Some(node),
                NifBlock::BSMultiBoundNode(block) => Some(&block.node),
                _ => None,
            })
            .collect()
    }

    pub fn get_skins(&self) -> Vec<&BSSkinInstance> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSSkinInstance(skin) => Some(skin),
                _ => None,
            })
            .collect()
    }

    pub fn get_shapes(&self) -> Vec<&BSTriShape> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSTriShape(shape) => Some(shape),
                _ => None,
            })
            .collect()
    }

    pub fn get_dynamic_shapes(&self) -> Vec<&BSDynamicTriShape> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSDynamicTriShape(shape) => Some(shape),
                _ => None,
            })
            .collect()
    }

    pub fn get_seg_shapes(&self) -> Vec<&BSSubIndexTriShape> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSSubIndexTriShape(shape) => Some(shape),
                _ => None,
            })
            .collect()
    }

    pub fn get_lod_shapes(&self) -> Vec<&BSLODTriShape> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSLODTriShape(shape) => Some(shape),
                _ => None,
            })
            .collect()
    }

    pub fn get_legacy_shapes(&self) -> Vec<(&NiTriShape, &NiTriShapeData)> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::NiTriShape(shape) => {
                    self.blocks
                        .get(shape.data as usize)
                        .and_then(|data| match data {
                            NifBlock::NiTriShapeData(data) => Some((shape, data)),
                            _ => None,
                        })
                }
                _ => None,
            })
            .collect()
    }

    pub fn get_materials(&self) -> Vec<&BSShaderTextureSet> {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                NifBlock::BSShaderTextureSet(set) => Some(set),
                _ => None,
            })
            .collect()
    }

    pub fn get_mesh_materials_ids(&self) -> Vec<u8> {
        let mut positions = Vec::new();
        let mut mat_index = 1;

        for (_index, block) in self.blocks.iter().enumerate() {
            match block {
                NifBlock::BSSubIndexTriShape(_shape) => {
                    positions.push(mat_index - 1);
                }

                NifBlock::BSShaderTextureSet(_textures) => {
                    mat_index += 1;
                }
                _ => {}
            }
        }
        positions
    }

    pub fn get_node_ref(&mut self, node_name: &str) -> Result<&mut NiNode, String> {
        if let Some(pos) = self.get_node_pos(node_name) {
            match &mut self.blocks[pos] {
                NifBlock::NiNode(node) => return Ok(node),
                NifBlock::BSMultiBoundNode(block) => return Ok(&mut block.node),
                _ => {}
            }
        }
        Err("Node not found".to_string())
    }

    pub fn get_node_pos(&self, node_name: &str) -> Option<usize> {
        for (index, block) in self.blocks.iter().enumerate() {
            let node = match block {
                NifBlock::NiNode(node) => node,
                NifBlock::BSMultiBoundNode(block) => &block.node,
                _ => continue,
            };
            if let Ok(name) = self.header.get_string(node.name() as usize) {
                if name == node_name {
                    return Some(index);
                }
            }
        }
        None
    }

    pub fn has_skeleton(&self) -> bool {
        for block in &self.blocks {
            match block {
                NifBlock::BSSkinInstance(_) => return true,
                _ => {}
            }
        }
        false
    }
}

impl Parse<&[u8]> for NifFile {
    fn parse(i: &[u8]) -> nom::IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        // Parse header first
        let (i, header) = NifHeader::parse(i)?;

        // Init block vector
        let mut blocks = Vec::new();

        // Set data to the rest of the buffer
        let mut data = i;

        // Parse blocks
        for index in 0..header.block_count as usize {
            let (i, raw) =
                take::<u32, &[u8], nom::error::Error<&[u8]>>(header.block_size_index[index])(data)?;
            data = i;
            let (_, block) =
                NifBlock::parse(raw, header.get_block_type(index).unwrap().to_string())?;
            blocks.push(block);
        }

        // Create nif file structure
        let nif = Self { header, blocks };

        if data.len() != 0 {
            warn!("Leftover data from NifFile::parse() : {:?}", data.len());
        }

        // Return nif file, and leftover data (should be zero leftover)
        Ok((i, nif))
    }
}

pub fn nif_to_model(nif: &NifFile, skeleton: Option<&NifFile>) -> Result<Model, String> {
    // Init empty model
    let mut model = Model::default();

    // Check if nif is a skeletal mesh
    if nif.has_skeleton() {
        // Init skeletal mesh
        let mut mesh = SkeletalMesh::default();

        // Check if skeleton is provided from another file
        if let Some(skeleton) = skeleton {
            // Get nodes from skeleton
            let skel_nodes = skeleton.get_nodes();

            // Loop through nodes and add them to the bone tree
            for skel_node in skel_nodes {
                // Get node name
                let node_name = skeleton
                    .header
                    .get_string(skel_node.name() as usize)
                    .unwrap();

                // Get children names from string indices
                let children_names = skel_node
                    .children
                    .iter()
                    .map(|index| {
                        let child_node = skeleton.blocks.get(*index as usize).unwrap();

                        match child_node {
                            NifBlock::NiNode(child_real_node) => skeleton
                                .header
                                .get_string(child_real_node.name() as usize)
                                .unwrap()
                                .to_string(),
                            _ => {
                                panic!("Child node is not a NiNode")
                            }
                        }
                    })
                    .collect::<Vec<String>>();

                // Create bone data
                let bone_data = BoneData {
                    transform: BoneTransform::new(
                        Some(skel_node.translation().0),
                        Some(skel_node.rotation().0),
                        Some(skel_node.scale().0),
                    ),
                    children: children_names.into_iter().collect(),
                };

                // Add bone to bone tree
                mesh.bones.add_bone(node_name, bone_data).unwrap();
            }

            // Get meshes from nif
            let shapes = nif.get_shapes();
            let dynamic_shapes = nif.get_dynamic_shapes();
            let seg_shapes = nif.get_seg_shapes();

            // Init empty mesh vector
            let mut meshes: Vec<SkeletalMesh> = Vec::new();

            // Loop through shapes and add them to the mesh vector
            for shape in shapes {
                let name = Some(
                    nif.header
                        .get_string(shape.av.object.name as usize)
                        .unwrap()
                        .to_string(),
                );
                mesh.mesh = Some(tri_shape_to_mesh(shape, name));

                let weights = shape.get_vertex_weights();
                let joints = shape.get_vertex_joints();

                mesh.weights = weights.clone();
                mesh.joints = joints.clone();

                meshes.push(mesh.clone());
            }

            for shape in dynamic_shapes {
                let shape = &shape.bs_tri_shape;
                let name = Some(
                    nif.header
                        .get_string(shape.av.object.name as usize)
                        .unwrap()
                        .to_string(),
                );
                mesh.mesh = Some(tri_shape_to_mesh(shape, name));
                mesh.weights = shape.get_vertex_weights();
                mesh.joints = shape.get_vertex_joints();
                meshes.push(mesh.clone());
            }

            // Loop through seg shapes and add them to the mesh vector
            for shape in seg_shapes {
                let name = Some(
                    nif.header
                        .get_string(shape.bs_tri_shape.av.object.name as usize)
                        .unwrap()
                        .to_string(),
                );
                mesh.mesh = Some(tri_shape_to_mesh(&shape.bs_tri_shape, name));

                let weights = shape.bs_tri_shape.get_vertex_weights();
                let joints = shape.bs_tri_shape.get_vertex_joints();

                mesh.weights = weights.clone();
                mesh.joints = joints.clone();

                meshes.push(mesh.clone());
            }

            for (skin_index, skin) in nif.get_skins().iter().enumerate() {
                let mut inverse_bind_matrices = Vec::new();

                // Get the inverse position matrices
                match &nif.blocks[skin.data as usize] {
                    NifBlock::BSSkinBoneData(bone_data) => {
                        for bone_transform in &bone_data.bone_list {
                            inverse_bind_matrices.push(bone_transform.into_matrix4());
                        }
                    }
                    _ => {
                        panic!("Inverse position block is not a BSSkinBoneData")
                    }
                }

                let mut joint_names = Vec::new();
                warn!("{:?}", skin.bones.len());

                for joint_index in &skin.bones {
                    match &nif.blocks[*joint_index as usize] {
                        NifBlock::NiNode(node) => {
                            joint_names.push(
                                nif.header
                                    .get_string(node.name() as usize)
                                    .unwrap()
                                    .to_string(),
                            );
                        }
                        _ => {
                            panic!("Skin bone is not a NiNode")
                        }
                    }
                }

                debug!("Joint names: {:?}", joint_names);

                meshes[skin_index].skin = joint_names;
                meshes[skin_index].inverse_bind_matrices = inverse_bind_matrices;

                //debug!("{:?}", meshes[skin_index].skin);
            }

            model.skeletal_meshes = meshes;

            let material = nif.get_materials();
            let mat_indices = nif.get_mesh_materials_ids();

            for mat in material {
                let diffuse = mat.diffuse.clone();
                let normal = mat.normal.clone();
                let glow = mat.glow.clone();
                let specular = mat.specular.clone();

                model.materials.push(super::model::all::Material {
                    diffuse,
                    normal,
                    glow,
                    specular,
                });
            }

            model.material_indices = mat_indices.clone();
        }
        // Use the nodes in the current file as the skeleton
        else {
            unimplemented!("No skeleton provided");
        }
    }
    // Proceed with static mesh conversion
    else {
        populate_static_scene(nif, &mut model)?;

        let material = nif.get_materials();
        let mat_indices = nif.get_mesh_materials_ids();

        for mat in material {
            let diffuse = mat.diffuse.clone();
            let normal = mat.normal.clone();
            let glow = mat.glow.clone();
            let specular = mat.specular.clone();

            model.materials.push(super::model::all::Material {
                diffuse,
                normal,
                glow,
                specular,
            });
        }
    }

    Ok(model)
}

pub fn tri_shape_to_mesh(tri_shape: &BSTriShape, name: Option<String>) -> StaticMesh {
    let mut mesh = StaticMesh::default();

    mesh.name = name;

    let positions: Vec<Vec3> = tri_shape.get_vertex_positions();
    let triangles = tri_shape.get_triangle_indices();
    let normals: Vec<Vec3> = tri_shape.get_vertex_normals();
    let uvs = tri_shape.get_vertex_uvs();

    mesh.positions.extend(positions);
    mesh.triangles.extend(triangles);
    mesh.normals.extend(normals);
    mesh.uvs.extend(uvs);

    mesh
}

pub fn legacy_tri_shape_to_mesh(data: &NiTriShapeData, name: Option<String>) -> StaticMesh {
    StaticMesh {
        name,
        positions: data.positions.clone(),
        normals: data.normals.clone(),
        uvs: data.uvs.clone(),
        triangles: data.triangles.clone(),
        colors: data.colors.iter().copied().map(BSVec4).collect(),
    }
}

/// Builds a bind-pose/static representation of every geometry block. World
/// rendering can use this when an old skin has no complete glTF skin export.
pub fn nif_to_static_model(nif: &NifFile) -> Result<Model, String> {
    let mut model = Model::default();
    populate_static_scene(nif, &mut model)?;
    model.materials = nif
        .get_materials()
        .into_iter()
        .map(|material| crate::model::all::Material {
            diffuse: material.diffuse.clone(),
            normal: material.normal.clone(),
            glow: material.glow.clone(),
            specular: material.specular.clone(),
        })
        .collect();
    model.validate()?;
    Ok(model)
}

fn populate_static_scene(nif: &NifFile, model: &mut Model) -> Result<(), String> {
    let supported_blocks: BTreeSet<u32> = nif
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            matches!(
                block,
                NifBlock::NiNode(_)
                    | NifBlock::BSFadeNode(_)
                    | NifBlock::BSMultiBoundNode(_)
                    | NifBlock::BSTriShape(_)
                    | NifBlock::BSDynamicTriShape(_)
                    | NifBlock::BSSubIndexTriShape(_)
                    | NifBlock::BSLODTriShape(_)
                    | NifBlock::NiTriShape(_)
            )
            .then_some(index as u32)
        })
        .collect();
    let distant_lod_blocks = distant_lod_subtree_blocks(nif);

    for (index, block) in nif.blocks.iter().enumerate() {
        let block_index = index as u32;
        match block {
            NifBlock::NiNode(node) | NifBlock::BSFadeNode(node) => {
                model.static_nodes.push(static_node_from_av(
                    nif,
                    block_index,
                    &node.av,
                    node.children
                        .iter()
                        .copied()
                        .filter(|child| supported_blocks.contains(child))
                        .collect(),
                    None,
                ));
            }
            NifBlock::BSMultiBoundNode(block) => {
                model.static_nodes.push(static_node_from_av(
                    nif,
                    block_index,
                    &block.node.av,
                    block
                        .node
                        .children
                        .iter()
                        .copied()
                        .filter(|child| supported_blocks.contains(child))
                        .collect(),
                    None,
                ));
            }
            NifBlock::BSTriShape(shape) => {
                push_modern_static_shape(
                    nif,
                    model,
                    block_index,
                    shape,
                    distant_lod_blocks[index],
                )?;
            }
            NifBlock::BSDynamicTriShape(shape) => {
                push_modern_static_shape(
                    nif,
                    model,
                    block_index,
                    &shape.bs_tri_shape,
                    distant_lod_blocks[index],
                )?;
            }
            NifBlock::BSSubIndexTriShape(shape) => {
                push_modern_static_shape(
                    nif,
                    model,
                    block_index,
                    &shape.bs_tri_shape,
                    distant_lod_blocks[index],
                )?;
            }
            NifBlock::BSLODTriShape(shape) => {
                push_modern_static_shape(
                    nif,
                    model,
                    block_index,
                    &shape.bs_tri_shape,
                    distant_lod_blocks[index],
                )?;
            }
            NifBlock::NiTriShape(shape) => {
                let data = nif
                    .blocks
                    .get(shape.data as usize)
                    .and_then(|block| match block {
                        NifBlock::NiTriShapeData(data) => Some(data),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        format!(
                            "NiTriShape block {block_index} references invalid data block {}",
                            shape.data
                        )
                    })?;
                let name = nif
                    .header
                    .get_string(shape.name as usize)
                    .ok()
                    .map(str::to_owned);
                let mesh_index = model.static_meshes.len();
                model
                    .static_meshes
                    .push(legacy_tri_shape_to_mesh(data, name.clone()));
                model.static_nodes.push(StaticSceneNode {
                    block_index,
                    name,
                    translation: shape.translation,
                    rotation: shape.rotation,
                    scale: shape.scale,
                    children: Vec::new(),
                    mesh: Some(mesh_index),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Marks the blocks a distant-LOD container holds: every `BSMultiBoundNode`
/// (the root of the `.btr`/`.bto` meshes under `meshes/terrain/`) and its
/// subtree. Shapes there can carry per-vertex tint, so they keep that
/// attribute. Ordinary world meshes use vertex colours for shader effects the
/// material contract does not model yet, and preserving them there would change
/// how most converted statics render.
pub(crate) fn distant_lod_subtree_blocks(nif: &NifFile) -> Vec<bool> {
    let mut reached = vec![false; nif.blocks.len()];
    let mut pending: Vec<u32> = nif
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            matches!(block, NifBlock::BSMultiBoundNode(_)).then_some(index as u32)
        })
        .collect();
    while let Some(index) = pending.pop() {
        let Some(slot) = reached.get_mut(index as usize) else {
            continue;
        };
        if std::mem::replace(slot, true) {
            continue;
        }
        if let Ok(node) = nif.blocks[index as usize].as_node() {
            pending.extend(node.children.iter().copied());
        }
    }
    reached
}

fn push_modern_static_shape(
    nif: &NifFile,
    model: &mut Model,
    block_index: u32,
    shape: &BSTriShape,
    preserve_vertex_colors: bool,
) -> Result<(), String> {
    let name = nif
        .header
        .get_string(shape.av.object.name as usize)
        .ok()
        .map(str::to_owned);
    let mesh_index = model.static_meshes.len();
    let mut mesh = tri_shape_to_mesh(shape, name.clone());
    if preserve_vertex_colors {
        mesh.colors = shape
            .vertex_data
            .iter()
            .filter_map(|vertex| vertex.vertex_colors)
            .map(|color| {
                BSVec4(glam::Vec4::new(
                    f32::from(color.x) / 255.0,
                    f32::from(color.y) / 255.0,
                    f32::from(color.z) / 255.0,
                    f32::from(color.w) / 255.0,
                ))
            })
            .collect();
    }
    if mesh.positions.is_empty() {
        if let Some(partition) = nif
            .blocks
            .get(shape.skin as usize)
            .and_then(|block| match block {
                NifBlock::NiSkinInstance(instance) => nif
                    .blocks
                    .get(instance.skin_partition as usize)
                    .and_then(|block| match block {
                        NifBlock::NiSkinPartition(partition) => Some(partition),
                        _ => None,
                    }),
                _ => None,
            })
        {
            mesh.positions = partition
                .vertex_data
                .iter()
                .filter_map(|vertex| vertex.position)
                .collect();
            mesh.normals = partition
                .vertex_data
                .iter()
                .filter_map(|vertex| vertex.normal)
                .collect();
            mesh.uvs = partition
                .vertex_data
                .iter()
                .filter_map(|vertex| vertex.uv)
                .collect();
            mesh.colors = partition
                .vertex_data
                .iter()
                .filter_map(|vertex| vertex.vertex_colors)
                .map(|color| {
                    BSVec4(glam::Vec4::new(
                        f32::from(color.x) / 255.0,
                        f32::from(color.y) / 255.0,
                        f32::from(color.z) / 255.0,
                        f32::from(color.w) / 255.0,
                    ))
                })
                .collect();
            mesh.triangles = partition.triangles.clone();
        }
    }
    if mesh.positions.is_empty()
        && shape.num_vertices == 0
        && shape.num_triangles == 0
        && shape.data_size == 0
    {
        warn!(
            "geometry block {block_index} ({name:?}) is an explicit empty shape; preserving it as a scene node without a mesh"
        );
        model.static_nodes.push(static_node_from_av(
            nif,
            block_index,
            &shape.av,
            Vec::new(),
            None,
        ));
        return Ok(());
    }
    if mesh.uvs.iter().any(|uv| !uv.is_finite()) {
        warn!(
            "geometry block {block_index} ({name:?}) contains non-finite UVs; omitting the UV attribute while preserving geometry"
        );
        mesh.uvs.clear();
    }
    let discarded_triangles = sanitize_non_finite_positions(&mut mesh);
    if discarded_triangles != 0 {
        warn!(
            "geometry block {block_index} ({name:?}) contains non-finite positions; discarded {discarded_triangles} affected triangles"
        );
    }
    mesh.validate()
        .map_err(|error| {
            format!(
                "geometry block {block_index}: {error}; descriptor={:?}, vertices={}, triangles={}, data_size={}",
                shape.vertex_desc, shape.num_vertices, shape.num_triangles, shape.data_size
            )
        })?;
    model.static_meshes.push(mesh);
    model.static_nodes.push(static_node_from_av(
        nif,
        block_index,
        &shape.av,
        Vec::new(),
        Some(mesh_index),
    ));
    Ok(())
}

/// Bethesda's shipped effect meshes occasionally contain sentinel NaN/Inf
/// vertices. They are not drawable geometry, but rejecting the whole NIF also
/// discards its valid sibling shapes. Remove only triangles that reference a
/// sentinel and replace the now-unused position with a finite value so model
/// validation and glTF bounds remain well-defined.
fn sanitize_non_finite_positions(mesh: &mut StaticMesh) -> usize {
    let invalid = mesh
        .positions
        .iter()
        .map(|position| !position.is_finite())
        .collect::<Vec<_>>();
    if !invalid.iter().any(|value| *value) {
        return 0;
    }

    for (position, is_invalid) in mesh.positions.iter_mut().zip(&invalid) {
        if *is_invalid {
            *position = Vec3::ZERO;
        }
    }
    let before = mesh.triangles.len();
    mesh.triangles.retain(|triangle| {
        [triangle.x, triangle.y, triangle.z]
            .into_iter()
            .all(|index| !invalid.get(usize::from(index)).copied().unwrap_or(true))
    });
    before - mesh.triangles.len()
}

#[cfg(test)]
mod position_sanitization_tests {
    use super::*;

    #[test]
    fn discards_only_triangles_that_reference_non_finite_positions() {
        let mut mesh = StaticMesh {
            positions: vec![
                Vec3::ZERO,
                Vec3::X,
                Vec3::Y,
                Vec3::new(f32::INFINITY, 0.0, 0.0),
            ],
            triangles: vec![
                glam::u16::U16Vec3::new(0, 1, 2),
                glam::u16::U16Vec3::new(1, 2, 3),
            ],
            ..Default::default()
        };

        assert_eq!(sanitize_non_finite_positions(&mut mesh), 1);
        assert_eq!(mesh.triangles.len(), 1);
        assert!(mesh.positions.iter().all(|position| position.is_finite()));
        mesh.validate().unwrap();
    }
}

fn static_node_from_av(
    nif: &NifFile,
    block_index: u32,
    av: &NiAVObject,
    children: Vec<u32>,
    mesh: Option<usize>,
) -> StaticSceneNode {
    StaticSceneNode {
        block_index,
        name: nif
            .header
            .get_string(av.object.name as usize)
            .ok()
            .map(str::to_owned),
        translation: av.translation.0 .0,
        rotation: av.rotation.0 .0,
        scale: av.scale.0,
        children,
        mesh,
    }
}

pub struct NifFileV3 {
    pub header: NifHeader,
    pub raw_blocks: Vec<NifBlock>,
    pub nodes: BTreeMap<u32, NiNode>,
    pub skins: BTreeMap<u32, BSSkinInstance>,
    pub materials: BTreeMap<u32, BSShaderTextureSet>,
    pub shapes: BTreeMap<u32, BSTriShape>,
    pub seg_shapes: BTreeMap<u32, BSSubIndexTriShape>,
}

impl NifFileV3 {
    pub fn open(path: &str) -> Result<Self, std::io::Error> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let (_, nif) = NifFileV3::parse(&buf).unwrap();
        Ok(nif)
    }

    pub fn get_node(&self, index: u32) -> Option<&NiNode> {
        self.nodes.get(&index)
    }

    pub fn get_skin(&self, index: u32) -> Option<&BSSkinInstance> {
        self.skins.get(&index)
    }

    pub fn get_material(&self, index: u32) -> Option<&BSShaderTextureSet> {
        self.materials.get(&index)
    }

    pub fn get_shape(&self, index: u32) -> Option<&BSTriShape> {
        self.shapes.get(&index)
    }

    pub fn get_seg_shape(&self, index: u32) -> Option<&BSSubIndexTriShape> {
        self.seg_shapes.get(&index)
    }

    pub fn get_mat_for_shape(&self, index: u32) -> Option<&BSShaderTextureSet> {
        for (mat_index, mat) in self.materials.iter() {
            if mat_index > &index {
                return Some(mat);
            }
        }

        let (_last_index, last_mat) = self.materials.iter().last().unwrap();
        Some(last_mat)
    }

    pub fn get_mat_id_for_shape(&self, index: u32) -> u32 {
        for (mat_index, _mat) in self.materials.iter() {
            if mat_index > &index {
                return *mat_index;
            }
        }

        let (last_index, _last_mat) = self.materials.iter().last().unwrap();
        *last_index
    }

    pub fn get_node_by_name(&self, name: &str) -> Option<&NiNode> {
        if let Some(name_id) = self
            .header
            .strings
            .iter()
            .position(|string| string.0 == name)
        {
            for node in self.nodes.values() {
                if node.name() == name_id as u32 {
                    return Some(node);
                }
            }
        }

        None
    }

    pub fn has_skin(&self) -> bool {
        !self.skins.is_empty()
    }

    pub fn get_string(&self, index: usize) -> Result<&str, String> {
        self.header.get_string(index)
    }

    pub fn get_mesh_materials_ids(&self) -> Vec<u8> {
        let mut current_mat = -1;

        let mut positions = Vec::new();

        for block in self.raw_blocks.iter() {
            match block {
                NifBlock::BSTriShape(_shape) => {
                    positions.push((current_mat).max(0) as u8);
                }
                NifBlock::BSSubIndexTriShape(_shape) => {
                    positions.push((current_mat).max(0) as u8);
                }
                NifBlock::BSShaderTextureSet(_textures) => {
                    current_mat += 1;
                }
                _ => {}
            }
        }

        positions
    }

    /// Basic hacky function to get materials and textures from the nif file
    /// Returns a tuple with a vector of materials and a set of textures
    /// TODO: There is a ton of room for improvement here, but it works for now
    pub fn build_materials(&self) -> (Vec<super::model::all::Material>, BTreeSet<String>) {
        // Init empty material vector
        let mut materials = Vec::new();

        // Init empty texture set
        let mut textures = BTreeSet::new();

        // Loop through raw blocks for materials
        for block in &self.raw_blocks {
            match block {
                NifBlock::BSShaderTextureSet(set) => {
                    // Init empty material
                    let mut diffuse = None;
                    let mut normal = None;
                    let mut glow = None;
                    let mut specular = None;

                    if let Some(raw_diffuse) = &set.diffuse {
                        let mut fixed = normalize_esm_path(&raw_diffuse);
                        ensure_texture_parent(&mut fixed);

                        textures.insert(fixed.clone());
                        diffuse = Some(fixed);
                    }

                    if let Some(raw_normal) = &set.normal {
                        let mut fixed = normalize_esm_path(&raw_normal);
                        ensure_texture_parent(&mut fixed);

                        textures.insert(fixed.clone());
                        normal = Some(fixed);
                    }

                    if let Some(raw_glow) = &set.glow {
                        let mut fixed = normalize_esm_path(&raw_glow);
                        ensure_texture_parent(&mut fixed);

                        textures.insert(fixed.clone());
                        glow = Some(fixed);
                    }

                    if let Some(raw_specular) = &set.specular {
                        let mut fixed = normalize_esm_path(&raw_specular);
                        ensure_texture_parent(&mut fixed);

                        textures.insert(fixed.clone());
                        specular = Some(fixed);
                    }

                    materials.push(super::model::all::Material {
                        diffuse,
                        normal,
                        glow,
                        specular,
                    });
                }
                _ => {}
            }
        }

        (materials, textures)
    }

    pub fn to_gltf(&self, file_name: String, skeleton: Option<&NifFileV3>) -> (Root, Vec<u8>) {
        let mut root = Root::default();
        let mut bin_data = Vec::new();

        root.extensions_used
            .push("KHR_materials_pbrSpecularGlossiness".to_string());
        root.extensions_required
            .push("KHR_materials_pbrSpecularGlossiness".to_string());

        // Check if the nif file has a skeleton
        if self.has_skin() {
            // Check if skeleton is provided from another file
            if let Some(skel) = skeleton {
                // Use imported nodes
                if push_nodes_to_root(&mut root, &skel).is_err() {
                    debug!("Unknown error: Failed to push nodes to root");
                    panic!("Unknown error: Failed to push nodes to root");
                }
            } else {
                // Use nodes from current file
                if push_nodes_to_root(&mut root, &self).is_err() {
                    debug!("Unknown error: Failed to push nodes to root");
                    panic!("Unknown error: Failed to push nodes to root");
                }
            }

            // Switch back to original nif file, process meshes from nif
            for (index, shape) in &self.seg_shapes {
                let mesh_name =
                    if let Ok(name) = self.get_string(shape.bs_tri_shape.av.object.name as usize) {
                        Some(name.to_string())
                    } else {
                        debug!(
                            "Mesh name lookup failed. Could not find index [{}] in string list.",
                            index
                        );
                        None
                    };

                let mut attributes = BTreeMap::new();

                attributes.insert(
                    Checked::Valid(Semantic::Positions),
                    Index::new(root.accessors.len() as u32),
                );
                push_positions_to_root(
                    &mut root,
                    shape.bs_tri_shape.get_vertices_as_bytes(),
                    &mut bin_data,
                );

                let (min, max) = shape.bs_tri_shape.get_vertices_min_max();
                root.accessors.last_mut().unwrap().min =
                    Some(Value::Array(vec![min.x.into(), min.y.into(), min.z.into()]));
                root.accessors.last_mut().unwrap().max =
                    Some(Value::Array(vec![max.x.into(), max.y.into(), max.z.into()]));

                attributes.insert(
                    Checked::Valid(Semantic::Normals),
                    Index::new(root.accessors.len() as u32),
                );
                push_normals_to_root(
                    &mut root,
                    shape.bs_tri_shape.get_normals_as_bytes(),
                    &mut bin_data,
                );

                attributes.insert(
                    Checked::Valid(Semantic::TexCoords(0)),
                    Index::new(root.accessors.len() as u32),
                );
                push_uvs_to_root(
                    &mut root,
                    shape.bs_tri_shape.get_uvs_as_bytes(),
                    &mut bin_data,
                );

                let indices = Some(Index::new(root.accessors.len() as u32));
                push_indices_to_root(
                    &mut root,
                    shape.bs_tri_shape.get_indices_as_bytes(),
                    &mut bin_data,
                );

                if bin_data.len() % 4 != 0 {
                    let padding = 4 - (bin_data.len() % 4);
                    bin_data.extend_from_slice(&vec![0; padding]);
                }

                let weights = shape.bs_tri_shape.get_bone_weights_as_bytes();
                attributes.insert(
                    Checked::Valid(Semantic::Weights(0)),
                    Index::new(root.accessors.len() as u32),
                );

                root.accessors.push(Accessor {
                    buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
                    byte_offset: None,
                    count: USize64(weights.len() as u64 / 16),
                    component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
                    extensions: None,
                    extras: Extras::default(),
                    type_: Checked::Valid(accessor::Type::Vec4),
                    min: None,
                    max: None,
                    name: Some(format!("WEIGHTS:{}", root.meshes.len())),
                    normalized: false,
                    sparse: None,
                });

                root.buffer_views.push(View {
                    buffer: Index::new(0),
                    byte_length: USize64(weights.len() as u64),
                    byte_offset: Some(USize64(bin_data.len() as u64)),
                    byte_stride: None,
                    name: Some(format!("WEIGHTS:{}", root.meshes.len())),
                    target: Some(Checked::Valid(buffer::Target::ArrayBuffer)),
                    extensions: None,
                    extras: Extras::default(),
                });

                bin_data.extend_from_slice(&weights);

                let joints = shape.bs_tri_shape.get_bone_indices_as_bytes();
                attributes.insert(
                    Checked::Valid(Semantic::Joints(0)),
                    Index::new(root.accessors.len() as u32),
                );

                root.accessors.push(Accessor {
                    buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
                    byte_offset: None,
                    count: USize64(joints.len() as u64 / 4),
                    component_type: Checked::Valid(GenericComponentType(ComponentType::U8)),
                    extensions: None,
                    extras: Extras::default(),
                    type_: Checked::Valid(accessor::Type::Vec4),
                    min: None,
                    max: None,
                    name: Some(format!("JOINTS:{}", root.meshes.len())),
                    normalized: false,
                    sparse: None,
                });

                root.buffer_views.push(View {
                    buffer: Index::new(0),
                    byte_length: USize64(joints.len() as u64),
                    byte_offset: Some(USize64(bin_data.len() as u64)),
                    byte_stride: None,
                    name: Some(format!("JOINTS:{}", root.meshes.len())),
                    target: Some(Checked::Valid(buffer::Target::ArrayBuffer)),
                    extensions: None,
                    extras: Extras::default(),
                });

                bin_data.extend_from_slice(&joints);

                let primitives = vec![Primitive {
                    attributes,
                    extensions: None,
                    extras: None,
                    indices,
                    material: Some(Index::new(
                        self.get_mesh_materials_ids()[root.meshes.len()] as u32,
                    )),
                    mode: Checked::Valid(mesh::Mode::Triangles),
                    targets: None,
                }];

                root.nodes.push(Node {
                    camera: None,
                    children: None,
                    extensions: None,
                    extras: None,
                    matrix: None,
                    mesh: Some(Index::new(root.meshes.len() as u32)),
                    name: mesh_name.clone(),
                    rotation: None,
                    scale: None,
                    translation: None,
                    skin: Some(Index::new(root.meshes.len() as u32)),
                    weights: None,
                });

                root.meshes.push(Mesh {
                    extensions: None,
                    extras: None,
                    name: mesh_name,
                    primitives,
                    weights: None,
                });
            }

            for (_block_index, skin) in &self.skins {
                let inverse_bind_matrices = Some(Index::new(root.accessors.len() as u32));
                let mut inverse_bind_data: Vec<u8> = Vec::new();
                let mut joints = Vec::new();

                for joint in skin.bones.iter() {
                    let joint_name = self
                        .get_string(self.get_node(*joint as u32).unwrap().name() as usize)
                        .unwrap();
                    let joint_pos = find_node_in_root(&root, joint_name);
                    let new_joint = Index::new(joint_pos.unwrap() as u32);

                    if !joints.contains(&new_joint) {
                        joints.push(new_joint);
                    }
                }

                match self.raw_blocks[skin.data as usize] {
                    NifBlock::BSSkinBoneData(ref bone_data) => {
                        for bone_transform in &bone_data.bone_list {
                            // let rotation = bone_transform.rotation.to_col_major();

                            // let rotation = Matrix4([
                            //     rotation.0[0],
                            //     rotation.0[1],
                            //     rotation.0[2],
                            //     0.0,
                            //     rotation.0[3],
                            //     rotation.0[4],
                            //     rotation.0[5],
                            //     0.0,
                            //     rotation.0[6],
                            //     rotation.0[7],
                            //     rotation.0[8],
                            //     0.0,
                            //     bone_transform.translation.x,
                            //     bone_transform.translation.y,
                            //     bone_transform.translation.z,
                            //     bone_transform.scale,
                            // ]);

                            inverse_bind_data.extend_from_slice(
                                bone_transform
                                    .into_matrix4()
                                    .0
                                    .to_cols_array()
                                    .as_slice()
                                    .as_bytes(),
                            );
                        }
                    }
                    _ => {
                        panic!("Inverse position block is not a BSSkinBoneData")
                    }
                }

                root.skins.push(gltf::json::Skin {
                    extensions: None,
                    extras: Extras::default(),
                    inverse_bind_matrices,
                    joints,
                    name: None,
                    skeleton: None,
                });

                root.accessors.push(Accessor {
                    buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
                    byte_offset: None,
                    count: USize64(inverse_bind_data.len() as u64 / 64),
                    component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
                    extensions: None,
                    extras: Extras::default(),
                    type_: Checked::Valid(accessor::Type::Mat4),
                    min: None,
                    max: None,
                    name: Some(format!("INVERSE_BIND_MATRICES:{}", root.skins.len() - 1)),
                    normalized: false,
                    sparse: None,
                });

                root.buffer_views.push(View {
                    buffer: Index::new(0),
                    byte_length: USize64(inverse_bind_data.len() as u64),
                    byte_offset: Some(USize64(bin_data.len() as u64)),
                    byte_stride: None,
                    name: Some(format!("INVERSE_BIND_MATRICES:{}", root.skins.len() - 1)),
                    target: None,
                    extensions: None,
                    extras: Extras::default(),
                });

                bin_data.extend_from_slice(&inverse_bind_data);
            }
        }
        // Process as static mesh
        else {
        }

        root.buffers.push(Buffer {
            byte_length: USize64(bin_data.len() as u64),
            extensions: None,
            extras: None,
            name: None,
            uri: Some(format!("{}.bin", file_name)),
        });

        let (materials, textures) = self.build_materials();

        for texture in &textures {
            root.textures.push(Texture {
                name: None,
                sampler: None,
                source: Index::new(root.images.len() as u32),
                extensions: None,
                extras: Extras::default(),
            });

            root.images.push(Image {
                buffer_view: None,
                mime_type: None,
                name: None,
                uri: Some(texture.clone().replace(".dds", ".png")),
                extensions: None,
                extras: Extras::default(),
            });
        }

        for material in &materials {
            let mut diffuse_texture = None;
            let mut normal_texture = None;
            let mut emissive_texture = None;
            let mut specular_glossiness_texture = None;

            if let Some(d) = &material.diffuse {
                diffuse_texture = Some(Info {
                    index: Index::new(textures.iter().position(|x| x == d).unwrap() as u32),
                    tex_coord: 0,
                    extensions: None,
                    extras: Extras::default(),
                });
            }

            if let Some(n) = &material.normal {
                normal_texture = Some(NormalTexture {
                    index: Index::new(textures.iter().position(|x| x == n).unwrap() as u32),
                    scale: 1.0,
                    tex_coord: 0,
                    extensions: None,
                    extras: Extras::default(),
                });
            }

            if let Some(e) = &material.glow {
                emissive_texture = Some(Info {
                    index: Index::new(textures.iter().position(|x| x == e).unwrap() as u32),
                    tex_coord: 0,
                    extensions: None,
                    extras: Extras::default(),
                });
            }

            if let Some(s) = &material.specular {
                specular_glossiness_texture = Some(Info {
                    index: Index::new(textures.iter().position(|x| x == s).unwrap() as u32),
                    tex_coord: 0,
                    extensions: None,
                    extras: Extras::default(),
                });
            }

            root.materials.push(gltf::json::Material {
                alpha_cutoff: None,
                alpha_mode: Checked::Valid(material::AlphaMode::Opaque),
                double_sided: false,
                name: None,
                pbr_metallic_roughness: PbrMetallicRoughness {
                    base_color_factor: PbrBaseColorFactor::default(),
                    base_color_texture: None,
                    metallic_factor: StrengthFactor::default(),
                    roughness_factor: StrengthFactor::default(),
                    metallic_roughness_texture: None,
                    extensions: None,
                    extras: Extras::default(),
                },
                normal_texture,
                occlusion_texture: None,
                emissive_texture,
                emissive_factor: EmissiveFactor::default(),
                extensions: Some(gltf::json::extensions::material::Material {
                    pbr_specular_glossiness: Some(PbrSpecularGlossiness {
                        diffuse_factor: PbrDiffuseFactor::default(),
                        diffuse_texture,
                        specular_factor: PbrSpecularFactor::default(),
                        glossiness_factor: StrengthFactor::default(),
                        specular_glossiness_texture,
                        others: serde_json::Map::new(),
                        extras: Extras::default(),
                    }),
                    others: serde_json::Map::new(),
                    ..Default::default()
                }),
                extras: Extras::default(),
            });
        }

        let root_node = find_node_in_root(&root, "SkeletonExport").unwrap();

        root.scenes.push(Scene {
            extensions: None,
            extras: Extras::default(),
            name: None,
            nodes: vec![Index::new(root_node as u32)],
        });

        root.scene = Some(Index::new(0));

        (root, bin_data)
    }
}

impl Parse<&[u8]> for NifFileV3 {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, header) = NifHeader::parse(i)?;

        //println!("NIF Header: {:#?}", header);

        println!("Parsing NIF v3 file with {} blocks.", header.block_count);
        let mut raw_blocks = Vec::new();
        let mut data = i;

        for index in 0..header.block_count as usize {
            println!(
                "Parsing block {}/{} as {}",
                index,
                header.block_count,
                header.get_block_type(index).unwrap().to_string()
            );
            let (i, raw) = nom::bytes::complete::take(header.block_size_index[index])(data)?;
            data = i;
            let (_, block) =
                NifBlock::parse(raw, header.get_block_type(index).unwrap().to_string())?;
            println!(
                "Parsed block type: {:?}",
                header.get_block_type(index).unwrap().to_string()
            );
            raw_blocks.push(block);
        }

        println!("Parsing nodes...");
        let mut nodes = BTreeMap::new();

        for (index, block) in raw_blocks.iter().enumerate() {
            match block {
                NifBlock::NiNode(node) => {
                    nodes.insert(index as u32, node.clone());
                }
                _ => {}
            }
        }

        let mut skins = BTreeMap::new();

        for (index, block) in raw_blocks.iter().enumerate() {
            match block {
                NifBlock::BSSkinInstance(skin) => {
                    skins.insert(index as u32, skin.clone());
                }
                _ => {}
            }
        }

        let mut materials = BTreeMap::new();

        for (index, block) in raw_blocks.iter().enumerate() {
            match block {
                NifBlock::BSShaderTextureSet(material) => {
                    materials.insert(index as u32, material.clone());
                }
                _ => {}
            }
        }

        let mut shapes = BTreeMap::new();

        for (index, block) in raw_blocks.iter().enumerate() {
            match block {
                NifBlock::BSTriShape(shape) => {
                    shapes.insert(index as u32, shape.clone());
                }
                _ => {}
            }
        }

        let mut seg_shapes = BTreeMap::new();

        for (index, block) in raw_blocks.iter().enumerate() {
            match block {
                NifBlock::BSSubIndexTriShape(shape) => {
                    seg_shapes.insert(index as u32, shape.clone());
                }
                _ => {}
            }
        }

        Ok((
            i,
            Self {
                header,
                raw_blocks,
                nodes,
                skins,
                materials,
                shapes,
                seg_shapes,
            },
        ))
    }
}

impl std::fmt::Debug for NifFileV3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NifFileV3\nHeader: {:#?}\nNodes: {}\nSkins: {}\nMaterials: {}\nShapes: {}\nSegShapes: {}", self.header, self.nodes.len(), self.skins.len(), self.materials.len(), self.shapes.len(), self.seg_shapes.len())
    }
}

pub fn find_node_in_root(root: &Root, name: &str) -> Option<usize> {
    for (index, node) in root.nodes.iter().enumerate() {
        if let Some(node_name) = &node.name {
            if node_name == name {
                return Some(index);
            }
        }
    }

    None
}

pub fn push_nodes_to_root(root: &mut Root, nif: &NifFileV3) -> Result<(), String> {
    for (index, node) in &nif.nodes {
        let name;

        if let Ok(node_name) = nif.get_string(node.av.object.name as usize) {
            name = Some(node_name.to_string());
        } else {
            debug!(
                "Node name lookup failed. Could not find index [{}] in string list.",
                index
            );
            panic!(
                "Node name lookup failed. Could not find index [{}] in string list.",
                index
            );
        }

        let scale = if node.av.scale.0 == 1.0 {
            None
        } else {
            Some([node.av.scale.0, node.av.scale.0, node.av.scale.0])
        };

        let translation = if node.av.translation == NifTranslation::default() {
            None
        } else {
            Some([
                node.av.translation.0 .0.x,
                node.av.translation.0 .0.y,
                node.av.translation.0 .0.z,
            ])
        };

        let rotation = if node.av.rotation == NifRotation::default() {
            None
        } else {
            let q = glam::Quat::from_mat3(&node.av.rotation.0 .0);

            Some(UnitQuaternion([q.x, q.y, q.z, q.w]))

            // let q = Quaternion::from(node.av.rotation.0.to_col_major());
            // Some(q.into())
        };

        root.nodes.push(Node {
            camera: None,
            children: None,
            extensions: None,
            extras: None,
            matrix: None,
            mesh: None,
            name,
            rotation,
            scale,
            translation,
            skin: None,
            weights: None,
        });
    }

    // Update the child references now that the nodes are added
    // TODO: Verify the nif files to see if the indices always start at block zero, or if they can be offset,
    //       then merge this loop with the previous one
    for (index, (_index, skel_node)) in nif.nodes.iter().enumerate() {
        if skel_node.children.len() > 0 {
            let children = skel_node
                .children
                .iter()
                .map(|child_index| {
                    nif.get_string(nif.nodes.get(child_index).unwrap().av.object.name as usize)
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<String>>();

            let mut child_indices = Vec::new();

            for child in children {
                if let Some(child_index) = find_node_in_root(&root, &child) {
                    child_indices.push(Index::new(child_index as u32));
                } else {
                    panic!("Child node not found in root");
                }
            }

            root.nodes[index].children = Some(child_indices);
        }
    }

    Ok(())
}

pub fn push_positions_to_root(root: &mut Root, positions: Vec<u8>, bin_data: &mut Vec<u8>) {
    root.accessors.push(Accessor {
        buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
        byte_offset: None,
        count: USize64(positions.len() as u64 / 12),
        component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
        extensions: None,
        extras: Extras::default(),
        type_: Checked::Valid(accessor::Type::Vec3),
        min: None,
        max: None,
        name: Some(format!("POSITIONS:{}", root.meshes.len())),
        normalized: false,
        sparse: None,
    });

    root.buffer_views.push(View {
        buffer: Index::new(0),
        byte_length: USize64(positions.len() as u64),
        byte_offset: Some(USize64(bin_data.len() as u64)),
        byte_stride: None,
        name: Some(format!("POSITIONS:{}", root.meshes.len())),
        target: Some(Checked::Valid(buffer::Target::ArrayBuffer)),
        extensions: None,
        extras: Extras::default(),
    });

    bin_data.extend_from_slice(&positions);
}

pub fn push_normals_to_root(root: &mut Root, normals: Vec<u8>, bin_data: &mut Vec<u8>) {
    root.accessors.push(Accessor {
        buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
        byte_offset: None,
        count: USize64(normals.len() as u64 / 12),
        component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
        extensions: None,
        extras: Extras::default(),
        type_: Checked::Valid(accessor::Type::Vec3),
        min: None,
        max: None,
        name: Some(format!("NORMALS:{}", root.meshes.len())),
        normalized: false,
        sparse: None,
    });

    root.buffer_views.push(View {
        buffer: Index::new(0),
        byte_length: USize64(normals.len() as u64),
        byte_offset: Some(USize64(bin_data.len() as u64)),
        byte_stride: None,
        name: Some(format!("NORMALS:{}", root.meshes.len())),
        target: Some(Checked::Valid(buffer::Target::ArrayBuffer)),
        extensions: None,
        extras: Extras::default(),
    });

    bin_data.extend_from_slice(&normals);
}

pub fn push_uvs_to_root(root: &mut Root, uvs: Vec<u8>, bin_data: &mut Vec<u8>) {
    root.accessors.push(Accessor {
        buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
        byte_offset: None,
        count: USize64(uvs.len() as u64 / 8),
        component_type: Checked::Valid(GenericComponentType(ComponentType::F32)),
        extensions: None,
        extras: Extras::default(),
        type_: Checked::Valid(accessor::Type::Vec2),
        min: None,
        max: None,
        name: Some(format!("UVS:{}", root.meshes.len())),
        normalized: false,
        sparse: None,
    });

    root.buffer_views.push(View {
        buffer: Index::new(0),
        byte_length: USize64(uvs.len() as u64),
        byte_offset: Some(USize64(bin_data.len() as u64)),
        byte_stride: None,
        name: Some(format!("UVS:{}", root.meshes.len())),
        target: Some(Checked::Valid(buffer::Target::ArrayBuffer)),
        extensions: None,
        extras: Extras::default(),
    });

    bin_data.extend_from_slice(&uvs);
}

pub fn push_indices_to_root(root: &mut Root, indices: Vec<u8>, bin_data: &mut Vec<u8>) {
    root.accessors.push(Accessor {
        buffer_view: Some(Index::new(root.buffer_views.len() as u32)),
        byte_offset: None,
        count: USize64(indices.len() as u64 / 2),
        component_type: Checked::Valid(GenericComponentType(ComponentType::U16)),
        extensions: None,
        extras: Extras::default(),
        type_: Checked::Valid(accessor::Type::Scalar),
        min: None,
        max: None,
        name: Some(format!("INDICES:{}", root.meshes.len())),
        normalized: false,
        sparse: None,
    });

    root.buffer_views.push(View {
        buffer: Index::new(0),
        byte_length: USize64(indices.len() as u64),
        byte_offset: Some(USize64(bin_data.len() as u64)),
        byte_stride: None,
        name: Some(format!("INDICES:{}", root.meshes.len())),
        target: Some(Checked::Valid(buffer::Target::ElementArrayBuffer)),
        extensions: None,
        extras: Extras::default(),
    });

    bin_data.extend_from_slice(&indices);
}

pub fn apply_ue5_mods(root: &mut Root) {
    let root_count = root.nodes.len();
    let nodes_copy = root.nodes.clone();

    let mut first = true;

    for skin in root.skins.iter_mut() {
        if first {
            first = false;
            continue;
        }

        root.nodes.extend_from_slice(&nodes_copy);

        let mut joints = Vec::new();

        for joint in skin.joints.iter() {
            let new_joint = joint.value();
            joints.push(Index::new(new_joint as u32 + root_count as u32));
        }

        skin.joints = joints;
    }
}
