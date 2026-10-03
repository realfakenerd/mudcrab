#![allow(unused_imports)]
use std::{io::Write, path::Path};

use crate::dev::*;

const OUT_DIR: &str = "./out/";

#[test]
#[ignore = "manual Fallout 4 fixture test; requires local Deathclaw assets"]
pub fn dev_test() {
    use log::*;
    use simplelog::*;
    use std::fs::File;

    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Info,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        WriteLogger::new(
            LevelFilter::Debug,
            Config::default(),
            File::create("./nif.log").unwrap(),
        ),
    ])
    .unwrap();

    const PATH: &str = ".\\Meshes\\Actors\\Deathclaw\\Deathclaw.nif";
    const SKEL: &str = ".\\Meshes\\Actors\\Deathclaw\\CharacterAssets\\skeleton.nif";

    let mut file = NifFileV3::open(PATH).unwrap();

    debug!("{:#?}", file);

    for block in &file.raw_blocks {
        match block {
            NifBlock::BSShaderTextureSet(tset) => {
                debug!("{:#?}", tset);
            }
            _ => {}
        }
    }

    //debug!("{:#?}", file.header);

    let skel = NifFileV3::open(SKEL).unwrap();

    let (gltf, bin_data) = file.to_gltf("Deathclaw".to_string(), Some(&skel));

    let path = Path::new(OUT_DIR).join("deathclaw.gltf");
    let mut file = File::create(&path).unwrap();
    let mut bin = File::create(path.with_extension("bin")).unwrap();

    file.write_all(gltf.to_string_pretty().unwrap().as_bytes())
        .unwrap();
    bin.write_all(&bin_data).unwrap();
}

/*
//#[test]
pub fn test_all_in_archive() {
    use ba2::prelude::*;
    use simplelog::*;
    use std::fs::File;


    CombinedLogger::init(
        vec![
            TermLogger::new(LevelFilter::Info, Config::default(), TerminalMode::Mixed, ColorChoice::Auto),
            WriteLogger::new(LevelFilter::Debug, Config::default(), File::create("./nif.log").unwrap()),
        ]
    ).unwrap();

    if let Ok(mut ba2) = GeneralArchive::open("D:\\SteamLibrary\\steamapps\\common\\Fallout 4\\Data\\Fallout4 - Meshes.ba2") {

        let names = ba2.entries.clone();

        for (name, _entry) in names {
            if name.ends_with(".nif") {
                if let Ok(data) = &ba2.read_file(name.as_str()) {
                    if let Ok((_data, nif)) = NifFile::parse(&data) {

                        let full = OUT_DIR.to_string() + &name.replace(".nif", ".gltf");

                        let path = Path::new(&full);


                        info!("Writing \"{}\"", path.to_string_lossy().to_string());

                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

                        if let Ok(mut file) = std::fs::File::create(path.to_string_lossy().to_string()) {
                            if let Ok(model) = Model::try_from(nif) {
                                let (root, bin_data) = model.to_gltf(path.file_stem().unwrap().to_string_lossy().to_string());

                                let mut bin = std::fs::File::create(path.with_extension("bin")).unwrap();

                                file.write_all(root.to_string_pretty().unwrap().as_bytes()).unwrap();
                                bin.write_all(&bin_data).unwrap();
                            } else {
                                error!("Failed to convert \"{}\"", name);
                            }
                        } else {
                            error!("Failed to write \"{}\"", name);
                        }




                    } else {
                        error!("Failed to parse \"{}\"", name);
                    }
                } else {
                    debug!("Failed to read \"{}\"", name);
                }

            } else {
                debug!("Skipping \"{}\"", name);
            }
        }

    } else {
        panic!("Failed to open archive");
    }

}
*/

mod multi_bound {
    use project_wormhole_ba2::dev::MaxRef;
    use project_wormhole_shared::glam::{Mat3, Vec3};

    use crate::dev::*;

    /// The compact `NiAVObject` Skyrim writes on scene nodes and shapes.
    fn av_object_bytes(name: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_u32(&mut bytes, name);
        push_u32(&mut bytes, u32::MAX); // extra data
        push_u32(&mut bytes, u32::MAX); // controller
        push_u32(&mut bytes, 0); // flags
        for value in [0.0f32, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        push_u32(&mut bytes, u32::MAX); // collision object
        bytes
    }

    /// An `NiNode` with one child and an empty effect list.
    fn node_bytes(name: u32, child: u32) -> Vec<u8> {
        let mut bytes = av_object_bytes(name);
        push_u32(&mut bytes, 1); // children
        push_u32(&mut bytes, child);
        push_u32(&mut bytes, 0); // effects
        bytes
    }

    fn multi_bound_node_bytes(name: u32, child: u32, bound: u32, culling: Option<u32>) -> Vec<u8> {
        let mut bytes = node_bytes(name, child);
        push_u32(&mut bytes, bound);
        if let Some(culling) = culling {
            push_u32(&mut bytes, culling);
        }
        bytes
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn parse_block(bytes: &[u8], block_type: &str) -> NifBlock {
        let (_, block) = NifBlock::parse(bytes, block_type.to_string())
            .unwrap_or_else(|error| panic!("{block_type} did not parse: {error:?}"));
        block
    }

    fn nif_file(blocks: Vec<NifBlock>, strings: &[&str]) -> NifFile {
        let strings = strings
            .iter()
            .map(|value| SizedString32((*value).to_owned()))
            .collect::<Vec<_>>();
        let block_count = blocks.len() as u32;
        NifFile {
            header: NifHeader {
                file_desc: StringN {
                    value: String::new(),
                },
                nif_version: NifFileVersion(0x1402_0007),
                endian_type: Endianess::Little,
                user_version: 12,
                block_count,
                bethesda_version: 100,
                author: None,
                process_script: None,
                export_script: None,
                max_filepath: None,
                block_types: Vec::new(),
                block_type_index: vec![0; blocks.len()],
                block_size_index: vec![0; blocks.len()],
                string_count: strings.len() as u32,
                string_max_size: 0,
                strings,
                groups: Vec::new(),
            },
            blocks,
        }
    }

    #[test]
    fn parses_multi_bound_data_reference() {
        let data = 3u32.to_le_bytes();
        let (rest, bound) = BSMultiBound::parse(&data).unwrap();
        assert!(rest.is_empty());
        assert_eq!(bound.data, MaxRef(Some(3)));

        let null = u32::MAX.to_le_bytes();
        let (_, empty) = BSMultiBound::parse(&null).unwrap();
        assert_eq!(empty.data, MaxRef(None));

        assert!(BSMultiBound::parse(&[]).is_err());
    }

    #[test]
    fn parses_multi_bound_volumes() {
        let mut aabb = Vec::new();
        for value in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            aabb.extend_from_slice(&value.to_le_bytes());
        }
        let (rest, parsed) = BSMultiBoundAABB::parse(&aabb).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed.position.0, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(parsed.extent.0, Vec3::new(4.0, 5.0, 6.0));
        assert!(BSMultiBoundAABB::parse(&aabb[..20]).is_err());

        let mut obb = Vec::new();
        for value in 0..15 {
            obb.extend_from_slice(&(value as f32).to_le_bytes());
        }
        let (rest, parsed) = BSMultiBoundOBB::parse(&obb).unwrap();
        assert!(rest.is_empty());
        assert_eq!(parsed.center.0, Vec3::new(0.0, 1.0, 2.0));
        assert_eq!(parsed.size.0, Vec3::new(3.0, 4.0, 5.0));
        // File matrices are row-major; the shared parser stores the transpose.
        assert_eq!(
            parsed.rotation.0,
            Mat3::from_cols_array(&[6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0]).transpose()
        );
        assert!(BSMultiBoundOBB::parse(&obb[..56]).is_err());
    }

    #[test]
    fn parses_multi_bound_node_with_and_without_a_culling_mode() {
        let bytes = multi_bound_node_bytes(1, 2, 3, Some(4));
        let (rest, node) = BSMultiBoundNode::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(node.node.name(), 1);
        assert_eq!(node.node.children, vec![2]);
        assert_eq!(node.bound, MaxRef(Some(3)));
        assert_eq!(node.culling_mode, Some(4));

        let bytes = multi_bound_node_bytes(1, 2, 3, None);
        let (rest, node) = BSMultiBoundNode::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(node.bound, MaxRef(Some(3)));
        assert_eq!(node.culling_mode, None);
    }

    #[test]
    fn dispatches_multi_bound_blocks_instead_of_falling_back() {
        let bytes = multi_bound_node_bytes(0, 1, 2, Some(3));
        assert!(matches!(
            parse_block(&bytes, "BSMultiBoundNode"),
            NifBlock::BSMultiBoundNode(_)
        ));
        assert!(matches!(
            parse_block(&[0u8; 4], "BSMultiBound"),
            NifBlock::BSMultiBound(_)
        ));
        assert!(matches!(
            parse_block(&[0; 24], "BSMultiBoundAABB"),
            NifBlock::BSMultiBoundAABB(_)
        ));
        assert!(matches!(
            parse_block(&[0; 60], "BSMultiBoundOBB"),
            NifBlock::BSMultiBoundOBB(_)
        ));
    }

    /// An empty `BSTriShape` payload: NiAVObject, bound, skin/shader/alpha
    /// references, vertex descriptor and zero counts.
    fn empty_shape_bytes() -> Vec<u8> {
        shape_bytes(0)
    }

    /// The same empty payload, declaring `num_triangles` triangles without
    /// storing geometry: a zero data size means there is nothing to read.
    fn shape_bytes(num_triangles: u16) -> Vec<u8> {
        let mut bytes = av_object_bytes(u32::MAX);
        for value in [0.0f32, 0.0, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes()); // bounding sphere
        }
        push_u32(&mut bytes, u32::MAX); // skin
        push_u32(&mut bytes, u32::MAX); // shader property
        push_u32(&mut bytes, u32::MAX); // alpha property
        bytes.extend_from_slice(&0u64.to_le_bytes()); // vertex descriptor
        bytes.extend_from_slice(&num_triangles.to_le_bytes()); // triangles
        bytes.extend_from_slice(&0u16.to_le_bytes()); // vertices
        push_u32(&mut bytes, 0); // data size
        bytes
    }

    fn sub_index_bytes(
        num_triangles: u16,
        declared_segments: u32,
        segments: &[(u8, u32, u32)],
    ) -> Vec<u8> {
        let mut bytes = shape_bytes(num_triangles);
        push_u32(&mut bytes, 0); // trailing shape value
        push_u32(&mut bytes, declared_segments);
        for (flag, value, primitives) in segments {
            bytes.push(*flag);
            push_u32(&mut bytes, *value);
            push_u32(&mut bytes, *primitives);
        }
        bytes
    }

    #[test]
    fn parses_the_measured_sub_index_segment_table() {
        let bytes = sub_index_bytes(42, 2, &[(0, 3, 12), (7, 4096, 30)]);
        let (rest, shape) = BSSubIndexTriShape::parse(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(shape.num_segments, 2);
        assert_eq!(shape.segments.len(), 2);
        assert_eq!(shape.segments[0].flag, 0);
        assert_eq!(shape.segments[0].value, 3);
        assert_eq!(shape.segments[0].num_primitives, 12);
        assert_eq!(shape.segments[1].flag, 7);
        assert_eq!(shape.segments[1].value, 4096);
        assert_eq!(shape.segments[1].num_primitives, 30);
        assert_eq!(shape.bs_tri_shape.num_triangles, 42);
        assert_eq!(shape.bs_tri_shape.num_vertices, 0);
    }

    #[test]
    fn rejects_a_truncated_or_implausible_segment_table() {
        // Two segments declared, one record present.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(1, 2, &[(0, 0, 1)])).is_err());

        // A segment count larger than the block can hold must not allocate.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(0, 1_000_000, &[])).is_err());

        // The segments partition the shape's triangles, so their primitive
        // counts have to add up to it.
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(42, 1, &[(0, 0, 41)])).is_err());
        assert!(BSSubIndexTriShape::parse(&sub_index_bytes(42, 0, &[])).is_err());
        let partitioned = sub_index_bytes(42, 2, &[(0, 0, 12), (0, 12, 30)]);
        let (rest, shape) = BSSubIndexTriShape::parse(&partitioned).unwrap();
        assert!(rest.is_empty());
        assert_eq!(shape.segments.len(), 2);
    }

    #[test]
    fn consumes_the_trailing_word_of_a_lod_shape() {
        // A `.btr` shape block is its geometry payload plus one zero word.
        let mut bytes = empty_shape_bytes();
        push_u32(&mut bytes, 0);
        let (rest, block) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert!(matches!(block, NifBlock::BSTriShape(_)));
        assert!(rest.is_empty());

        // An ordinary shape ends at its payload.
        let bytes = empty_shape_bytes();
        let (rest, _) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert!(rest.is_empty());

        // Anything else is left for the leftover warning.
        let mut bytes = empty_shape_bytes();
        push_u32(&mut bytes, 5);
        let (rest, _) = NifBlock::parse(&bytes, "BSTriShape".to_string()).unwrap();
        assert_eq!(rest.len(), 4);
    }

    #[test]
    fn distant_lod_vertex_colours_stay_inside_the_container() {
        let nif = nif_file(
            vec![
                parse_block(&node_bytes(0, 1), "NiNode"),
                parse_block(
                    &multi_bound_node_bytes(1, 2, 5, Some(3)),
                    "BSMultiBoundNode",
                ),
                parse_block(&shape_bytes(0), "BSTriShape"),
                parse_block(&node_bytes(3, 4), "NiNode"),
                parse_block(&shape_bytes(0), "BSTriShape"),
                parse_block(&[0u8; 4], "BSMultiBound"),
            ],
            &["root"],
        );
        // Only the container and its subtree keep vertex colours: the root node
        // above it and the second node's shape do not.
        assert_eq!(
            crate::nif_file::distant_lod_subtree_blocks(&nif),
            vec![false, true, true, false, false, false]
        );
    }

    #[test]
    fn multi_bound_nodes_behave_as_nodes() {
        let root = parse_block(
            &multi_bound_node_bytes(0, 1, 2, Some(3)),
            "BSMultiBoundNode",
        );
        let child = parse_block(
            &multi_bound_node_bytes(1, u32::MAX, 2, Some(3)),
            "BSMultiBoundNode",
        );
        assert!(root.as_node().unwrap().children == vec![1]);

        let mut nif = nif_file(vec![root, child], &["root", "child"]);
        assert_eq!(nif.get_node_pos("root"), Some(0));
        assert_eq!(nif.get_node_pos("child"), Some(1));
        assert_eq!(nif.get_nodes().len(), 2);

        // The root's child must reach the static scene as an ordinary node.
        let model = nif_to_static_model(&nif).unwrap();
        assert_eq!(model.static_nodes.len(), 2);
        let root_node = model
            .static_nodes
            .iter()
            .find(|node| node.name.as_deref() == Some("root"))
            .unwrap();
        assert_eq!(root_node.children, vec![1]);
        assert!(root_node.mesh.is_none());
    }
}
