//! Skyrim sidecars through archive ingestion, loose overrides, and LOD compilation.

use dummy_content::{Entry, bsa, layout};
use std::{fs, path::Path};

// Skyrim layout: two i16 origins, then i32 stride/minimum/maximum levels.
const PACKED_SETTINGS: [u8; 16] = [0xfc, 0xff, 0xfc, 0xff, 32, 0, 0, 0, 4, 0, 0, 0, 32, 0, 0, 0];
const LOOSE_SETTINGS: [u8; 16] = [0xf8, 0xff, 0xf8, 0xff, 32, 0, 0, 0, 4, 0, 0, 0, 32, 0, 0, 0];

fn generate_data(root: &Path) {
    layout::prepare_directory(root, false).unwrap();
    let formats = layout::Formats::parse("dds,pex,nif,esm").unwrap();
    layout::generate(root, layout::DEFAULT_SEED, formats).unwrap();
    let archive = bsa::v105(
        &[Entry {
            name: "LODSettings/GeneratedWorld.LOD",
            data: &PACKED_SETTINGS,
        }],
        bsa::Compression::None,
    )
    .unwrap();
    fs::write(root.join("Skyrim - Misc.bsa"), archive).unwrap();
}

async fn convert(data: &Path, output: &Path) -> converter::pipeline::PipelineReport {
    convert_config(converter::PipelineConfig::new(data, output)).await
}

async fn convert_config(config: converter::PipelineConfig) -> converter::PipelineReport {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let report = converter::AssetPipeline::run_async(config, tx)
        .await
        .unwrap();
    drain.await.unwrap();
    report
}

#[tokio::test]
async fn packed_sidecars_compile_lod_and_loose_settings_override_them() {
    for loose_override in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("Data");
        generate_data(&data);
        if loose_override {
            fs::create_dir(data.join("LodSettings")).unwrap();
            fs::write(data.join("LodSettings/GeneratedWorld.LOD"), LOOSE_SETTINGS).unwrap();
        }
        let output = directory.path().join("modern");
        let report = convert(&data, &output).await;

        assert!(report.complete, "{:?}", report.warnings);
        assert!(report.lod_warnings.is_empty(), "{:?}", report.lod_warnings);
        assert!(report.lod_chunks > 0, "archive sidecar must produce chunks");
        let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
        let origin: (i32, i32) = connection
            .query_row(
                "SELECT lod_origin_x, lod_origin_y FROM worldspaces WHERE editor_id = 'GeneratedWorld'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, if loose_override { (-8, -8) } else { (-4, -4) });
        let indexed: u64 = connection
            .query_row("SELECT count(*) FROM lod_chunks", [], |row| row.get(0))
            .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("lod-manifest.json")).unwrap()).unwrap();
        assert_eq!(indexed, report.lod_chunks);
        assert_eq!(manifest["chunks"], indexed);
        let paths: Vec<String> = connection
            .prepare("SELECT payload_path FROM lod_chunks")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for path in paths {
            let bytes = fs::read(output.join(path)).unwrap();
            let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
            let gltf: serde_json::Value =
                serde_json::from_slice(&bytes[20..20 + json_length]).unwrap();
            assert_eq!(
                gltf["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"], 0,
                "LOD-V9: terrain must carry baked LAND albedo, not vertex tint alone"
            );
            assert_eq!(gltf["images"][0]["mimeType"], "image/ktx2");
            assert!(gltf["images"][0]["bufferView"].is_u64());
        }
    }
}

#[tokio::test]
async fn malformed_loose_settings_do_not_fall_back_to_packed_settings() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    generate_data(&data);
    fs::create_dir(data.join("LodSettings")).unwrap();
    fs::write(data.join("LodSettings/GeneratedWorld.LOD"), b"invalid").unwrap();
    let output = directory.path().join("modern");
    let report = convert(&data, &output).await;

    assert!(report.complete);
    assert_eq!(report.lod_chunks, 0);
    assert_eq!(report.lod_warnings.len(), 1);
    assert!(report.lod_warnings[0].contains("invalid LOD settings"));
    assert!(!output.join("lod-manifest.json").exists());
    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let origin: (Option<i32>, Option<i32>) = connection
        .query_row(
            "SELECT lod_origin_x, lod_origin_y FROM worldspaces",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(origin, (None, None));
}

fn indexed_chunks(output: &Path) -> Vec<(i32, i32, i32, String)> {
    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    connection
        .prepare("SELECT tier, anchor_x, anchor_y, content_hash FROM lod_chunks ORDER BY tier, anchor_x, anchor_y")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn resumed_lod_with_changed_origin_matches_a_clean_build() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    generate_data(&data);
    let output = directory.path().join("modern");
    convert(&data, &output).await;
    let staging = output.with_extension("staging-lod-retry");
    fs::rename(&output, &staging).unwrap();

    let mut resumed = converter::PipelineConfig::new(&data, &output);
    resumed.resume_staging = Some(staging);
    resumed
        .lod_origins
        .insert("GeneratedWorld".into(), [96, 96]);
    let report = convert_config(resumed.clone()).await;
    let clean_output = directory.path().join("clean");
    let mut clean = resumed;
    clean.output_dir = clean_output.clone();
    clean.resume_staging = None;
    convert_config(clean).await;

    assert_eq!(indexed_chunks(&output), indexed_chunks(&clean_output));
    assert_eq!(indexed_chunks(&output).len() as u64, report.lod_chunks);
    let connection = rusqlite::Connection::open(output.join("skyrim_world.db")).unwrap();
    let indexed: u64 = connection
        .query_row("SELECT count(*) FROM lod_chunks_spatial", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(indexed, report.lod_chunks);
}

#[tokio::test]
async fn resumed_lod_drops_removed_sidecars_and_the_previous_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    generate_data(&data);
    let output = directory.path().join("modern");
    convert(&data, &output).await;
    let staging = output.with_extension("staging-lod-retry");
    fs::rename(&output, &staging).unwrap();
    fs::remove_file(data.join("Skyrim - Misc.bsa")).unwrap();

    let mut config = converter::PipelineConfig::new(&data, &output);
    config.resume_staging = Some(staging);
    let report = convert_config(config).await;
    assert!(report.complete);
    assert_eq!(report.lod_chunks, 0);
    assert!(indexed_chunks(&output).is_empty());
    assert!(!output.join("lod-manifest.json").exists());
    assert!(!output.join("lod").exists());
    assert!(!output.join("vfs/lodsettings/generatedworld.lod").exists());
}

#[tokio::test]
async fn resumed_conversion_drops_removed_textures_and_stale_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    generate_data(&data);
    let output = directory.path().join("modern");
    assert!(convert(&data, &output).await.complete);
    let staging = output.with_extension("staging-assets-retry");
    fs::rename(&output, &staging).unwrap();
    fs::write(staging.join("scripts/obsolete.luau"), b"-- removed script").unwrap();
    for sidecar in [
        "metadata-rebuild.json",
        "metadata-source-conversion-manifest.json",
    ] {
        fs::write(staging.join(sidecar), b"obsolete provenance").unwrap();
    }
    fs::remove_file(data.join(layout::GENERATED_DIFFUSE_PATH)).unwrap();
    let mut resumed = converter::PipelineConfig::new(&data, &output);
    resumed.resume_staging = Some(staging);
    assert!(convert_config(resumed).await.complete);
    let clean = directory.path().join("clean");
    assert!(convert(&data, &clean).await.complete);
    let published = |root: &Path| -> converter::cache::ConversionManifest {
        serde_json::from_slice(&fs::read(root.join("conversion-manifest.json")).unwrap()).unwrap()
    };
    assert_eq!(published(&output).entries, published(&clean).entries);
    assert_eq!(
        published(&output).pruned_texture_references,
        published(&clean).pruned_texture_references
    );
    let asset_files = |root: &Path| {
        let mut files = std::collections::BTreeMap::new();
        for directory in ["meshes", "textures", "scripts"] {
            for entry in walkdir::WalkDir::new(root.join(directory)) {
                let entry = entry.unwrap();
                if entry.file_type().is_file() {
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_path_buf(),
                        converter::cache::hash_file(entry.path()).unwrap(),
                    );
                }
            }
        }
        files
    };
    assert_eq!(asset_files(&output), asset_files(&clean));
    for sidecar in [
        "metadata-rebuild.json",
        "metadata-source-conversion-manifest.json",
    ] {
        assert!(!output.join(sidecar).exists());
    }
}
