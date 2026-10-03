//! Synthetic grass plugins exercise the merged database, without game assets.
use converter::esm::{
    EsmParser,
    exporter::{create_tables, export_to_db, export_to_db_with_load_order, validate_database},
    load_order::LoadOrder,
};
use rusqlite::Connection;
use std::{fs, path::Path, process::Command};

/// Encodes one subrecord: tag, little-endian u16 size, payload.
fn sub(tag: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    [tag.as_slice(), &(bytes.len() as u16).to_le_bytes(), bytes].concat()
}

/// Encodes one uncompressed record header followed by its subrecord payload.
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

/// Writes a plugin file with a TES4 header naming `masters`, followed by `records`.
fn plugin(
    root: &Path,
    name: &str,
    masters: &[&str],
    flags: u32,
    records: Vec<u8>,
) -> std::path::PathBuf {
    let mut header = Vec::new();
    for master in masters {
        header.extend(sub(b"MAST", format!("{master}\0").as_bytes()));
        header.extend(sub(b"DATA", &[0; 8]));
    }
    let path = root.join(name);
    fs::write(&path, [record(b"TES4", 0, flags, header), records].concat()).unwrap();
    path
}

/// xEdit's GRAS DATA layout: u8/u8/u8/pad, u16/pad, u32, four f32,
/// flags/pad. Distinct sentinels ensure padding never becomes an authored field.
fn grass(id: u32, density: u8) -> Vec<u8> {
    let mut data = vec![density, 11, 73, 0xA3, 0x34, 0x12, 0xB6, 0xC7, 6, 0, 0, 0];
    for value in [19.25f32, 0.375, 0.625, 2.75] {
        data.extend(value.to_le_bytes());
    }
    data.extend([5, 0xD9, 0xEA, 0xFB]);
    record(
        b"GRAS",
        id,
        0,
        [
            sub(b"EDID", b"ModdedGrass\0"),
            sub(b"MODL", b"Meshes\\Grass\\Meadow.v2.NIF\0"),
            sub(b"DATA", &data),
        ]
        .concat(),
    )
}

/// Builds an LTEX record whose repeated GNAM subrecords list `grasses` in order.
fn landscape(id: u32, grasses: &[u32]) -> Vec<u8> {
    record(
        b"LTEX",
        id,
        0,
        grasses
            .iter()
            .flat_map(|id| sub(b"GNAM", &id.to_le_bytes()))
            .collect(),
    )
}

/// Reads every texture-grass association, sorted by texture then grass.
fn associations(conn: &Connection) -> Vec<(u32, u32)> {
    conn.prepare("SELECT ltex_id, gras_id FROM landscape_texture_grasses ORDER BY ltex_id, gras_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Every GRAS field is projected, and the winning LTEX list resolves full and light-plugin IDs.
#[test]
fn exports_all_grass_fields_and_winning_full_and_light_links() {
    let dir = tempfile::tempdir().unwrap();
    let filler = plugin(dir.path(), "Filler.esm", &[], 0, Vec::new());
    let base = plugin(
        dir.path(),
        "Base.esm",
        &[],
        0,
        [grass(0x801, 22), landscape(0x901, &[0x801])].concat(),
    );
    let light = plugin(
        dir.path(),
        "Meadow.esl",
        &["Base.esm"],
        0x200,
        grass(0x01000813, 47),
    );
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &["Base.esm", "Meadow.esl"],
        0,
        [
            grass(0x801, 81),
            landscape(0x901, &[0x01000813, 0x801, 0x801, 0]),
        ]
        .concat(),
    );
    let paths = [filler, base, light, patch];
    let db = dir.path().join("world.db");
    EsmParser::convert_plugins(&paths, &db).unwrap();
    let conn = Connection::open(db).unwrap();
    assert_eq!(
        conn.query_row("SELECT version FROM schema_info", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        shared::WORLD_DATABASE_SCHEMA_VERSION
    );
    // Base.esm is slot 1 globally but master 0 in Patch.esp.
    let id = 0x01000801u32;
    conn.query_row(
        "SELECT editor_id, model_path, density, min_slope, max_slope, units_from_water, water_comparison, position_range, height_range, color_range, wave_period, flags, load_order FROM grass_types WHERE id=?1",
        [id], |row| {
            assert_eq!(row.get::<_, String>(0)?, "ModdedGrass");
            assert_eq!(row.get::<_, String>(1)?, "meshes/grass/meadow.v2.glb");
            for (column, expected) in [(2,81), (3,11), (4,73), (5,0x1234), (6,6), (11,5), (12,3)] {
                assert_eq!(row.get::<_, u32>(column)?, expected);
            }
            for (column, expected) in [(7,19.25f32), (8,0.375), (9,0.625), (10,2.75)] {
                assert_eq!(row.get::<_, f32>(column)?, expected);
            }
            Ok(())
        }).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT density FROM grass_types WHERE id=?1",
            [0xFE000813u32],
            |row| row.get::<_, u8>(0)
        )
        .unwrap(),
        47
    );
    assert_eq!(
        associations(&conn),
        [(0x01000901, id), (0x01000901, 0xFE000813)]
    );
    assert_eq!(conn.query_row(
        "SELECT plugin_name, internal_id, records.load_order FROM formid_map JOIN records USING(form_id) WHERE form_id=?1",
        [id], |row| Ok((row.get::<_,String>(0)?, row.get::<_,u32>(1)?, row.get::<_,u32>(2)?))).unwrap(),
        ("base.esm".into(),0x801,3));
}

/// Deleted grass, replaced lists and deleted textures leave no stale rows after a complete export.
#[test]
fn deleted_grass_and_replaced_or_deleted_texture_lists_do_not_survive_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(
        dir.path(),
        "Base.esm",
        &[],
        0,
        [
            grass(0x801, 22),
            grass(0x802, 33),
            landscape(0x901, &[0x801, 0x802]),
            landscape(0x902, &[0x802]),
        ]
        .concat(),
    );
    let patch = plugin(
        dir.path(),
        "Patch.esp",
        &["Base.esm"],
        0,
        [
            record(b"GRAS", 0x801, 0x20, Vec::new()),
            landscape(0x901, &[0x802]),
            record(b"LTEX", 0x902, 0x20, Vec::new()),
        ]
        .concat(),
    );
    let db = dir.path().join("world.db");
    EsmParser::convert_plugins(std::slice::from_ref(&base), &db).unwrap();
    let conn = Connection::open(db).unwrap();
    assert_eq!(associations(&conn).len(), 3);
    let effective = EsmParser::merge_plugins(&[base.clone(), patch.clone()]).unwrap();
    assert!(!effective.contains_key(&0x801));
    export_to_db_with_load_order(
        &conn,
        &effective,
        &LoadOrder::read(&[base.clone(), patch.clone()]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM grass_types WHERE id=2049",
            [],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(associations(&conn), [(0x901, 0x802)]);
    // A later override removes the remaining list altogether.
    let clear = plugin(
        dir.path(),
        "Clear.esp",
        &["Base.esm"],
        0,
        landscape(0x901, &[]),
    );
    let paths = [base, patch, clear];
    export_to_db_with_load_order(
        &conn,
        &EsmParser::merge_plugins(&paths).unwrap(),
        &LoadOrder::read(&paths).unwrap(),
    )
    .unwrap();
    assert!(associations(&conn).is_empty());
    export_to_db_with_load_order(&conn, &Default::default(), &LoadOrder::read(&[]).unwrap())
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM grass_types", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

/// Short DATA and an unsafe model path become NULL columns while the raw subrecords are kept.
#[test]
fn incomplete_rules_and_unsafe_model_paths_are_nullable_without_losing_raw_data() {
    let dir = tempfile::tempdir().unwrap();
    let payload = [
        sub(b"DATA", &[62, 7, 88]),
        sub(b"MODL", b"..\\outside.nif\0"),
    ]
    .concat();
    let base = plugin(
        dir.path(),
        "Partial.esm",
        &[],
        0,
        record(b"GRAS", 0x805, 0, payload),
    );
    let db = dir.path().join("world.db");
    EsmParser::convert_plugins(&[base], &db).unwrap();
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT model_path, density, min_slope, max_slope, units_from_water, water_comparison, position_range, height_range, color_range, wave_period, flags FROM grass_types WHERE id=2053", [], |row| {
        assert_eq!(row.get::<_, Option<String>>(0)?, None);
        assert_eq!((row.get::<_,u8>(1)?,row.get::<_,u8>(2)?,row.get::<_,u8>(3)?), (62,7,88));
        for column in 4..=10 { assert_eq!(row.get::<_,Option<f64>>(column)?,None); }
        Ok(())
    }).unwrap();
    assert_eq!(
        conn.query_row("SELECT data FROM records WHERE form_id=2053", [], |row| row
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        converter::esm::extractors::serialize_subrecords(&[
            (b"DATA".to_vec(), vec![62, 7, 88]),
            (b"MODL".to_vec(), b"..\\outside.nif\0".to_vec()),
        ])
    );
}

/// A complete export into a reused schema-4 file stamps the current schema, so
/// the database it produces passes the converter's own validation.
#[test]
fn complete_export_into_a_schema_four_database_stamps_the_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    let base = plugin(
        dir.path(),
        "Base.esm",
        &[],
        0,
        [grass(0x801, 22), landscape(0x901, &[0x801])].concat(),
    );
    let db = dir.path().join("world.db");
    {
        let conn = Connection::open(&db).unwrap();
        create_tables(&conn).unwrap();
        conn.execute("UPDATE schema_info SET version=4", [])
            .unwrap();
        assert!(validate_database(&conn).is_err());
    }
    EsmParser::convert_plugins(&[base], &db).unwrap();
    let conn = Connection::open(db).unwrap();
    let versions: Vec<u32> = conn
        .prepare("SELECT version FROM schema_info")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(versions, [shared::WORLD_DATABASE_SCHEMA_VERSION]);
    validate_database(&conn).unwrap();
    assert_eq!(associations(&conn), [(0x901, 0x801)]);
}

/// Out-of-range authored values are kept as written; NaN and infinite floats become NULL.
#[test]
fn unusual_authored_values_are_not_clamped_and_nonfinite_fields_are_nullable() {
    let dir = tempfile::tempdir().unwrap();
    let mut data = vec![0xFF; 32];
    data[12..16].copy_from_slice(&f32::NAN.to_le_bytes());
    data[16..20].copy_from_slice(&1.75f32.to_le_bytes());
    data[20..24].copy_from_slice(&f32::INFINITY.to_le_bytes());
    data[24..28].copy_from_slice(&(-2.5f32).to_le_bytes());
    let base = plugin(
        dir.path(),
        "Unusual.esm",
        &[],
        0,
        record(
            b"GRAS",
            0x807,
            0,
            [sub(b"MODL", b"grass/unusual.nif\0"), sub(b"DATA", &data)].concat(),
        ),
    );
    let db = dir.path().join("world.db");
    EsmParser::convert_plugins(&[base], &db).unwrap();
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT density, min_slope, max_slope, units_from_water, water_comparison, position_range, height_range, color_range, wave_period, flags FROM grass_types WHERE id=2055", [], |row| {
        for column in [0,1,2,9] { assert_eq!(row.get::<_,u8>(column)?, 255); }
        assert_eq!(row.get::<_,u16>(3)?, u16::MAX);
        assert_eq!(row.get::<_,u32>(4)?, u32::MAX);
        assert_eq!(row.get::<_,Option<f32>>(5)?, None);
        assert_eq!(row.get::<_,f32>(6)?, 1.75);
        assert_eq!(row.get::<_,Option<f32>>(7)?, None);
        assert_eq!(row.get::<_,f32>(8)?, -2.5);
        Ok(())
    }).unwrap();
}

/// Runs the movement annotator on a database stamped `version` and checks grass and stamp survive.
fn movement_annotation_preserves_grass(version: u32) {
    let dir = tempfile::tempdir().unwrap();
    let speeds: Vec<u8> = (1..=10)
        .flat_map(|n| (n as f32 * 17.0).to_le_bytes())
        .collect();
    let mut records = [
        grass(0x801, 22),
        landscape(0x901, &[0x801]),
        record(b"RACE", 0x13746, 0, sub(b"EDID", b"NordRace\0")),
        record(
            b"MOVT",
            0x3580D,
            0,
            [sub(b"EDID", b"NPC_Default_MT\0"), sub(b"SPED", &speeds)].concat(),
        ),
    ]
    .concat();
    for (id, name, value) in [
        (0x1EC72, "fMoveCharWalkBase", 117.0f32),
        (0xABEF6, "fJumpHeightMin", 76.0),
    ] {
        records.extend(record(
            b"GMST",
            id,
            0,
            [
                sub(b"EDID", format!("{name}\0").as_bytes()),
                sub(b"DATA", &value.to_le_bytes()),
            ]
            .concat(),
        ));
    }
    let source = plugin(dir.path(), "Movement.esm", &[], 0, records);
    let database = dir.path().join("world.db");
    EsmParser::convert_plugins(&[source], &database).unwrap();
    let conn = Connection::open(&database).unwrap();
    let before_links = associations(&conn);
    let before_grass: (u32, Vec<u8>) = conn
        .query_row(
            "SELECT density, data FROM grass_types JOIN records ON id=form_id WHERE id=2049",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    conn.execute("UPDATE schema_info SET version=?1", [version])
        .unwrap();
    // Prove that the real CLI rebuilds the missing movement projection, rather
    // than succeeding without exercising its intentionally partial export.
    conn.execute("DELETE FROM movement_types", []).unwrap();
    drop(conn);
    let output = Command::new(env!("CARGO_BIN_EXE_movement-profile-annotate"))
        .arg(&database)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = Connection::open(database).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT forward_walk FROM movement_types WHERE id=219149",
            [],
            |row| row.get::<_, f32>(0)
        )
        .unwrap(),
        85.0
    );
    assert_eq!(associations(&conn), before_links);
    assert_eq!(
        conn.query_row(
            "SELECT density, data FROM grass_types JOIN records ON id=form_id WHERE id=2049",
            [],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
        )
        .unwrap(),
        before_grass
    );
    assert_eq!(
        conn.query_row("SELECT version FROM schema_info", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        version
    );
}

/// Annotating a current-schema database keeps its grass projections.
#[test]
fn movement_annotation_does_not_erase_grass() {
    movement_annotation_preserves_grass(shared::WORLD_DATABASE_SCHEMA_VERSION);
}

/// Annotating a schema-4 database still works and keeps its schema-4 stamp.
#[test]
fn movement_annotation_still_accepts_schema_four() {
    movement_annotation_preserves_grass(4);
}

/// A subset export keeps unrelated grass and replaces only the lists of the textures it includes.
#[test]
fn partial_grass_updates_preserve_unrelated_definitions_and_replace_only_their_texture_list() {
    let dir = tempfile::tempdir().unwrap();
    let source = plugin(
        dir.path(),
        "Base.esm",
        &[],
        0,
        [
            grass(0x801, 22),
            grass(0x802, 33),
            landscape(0x901, &[0x801]),
            landscape(0x902, &[0x802]),
        ]
        .concat(),
    );
    let database = dir.path().join("world.db");
    EsmParser::convert_plugins(&[source], &database).unwrap();
    let conn = Connection::open(database).unwrap();
    let update = plugin(
        dir.path(),
        "Subset.esm",
        &[],
        0,
        [grass(0x801, 91), landscape(0x901, &[])].concat(),
    );
    export_to_db(&conn, &EsmParser::merge_plugins(&[update]).unwrap()).unwrap();
    let densities: Vec<(u32, u32)> = conn
        .prepare("SELECT id,density FROM grass_types ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(densities, [(0x801, 91), (0x802, 33)]);
    assert_eq!(associations(&conn), [(0x902, 0x802)]);
}
