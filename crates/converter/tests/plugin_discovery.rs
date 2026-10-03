//! Exercise automatic plugin discovery through the production conversion path.
use converter::{
    AssetPipeline, PipelineConfig,
    esm::{EsmParser, load_order::LoadOrder},
};
use dummy_content::layout;
use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn sub(tag: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    [tag.as_slice(), &(bytes.len() as u16).to_le_bytes(), bytes].concat()
}

fn record(tag: &[u8; 4], id: u32, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    [
        tag.as_slice(),
        &(payload.len() as u32).to_le_bytes(),
        &flags.to_le_bytes(),
        &id.to_le_bytes(),
        &[0; 8],
        &payload,
    ]
    .concat()
}

fn plugin(root: &Path, name: &str, masters: &[&str], flags: u32, value: Option<f32>) -> PathBuf {
    let mut header = Vec::new();
    for master in masters {
        header.extend(sub(b"MAST", format!("{master}\0").as_bytes()));
        header.extend(sub(b"DATA", &[0; 8]));
    }
    let mut bytes = record(b"TES4", 0, flags, header);
    if let Some(value) = value {
        let payload = [
            sub(b"EDID", b"fJumpHeightMin\0"),
            sub(b"DATA", &value.to_le_bytes()),
        ]
        .concat();
        bytes.extend(record(
            b"GMST",
            ((masters.len() as u32) << 24) | 0x800,
            0,
            payload,
        ));
    }
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

async fn run(config: PipelineConfig) -> Result<converter::PipelineReport, String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = AssetPipeline::run_async(config, tx)
        .await
        .map_err(|error| format!("{error:?}"));
    drain.await.unwrap();
    result
}

#[tokio::test]
async fn fallback_orders_dependencies_and_ignores_nested_plugins_but_keeps_assets() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
    plugin(&data, "ZMod.esp", &["Skyrim.esm"], 0, Some(100.0));
    plugin(
        &data,
        "APatch.esp",
        &["Skyrim.esm", "zMOD.esp"],
        0,
        Some(222.0),
    );
    // Header flags, not just the filename suffix, determine master priority.
    plugin(&data, "YMaster.esp", &["Skyrim.esm"], 1, None);
    // Extensions imply early loading even without the ESM header bit.
    plugin(&data, "WMaster.esm", &["Skyrim.esm"], 0, None);
    plugin(&data, "VLight.esl", &["Skyrim.esm"], 0, None);
    plugin(&data, "XLight.esp", &["Skyrim.esm"], 0x200, None);
    plugin(&data, "BIndependent.esp", &["Skyrim.esm"], 0, None);
    fs::create_dir_all(data.join("Optional")).unwrap();
    fs::write(data.join("Optional/ZMod.esp"), b"invalid duplicate backup").unwrap();
    fs::write(data.join("Optional/Unused.esp"), b"invalid optional plugin").unwrap();
    let output = dir.path().join("modern");
    let config = PipelineConfig::new(&data, &output);
    assert!(config.plugins_file.is_none());
    assert!(run(config.clone()).await.unwrap().complete);
    let conn = Connection::open(output.join("skyrim_world.db")).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM plugins ORDER BY priority")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        names,
        [
            "Skyrim.esm",
            "VLight.esl",
            "WMaster.esm",
            "YMaster.esp",
            "BIndependent.esp",
            "XLight.esp",
            "ZMod.esp",
            "APatch.esp"
        ]
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM movement_game_settings WHERE editor_id='fJumpHeightMin'",
            [],
            |row| row.get::<_, f64>(0)
        )
        .unwrap(),
        222.0
    );
    assert!(output.join("meshes/generated.glb").is_file());
    assert!(output.join("textures/generated_color.ktx2").is_file());
    drop(conn);
    // Identical fallback ordering on resume keeps asset reuse intact.
    let second = run(config).await.unwrap();
    assert!(second.complete);
    assert_eq!(second.converted, 0);
    assert!(second.cache_hits > 0);
}

/// Nested plugins are not conversion inputs; their advisory must not block assets.
#[tokio::test]
async fn nested_only_plugins_warn_but_asset_conversion_completes_without_database() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
    let nested = data.join("Optional");
    fs::create_dir(&nested).unwrap();
    fs::rename(data.join("Skyrim.esm"), nested.join("Skyrim.esm")).unwrap();
    fs::write(nested.join("Backup.esp"), b"invalid ignored plugin").unwrap();
    let output = dir.path().join("modern");
    let report = run(PipelineConfig::new(&data, &output)).await.unwrap();
    assert!(report.complete, "{report:?}");
    assert!(report.converted > 0);
    assert_eq!(report.skipped, 0);
    assert_eq!(
        report.notices,
        [format!(
            "found 2 plugin files, but none directly in {}; plugins in subfolders are ignored",
            data.display()
        )]
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(!output.join("skyrim_world.db").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["complete"], true);
}

/// The launcher only sees progress events, so the nested-plugins notice must arrive there too,
/// marked as a notice rather than a status update.
#[tokio::test]
async fn nested_only_plugin_notice_reaches_the_progress_channel() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
    let nested = data.join("Optional");
    fs::create_dir(&nested).unwrap();
    fs::rename(data.join("Skyrim.esm"), nested.join("Skyrim.esm")).unwrap();
    let output = dir.path().join("modern");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<converter::progress::ProgressEvent>(64);
    let collect = tokio::spawn(async move {
        let mut notices = Vec::new();
        while let Some(event) = rx.recv().await {
            if event.notice {
                notices.push(event.message);
            }
        }
        notices
    });
    let report = AssetPipeline::run_async(PipelineConfig::new(&data, &output), tx)
        .await
        .map_err(|error| format!("{error:?}"))
        .unwrap();
    let notices = collect.await.unwrap();
    assert!(report.complete, "{report:?}");
    let expected = format!(
        "note: found 1 plugin file, but none directly in {}; plugins in subfolders are ignored",
        data.display()
    );
    assert_eq!(
        notices
            .iter()
            .filter(|message| **message == expected)
            .count(),
        1,
        "{notices:?}"
    );
}

/// A deliberately asset-only input needs neither a plugin warning nor a database.
#[tokio::test]
async fn no_plugins_converts_assets_without_warning_or_database() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(
        &data,
        layout::DEFAULT_SEED,
        layout::Formats {
            esm: false,
            // The LOD settings sidecar describes the plugin's worldspace.
            lodsettings: false,
            ..layout::Formats::all()
        },
    )
    .unwrap();
    let output = dir.path().join("modern");
    let report = run(PipelineConfig::new(&data, &output)).await.unwrap();
    assert!(report.complete, "{report:?}");
    assert!(report.converted > 0);
    assert_eq!(report.skipped, 0);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(report.notices.is_empty(), "{:?}", report.notices);
    assert!(!output.join("skyrim_world.db").exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("conversion-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["complete"], true);
}

#[tokio::test]
async fn fallback_keeps_espfe_in_regular_order_without_changing_light_slots() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
    plugin(&data, "ARegular.esp", &["Skyrim.esm"], 0, Some(100.0));
    plugin(&data, "ZLight.esp", &["Skyrim.esm"], 0x200, Some(222.0));
    let output = dir.path().join("modern");
    assert!(
        run(PipelineConfig::new(&data, &output))
            .await
            .unwrap()
            .complete
    );
    let conn = Connection::open(output.join("skyrim_world.db")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT value FROM movement_game_settings WHERE editor_id='fJumpHeightMin'",
            [],
            |row| row.get::<_, f64>(0)
        )
        .unwrap(),
        222.0
    );
    let order = LoadOrder::read(&[
        data.join("Skyrim.esm"),
        data.join("ARegular.esp"),
        data.join("ZLight.esp"),
    ])
    .unwrap();
    assert_eq!(order.normal["aregular.esp"], 1);
    assert_eq!(order.light["zlight.esp"], 0);
    assert!(!order.normal.contains_key("zlight.esp"));
}

#[tokio::test]
async fn fallback_reports_missing_masters_and_cycles_with_plugin_names() {
    for cycle in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("Data");
        fs::create_dir(&data).unwrap();
        plugin(&data, "A.esp", &["B.esp"], 0, None);
        if cycle {
            plugin(&data, "B.esp", &["A.esp"], 0, None);
        }
        let error = run(PipelineConfig::new(&data, dir.path().join("modern")))
            .await
            .unwrap_err();
        assert!(
            error.contains("A.esp") && error.contains("B.esp"),
            "{error}"
        );
        assert!(
            error.contains(if cycle {
                "cyclic plugin dependencies"
            } else {
                "required master"
            }),
            "{error}"
        );
    }
}

#[tokio::test]
async fn explicit_plugin_order_is_preserved_and_not_silently_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    layout::prepare_directory(&data, false).unwrap();
    layout::generate(&data, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
    plugin(&data, "Z.esp", &["Skyrim.esm"], 0, Some(100.0));
    plugin(&data, "A.esp", &["Skyrim.esm"], 0, Some(222.0));
    let list = dir.path().join("plugins.txt");
    fs::write(&list, "Skyrim.esm\n*Z.esp\n*A.esp\n").unwrap();
    let output = dir.path().join("modern");
    let mut config = PipelineConfig::new(&data, &output);
    config.plugins_file = Some(list.clone());
    assert!(run(config.clone()).await.unwrap().complete);
    let conn = Connection::open(output.join("skyrim_world.db")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT value FROM movement_game_settings WHERE editor_id='fJumpHeightMin'",
            [],
            |row| row.get::<_, f64>(0)
        )
        .unwrap(),
        222.0
    );
    drop(conn);
    fs::write(&list, "*A.esp\nSkyrim.esm\n*Z.esp\n").unwrap();
    let error = run(config).await.unwrap_err();
    assert!(error.contains("must precede"), "{error}");
}

#[test]
fn header_and_record_errors_include_the_plugin_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Broken.esp");
    fs::write(&path, b"TES4").unwrap();
    let error = LoadOrder::read(std::slice::from_ref(&path)).err().unwrap();
    assert!(format!("{error:?}").contains(path.to_str().unwrap()));
    let malformed = record(b"STAT", 0x800, 0, b"DATA\x04\x00\x01".to_vec());
    fs::write(&path, [record(b"TES4", 0, 0, vec![]), malformed].concat()).unwrap();
    let error = EsmParser::merge_plugins(std::slice::from_ref(&path)).unwrap_err();
    let message = format!("{error:?}");
    assert!(
        message.contains(path.to_str().unwrap()) && message.contains("truncated DATA"),
        "{message}"
    );
}
