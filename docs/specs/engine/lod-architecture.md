# Engine LOD architecture

Native LOD for the Bevy runtime. LOD geometry joins `WORLD_LAYER`, so it
appears in the water reflection pass by design (`render.rs`,
`REFLECTION_VIEW_LAYERS`); reflection cost is budgeted, not ignored.

## Tier model

Fixed spatial tiers past the full-detail grid: 4-, 8-, and 16-cell blocks
(`side_cells` 4/8/16, DynDOLOD GEOM-01). Block size, mesh source, distance
policy, and variant stay separate dimensions (ARCH-03). Per-worldspace LOD
origins anchor chunks; floor division rounds toward negative infinity so a
position just west of zero lands in cell -1 (GEOM-02). `worldspaces` needs
origin columns in world database schema 5.

Selection is tier-based first. Projected-size selection is a later option,
not the initial contract.

## Streaming integration

`plan_cells`/`collect_cells` key on single cells with generation-guarded
stale discard and a default of one commit per frame
(`max_cell_commits_per_frame`, tunable in `EngineConfig`). Chunks get a parallel key space
`(world, tier, anchor)` sharing the same commit budget and epoch handling,
so fast travel cannot strand half-loaded chunks. Unload uses hysteresis:
drop a tier only when the camera leaves tier range plus a margin ring, so
boundary oscillation does not thrash loads.
Each tier queries, admits, retains and unloads at its own reach plus two
cells. With the default reach of 12/25/91 cells that is 14/27/93 cells for
tiers 4/8/16. Coarse tiers retain inner coverage for fallback while finer data is
pending or unavailable.

## Distance configuration (Skyrim INI parity)

Tier reach and the full-detail grid use Skyrim's own setting names, so values a
player, mod manager or LOD generator writes for Skyrim carry over unchanged.
`--ini <path>` reads a `Skyrim.ini`/`SkyrimPrefs.ini`-format file; files apply
in the order given, later files override earlier ones key by key, and explicit
command-line options override every file wherever they appear
(`crates/engine/src/skyrim_ini.rs`).

| Section | Key | Effect |
|---|---|---|
| `[General]` | `uGridsToLoad` | full-detail grid: `stream_radius = (uGridsToLoad - 1) / 2`; odd values only |
| `[TerrainManager]` | `fBlockLevel0Distance` | tier-4 reach before the multiplier |
| `[TerrainManager]` | `fBlockLevel1Distance` | tier-8 reach before the multiplier |
| `[TerrainManager]` | `fBlockMaximumDistance` | tier-16 reach before the multiplier |
| `[TerrainManager]` | `fSplitDistanceMult` | terrain multiplier on the three block distances |

A tier's reach in cells is `floor(block distance * fSplitDistanceMult / 4096)`,
compared with the Chebyshev cell distance from the camera's cell, so each
tier covers a square of cells around the camera. The terrain multiplier follows DynDOLOD's definition: the object LOD
distances times `fSplitDistanceMult` give the terrain LOD distances. Object LOD
(Phase 2) will read the same three block distances without the multiplier.

Defaults are Skyrim Special Edition's own `SkyrimPrefs.ini` values: 35000,
70000 and 250000 with a multiplier of 1.5
([STEP](https://stepmodifications.org/wiki/SkyrimSE:SkyrimPrefs_INI/TerrainManager)),
which reach 12/25/91 cells. The initial terrain slice was measured and
accepted at a 4/8/16-cell reach (`fSplitDistanceMult=1` with block distances
16384/32768/65536). The [`cb617fc0` candidate](../../research/lod-rc-20261002.md)
was approved visually at the Skyrim defaults; performance at that reach was
deferred and has not been measured. Pass the old values through `--ini` to
reproduce the earlier runs.
The camera far plane follows the largest configured reach.

Not yet honoured: level-32 terrain (no compiled tier), `fTreeLoadDistance`
(Phase 3), `uLargeRefLODGridSize` (large references are omitted), and the
Skyrim quality presets themselves. The engine does not search for the user's
`My Games` INI files; a launcher or script passes them explicitly.

Handoff: each GLB chunk has a stable source-cell node with separately
hideable terrain and object groups, and compatible material batches beneath
each group. A group hides its LOD only when the matching full cell or nearer
tier has passed scene/dependency validation and
is ready to present; `CellStatus::Resident` alone is insufficient. Phase 2
must settle how a group with mixed-ready or failed near models transitions
without holes or duplicates. Bounds are conservative and include every
emitted component, tested across chunk and world boundaries (GEOM-05/06,
RUN-07).

Failure policy: keep metadata for transient chunk I/O errors; queue at most
three retries after 1/2/4 seconds, within shared commit budget and current
generation. Missing payloads, hash mismatches and invalid scene content remain
terminal. Existing ready coarser data remains eligible for handoff. Bevy 0.19
`AssetServer::load` restarts an observed failed asset synchronously; avoid
polling cached failure from asynchronous `reload` as a new attempt.
Synthetic-clock scheduling and file hash fixtures cover classification,
duplicate-queue prevention and retry bounds. End-to-end reader fault injection
and target-hardware recovery captures remain acceptance work.

Screenshot readiness waits for queued loads and retries and rejects current
unrecovered chunk/query failures. Successful recovery does not block a PNG.
The smoke-capture scripts still require zero cumulative LOD failures: they
certify a fault-free run, not recovery. Recovery acceptance needs a separate fault-injection
protocol that records the failure, verifies bounded retries and eventual
drawable coverage, and does not erase the cumulative diagnostics.

The shared commit cap is unchanged. Pending LOD work reserves one commit
when the cap exceeds one; at cap one, near cells and LOD alternate priority.
Cell crossings retain immutable queued chunk metadata only if it belongs to
the current worldspace and remains in range. Old query responses are still
discarded by generation. Only transient SQLite busy/locked/IO failures retry
at most three times after 1/2/4 seconds in the same generation. Invalid
metadata is terminal and remains fail-closed; persistent failures remain visible in
cumulative diagnostics and the unrecovered gauge.

## Camera, shadows, fog

Camera far is currently `CELL_SIZE * (stream_radius + 2) * 2` (`app.rs`),
32,768 units at the default radius 2. Once terrain LOD chunks arrive it
extends to cover the largest configured tier reach (`streaming/lod.rs`). Sun shadow cascades derive from the
same radius. LOD range must be fitted with the far plane and fog; distant
shadow coverage needs its own quality and cost decision. `SkyrimClear` fog
reaches its maximum amount of 0.85 at 53,289 units (`sky.rs`), so it does not
make geometry beyond that distance invisible. See the
[Phase 0 evidence](../../research/lod-phase0-evidence.md).

## Render path

- LOD meshes render with the same GPU preprocessing, indirect draw, and
  HZB path as full geometry; the existing proof bridge (`render.rs`)
  covers them without special cases.
- Billboards get a dedicated material (see `billboard-generation.md`).
- Dynamic proxies are ordinary entities with distance/state systems (see
  `lod-runtime-proxies.md`), spatially bucketed, never full-scene scanned
  per frame (RUN-05).

## Delivery plan

The initial terrain slice is implemented and delivered for scoped human
testing. The latest [Fiji handoff](../../research/lod-rc-20261002.md) records
source, asset validation and human sign-off at the Skyrim-default reach; its
performance was deliberately not measured. The
[earlier handoff](../../research/lod-rc-20260930.md) records scripted captures
and unchanged performance gates at the 4/8/16-cell reach. Terrain quality
remains coarse; performance at the new reach, moving-camera, recovery, and
long-session acceptance remain open. Launcher reporting is implemented separately from that candidate.

The table below describes full phase acceptance, not implementation status.
A green build or completed task list does not open its dependents; each gate
needs recorded evidence and review. The
[Phase 0 evidence](../../research/lod-phase0-evidence.md) is historical research
context; its pre-implementation schema/metadata inventory is superseded by
the [consolidation report](../../research/lod-consolidation-20260930.md).
Skyrim-compatible asset output remains outside this plan.

| Phase | Capability | Depends on | Acceptance gate |
|-------|------------|------------|-----------------|
| 0 | Approved policies and baseline protocol | None | Remaining handoff failure policy, input/publication mechanics, and measurement protocol resolved; no behavior change |
| 1 | Generation-safe terrain LOD slice | Phase 0 accepted | Two-cell handoff fixture, terrain ring captures, schema/build validation, and measured budgets pass |
| 2 | Eligible static object chunks | Phase 1 accepted | Classification, omission trace, rebuild equivalence, handoff, and per-tier budgets pass |
| 3 | Object-route tree billboards | Phase 2 accepted | Atlas/mip coverage and matched foliage captures pass |
| 4 | Dynamic proxies and glow | Phase 2 accepted | Enable-state, epoch, and approved animation tests pass |
| 5 | Integrated quality and performance proof | Phases 3 and 4 accepted | Repeated target-hardware campaign and signed visual review pass |

Phase 0 settles contracts that would otherwise invalidate the DB and renderer
work in Phase 1. Phase 1 proves generation identity and handoff before objects
multiply the content and state cases. Phase 3 reuses Phase 2's object selection,
provenance, and handoff path. Phase 4 handles references Phase 2 deliberately
omits, but does not depend on Phase 3's atlas. Phase 5 tests their combined cost
and appearance.

### Phase 0: finish the approved contracts

The following policy choices are approved. The initial slice implements
terrain node identity, settings resolution, build identity, asset locks, and
reference-state metadata. Object-group readiness, complete recovery proof,
and broader visual acceptance remain later gates:

- Keep ADR-0010's file-backed GLB and R-tree direction. Use a stable
  source-cell node with separately hideable terrain and object groups,
  batching compatible materials below each group. Identify nodes without
  relying on traversal order. Define terrain/object group readiness and
  failure handoff using a two-cell,
  two-material fixture; a resident root is not enough.
- Publish only while the engine is stopped for v1. Build from an immutable
  input boundary, carry one build identity across DB, manifest, and chunks,
  validate the complete staged set, and preserve the last-good output on
  failure. Live asset replacement is out of scope. Specify how the offline
  boundary is enforced and how interruption/rollback are recovered before
  changing the world DB schema.
- Use a valid `lodsettings/<worldspace>.lod` as the origin source for installed
  worlds. Custom worlds may provide an explicit per-worldspace origin. If
  neither is valid, skip that world's LOD with an actionable error; never
  silently assume zero. Use floor division for negative chunk coordinates.
- Start object compilation with verified, unconditionally enabled fixed
  references, initially a narrow `STAT` class. Extract reference header flags
  and XESP parent/inversion so initially disabled or enable-dependent records
  are omitted with reasons, not baked into static chunks.
- Determine visible range from projected error, camera far, weather, and
  measured cost. Keep distant-shadow coverage independent of camera far;
  measure main and reflection passes separately. Fog's 0.85 cap is not a
  visibility cutoff. Use scripted matched captures and target-hardware
  profiling to set numerical budgets rather than inventing them here.

Gate: document the remaining failure/recovery mechanics, stable node identity,
and measurement protocol; review the two-cell fixture design. No runtime
behavior changes in Phase 0. Phase 1 stays closed until these are accepted.

### Phase 1: prove terrain and shared streaming

Build the offline input/publication contract, versioned chunk metadata and
R-tree, validated GLB payload path, and epoch-safe streaming. Compile a coarse
terrain ring from the cell cache beyond the full-detail unload ring. Preserve
negative-grid anchoring, conservative bounds, and a declared seam/skirt policy.
Handoff must hide only the covered source cell when its full terrain or nearer
tier is drawable, then recover on failed or cancelled loads.

Gate: the two-cell fixture proves independent visibility through load, failure,
unload, teleport, and tier transitions, with no persistent hole or duplicate.
Fresh scripted terrain captures show no unacceptable seams. DB, manifest, and
GLB identity checks reject missing or mixed generations; interrupted builds
leave the last valid output usable. Record matched frame P95/P99, load latency,
memory, primitive count, main/reflection GPU cost where available, and near
shadow quality against the Phase 0 baseline. Do not treat unavailable GPU
counters as zero. Static objects and billboards remain out of scope.

Capture invariants:

- LOD-CV1: capture output is new and outside the package after path
  canonicalization. A rejected output request writes no package files.
- LOD-CV2: handoff capture validates a typed commit/dirty flag, exact binary
  and metadata checksums, passing integration, and the expected schema-5
  DB/manifest build identity before launch. Both runs require parseable,
  passing benchmark reports, ready screenshots, and zero failed/pending
  streaming work. An engine exit code alone is insufficient evidence.
- LOD-CV3: reused enriched GLBs preserve their original bytes. Each installed
  GLB must match the verified baseline after removing only the first scene's
  `openSkyrimCollision` addition and its newly empty extras container. Other
  assets match exactly. Record both raw identities and the comparison method;
  this comparison does not establish collision correctness or authored
  collision support in the runtime.
- LOD-CV4: unchanged camera cell/radius and terrain coverage/readiness leaves
  selected LOD patches and cached visibility metrics unchanged without
  rebuilding coverage maps. Camera-cell movement, full/LOD readiness changes,
  and patch/component removal recompute selection with the existing fallback
  and handoff rules. Origin rebasing alone does not change absolute coverage.

Capture bug history:

LOD-CB1|2026-09-29|new capture output could be nested inside the package|LOD-CV1
LOD-CB2|2026-09-29|engine could exit successfully without writing benchmark JSON|LOD-CV2
LOD-CB3|2026-09-29|Fiji GLB enrichment lacked a complete mutation/provenance audit|LOD-CV3
LOD-CB4|2026-09-30|every frame rebuilt terrain coverage maps and scanned all LOD patches twice; native visibility mean cost about 6 ms|LOD-CV4

### Phase 2: compile static objects conservatively

Use the authoritative base record type, reference header flags, XESP state,
model behavior, and bounds for eligibility; `statics` is a model catalog, not
an allowlist. Start with verified fixed `STAT` references. Omit unknown,
initially disabled, enable-dependent, movable, animated, and unresolved
large-reference cases with a recorded reason until their later route exists.
Preserve authored LOD shape metadata before using those NIF blocks as tier
sources. Compile compatible material batches within the Phase 0 visibility
unit, and retain provenance and a missing-reference inspector.

Vanilla object LOD (`.bto`, readable through the `BSMultiBoundNode` and
`BSSubIndexTriShape` reader support) may serve as a validation reference, not a
source. Comparisons are advisory and per worldspace: a load order that changes
a worldspace's winning references, statics or LOD models makes vanilla a stale
reference, so that worldspace's comparison is skipped and reported as such, and
a configuration option disables or overrides the reference set for modded
installs. A mismatch never fails a build whose inputs differ from vanilla.

Gate: representative state, movement, large-reference, negative-grid, and
boundary fixtures show no baked stale object or lasting overlap. Partial and
clean builds from identical inputs are semantically equal; source moves
invalidate every affected tier. Record per-tier/category geometry, texture,
draw, memory, and main/reflection pass costs. Unsupported dynamics stay
omitted; Phase 4 owns their proxy route.

### Phase 3: add tree billboards

Compile object-route tree billboards and a bounded per-worldspace atlas; test
tilt/fall transforms, alpha and mip coverage, and atlas invalidation. Keep
unsupported grass LOD and Skyrim `.LST`/`.BTT` output deferred. Gate: scripted
matched near/far foliage captures retain the accepted palette (ADR-0009),
while atlas, overdraw, memory, and frame budgets hold on target hardware.

### Phase 4: add dynamic proxies

Implement state-following individual proxies for an approved set of enable
parents, glow/window overlays, and animations. Keep scripts, collision, AI,
inventory, and unapproved movement in the full runtime. Gate: both XESP
inversion states, parent errors/cycles, load/fast-travel epochs, save/reload,
and full/proxy handoff pass without stale visuals or duplicated gameplay
effects. Phase 3 foliage is not a prerequisite for this independent route.

### Phase 5: prove the combined result

Run matched, repeated target-hardware campaigns for near-field baseline,
terrain only, static objects, trees, and dynamic effects. Retain raw captures,
camera paths, settings, hardware/build IDs, profiler bundles, and per-tier
quality/performance comparisons. Gate: declared budgets and visual tolerances
pass, handoff sequences have no persistent holes or duplicates, and a signed
review accepts fresh scripted vista captures. No visual-readiness claim rests
on CI alone.
