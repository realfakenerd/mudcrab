# LOD Phase 0 evidence and remaining gates

Status: research evidence. The policy choices in the
[delivery plan](../specs/engine/lod-architecture.md) are approved, but the
fixture, failure/recovery details, and target-hardware budgets are pending.

## Cell-exact handoff

The current runtime gives each loaded cell a root entity and parents its
terrain and references beneath it (`crates/engine/src/streaming.rs:599-801`).
Bevy 0.19's glTF loader spawns node entities and a mesh entity per primitive;
inherited visibility follows the entity hierarchy. A GLB may therefore hold
separately hideable source-cell nodes, but a single combined mesh for an entire
4/8/16-cell chunk cannot satisfy GEOM-05 when only one full cell attaches.

Approved layout: one file per chunk, one stable node per source cell, separately
hideable terrain and object groups beneath it, and material-compatible batches
under each group. This does not prove that node identity, GLB extras, or
draw-count behavior survive the current conversion/loading path; the fixture
and GPU measurements must verify those details.

`CellStatus::Resident` is set after spawning the root, before terrain and model
dependencies finish validation (`crates/engine/src/streaming.rs:477-497`,
`1007-1212`, `1231-1364`). Handoff must use drawable coverage, not residence
alone. Even the current model readiness gate establishes scene instantiation,
loaded dependencies, and CPU-side validation; it does not prove GPU upload or
first-frame presentation. A two-cell, two-material fixture should delay and
fail each side independently, then measure overlap, holes, primitive count,
and transition latency across full-cell and nearer-tier handoffs.

Sources: [Bevy 0.19 glTF loader](https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_gltf/src/loader/mod.rs#L1615-L1674),
[Bevy visibility](https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_camera/src/visibility/mod.rs#L70-L95),
[Bevy asset load states](https://github.com/bevyengine/bevy/blob/v0.19.0/crates/bevy_asset/src/server/mod.rs#L1329-L1339).

## Build snapshot and publication

The converter stages DB and assets, hashes individual source paths, and
publishes the output directory (`crates/converter/src/pipeline.rs:309-327`,
`605-709`, `1274-1291`). Source hashing precedes conversion; there is no
post-read stability check. Staging an output is not an immutable input snapshot,
so ARCH-04 requires new work. The requirement mapping now marks it Build.

The existing publisher first renames the old directory to a backup, then
renames staging into place. Those are separate operations. The engine opens
the database by path, mmaps the cell cache, and loads GLBs later by path
(`crates/engine/src/world/database.rs:264-279`,
`crates/engine/src/world/cache.rs:35-54`,
`crates/engine/src/streaming.rs:782-798`). Replacing the asset directory while
the engine runs could mix old DB/cache handles with new GLB paths. A SQLite
transaction cannot include those files.

Approved policy: publication is offline-only in v1, with an enforced
engine-stop boundary and no live asset replacement. Define the enforcement
and rollback mechanics before implementation. One build identity must cover
ordered plugins, resolved loose and archived inputs, settings/rules, compiler
version, DB schema, and chunk hashes. Test source mutation during
capture/build, interrupted publication, and refusal to publish while an engine
is using the asset set. Immutable generation directories and a pinned runtime
reader remain an option only if live replacement becomes a requirement.

Sources: [SQLite isolation](https://www.sqlite.org/isolation.html),
[SQLite transactions](https://www.sqlite.org/lang_transaction.html),
[Linux rename semantics](https://man7.org/linux/man-pages/man2/rename.2.html).

## Static object eligibility

The `statics` table is a model catalog, not a static-LOD allowlist. It includes
movable and stateful base types such as `CONT`, `DOOR`, `ACTI`, and `MISC`
(`crates/converter/src/esm/exporter.rs:204-225`); the runtime separately keeps
the authoritative base record type (`crates/engine/src/world/database.rs:36`).
The exporter preserves reference subrecords in a blob, but its normalized
`references` row has neither header flags nor XESP parent/inversion fields
(`crates/converter/src/esm/exporter.rs:390-441`). Its current collision-proxy
allowlist is not a distant-rendering policy.

Phase 2 should default to a conservative, explicit eligibility rule: only
unconditionally enabled exterior references with verified static behavior,
valid transforms/bounds, and a suitable model can enter combined chunks.
Unknown enable state, XESP followers, initially disabled references, movable
or animated content, and unresolved large-reference overlaps need an omission
reason or a separately validated proxy route. Do not infer eligibility from a
`statics` row. Test enable-parent inversion, missing/cyclic parents, disabled
state, movement, large-reference overlap, and animation before broadening the
allowlist. DynDOLOD also distinguishes combined [object LOD](https://dyndolod.info/Help/Object-LOD)
from [dynamic LOD](https://dyndolod.info/Help/Dynamic-LOD).

## Visible range and budgets

At the default stream radius of 2, the camera far plane is 32,768 Creation
units (`crates/engine/src/app.rs:1462`). The unload ring is radius 3
(`crates/engine/src/config.rs:59-60`), and sun cascades are currently fitted
to its far corner, not to any LOD range (`crates/engine/src/app.rs:1380-1427`).
Phase 1 needs a measured camera far-plane extension to expose the terrain
ring, but extending shadow cascades to that same distance would spread the
fixed 2048-texel shadow map more thinly. Set a separate distant-LOD shadow
policy and measure near-field shadow quality and GPU cost.

`SkyrimClear` fog reaches its maximum amount of 0.85 at about 53,289 units;
it never becomes fully opaque (`crates/engine/src/sky.rs:215-256`). The claim
that work past this distance is wasted would be too strong: distant geometry
still contributes through the remaining 15 percent. Choose range from
projected error, weather/fog, and measured cost,
not this distance alone. Reuse the existing target-hardware profiling campaign
(`docs/roadmap/02-profiling.md`) with matched camera paths and scripted captures;
record main and reflection pass cost, frame P95/P99, memory, load latency,
primitive/draw counts where available, and visible seam/handoff errors.

## Work remaining before implementation

1. Specify stable source-cell node identity, group readiness, and the
   mixed-ready/failed-near-model handoff rule; review the fixture design.
2. Specify immutable input capture, offline publication enforcement, build
   identity validation, interruption recovery, and last-good rollback.
3. Define the narrow fixed-`STAT` eligibility checks and omission reasons,
   including header flags and XESP extraction.
4. Establish the scripted matched-capture protocol before implementation.
   Set numerical far-plane, shadow, reflection, and frame/memory budgets from
   target-hardware baselines before Phase 1 acceptance; mark unavailable GPU
   counters as unavailable, not zero.
