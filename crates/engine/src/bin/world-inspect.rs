use color_eyre::{Result, eyre::WrapErr};
use engine::world::{
    cache::CellCache,
    components::{InstanceBounds, WorldPosition},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

const FORMAT_VERSION: u32 = 1;
const CELL_SIZE: f32 = 4096.0;

fn main() -> Result<()> {
    color_eyre::install()?;
    let options = Options::parse(env::args_os().skip(1))?;
    let _asset_lock = shared::asset_lock::AssetLock::acquire_shared(&options.assets)
        .wrap_err_with(|| format!("cannot inspect assets at {}", options.assets.display()))?;
    let report = inspect_world(&options)?;
    let json = serde_json::to_vec_pretty(&report)?;
    if let Some(output) = &options.output {
        if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, &json)
            .wrap_err_with(|| format!("failed to write {}", output.display()))?;
        println!("world inspection written to {}", output.display());
    } else {
        println!("{}", String::from_utf8_lossy(&json));
    }
    Ok(())
}

#[derive(Debug)]
struct Options {
    assets: PathBuf,
    worldspace: u32,
    grid_x: i32,
    grid_y: i32,
    radius: i32,
    reference: Option<u32>,
    output: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut args = args.into_iter();
        let assets = PathBuf::from(args.next().ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "usage: world-inspect <assets> <worldspace> <grid-x> <grid-y> [--radius N] [--reference FORM_ID] [--output report.json]"
            )
        })?);
        let worldspace = parse_number(args.next(), "worldspace")?;
        let grid_x = parse_i32(args.next(), "grid-x")?;
        let grid_y = parse_i32(args.next(), "grid-y")?;
        let mut radius = 0;
        let mut reference = None;
        let mut output = None;
        while let Some(argument) = args.next() {
            let argument = argument
                .into_string()
                .map_err(|_| color_eyre::eyre::eyre!("argument is not valid UTF-8"))?;
            match argument.as_str() {
                "--radius" => radius = parse_i32(args.next(), "radius")?,
                "--reference" => reference = Some(parse_number(args.next(), "reference")?),
                "--output" => {
                    output = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| color_eyre::eyre::eyre!("missing output"))?,
                    ));
                }
                _ => color_eyre::eyre::bail!("unknown argument: {argument}"),
            }
        }
        color_eyre::eyre::ensure!(
            (0..=16).contains(&radius),
            "radius must be between 0 and 16"
        );
        Ok(Self {
            assets,
            worldspace,
            grid_x,
            grid_y,
            radius,
            reference,
            output,
        })
    }
}

#[derive(Debug, Serialize)]
struct InspectionReport {
    format_version: u32,
    assets_root: String,
    worldspace_id: String,
    center_grid: [i32; 2],
    radius: i32,
    reference_filter: Option<String>,
    database_schema: u32,
    converter_manifest: ContractStatus,
    integration_report: ContractStatus,
    lod_manifest: LodContractStatus,
    summary: InspectionSummary,
    cells: Vec<CellReport>,
    lod_chunks: Vec<LodChunkReport>,
    references: Vec<ReferenceReport>,
    assets: Vec<AssetReport>,
}

#[derive(Debug, Default, Serialize)]
struct InspectionSummary {
    requested_cells: usize,
    found_cells: usize,
    references: usize,
    references_without_model: usize,
    unique_models: usize,
    ready_models: usize,
    missing_models: usize,
    invalid_models: usize,
    models_with_missing_textures: usize,
    missing_texture_uris: usize,
    materialless_primitives: usize,
}

#[derive(Debug, Serialize)]
struct ContractStatus {
    path: String,
    exists: bool,
    schema_version: Option<u32>,
    passed: Option<bool>,
}

/// The LOD build-identity contract: the manifest on disk plus the identity
/// the database claims, which must match before any chunk is trusted.
#[derive(Debug, Serialize)]
struct LodContractStatus {
    path: String,
    exists: bool,
    manifest_identity: Option<String>,
    database_identity: Option<String>,
    identities_match: bool,
    chunks: usize,
}

#[derive(Debug, Serialize)]
struct LodChunkReport {
    tier: i32,
    anchor: [i32; 2],
    payload_path: String,
    payload_exists: bool,
    content_hash_matches: Option<bool>,
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
    source_cells: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CellReport {
    cell_id: String,
    grid: [i32; 2],
    reference_count: usize,
    terrain: Option<TerrainReport>,
}

#[derive(Debug, Serialize)]
struct TerrainReport {
    dimensions: [u16; 2],
    height_range: Option<[f32; 2]>,
    center_height: Option<f32>,
    layer_count: usize,
    water_height: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ReferenceReport {
    form_id: String,
    cell_id: String,
    base_form_id: String,
    editor_id: Option<String>,
    source_model: Option<String>,
    runtime_model: Option<String>,
    position_creation: [f32; 3],
    rotation_creation_radians: [f32; 3],
    scale: f32,
    translation_runtime: [f32; 3],
    rotation_runtime_quaternion: [f32; 4],
    source_bounds: Option<BoundsReport>,
    transformed_bounds: Option<BoundsReport>,
    diagnostic: String,
}

#[derive(Debug, Serialize)]
struct BoundsReport {
    min: [f32; 3],
    max: [f32; 3],
}

#[derive(Debug, Clone, Serialize)]
struct AssetReport {
    runtime_model: String,
    status: String,
    error: Option<String>,
    node_count: usize,
    mesh_count: usize,
    primitive_count: usize,
    materialless_primitives: usize,
    primitives: Vec<PrimitiveReport>,
    materials: Vec<MaterialReport>,
    images: Vec<ImageReport>,
}

#[derive(Debug, Clone, Serialize)]
struct PrimitiveReport {
    mesh_index: usize,
    mesh_name: Option<String>,
    primitive_index: usize,
    material_index: Option<usize>,
    mode: u64,
    attributes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MaterialReport {
    index: usize,
    name: Option<String>,
    alpha_mode: String,
    alpha_cutoff: Option<f64>,
    double_sided: bool,
    base_color_factor: Option<[f64; 4]>,
    emissive_factor: Option<[f64; 3]>,
    textures: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
struct ImageReport {
    index: usize,
    uri: Option<String>,
    resolved_path: Option<String>,
    exists: Option<bool>,
}

#[derive(Debug)]
struct ReferenceRow {
    form_id: u32,
    cell_id: u32,
    base_form_id: u32,
    editor_id: Option<String>,
    model_path: Option<String>,
    position: [f32; 3],
    rotation: [f32; 3],
    scale: f32,
    bounds_min: [f32; 3],
    bounds_max: [f32; 3],
    bounds_valid: bool,
}

fn inspect_world(options: &Options) -> Result<InspectionReport> {
    let assets = options
        .assets
        .canonicalize()
        .unwrap_or_else(|_| options.assets.clone());
    let database_path = assets.join("skyrim_world.db");
    let connection = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .wrap_err_with(|| format!("failed to open {}", database_path.display()))?;
    let database_schema: u32 = connection
        .query_row("SELECT version FROM schema_info LIMIT 1", [], |row| {
            row.get(0)
        })
        .wrap_err("world database has no schema version")?;
    color_eyre::eyre::ensure!(
        shared::supports_runtime_world_database_schema(database_schema),
        "world database schema {database_schema} is unsupported; reconvert assets for a schema \
         from {} through {}",
        shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION,
        shared::WORLD_DATABASE_SCHEMA_VERSION
    );
    let cache = CellCache::open(&assets.join("cell_cache.rkyv"))?;
    let lod_manifest = lod_contract_status(&assets, &connection);
    let lod_chunks = load_lod_chunks(&connection, &assets, options.worldspace)?;
    let mut cells = Vec::new();
    let mut rows = Vec::new();
    for y in -options.radius..=options.radius {
        for x in -options.radius..=options.radius {
            let grid_x = options.grid_x + x;
            let grid_y = options.grid_y + y;
            let cell_id = connection
                .query_row(
                    "SELECT id FROM cells WHERE worldspace_id=?1 AND grid_x=?2 AND grid_y=?3",
                    params![options.worldspace, grid_x, grid_y],
                    |row| row.get::<_, u32>(0),
                )
                .optional()?
                .ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "cell was not found: worldspace={:08X} grid=({grid_x},{grid_y})",
                        options.worldspace
                    )
                })?;
            let mut cell_rows = load_references(
                &connection,
                options.worldspace,
                grid_x,
                grid_y,
                options.reference,
            )?;
            let terrain = cache.terrain(cell_id).map(|terrain| {
                let min = terrain.heights.iter().copied().reduce(f32::min);
                let max = terrain.heights.iter().copied().reduce(f32::max);
                let center = usize::from(terrain.height / 2) * usize::from(terrain.width)
                    + usize::from(terrain.width / 2);
                TerrainReport {
                    dimensions: [terrain.width, terrain.height],
                    height_range: min.zip(max).map(|(min, max)| [min, max]),
                    center_height: terrain.heights.get(center).copied(),
                    layer_count: terrain.layers.len(),
                    water_height: terrain.water_height,
                }
            });
            cells.push(CellReport {
                cell_id: hex_id(cell_id),
                grid: [grid_x, grid_y],
                reference_count: cell_rows.len(),
                terrain,
            });
            rows.append(&mut cell_rows);
        }
    }
    rows.sort_by_key(|row| row.form_id);

    let mut model_paths = BTreeSet::new();
    for row in &rows {
        if let Some(path) = row.model_path.clone().and_then(converted_model_path) {
            model_paths.insert(path);
        }
    }
    let mut inspected = BTreeMap::new();
    for path in model_paths {
        inspected.insert(path.clone(), inspect_glb(&assets, &path));
    }

    let references = rows
        .iter()
        .map(|row| reference_report(row, &inspected))
        .collect::<Vec<_>>();
    let assets_report = inspected.into_values().collect::<Vec<_>>();
    let summary = InspectionSummary {
        requested_cells: ((options.radius * 2 + 1).pow(2)) as usize,
        found_cells: cells.len(),
        references: references.len(),
        references_without_model: references
            .iter()
            .filter(|reference| reference.runtime_model.is_none())
            .count(),
        unique_models: assets_report.len(),
        ready_models: assets_report
            .iter()
            .filter(|asset| asset.status == "ready")
            .count(),
        missing_models: assets_report
            .iter()
            .filter(|asset| asset.status == "missing_glb")
            .count(),
        invalid_models: assets_report
            .iter()
            .filter(|asset| asset.status == "invalid_glb")
            .count(),
        models_with_missing_textures: assets_report
            .iter()
            .filter(|asset| asset.status == "missing_textures")
            .count(),
        missing_texture_uris: assets_report
            .iter()
            .flat_map(|asset| &asset.images)
            .filter(|image| image.exists == Some(false))
            .count(),
        materialless_primitives: assets_report
            .iter()
            .map(|asset| asset.materialless_primitives)
            .sum(),
    };

    Ok(InspectionReport {
        format_version: FORMAT_VERSION,
        assets_root: assets.display().to_string(),
        worldspace_id: hex_id(options.worldspace),
        center_grid: [options.grid_x, options.grid_y],
        radius: options.radius,
        reference_filter: options.reference.map(hex_id),
        database_schema,
        converter_manifest: contract_status(&assets.join("conversion-manifest.json"), "complete"),
        integration_report: contract_status(&assets.join("integration-report.json"), "passed"),
        lod_manifest,
        summary,
        cells,
        lod_chunks,
        references,
        assets: assets_report,
    })
}

fn lod_contract_status(assets: &Path, connection: &Connection) -> LodContractStatus {
    let path = assets.join("lod-manifest.json");
    let manifest_identity = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|manifest| {
            manifest
                .get("build_identity")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let database_identity: Option<String> = connection
        .query_row(
            "SELECT build_identity FROM lod_build WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None)
        .flatten();
    let chunks: usize = connection
        .query_row("SELECT count(*) FROM lod_chunks", [], |row| row.get(0))
        .unwrap_or(0);
    let identities_match = manifest_identity.is_some() && manifest_identity == database_identity;
    LodContractStatus {
        path: path.display().to_string(),
        exists: path.is_file(),
        manifest_identity,
        database_identity,
        identities_match,
        chunks,
    }
}

fn load_lod_chunks(
    connection: &Connection,
    assets: &Path,
    worldspace: u32,
) -> Result<Vec<LodChunkReport>> {
    let has_lod_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='lod_chunks')",
        [],
        |row| row.get(0),
    )?;
    if !has_lod_table {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT tier, anchor_x, anchor_y, payload_path, content_hash,
                bounds_min_x, bounds_min_y, bounds_min_z,
                bounds_max_x, bounds_max_y, bounds_max_z, source_cells
         FROM lod_chunks WHERE worldspace_id = ?1 ORDER BY tier, anchor_x, anchor_y",
    )?;
    let rows = statement.query_map([worldspace], |row| {
        Ok((
            row.get::<_, i32>(0)?,
            row.get::<_, i32>(1)?,
            row.get::<_, i32>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            [
                row.get::<_, f32>(5)?,
                row.get::<_, f32>(6)?,
                row.get::<_, f32>(7)?,
            ],
            [
                row.get::<_, f32>(8)?,
                row.get::<_, f32>(9)?,
                row.get::<_, f32>(10)?,
            ],
            row.get::<_, String>(11)?,
        ))
    })?;
    let mut chunks = Vec::new();
    for row in rows {
        let (
            tier,
            anchor_x,
            anchor_y,
            payload_path,
            content_hash,
            bounds_min,
            bounds_max,
            source_cells,
        ) = row?;
        let payload = assets.join(&payload_path);
        let content_hash_matches = fs::read(&payload).ok().map(|bytes| {
            let digest = sha2::Sha256::digest(&bytes);
            let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            actual == content_hash
        });
        chunks.push(LodChunkReport {
            tier,
            anchor: [anchor_x, anchor_y],
            payload_path,
            payload_exists: payload.is_file(),
            content_hash_matches,
            bounds_min,
            bounds_max,
            source_cells: source_cells
                .split(';')
                .filter(|cell| !cell.is_empty())
                .map(str::to_owned)
                .collect(),
        });
    }
    Ok(chunks)
}

fn load_references(
    connection: &Connection,
    worldspace: u32,
    grid_x: i32,
    grid_y: i32,
    reference: Option<u32>,
) -> Result<Vec<ReferenceRow>> {
    let sql =
        "SELECT r.id,r.cell_id,r.base_form_id,s.editor_id,s.model_path,r.pos_x,r.pos_y,r.pos_z,
                r.rot_x,r.rot_y,r.rot_z,r.scale,
                COALESCE(s.bounds_min_x,-64),COALESCE(s.bounds_min_y,-64),COALESCE(s.bounds_min_z,-64),
                COALESCE(s.bounds_max_x,64),COALESCE(s.bounds_max_y,64),COALESCE(s.bounds_max_z,64),
                COALESCE(s.bounds_valid,0)
         FROM exterior_spatial x JOIN \"references\" r ON r.id=x.id
         LEFT JOIN statics s ON s.id=r.base_form_id
         WHERE x.worldspace_id=?1 AND x.minX>=?2 AND x.minX<?3 AND x.minY>=?4 AND x.minY<?5
           AND (?6 IS NULL OR r.id=?6)
         ORDER BY r.id";
    let min_x = grid_x as f32 * CELL_SIZE;
    let min_y = grid_y as f32 * CELL_SIZE;
    connection
        .prepare(sql)?
        .query_map(
            params![
                worldspace,
                min_x,
                min_x + CELL_SIZE,
                min_y,
                min_y + CELL_SIZE,
                reference
            ],
            |row| {
                Ok(ReferenceRow {
                    form_id: row.get(0)?,
                    cell_id: row.get(1)?,
                    base_form_id: row.get(2)?,
                    editor_id: row.get(3)?,
                    model_path: row.get(4)?,
                    position: [row.get(5)?, row.get(6)?, row.get(7)?],
                    rotation: [row.get(8)?, row.get(9)?, row.get(10)?],
                    scale: row.get(11)?,
                    bounds_min: [row.get(12)?, row.get(13)?, row.get(14)?],
                    bounds_max: [row.get(15)?, row.get(16)?, row.get(17)?],
                    bounds_valid: row.get(18)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn reference_report(row: &ReferenceRow, assets: &BTreeMap<String, AssetReport>) -> ReferenceReport {
    use bevy::prelude::{Mat4, Quat, Vec3};
    let position = Vec3::from_array(row.position);
    let world_position = WorldPosition::from_creation_units(position);
    let translation = Vec3::from_array(shared::coordinates::creation_to_runtime_vector(
        world_position.relative_to(world_position.grid).to_array(),
    ));
    let rotation = shared::coordinates::creation_euler_to_runtime_quaternion(row.rotation);
    let transform = Mat4::from_scale_rotation_translation(
        Vec3::splat(row.scale),
        Quat::from_array(rotation),
        translation,
    );
    let runtime_model = row.model_path.clone().and_then(converted_model_path);
    let diagnostic = runtime_model.as_ref().map_or("no_model", |path| {
        assets
            .get(path)
            .map_or("not_inspected", |asset| asset.status.as_str())
    });
    let source_bounds = row.bounds_valid.then_some(BoundsReport {
        min: row.bounds_min,
        max: row.bounds_max,
    });
    let transformed_bounds = row.bounds_valid.then(|| {
        let bounds = InstanceBounds::transformed(
            Vec3::from_array(row.bounds_min),
            Vec3::from_array(row.bounds_max),
            transform,
        );
        BoundsReport {
            min: bounds.min.to_array(),
            max: bounds.max.to_array(),
        }
    });
    ReferenceReport {
        form_id: hex_id(row.form_id),
        cell_id: hex_id(row.cell_id),
        base_form_id: hex_id(row.base_form_id),
        editor_id: row.editor_id.clone(),
        source_model: row.model_path.clone(),
        runtime_model,
        position_creation: row.position,
        rotation_creation_radians: row.rotation,
        scale: row.scale,
        translation_runtime: translation.to_array(),
        rotation_runtime_quaternion: rotation,
        source_bounds,
        transformed_bounds,
        diagnostic: diagnostic.to_owned(),
    }
}

fn inspect_glb(assets_root: &Path, runtime_model: &str) -> AssetReport {
    let path = assets_root.join(runtime_model);
    if !path.is_file() {
        return failed_asset(
            runtime_model,
            "missing_glb",
            format!("{} does not exist", path.display()),
        );
    }
    let result = fs::read(&path)
        .wrap_err_with(|| format!("failed to read {}", path.display()))
        .and_then(|bytes| glb_document(&bytes));
    let document = match result {
        Ok(document) => document,
        Err(error) => return failed_asset(runtime_model, "invalid_glb", format!("{error:#}")),
    };
    let images = document
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, image)| {
            let uri = image.get("uri").and_then(Value::as_str).map(str::to_owned);
            let resolved = uri
                .as_deref()
                .map(|uri| path.parent().unwrap_or(assets_root).join(uri));
            ImageReport {
                index,
                uri,
                resolved_path: resolved.as_ref().map(|path| path.display().to_string()),
                exists: resolved.as_ref().map(|path| path.is_file()),
            }
        })
        .collect::<Vec<_>>();
    let materials = document
        .get("materials")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, material)| material_report(index, material, &document))
        .collect::<Vec<_>>();
    let meshes = document.get("meshes").and_then(Value::as_array);
    let primitives = meshes
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(mesh_index, mesh)| {
            let mesh_name = mesh.get("name").and_then(Value::as_str).map(str::to_owned);
            mesh.get("primitives")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .map(move |(primitive_index, primitive)| PrimitiveReport {
                    mesh_index,
                    mesh_name: mesh_name.clone(),
                    primitive_index,
                    material_index: primitive
                        .get("material")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok()),
                    mode: primitive.get("mode").and_then(Value::as_u64).unwrap_or(4),
                    attributes: primitive
                        .get("attributes")
                        .and_then(Value::as_object)
                        .map(|attributes| attributes.keys().cloned().collect())
                        .unwrap_or_default(),
                })
        })
        .collect::<Vec<_>>();
    let primitive_count = primitives.len();
    let materialless_primitives = primitives
        .iter()
        .filter(|primitive| primitive.material_index.is_none())
        .count();
    let status = if images.iter().any(|image| image.exists == Some(false)) {
        "missing_textures"
    } else if materialless_primitives > 0 {
        "materialless_primitives"
    } else {
        "ready"
    };
    AssetReport {
        runtime_model: runtime_model.to_owned(),
        status: status.to_owned(),
        error: None,
        node_count: document
            .get("nodes")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        mesh_count: meshes.map_or(0, Vec::len),
        primitive_count,
        materialless_primitives,
        primitives,
        materials,
        images,
    }
}

fn material_report(index: usize, material: &Value, document: &Value) -> MaterialReport {
    let mut textures = BTreeMap::new();
    for (semantic, pointer) in [
        ("base_color", "/pbrMetallicRoughness/baseColorTexture/index"),
        (
            "metallic_roughness",
            "/pbrMetallicRoughness/metallicRoughnessTexture/index",
        ),
        ("normal", "/normalTexture/index"),
        ("occlusion", "/occlusionTexture/index"),
        ("emissive", "/emissiveTexture/index"),
        (
            "legacy_diffuse",
            "/extensions/KHR_materials_pbrSpecularGlossiness/diffuseTexture/index",
        ),
        (
            "legacy_specular_glossiness",
            "/extensions/KHR_materials_pbrSpecularGlossiness/specularGlossinessTexture/index",
        ),
    ] {
        if let Some(texture) = material.pointer(pointer).and_then(Value::as_u64) {
            textures.insert(semantic.to_owned(), texture_uri(document, texture as usize));
        }
    }
    MaterialReport {
        index,
        name: material
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        alpha_mode: material
            .get("alphaMode")
            .and_then(Value::as_str)
            .unwrap_or("OPAQUE")
            .to_owned(),
        alpha_cutoff: material.get("alphaCutoff").and_then(Value::as_f64),
        double_sided: material
            .get("doubleSided")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        base_color_factor: json_array(material.pointer("/pbrMetallicRoughness/baseColorFactor")),
        emissive_factor: json_array(material.get("emissiveFactor")),
        textures,
    }
}

fn texture_uri(document: &Value, texture_index: usize) -> Option<String> {
    let image_index = document
        .get("textures")?
        .get(texture_index)?
        .get("source")?
        .as_u64()? as usize;
    document
        .get("images")?
        .get(image_index)?
        .get("uri")?
        .as_str()
        .map(str::to_owned)
}

fn json_array<const N: usize>(value: Option<&Value>) -> Option<[f64; N]> {
    let values = value?.as_array()?;
    if values.len() != N {
        return None;
    }
    let mut output = [0.0; N];
    for (target, value) in output.iter_mut().zip(values) {
        *target = value.as_f64()?;
    }
    Some(output)
}

fn glb_document(bytes: &[u8]) -> Result<Value> {
    color_eyre::eyre::ensure!(
        bytes.len() >= 20 && &bytes[..4] == b"glTF",
        "invalid GLB header"
    );
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    color_eyre::eyre::ensure!(&bytes[16..20] == b"JSON", "GLB JSON chunk is missing");
    let end = 20usize
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON range overflow"))?;
    serde_json::from_slice(
        bytes
            .get(20..end)
            .ok_or_else(|| color_eyre::eyre::eyre!("truncated GLB JSON chunk"))?,
    )
    .wrap_err("invalid GLB JSON")
}

fn failed_asset(runtime_model: &str, status: &str, error: String) -> AssetReport {
    AssetReport {
        runtime_model: runtime_model.to_owned(),
        status: status.to_owned(),
        error: Some(error),
        node_count: 0,
        mesh_count: 0,
        primitive_count: 0,
        materialless_primitives: 0,
        primitives: Vec::new(),
        materials: Vec::new(),
        images: Vec::new(),
    }
}

fn contract_status(path: &Path, passed_key: &str) -> ContractStatus {
    let document = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    ContractStatus {
        path: path.display().to_string(),
        exists: path.is_file(),
        schema_version: document
            .as_ref()
            .and_then(|value| value.get("schema_version"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        passed: document
            .as_ref()
            .and_then(|value| value.get(passed_key))
            .and_then(Value::as_bool),
    }
}

fn converted_model_path(path: String) -> Option<String> {
    // Converted assets are published with lowercase canonical paths, so the
    // lookup must lowercase too (matching the engine runtime resolver).
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let without_prefix = normalized.strip_prefix("meshes/").unwrap_or(&normalized);
    if without_prefix.is_empty() {
        return None;
    }
    let mut converted = PathBuf::from("meshes").join(without_prefix);
    converted.set_extension("glb");
    Some(converted.to_string_lossy().replace('\\', "/"))
}

fn hex_id(value: u32) -> String {
    format!("{value:08X}")
}

fn parse_number(value: Option<std::ffi::OsString>, name: &str) -> Result<u32> {
    let value = value
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| color_eyre::eyre::eyre!("missing {name}"))?;
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(|| value.parse(), |value| u32::from_str_radix(value, 16))
        .wrap_err_with(|| format!("invalid {name}"))
}

fn parse_i32(value: Option<std::ffi::OsString>, name: &str) -> Result<i32> {
    value
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| color_eyre::eyre::eyre!("missing {name}"))?
        .parse()
        .wrap_err_with(|| format!("invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_database_without_lod_table_has_no_lod_chunks() {
        let connection = Connection::open_in_memory().unwrap();
        for schema in [3, 4] {
            connection
                .execute_batch(&format!(
                    "DROP TABLE IF EXISTS schema_info; CREATE TABLE schema_info(version INTEGER);
                     INSERT INTO schema_info VALUES ({schema});"
                ))
                .unwrap();
            assert!(
                load_lod_chunks(&connection, Path::new("unused"), 1)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn malformed_present_lod_table_is_not_treated_as_legacy() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE lod_chunks(worldspace_id INTEGER);")
            .unwrap();
        assert!(load_lod_chunks(&connection, Path::new("unused"), 1).is_err());
    }

    fn glb(json: Value) -> Vec<u8> {
        let mut json = serde_json::to_vec(&json).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let length = 20 + json.len();
        let mut output = Vec::new();
        output.extend_from_slice(b"glTF");
        output.extend_from_slice(&2u32.to_le_bytes());
        output.extend_from_slice(&(length as u32).to_le_bytes());
        output.extend_from_slice(&(json.len() as u32).to_le_bytes());
        output.extend_from_slice(b"JSON");
        output.extend_from_slice(&json);
        output
    }

    #[test]
    fn inspects_material_semantics_and_missing_images() {
        let directory = tempfile::tempdir().unwrap();
        let model = "meshes/test.glb";
        let path = directory.path().join(model);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            glb(serde_json::json!({
                "asset": {"version": "2.0"},
                "nodes": [{"mesh": 0}],
                "meshes": [{"primitives": [{"attributes": {}, "material": 0}]}],
                "materials": [{
                    "name": "cutout",
                    "alphaMode": "MASK",
                    "alphaCutoff": 0.4,
                    "doubleSided": true,
                    "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}
                }],
                "textures": [{"source": 0}],
                "images": [{"uri": "../textures/missing.ktx2"}]
            })),
        )
        .unwrap();
        let report = inspect_glb(directory.path(), model);
        assert_eq!(report.status, "missing_textures");
        assert_eq!(report.materials[0].alpha_mode, "MASK");
        assert!(report.materials[0].double_sided);
        assert_eq!(
            report.materials[0].textures["base_color"].as_deref(),
            Some("../textures/missing.ktx2")
        );
    }

    #[test]
    fn resolves_models_to_lowercase_canonical_paths() {
        assert_eq!(
            converted_model_path(r"Meshes\Clutter\Bones\HumanArmRight.NIF".to_owned()),
            Some("meshes/clutter/bones/humanarmright.glb".to_owned())
        );
        assert_eq!(
            converted_model_path("meshes/architecture/farmhouse/chimney01.nif".to_owned()),
            Some("meshes/architecture/farmhouse/chimney01.glb".to_owned())
        );
        assert_eq!(converted_model_path("meshes/".to_owned()), None);
    }

    #[test]
    fn parses_optional_filters() {
        let options = Options::parse(
            [
                "assets",
                "0x3c",
                "18",
                "-5",
                "--radius",
                "2",
                "--reference",
                "0x123",
                "--output",
                "report.json",
            ]
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert_eq!(options.worldspace, 0x3c);
        assert_eq!(
            (options.grid_x, options.grid_y, options.radius),
            (18, -5, 2)
        );
        assert_eq!(options.reference, Some(0x123));
        assert_eq!(options.output, Some(PathBuf::from("report.json")));
    }

    #[test]
    fn rejects_mismatched_schema_before_any_further_query() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("skyrim_world.db");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE schema_info(version INTEGER NOT NULL);
                 INSERT INTO schema_info VALUES({});",
                shared::WORLD_DATABASE_SCHEMA_VERSION + 1
            ))
            .unwrap();
        drop(connection);
        // Intentionally no `cells` table (or cell_cache.rkyv): the old code
        // queried the database before checking the schema version, so on a
        // stale database it failed with an unrelated "no such table" error
        // instead of the schema-mismatch message checked for below.

        let options = Options {
            assets: directory.path().to_owned(),
            worldspace: 0x3c,
            grid_x: 0,
            grid_y: 0,
            radius: 0,
            reference: None,
            output: None,
        };
        let error = inspect_world(&options).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("schema") && message.contains("unsupported"),
            "expected a schema-mismatch error, got: {message}"
        );
        assert!(
            !message.contains("no such table"),
            "must not run further queries before the schema check: {message}"
        );
    }
}
