# OpenSkyrim SQLite 3 Database Schema (`skyrim_world.db`)

This specification details the canonical DDL schema, tables, indices, and column constraints for `skyrim_world.db`, as implemented in [`crates/converter/src/esm/exporter.rs`](file:///C:/Users/lucas.augusto/Documents/programs/OpenSkyrim/crates/converter/src/esm/exporter.rs).

---

## 1. Schema Overview

`skyrim_world.db` is built by `crates/converter` by parsing master files (`Skyrim.esm`) and plugin files (`.esp`/`.esl`). When `PipelineConfig.plugins_file` is supplied, its explicit order is validated and preserved. The CLI and launcher currently use automatic discovery: only plugins directly in Data are selected, with dependencies ordered before dependents. Among available plugins, ESM-flagged plugins and `.esm`/`.esl` files take priority, followed by the five official files' conventional order and case-insensitive filename order. The ESL header flag alone assigns a light slot; an ESL-flagged `.esp` stays among regular plugins. Missing masters and dependency cycles fail with diagnostics. This deterministic fallback cannot infer a user's intended override order between unrelated mods; nested backup/optional plugins are ignored while nested assets remain discoverable.

The database stamps its own version in `schema_info`; the current one is **4**
(`shared::WORLD_DATABASE_SCHEMA_VERSION`), which added the `lights` table and
`references.radius_override`. The runtime (engine and `world-inspect`) accepts world database
schemas **3 through 4** (`shared::MIN_RUNTIME_WORLD_DATABASE_SCHEMA_VERSION` through
`shared::WORLD_DATABASE_SCHEMA_VERSION`, checked by `shared::supports_runtime_world_database_schema`);
schema 3 loads because every runtime query probes for the tables and columns schema 4 added. The
launcher's "ready to play" check accepts the same range.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      `skyrim_world.db` Implemented Schema                   │
│  ┌──────────────────────────┬──────────────────────┬─────────────────────┐  │
│  │   `plugins`              │   `records`          │ `worldspaces`       │  │
│  │   (Active Plugin Order)  │   (Raw FormID Data)  │ (Worldspace EDIDs)  │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `cells`                │   `references`       │ `refs_rtree`        │  │
│  │   (Cell Grid & Names)    │   (3D World Placements)│ (3D Spatial R-Tree) │  │
│  ├──────────────────────────┼──────────────────────┼─────────────────────┤  │
│  │   `land`                 │   `lod`              │ `scripts`           │  │
│  │   (Terrain Heightmaps)   │   (Mesh LOD Levels)  │ (Papyrus Bytecode)  │  │
│  ├──────────────────────────┴──────────────────────┴─────────────────────┤  │
│  │   `formid_map` & `conversion_cache`                                   │  │
│  │   (32-bit to 64-bit ID Bridge & Cache Hashes)                        │  │
│  └───────────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Table Definitions

### 1. Active Plugin Registry (`plugins`)

Stores loaded `.esm`/`.esp`/`.esl` plugin file metadata, load order priority, and checksums.

```sql
CREATE TABLE IF NOT EXISTS plugins (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    priority INTEGER NOT NULL,
    checksum BLOB NOT NULL
);
```

---

### 2. Primary Record Database (`records`)

Stores unparsed raw subrecord byte payloads indexed by 32-bit Skyrim `FormID` and 4-character record type codes.

```sql
CREATE TABLE IF NOT EXISTS records (
    id INTEGER PRIMARY KEY,
    form_id INTEGER NOT NULL,
    record_type TEXT NOT NULL,          -- 'CELL', 'REFR', 'NPC_', 'WEAP', 'ARMOR', 'SPEL', etc.
    data BLOB NOT NULL                  -- Serialized subrecords payload
);

CREATE INDEX IF NOT EXISTS idx_records_formid ON records(form_id);
CREATE INDEX IF NOT EXISTS idx_records_type ON records(record_type);
```

---

### 3. Worldspace Registry (`worldspaces`)

Stores worldspace hierarchy and parent world relations (e.g. Tamriel `0x0000003C`, Solstheim).

```sql
CREATE TABLE IF NOT EXISTS worldspaces (
    id INTEGER PRIMARY KEY,             -- WorldSpace FormID
    editor_id TEXT NOT NULL,            -- EDID string (e.g. 'Tamriel')
    parent_world INTEGER,               -- Parent WorldSpace FormID (if child worldspace)
    flags INTEGER NOT NULL
);
```

---

### 4. Cell Registry (`cells`)

Stores exterior cell grid coordinates and interior cell names.

```sql
CREATE TABLE IF NOT EXISTS cells (
    id INTEGER PRIMARY KEY,             -- CELL FormID
    worldspace_id INTEGER NOT NULL,     -- Parent WorldSpace FormID
    grid_x INTEGER,                     -- Exterior cell Grid X (NULL if interior)
    grid_y INTEGER,                     -- Exterior cell Grid Y (NULL if interior)
    interior_name TEXT,                 -- Interior cell name (NULL if exterior)
    flags INTEGER NOT NULL,
    data BLOB                           -- Optional cell binary payload
);
```

---

### 5. Placed World References (`references`)

Stores 3D positions, rotations, scales, and cell parentage for all placed world objects (`REFR`, `ACHR`, `ACRE`, `PGRE`, `PMIS`).

```sql
CREATE TABLE IF NOT EXISTS references (
    id INTEGER PRIMARY KEY,
    cell_id INTEGER NOT NULL,           -- Parent CELL FormID
    form_id INTEGER NOT NULL,           -- Base Object FormID
    pos_x REAL NOT NULL,                -- 3D X Coordinate
    pos_y REAL NOT NULL,                -- 3D Y Coordinate
    pos_z REAL NOT NULL,                -- 3D Z Coordinate
    rot_x REAL NOT NULL,                -- Rotation X (Radians)
    rot_y REAL NOT NULL,                -- Rotation Y (Radians)
    rot_z REAL NOT NULL,                -- Rotation Z (Radians)
    scale REAL NOT NULL DEFAULT 1.0,    -- Scale multiplier
    radius_override REAL,               -- XRDS radius in Creation units (NULL when the REFR has none)
    data BLOB                           -- Subrecords payload
);

-- Index for O(1) interior cell reference loading
CREATE INDEX IF NOT EXISTS idx_references_cell_id ON references(cell_id);
```

---

### 6. Hybrid Spatial Indexing (`refs_rtree` & Interior `cell_id` Index)

To prevent `float32` single-precision accuracy loss at large exterior coordinates (e.g. Tamriel bounds $\pm 200,000$) and avoid coordinate collisions between interior local origins $(0,0,0)$ and exterior global space, OpenSkyrim uses a **Two-Tier Hybrid Spatial Strategy**:

1. **Exterior Worldspace R-Tree (`refs_rtree`):** Coordinates inside the R-Tree virtual table are stored normalized relative to cell centers (values constrained between $-2048.0$ and $+2048.0$), keeping numbers small to guarantee high single-precision float accuracy.
2. **Interior Cell Direct Lookup (`idx_references_cell_id`):** Interior dungeons and houses do not use R-Trees. All interior references are loaded directly by `cell_id` for instant $O(1)$ lookup upon entering interior doors.

```sql
-- R-Tree virtual table for Exterior 3D bounding box spatial queries
CREATE VIRTUAL TABLE IF NOT EXISTS refs_rtree USING rtree(
    id,                                 -- Matches internal reference ID
    minX, maxX,                         -- Local cell X offset (-2048.0 to +2048.0)
    minY, maxY,                         -- Local cell Y offset (-2048.0 to +2048.0)
    minZ, maxZ,                         -- World Z Height units
    +cell_id,                           -- Exterior CELL FormID
    +worldspace_id                      -- Parent WorldSpace FormID (e.g. 0x0000003C for Tamriel)
);
```

---

### 7. Terrain Heightmaps (`land`)

Stores 33x33 terrain heightmap data, vertex textures (`vtex`), and vertex colors (`vclr`) extracted from `LAND` records.

```sql
CREATE TABLE IF NOT EXISTS land (
    cell_id INTEGER PRIMARY KEY,        -- Parent CELL FormID
    heightmap BLOB NOT NULL,            -- 33x33 float/byte heightmap buffer
    vtext BLOB,                         -- Land texture layers
    vclr BLOB                           -- Land vertex colors
);
```

---

### 8. Mesh Level of Detail (`lod`)

Stores terrain and mesh Level of Detail (LOD) geometry chunks.

```sql
CREATE TABLE IF NOT EXISTS lod (
    cell_id INTEGER NOT NULL,
    lod_level INTEGER NOT NULL,         -- LOD Level (4, 8, 16, 32)
    mesh_data BLOB NOT NULL,            -- Pre-cooked LOD geometry mesh
    PRIMARY KEY (cell_id, lod_level)
);
```

---

### 9. Compiled Scripts (`scripts`)

Stores compiled Papyrus script bytecode and property bindings.

```sql
CREATE TABLE IF NOT EXISTS scripts (
    form_id INTEGER PRIMARY KEY,        -- Script FormID
    script_name TEXT NOT NULL,          -- Script EDID name
    bytecode BLOB NOT NULL,             -- Papyrus PEX binary bytecode
    properties BLOB                     -- Script properties table
);
```

---

### 10. FormID Translation Map (`formid_map`)

Maps each resolved 32-bit FormID to its stable owning plugin and plugin-local
ID. `plugin_name` is the lowercase owning filename, such as `skyrim.esm`;
`internal_id` is the low 24 bits for full plugins or the low 12 bits for light
plugins. Full and light slots are assigned independently. The pair
`(plugin_name, internal_id)` survives changes to the load-order slots.
`plugin_name` is the lowercase filename; join to `plugins` with
`lower(plugins.name) = formid_map.plugin_name`.

Ownership differs from override provenance: `records.load_order` identifies
the winning plugin's priority. Game settings override by case-insensitive
EditorID and retain the first definition's identity. A deletion with no EDID
resolves through any previously encountered non-null FormID alias; an unknown
header-only deletion is skipped with a warning. Later restorations keep the
original identity. Ambiguous aliases and live settings without an EDID are
errors, rather than silently replacing or dropping another record.

Known FormID fields inside the published subrecord blobs (`records.data`,
`cells.data`, `references.data`) are rewritten into the same load-order numbering
as the record keys, so a consumer can look them up directly. That covers the
single-FormID fields in the converter's `is_form_id_subrecord`, the
LAND/GRAS/LTEX payloads, the primary VMAD script-property references, and these
reference and cell fields, validated against their expected sizes:

| Field | Records | FormIDs |
| --- | --- | --- |
| `XTEL` | placed references | door destination reference (bytes 0-3 of 32) |
| `XESP` | placed references | enable parent (bytes 0-3 of 8; flags and unused bytes preserved) |
| `XLKR` | placed references | keyword and linked reference (8 bytes), or the linked reference alone (legacy 4 bytes) |
| `XNDP` | placed references | navmesh (bytes 0-3 of 8) |
| `XEMI` | placed references | emitted light or region |
| `XAPR` | placed references | activate-parent reference (one 8-byte subrecord per parent) |
| `XLRT` | placed references | location reference types (array) |
| `XHOR` | ACHR | horse reference |
| `LTMP` | CELL, WRLD | lighting template |
| `XCIM`, `XCMO`, `XCAS` | CELL | image space, music type, acoustic space |
| `XCCM` | CELL | region the cell takes its sky and weather from |
| `XCLR` | CELL | regions (array) |

For blob remapping, placed references are REFR, ACHR, ACRE, PGRE, PMIS, PHZD,
PARW, PBAR, PBEA, PCON and PFLA. This does not expand the record types exported
to the `references` table; the six newly covered types retain their blobs in
`records` without adding `references` rows.
Fields not covered may still hold plugin-local FormIDs. In particular, do not
consume `WRLD.RNAM` large-reference lists, `XLOC` lock keys, `XPWR` water
reflections, or `XPOD`/`XLRM` room and portal links as resolved IDs. Other
unconverted fields include `XLIB`, `XMBR`, `XATR`, `XTNM`, `PDTO`, `CELL.XILL`
and `WRLD.ZNAM`. Check the converter's field-specific handling before using
these payloads; this list is not exhaustive.

The fields in this table validate master indices and light-plugin local IDs.
An out-of-range index or light-plugin local ID wider than 12 bits is an invalid
optional link: the converter sets that FormID to zero and reports a warning,
aggregated per source plugin with the count and first record/field diagnostic.
Other records and valid links continue to convert. Malformed field lengths still
fail conversion, since their FormID offsets cannot be decoded safely. Existing
validation of record headers and required fields is unchanged. Zero FormIDs
remain zero. Other bytes (such as teleport coordinates, enable flags, navmesh
triangles and activation delays) are preserved. Unlisted fields remain opaque,
not an assurance that all FormIDs in arbitrary Skyrim or mod subrecords have
been resolved.

Existing packs must be converted again to obtain these corrected links. This
does not change the schema-4 table or blob layout, so the database and asset
cache versions are unchanged. Every pipeline run, including resume, rebuilds
the database from source plugins while reusing unaffected assets. Old packs
are still accepted by the runtime: schema 4 alone does not certify these links
were remapped. A future consumer of these fields must account for old packs.
`export_to_db` without a load order refreshes already-resolved records (for
example movement annotations), preserving established `formid_map` ownership;
it neither resolves plugin-local IDs nor upgrades an old pack.

```sql
CREATE TABLE IF NOT EXISTS formid_map (
    form_id INTEGER PRIMARY KEY,       -- Resolved 32-bit Skyrim FormID
    plugin_name TEXT NOT NULL,          -- Owning plugin filename
    internal_id INTEGER NOT NULL,       -- Plugin-local ID (24 or 12 bits)
    record_type TEXT NOT NULL           -- Record type ('REFR', 'NPC_', etc.)
);
```

---

### 11. Asset Conversion Cache (`conversion_cache`)

Stores plugin file path hashes and timestamps to bypass re-converting unchanged files.

```sql
CREATE TABLE IF NOT EXISTS conversion_cache (
    plugin_path TEXT PRIMARY KEY,
    file_hash BLOB NOT NULL,
    last_converted INTEGER NOT NULL
);
```

---

### 12. Point Light Sources (`lights`)

One row per `LIGH` base record: radius, colour, flags, falloff exponent and the
optional `FNAM` fade, which is what the runtime places a point light from. The
radius is a `DATA` `u32` in Creation units widened to a float, and the row is
written whether or not the record has a `MODL`: an invisible light still lights
the space, and most `LIGH` records in `Skyrim.esm` are invisible. A record whose
`DATA` is missing or holds fewer than the 20 bytes these columns need gets no
row rather than invented values, and a `LIGH` without a `MODL` gets no `statics`
row either - there would be no mesh to draw.

A reference that places a light usually carries its own `XRDS` radius, stored in
`references.radius_override` (10,810 of the 12,148 `LIGH` references in
`Skyrim.esm`), which overrides the base record's radius for that placement.

```sql
CREATE TABLE IF NOT EXISTS lights (
    id INTEGER PRIMARY KEY,        -- LIGH FormID
    editor_id TEXT,
    radius REAL NOT NULL,          -- Creation units (DATA u32)
    color_r INTEGER NOT NULL, color_g INTEGER NOT NULL, color_b INTEGER NOT NULL,
    flags INTEGER NOT NULL,        -- DATA flags (dynamic, can carry, negative, flicker, off by default, ...)
    falloff REAL NOT NULL,
    fade REAL                      -- FNAM, if present
);
```

---

### 13. Water Definitions (`waters`)

Stores one row per `WATR` record: Skyrim's per-water colours and reflectivity, decoded from the
record's `DNAM` subrecord (offsets in `docs/research/water.md` section 1.2), plus the raw
subrecords for anything not broken out into a column.

```sql
CREATE TABLE IF NOT EXISTS waters (
    id INTEGER PRIMARY KEY,             -- WATR FormID
    editor_id TEXT,
    opacity INTEGER,                    -- ANAM, 0-100
    flags INTEGER NOT NULL,             -- record header flags
    shallow_color INTEGER,              -- DNAM+40: packed 0x00BBGGRR
    deep_color INTEGER,                 -- DNAM+44: packed 0x00BBGGRR
    reflection_color INTEGER,           -- DNAM+48: packed 0x00BBGGRR
    fresnel REAL,                       -- DNAM+24: Fresnel Amount (Schlick F0)
    reflectivity REAL,                  -- DNAM+20: Reflectivity Amount
    flow_normal_path TEXT,              -- NAM5, canonicalised (SSE flowmap waters only)
    data BLOB NOT NULL                  -- Serialized subrecords payload
);
```

`shallow_color`/`deep_color`/`reflection_color`/`fresnel`/`reflectivity` are additive columns: a
`skyrim_world.db` built before they existed has a `waters` table without them, and
`AssetCatalog::water_colors` (`crates/engine/src/world/database.rs`) returns `None` for every
water against such a database rather than failing to open it. The engine falls back to Skyrim's
DefaultWater values (`render::DEFAULT_WATER_FRESNEL` / `render::DEFAULT_WATER_REFLECTIVITY`, and
`streaming.rs`'s deep-colour constant) until a reconversion populates them. Adding them did **not**
bump `shared::WORLD_DATABASE_SCHEMA_VERSION`: nothing that already reads `waters` depends on their
presence, and the fallback exists specifically so a reconversion is not required.
