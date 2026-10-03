# LOD: DynDOLOD requirement mapping

Maps the DynDOLOD reimplementation spec (NATIVE target only) onto this repo.
Verdicts: **Reuse** (exists, extend it), **Build** (design below), **Defer**
(explicitly out of early phases). Native output uses GLB chunks and a database
index. It does not serialize Skyrim LOD asset files or write plugin records.

## Skyrim storage model

Skyrim LOD is split across sidecar settings, generated assets, and plugin data.
It is not one plugin record type.

| Data | Skyrim storage | Native-plan consequence |
|------|----------------|-------------------------|
| Worldspace LOD grid | `lodsettings/<worldspace>.lod`; xEdit reads a 16-byte settings file with origin, stride, and level range. | Use a valid sidecar origin for installed worlds; custom worlds require an explicit origin. Missing or malformed origins skip that world's LOD with an error. |
| Terrain LOD | `Meshes/Terrain/<world>/<world>.<level>.<x>.<y>.BTR` plus terrain diffuse/normal DDS textures. | Native terrain chunks are compiled from the repo's cell cache; no BTR writer is proposed. |
| Object LOD | `Meshes/Terrain/<world>/Objects/<world>.<level>.<x>.<y>.BTO` plus shared object atlas textures. | Native static chunks use GLB and retain index/material metadata in the database. |
| Standard tree LOD | `Meshes/Terrain/<world>/Trees/<world>.LST`, per-block `.BTT` files, and `Textures/Terrain/<world>/Trees/<world>TreeLOD.DDS`. | Native billboards use the proposed per-worldspace atlas; no LST/BTT writer is proposed. |
| World and placement data | Plugin records such as `WRLD`, `CELL`, and `REFR`, including relevant subrecords. | These remain inputs. `RNAM` and `TVDT` are subrecord signatures, not standalone asset files; their Skyrim-specific behavior is outside native output. |

The asset paths and settings layout are taken from xEdit's implementation,
which is a reverse-engineered format reference rather than a Bethesda
serialization specification. DynDOLOD's documentation separately describes
native large-reference metadata and cell occlusion subrecords. Those are
plugin-level compatibility concerns, not mesh payloads.

These are virtual asset paths: a game's VFS can resolve a file from a BSA
archive or a loose override. Packaging does not change whether the data is a
plugin record or an asset.

References:

- [xEdit Skyrim LOD implementation](https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.6/Core/wbLOD.pas)
- [DynDOLOD object LOD](https://dyndolod.info/Help/Object-LOD), [standard tree LOD](https://dyndolod.info/Help/Tree-LOD), [occlusion data](https://dyndolod.info/Help/Occlusion-Data), and [large references](https://dyndolod.info/Help/Large-References)
- [Wrye Bash Skyrim record definitions](https://github.com/wrye-bash/wrye-bash/blob/7469ce4daea0e3f32e0029097a0e28d5abef885f/Mopy/bash/game/skyrim/records.py)

## Architecture (ARCH)

| Req | Subject | Verdict | Repo anchor |
|-----|---------|---------|-------------|
| ARCH-01 | Canonical IDs separate from runtime IDs | Reuse | `formid_map`, `ReferenceRow` in `crates/engine/src/world/database.rs` |
| ARCH-02 | Provenance per decision | Reuse | Pipeline manifests, `AssetFailure` chains in `streaming.rs` |
| ARCH-03 | Separate block size, mesh detail, distance, variant | Build | `docs/specs/engine/lod-architecture.md` |
| ARCH-04 | Immutable build snapshot | Build | Existing staging and source hashes do not freeze inputs or pin a running reader to one output generation; see [Phase 0 evidence](lod-phase0-evidence.md). |
| ARCH-05 | Generated content outside sources | Reuse | Staging dir, `invalidate_staged_generated_outputs` in `pipeline.rs` |

## Input and classification (INPUT)

| Req | Subject | Verdict | Note |
|-----|---------|---------|------|
| INPUT-01/02 | Plugin loading, VFS precedence | Build | Reuse `asset_path` and archive overlay; resolve the authoritative `.lod` sidecar through the same load-order VFS before reading its origin. |
| INPUT-04 | Scale, rotation, enable parents | Build | Transforms exist; `XESP`/inversion unextracted (`exporter.rs` has no `XESP`) |
| INPUT-05 | Season/swap resolution | Defer | No seasonal or swap consumer exists |
| INPUT-06 | Component-level classification | Build | Per-shape material contract is the pattern to follow |
| INPUT-07 | Gameplay refs stay authoritative | Reuse | Converter never mutates source records |

## Rules and content (RULE/CONTENT)

Ordered rule engine with `explain(reference)` is Build; start minimal
(explicit record rule, then type default, then omit-with-reason). A shipped
content library is Defer; early phases reuse converted full assets.

## Textures and billboards (TEX/MAT)

| Req | Subject | Verdict | Note |
|-----|---------|---------|------|
| TEX-01..05 | Recipe-driven render, framing, pivot | Build | `docs/specs/converters/billboard-generation.md` |
| TEX-06 | Mip coverage, gutters | Build | Same doc; MASK cutoff 128 already matches (`material.rs:451`) |
| TEX-08 | No recursion into generated assets | Reuse | Staging/source separation |
| MAT-01/02 | Material batching by compatibility | Reuse | `NifShapeMaterial` contract per shape |
| MAT-04 | UV repeat fallback | Build | Direct-texture escape hatch in chunk compiler |

## Static compilation (GEOM)

Fixed 4/8/16 tiers first; per-worldspace LOD origins are new DB columns
(`worldspaces` holds only id/editor/parent/flags today, with parentage from
`WNAM`; only the LOD origins are missing, not the parent link). Subcell handoff
must be designed against single-cell streaming keys. See
`docs/specs/converters/lod-compiler.md`.

The native 4/8/16-cell chunk grid and its origin policy are separate from
Skyrim's LOD level range in the sidecar settings file. Neither commits this
backend to Bethesda's asset serialization.

## Trees and grass (TREE/GRASS)

Object-route billboards first (TREE-03/04); tilted and fallen transforms
preserved. Standard-tree path (TREE-01/02) is out of scope for NATIVE.
Grass LOD is Deferred: `GRAS` is not exported and no placement cache
reader exists.

## Runtime (RUN/GLOW)

Enable-state proxies, near/far/persistent classes, epochs on travel/load,
and the constant-vs-external emittance split are Build; see
`docs/specs/engine/lod-runtime-proxies.md`. Animation stays at the
approved-asset list (windmill/waterfall/flame pattern); no door pose,
destruction, or script-variable sync.

## World, map, occlusion (WORLD/MAP/OCC/UNDER)

Child/parent city copies, map-only level 32, precomputed occlusion, and
underside geometry are all Deferred. The engine already has runtime HZB
occlusion with a proof bridge (`render.rs`), which covers the near field;
revisit precomputed data only with measured far-field need.

## Phase 0 research

Approved policy: use valid `.lod` sidecar origins for installed worlds and
explicit origins for custom worlds; missing or malformed origins skip that
world's LOD with an error. Use source-cell visibility nodes and validated
drawable readiness, publish only while the engine is stopped for v1, and
begin static compilation with verified fixed references. Far plane, shadow
coverage, and reflection cost are measured separately. Phase 0 still owns the
exact failure/recovery mechanics, XESP extraction design, and target-hardware
budget protocol; no runtime behavior changes. See the
[Phase 0 evidence](lod-phase0-evidence.md) and
[delivery plan](../specs/engine/lod-architecture.md).

## Workflow and builds (UX/BUILD/PERF)

Reuse: versioned presets, noninteractive CLI on the same settings model,
transactional publish with last-good preserved, content-hash invalidation,
dry-run impact. New: per-tier/per-category budgets, tier captures, handoff
sequences, rebuild-equivalence checks. See Phase 5 in `lod-architecture.md`.
