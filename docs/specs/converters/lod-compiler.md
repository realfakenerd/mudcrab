# LOD chunk compiler

Offline compiler turning `references` + `statics` + cell cache into
spatial LOD chunks. Uses canonical paths, an immutable input boundary,
staged outputs, a validated build identity, content-hash invalidation, and
schema-versioned manifests. The runtime holds a shared sibling lock while it
reads an asset directory. Conversion takes a nonblocking exclusive lock before
reading the prior package and holds it through publication; an active reader
rejects the build before conversion work. Symlinked output directories are
rejected. An interrupted swap restores only a destination-owned, verified
backup. The sibling publication record seals manifests and generated files;
ambiguous legacy backups require manual recovery. Cleanup validates the new
package before deleting its owned predecessor. Validation errors identify file,
block, and shape.

## Inputs

- `references` (placement, rotation, scale, `radius_override`), `statics`
  (model path, bounds), exterior R-tree, cell cache terrain.
- Per-worldspace LOD origins (new `worldspaces` columns; required by
  GEOM-02, missing today): use a valid `lodsettings/<worldspace>.lod`
  sidecar or an explicit origin for a custom world. Otherwise skip that
  world's LOD with an actionable error; never assume an origin of zero.
  Resolve sidecars from the staged, canonical VFS after archive extraction
  and loose overrides. Skyrim's 16-byte layout is two little-endian `i16`
  origins followed by `i32` stride, minimum level, and maximum level, per
  [xEdit's reader](https://github.com/TES5Edit/TES5Edit/blob/dev/wbLOD.pas#L453).
  These fields are not width/height. Native tiers and source-cell coverage
  remain independent of Skyrim's stride and level range.
- Reference header flags and enable-state columns (new; `XESP` parent +
  inversion flags), extracted at convert time so eligibility and runtime
  queries do not depend on per-query blob parsing.
- Rule set: explicit record rule, then component/type default, then
  omit-with-reason. Every decision records winner, rule, model, and
  fallback reason (ARCH-02, RULE-03).

## Selection

Per-tier source choice: a landmark may reuse its full GLB at tier 4 while
clutter drops out before tier 16 (GEOM-03). Authored `_lod` NIF variants
match by convention where present. The vendor parser reads
`BSLODTriShape.lod_sizes` and `BSSubIndexTriShape` segment data, but the
current exporter passes only each block's base `BSTriShape` to the generic
mesh path. Those per-level counts and segment ranges are lost. Preserve and
validate them before using these blocks as tier-specific sources. Missing
source means omit-with-reason, never silent near-match substitution (RULE-05).
The first static pass admits only verified, unconditionally enabled fixed
`STAT` references. `statics` is a model catalog, not an eligibility flag;
unknown, initially disabled, XESP-controlled, movable, animated, and
unresolved large-reference cases are omitted with reasons until a validated
individual proxy or handoff route exists.

## Chunk build

- Transform in double precision, then emit chunk-local coordinates;
  transform normals and winding with the shape transform (GEOM-04).
- Emit a stable node per source cell in each GLB chunk, with independently
  hideable terrain and object groups. Partition batches beneath each group by
  rendering compatibility (material family, alpha mode, overlay identity),
  not by texture filename (MAT-02).
- UVs outside 0-1 fall back to direct textures; never clamp repeating
  UVs without a preserving conversion (MAT-04).
- Strip collision and scene-graph extras from static output; animation
  and effect assets take the proxy path instead (GEOM-07).
- Destructive optimizations (dedup, hidden-face removal) are individually
  disablable; hidden-face removal uses a tolerance and conservative test
  (GEOM-08/09). Terrain may hide triangles only with the same
  conservatism; bridges and caves are never deleted on terrain say-so.
- Chunks whose references all sit in unreachable space may be pruned only
  against a reachable-viewpoint set, with a no-pruning reference mode
  for comparison (GEOM-10).

## Outputs

Chunk payloads are GLB files with the database holding a spatial index,
per ADR-0010: chunk key `(world, tier, anchor)`, bounds, batch list,
content hashes, and manifest references, plus an R-tree over world bounds
for range queries. Each chunk ships conservative bounds, a material batch
list, source-cell/group node identity for handoff (GEOM-05), and a manifest entry:
content hashes of inputs, rule set, settings, and compiler version
(BUILD-01/02). DB, manifest, and chunk records share one build identity;
publish only after every referenced payload validates. The `lod` reshape
ships with a world DB version bump.

The consolidated implementation uses converter schema 17 and world schema 5.
Main's converter schema 16 identifies collision-aware assets; retained assets
keep their producer schema/configuration during metadata-only rebuilds.

GLB vertex and accessor bounds are chunk-local, relative to the chunk root.
Database bounds and R-tree XY bounds are world-space Creation units: translate
the chunk-local X/Y extent by `(origin + anchor * tier_side_cells) * 4096`; Z
is unchanged. Never index chunk-local coordinates as world-space bounds.

## Incremental builds

Reference moves propagate to every affected tier, world copy, and variant
(BUILD-03). Atlas pixel changes avoid remeshing only when UV layout and
material contracts are unchanged (BUILD-02). Partial builds must equal
clean builds semantically or be marked unsafe (BUILD-04); a dry-run impact
report precedes expensive regeneration (BUILD-05).
Resumes reconstruct the effective VFS and regenerate the world database,
cell cache, integration report, and entire LOD generation. Journal-verified
textures and scripts remain reusable; omitted worlds/chunks and removed
settings cannot survive in staged metadata or payloads.

## Regression invariants

- LOD-V1: installed Tamriel bytes decode to origin `(-96, -96)`, stride
  `256`, levels `4..32`; fixture writer tests use independent expected bytes.
- LOD-V2: archive-only settings compile LOD; case normalization matches the
  VFS; loose settings override archives; malformed winning settings skip
  LOD without falling back to a lower-priority origin.
- LOD-V3: resumed output after an origin change equals a clean build's chunk
  keys/hashes; removing the winning sidecar removes old chunks, R-tree rows,
  payloads, and manifest. Full-detail conversion remains usable.
- LOD-V4: `--reuse-assets DIR` requires complete schema 15 through 17 source;
  retained bytes match manifest hashes; source DB plugin order/checksums
  match originals before rebuild. New disjoint output only; source unchanged.
  Retained meshes/textures/scripts reflect source package, not later Data
  asset replacements. Normal conversion required to refresh those assets.
  Rebuild schema-5 headers/enable state, cache, origins, chunks, R-tree,
  manifests and integration report; never default missing metadata or copy
  stale generated outputs. `metadata-rebuild.json` retains source schema,
  manifest/configuration hash and copied asset hashes. Fresh archive/loose
  settings resolution; no VFS/archive-cache inheritance.
- LOD-V5: pruned GLB manifest/journal hashes and sizes describe final bytes.
  Legacy pre-prune hash accepted only with matching raw NIF hash, exact
  regenerated original hash/size, exact recorded prune set, and exact
  retained post-prune hash/size. Record replay provenance; reject other edits.
  Published packs omit raw `vfs/` sources; legacy replay without raw NIFs
  fails with an actionable normal-conversion instruction before publication.
- LOD-V6: normalize XESP parent FormIDs with owning plugin/master load order,
  including ESL index; preserve inversion flags and reference header flags.
- LOD-V7: metadata-only migration preserves the original mesh cache contract
  in `retained_mesh_schema_version`, including repeated rebuilds. Normal
  conversion regenerates retained older-contract GLBs but still reuses
  compatible textures/scripts. Runtime metadata schema is independent.
  Preserve the exact source manifest beside the derived provenance report;
  rebuilding metadata does not assert a newer asset producer.
  Markerless metadata-only sources cannot become GLB cache hits and are
  rejected by metadata reuse; recover from verified original assets or an
  independently audited provenance migration, never infer a current producer.
  Preserve `retained_asset_configuration_hash` independently of rebuilt
  metadata settings. Explicit reuse accepts the native schema-16 projection
  with fixed `texture_zstd_level=6` only when every other setting matches;
  normal conversion reuses schema-16 meshes when source, bytes and actual
  configuration match; schema 12-15 mesh contracts still invalidate. Equal schema
  numbers do not establish equal producer contracts. Repeated rebuilds keep
  the original retained configuration hash and verify it again. Missing
  producer configuration in metadata-only output rejects metadata reuse and
  normal-conversion cache reuse; recover from verified original assets.
- LOD-V8: resumed conversion after source removal equals a clean build's
  asset paths, bytes, manifest entries and texture-prune decisions. Remove
  obsolete staged GLB/KTX2/Luau files and metadata-only provenance before
  rebuilding; file existence alone is not current-input provenance. Preserve
  current-input outputs matching staging journal source/schema/configuration
  and bytes; prune orphaned paths only after reconstructing effective VFS.
  Remove invalid staged outputs before reconversion; source read/conversion
  failure cannot publish stale GLB/KTX2/Luau bytes outside the new manifest.
  Failed removal aborts publication even without fail-fast.
- LOD-V9: terrain LOD carries baked diffuse albedo from winning LAND
  BTXT/ATXT/VTXT layers and current canonical DDS inputs. Match near terrain's
  shared `LAND_TEXTURE_REPEATS_PER_CELL` (24), ordered six-layer limit, bilinear opacity grids,
  normalized overlay sum, linear-light diffuse blending and linear VCLR tint.
  Bake tint once; embedded sRGB KTX2 atlas, mip chain and padded quadrant UVs
  belong to payload checksum. Missing/invalid material inputs skip world's
  LOD with explicit diagnostic, never substitute invented ground colors.
  Metadata-only rebuild resolves current archived/loose terrain DDS inputs;
  retained texture source hashes must agree before baking.
  Layerless source quadrants retain neutral diffuse with source VCLR,
  matching near terrain; they differ from unresolved referenced materials.
- LOD-V10: every LOD quadrant preserves all 17 source height samples per
  boundary, independent of tier; adjacent full/LOD and LOD/LOD source edges
  coincide when source heights coincide. Shared compiler/runtime constants
  require 65 vertices and 192 indices: 64 perimeter samples plus center.
  Nonlinear edge fixtures verify source equality and tier/cell agreement.
- LOD-V11: padded atlas sampling cannot include neighboring tiles or unused
  atlas space at any emitted mip. With two-pixel gutters and fixed GLB UVs,
  emit levels 0..2 only, from linear-light aligned box downsampling. Smaller
  globally generated mips violate tile isolation; distant aliasing remains
  a visual acceptance concern, not proof of full normal/material parity.

## Bug history

Consolidation gate: schema-17 outputs distinguish LOD from main's schema-16
collision producer; existing schema-3/4 databases remain full-detail-only and
cannot advertise LOD. Current-schema databases require LOD tables. A world
without compiled chunks does not require an invented LOD origin. Regression
tests cover these cases in `engine::world::database`.

id|date|cause|fix
LOD-B1|2026-09-29|four-i32 parser and matching fixture invented width/height|LOD-V1
LOD-B2|2026-09-29|settings lookup bypassed archive VFS and loose normalization|LOD-V2
LOD-B3|2026-09-29|resume reopened generated DB and retained removed VFS inputs|LOD-V3
LOD-B4|2026-09-29|GLB texture pruning occurred after manifest/journal hash capture|LOD-V5
LOD-B5|2026-09-29|eight-byte XESP skipped four-byte-only FormID remapper|LOD-V6
LOD-B6|2026-09-29|metadata-only migration promoted retained schema-15 GLBs into schema-16 cache hits|LOD-V7
LOD-B7|2026-09-30|removed DDS left a staged KTX2 that suppressed GLB reference pruning|LOD-V8
LOD-B8|2026-09-30|markerless metadata-only schema-16 output implied a current mesh producer|LOD-V7
LOD-B9|2026-09-30|terrain LOD treated VCLR tint as albedo and omitted LAND texture layers; native radius-0 capture showed white terrain|LOD-V9
LOD-B10|2026-09-30|resume orphan cleanup deleted journal-verified current assets before their reuse check|LOD-V8
LOD-B11|2026-09-30|native schema-16 config included Zstd settings absent in LOD producer; metadata-only relabel risked losing original asset config|LOD-V7
LOD-B12|2026-09-30|changed malformed source could leave its old staged output published outside the manifest|LOD-V8
LOD-B13|2026-09-30|coarse quadrant edges omitted intermediate full terrain heights and opened visible cracks|LOD-V10
LOD-B14|2026-09-30|globally generated atlas mips outgrew tile gutters and could sample adjacent or unused tiles|LOD-V11
