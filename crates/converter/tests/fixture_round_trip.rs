//! Round-trip tests that feed generated `dummy-content` fixtures to the real
//! converter parsers.

use converter::{
    archive::ArchiveExtractor,
    script::ScriptConverter,
    texture::{TextureConverter, TextureEncoding, inspect_ktx2},
};
use dummy_content::{Entry, ba2, bsa, dds, pex, rng::Rng};
use std::{fs, path::Path};

fn write(directory: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn generated_bsa_archives_extract_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        Entry::new("scripts/one.pex", b"PEX"),
        Entry::new("textures/two.dds", b"DDS "),
        Entry::new("scripts/three.pex", b"PEX3"),
    ];
    let cases = [
        (
            "v105-plain",
            bsa::v105(&entries, bsa::Compression::None).unwrap(),
        ),
        (
            "v105-zlib",
            bsa::v105(&entries, bsa::Compression::Zlib).unwrap(),
        ),
        (
            "v105-lz4",
            bsa::v105(&entries, bsa::Compression::Lz4).unwrap(),
        ),
        (
            "v104-plain",
            bsa::v104(&entries, bsa::Compression::None).unwrap(),
        ),
        (
            "v104-zlib",
            bsa::v104(&entries, bsa::Compression::Zlib).unwrap(),
        ),
    ];

    for (label, bytes) in cases {
        let archive = write(directory.path(), &format!("{label}.bsa"), &bytes);
        let output = directory.path().join(label);
        let extracted = ArchiveExtractor::extract(&archive, &output).unwrap();
        assert_eq!(extracted.len(), 3, "{label}");
        assert_eq!(
            fs::read(output.join("scripts/one.pex")).unwrap(),
            b"PEX",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("textures/two.dds")).unwrap(),
            b"DDS ",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("scripts/three.pex")).unwrap(),
            b"PEX3",
            "{label}"
        );
    }
}

#[test]
fn generated_ba2_archives_extract_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        Entry::new("textures/one.dds", b"DDS "),
        Entry::new("meshes/two.nif", b"NIF"),
    ];
    let cases = [
        (
            "gnrl-plain",
            ba2::general(&entries, ba2::Compression::None).unwrap(),
        ),
        (
            "gnrl-zlib",
            ba2::general(&entries, ba2::Compression::Zlib).unwrap(),
        ),
    ];

    for (label, bytes) in cases {
        let archive = write(directory.path(), &format!("{label}.ba2"), &bytes);
        let output = directory.path().join(label);
        let extracted = ArchiveExtractor::extract(&archive, &output).unwrap();
        assert_eq!(extracted.len(), 2, "{label}");
        assert_eq!(
            fs::read(output.join("textures/one.dds")).unwrap(),
            b"DDS ",
            "{label}"
        );
        assert_eq!(
            fs::read(output.join("meshes/two.nif")).unwrap(),
            b"NIF",
            "{label}"
        );
    }

    let pixels = [0xAB; 8];
    let bytes = ba2::dx10(&[ba2::Dx10Texture::new(
        "textures/dx10.dds",
        4,
        4,
        71,
        &pixels,
    )])
    .unwrap();
    let archive = write(directory.path(), "dx10.ba2", &bytes);
    let output = directory.path().join("dx10");
    ArchiveExtractor::extract(&archive, &output).unwrap();
    let dds_bytes = fs::read(output.join("textures/dx10.dds")).unwrap();
    assert_eq!(&dds_bytes[..4], b"DDS ");
    let ktx2 = TextureConverter::convert(&dds_bytes, TextureEncoding::ColorSrgb).unwrap();
    inspect_ktx2(&ktx2, TextureEncoding::ColorSrgb).unwrap();
}

#[test]
fn generated_dds_textures_convert_to_ktx2() {
    let mut rng = Rng::new(42);
    let cases = [
        (
            dds::Spec::new(dds::Format::X8R8G8B8, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
        (
            dds::Spec::new(dds::Format::Bc1Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
        (
            dds::Spec::new(dds::Format::Bc5Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::NormalLinear,
        ),
        (
            dds::Spec::new(dds::Format::Bc7Unorm, 8, 8).with_mip_levels(3),
            TextureEncoding::ColorSrgb,
        ),
    ];

    for (spec, encoding) in cases {
        let bytes = dds::generate(&spec, &mut rng).unwrap();
        let ktx2 = TextureConverter::convert(&bytes, encoding)
            .unwrap_or_else(|error| panic!("{spec:?}: {error:#}"));
        let metadata = inspect_ktx2(&ktx2, encoding).unwrap();
        assert_eq!(metadata.width, spec.width, "{spec:?}");
        assert_eq!(metadata.height, spec.height, "{spec:?}");
        assert_eq!(metadata.levels, spec.mip_levels, "{spec:?}");
    }

    let cube = dds::generate(
        &dds::Spec::new(dds::Format::Bc1Unorm, 4, 4).as_cubemap(),
        &mut rng,
    )
    .unwrap();
    let metadata = inspect_ktx2(
        &TextureConverter::convert(&cube, TextureEncoding::ColorSrgb).unwrap(),
        TextureEncoding::ColorSrgb,
    )
    .unwrap();
    assert_eq!(metadata.faces, 6);

    let volume = dds::generate(
        &dds::Spec::new(dds::Format::Bc1Unorm, 4, 4).with_depth(4),
        &mut rng,
    )
    .unwrap();
    let metadata = inspect_ktx2(
        &TextureConverter::convert(&volume, TextureEncoding::DataLinear).unwrap(),
        TextureEncoding::DataLinear,
    )
    .unwrap();
    assert_eq!(metadata.depth, 4);
}

#[test]
fn generated_pex_scripts_convert_to_luau() {
    let directory = tempfile::tempdir().unwrap();
    let input = write(
        directory.path(),
        "script.pex",
        &pex::minimal("Generated").unwrap(),
    );
    let output = directory.path().join("scripts/script.luau");
    ScriptConverter::convert_pex_to_luau(&input, &output).unwrap();
    let generated = fs::read_to_string(output).unwrap();
    assert!(generated.contains("Generated"));
    assert!(generated.ends_with("return Script\n"));
}

#[test]
fn generated_nif_static_shape_converts_to_glb() {
    use converter::mesh::MeshConverter;

    let directory = tempfile::tempdir().unwrap();
    let nif_path = directory.path().join("generated.nif");
    let shape = dummy_content::nif::StaticShape {
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
    };
    fs::write(&nif_path, dummy_content::nif::static_shape(&shape).unwrap()).unwrap();

    let diagnostics = MeshConverter::inspect_nif(&nif_path).unwrap();
    assert_eq!(diagnostics.geometry_block_count, 1);
    assert_eq!(diagnostics.validated_material_shape_count, 1);

    let output = directory.path().join("generated.glb");
    MeshConverter::convert_nif_to_glb(&nif_path, &output).unwrap();
    assert!(output.is_file());
    let bounds = MeshConverter::glb_bounds(&output).unwrap();
    // The exporter bakes the Z-up to Y-up runtime rotation into the mesh, so
    // the quad spans -1..1 on X/Z with a flat Y axis.
    for (axis, value) in bounds.min.iter().enumerate() {
        let expected = if axis == 1 { 0.0 } else { -1.0 };
        assert!(
            (value - expected).abs() < 1.0e-5,
            "min axis {axis}: {value} != {expected}"
        );
    }
    for (axis, value) in bounds.max.iter().enumerate() {
        let expected = if axis == 1 { 0.0 } else { 1.0 };
        assert!(
            (value - expected).abs() < 1.0e-5,
            "max axis {axis}: {value} != {expected}"
        );
    }
}

#[test]
fn generated_esm_plugin_exports_world_database() {
    use converter::esm::{
        EsmParser,
        cell_cache::{validate_cell_cache, write_cell_cache},
        exporter::validate_database,
    };

    let directory = tempfile::tempdir().unwrap();
    let plugin_path = directory.path().join("Skyrim.esm");
    let cells = [
        dummy_content::esm::Cell {
            grid_x: 0,
            grid_y: 0,
        },
        dummy_content::esm::Cell {
            grid_x: 1,
            grid_y: 0,
        },
        dummy_content::esm::Cell {
            grid_x: 0,
            grid_y: 1,
        },
        dummy_content::esm::Cell {
            grid_x: 1,
            grid_y: 1,
        },
    ];
    let plugin = dummy_content::esm::plugin(&dummy_content::esm::Plugin {
        author: "OpenSkyrim dummy-content",
        worldspace: "GeneratedWorld",
        cells: &cells,
        model_path: "meshes/generated.nif",
        diffuse: "textures/generated_color.dds",
        normal_texture: "textures/generated_normal.dds",
    })
    .unwrap();
    fs::write(&plugin_path, plugin).unwrap();

    let db_path = directory.path().join("skyrim_world.db");
    EsmParser::convert_plugins(std::slice::from_ref(&plugin_path), &db_path).unwrap();
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    validate_database(&connection).unwrap();
    for (table, expected) in [
        ("worldspaces", 1_i64),
        ("cells", 4),
        ("land", 4),
        ("\"references\"", 4),
        ("statics", 1),
        ("texture_sets", 1),
        ("landscape_textures", 1),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, expected, "unexpected row count for {table}");
    }

    let merged = EsmParser::merge_plugins(std::slice::from_ref(&plugin_path)).unwrap();
    let cache_path = directory.path().join("cell_cache.rkyv");
    let cached_cells = write_cell_cache(&merged, &cache_path).unwrap();
    assert_eq!(cached_cells, 4);
    validate_cell_cache(&cache_path).unwrap();
}

#[test]
#[ignore = "requires OPENSKYRIM_STATIC_NIF_FIXTURE with a locally installed Skyrim NIF"]
fn real_static_nif_matches_writer_version_assumptions() {
    use converter::mesh::MeshConverter;

    let path = std::env::var_os("OPENSKYRIM_STATIC_NIF_FIXTURE")
        .map(std::path::PathBuf::from)
        .expect("set OPENSKYRIM_STATIC_NIF_FIXTURE to a static Skyrim NIF");
    let bytes = fs::read(&path).unwrap();
    let line = b"Gamebryo File Format, Version 20.2.0.7\n";
    assert!(bytes.starts_with(line), "unexpected NIF signature");
    let version = u32::from_le_bytes(bytes[line.len()..line.len() + 4].try_into().unwrap());
    let user = u32::from_le_bytes(bytes[line.len() + 5..line.len() + 9].try_into().unwrap());
    let bethesda = u32::from_le_bytes(bytes[line.len() + 13..line.len() + 17].try_into().unwrap());
    assert_eq!(version, 0x1402_0007);
    assert_eq!(user, 12);
    assert_eq!(bethesda, 100);

    let diagnostics = MeshConverter::inspect_nif(&path).unwrap();
    assert!(diagnostics.block_count > 0);
}

/// Conversion regenerates terrain while retaining compatible current and historical asset caches.
#[tokio::test]
async fn generated_data_directory_converts_end_to_end() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    dummy_content::layout::prepare_directory(&data, false).unwrap();
    dummy_content::layout::generate(
        &data,
        dummy_content::layout::DEFAULT_SEED,
        dummy_content::layout::Formats::all(),
    )
    .unwrap();

    let output = directory.path().join("modern");
    let config = converter::PipelineConfig::new(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();

    assert!(report.complete);
    assert_eq!(report.skipped, 0);
    assert!(report.lod_chunks > 0);
    assert!(output.join("conversion-manifest.json").is_file());
    let lod_manifest_path = output.join("lod-manifest.json");
    assert!(lod_manifest_path.is_file());
    let lod_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&lod_manifest_path).unwrap()).unwrap();
    let database = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let database_identity: String = database
        .query_row(
            "SELECT build_identity FROM lod_build WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let database_chunks: i64 = database
        .query_row("SELECT count(*) FROM lod_chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(lod_manifest["build_identity"], database_identity);
    assert_eq!(lod_manifest["chunks"].as_i64(), Some(database_chunks));
    assert_eq!(database_chunks as u64, report.lod_chunks);
    for relative in [
        "lod-manifest.json",
        "scripts/generated.luau",
        "scripts/second.luau",
        "textures/generated_color.ktx2",
        "textures/generated_normal.ktx2",
        "textures/generated_color_x8.ktx2",
        "textures/generated_cube.ktx2",
        "textures/generated_volume.ktx2",
        "meshes/generated.glb",
        "skyrim_world.db",
        "cell_cache.rkyv",
    ] {
        assert!(output.join(relative).is_file(), "missing {relative}");
    }

    let config = converter::PipelineConfig::new(&data, &output);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    assert!(report.complete);
    assert_eq!(report.converted, 0);
    assert!(report.cache_hits > 0);

    // Hashes from the pre-terrain-change asset contract at 7740c8e, with default
    // settings. Do not generate these using the implementation under test: that
    // would hide accidental invalidation of previously published manifests.
    let historical_hashes = [
        (
            12,
            "25430020029bb7e46480eaea182f72514a085d28dd1cffc6997039a496a7a72e",
        ),
        (
            13,
            "d47cc9dcd23a7076a3fc1a19324c1ce05ba2df558d7549e5465236daa1f1d57f",
        ),
        (
            14,
            "1a56351b088bb4a65b89e4313abc9a0813c5f4fa29dae55695316b6d8970fa41",
        ),
        (
            15,
            "9a58fda00b27d0f2a8e46afb9334ea869602556393a35bffd7fcb39582a08a4f",
        ),
        (
            16,
            "ebc4fe2f7d531e796e6a5e22b70a1f6a877c01e963b54b416b789d0f64312427",
        ),
    ];
    let manifest_path = output.join("conversion-manifest.json");
    let published: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let cache_path = output.join("cell_cache.rkyv");
    let empty_cache = rkyv::to_bytes::<rkyv::rancor::Error>(&shared::CellCache {
        version: shared::CELL_CACHE_VERSION,
        cells: Vec::new(),
    })
    .unwrap();
    for (schema, configuration_hash) in historical_hashes {
        let mut manifest = published.clone();
        manifest["schema_version"] = schema.into();
        manifest["configuration_hash"] = configuration_hash.into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::write(&cache_path, &empty_cache).unwrap();
        let config = converter::PipelineConfig::new(&data, &output);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let report = converter::AssetPipeline::run_async(config, tx)
            .await
            .unwrap();
        drain.await.unwrap();
        assert!(report.complete, "schema {schema}");
        // Historical migrations deliberately rebuild the one model, retaining
        // five textures and two scripts; schema 16 also retains the model.
        assert_eq!(report.converted, u64::from(schema < 16), "schema {schema}");
        assert!(report.cache_hits >= 7, "schema {schema}: {report:?}");
        let mmap = converter::esm::cell_cache::validate_cell_cache(&cache_path).unwrap();
        let cache = rkyv::access::<shared::ArchivedCellCache, rkyv::rancor::Error>(&mmap).unwrap();
        assert_eq!(
            cache.cells.len(),
            9,
            "terrain must regenerate despite asset reuse"
        );
    }
}
