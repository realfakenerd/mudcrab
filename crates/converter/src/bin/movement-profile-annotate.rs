//! Add the selected movement projection to an existing schema-4 or newer compatible package.
//! Reads the package's own winning raw records; no source plugin is reparsed.

use color_eyre::{
    Result,
    eyre::{WrapErr, ensure},
};
use converter::esm::{
    exporter::{create_tables, export_to_db},
    records::RawRecord,
    types::ArchivedRecordData,
};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::PathBuf};

/// Rebuilds the movement projection of the database named on the command line.
fn main() -> Result<()> {
    color_eyre::install()?;
    let path = PathBuf::from(std::env::args_os().nth(1).ok_or_else(|| {
        color_eyre::eyre::eyre!("usage: movement-profile-annotate <skyrim_world.db>")
    })?);
    let conn = Connection::open(&path).wrap_err_with(|| format!("opening {}", path.display()))?;
    let version: u32 = conn.query_row("SELECT version FROM schema_info", [], |row| row.get(0))?;
    ensure!(
        (4..=shared::WORLD_DATABASE_SCHEMA_VERSION).contains(&version),
        "database schema {version} is unsupported"
    );
    let mut selected = HashMap::new();
    for (form_id, expected_type) in [
        (0x0001_3746_u32, "RACE"),
        (0x0003_580D_u32, "MOVT"),
        (0x0001_EC72, "GMST"),
        (0x000A_BEF6, "GMST"),
    ] {
        let (record_type, blob, load_order): (String, Vec<u8>, u32) = conn
            .query_row(
                "SELECT record_type, data, load_order FROM records WHERE form_id=?1",
                params![form_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .wrap_err_with(|| format!("missing record {form_id:08X} {expected_type}"))?;
        ensure!(
            record_type == expected_type,
            "record {form_id:08X} is {record_type}, expected {expected_type}"
        );
        let decoded: ArchivedRecordData =
            rkyv::from_bytes::<ArchivedRecordData, rkyv::rancor::Error>(&blob)
                .wrap_err_with(|| format!("decoding record {form_id:08X}"))?;
        selected.insert(
            form_id,
            RawRecord {
                form_id,
                record_type: expected_type.as_bytes().try_into().unwrap(),
                flags: 0,
                subrecords: decoded
                    .subrecords
                    .into_iter()
                    .map(|sub| (sub.tag.to_vec(), sub.data))
                    .collect(),
                cell_form_id: None,
                worldspace_form_id: None,
                load_order,
            },
        );
    }
    create_tables(&conn)?;
    export_to_db(&conn, &selected)?;
    println!(
        "movement profile projected from winning records in {}",
        path.display()
    );
    Ok(())
}
