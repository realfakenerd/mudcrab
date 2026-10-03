# LOD review and Fiji delivery gate

Historical record for the old LOD worktree. The current human-testing candidate
and fresh passing smoke captures are in [lod-rc-20260930.md](lod-rc-20260930.md).
That report supersedes the delivery gates below; historical findings remain as
an audit trail, not a current release decision. Broader visual acceptance remains open.

Branch: `lod/phase1-terrain-build`, worktree `OpenSkyrim-lod`.
Reviewed base: `beec66fa7e626d1977c2ff29d196efdd8bdce26f`.
Follow-up fixes remain in this worktree. Main and Fiji's original package
are unchanged. Four Luna reviewers covered compiler, runtime, publication,
and capture/delivery evidence.

## Gate

**No-go pending refreshed native Fiji captures.** The source is now the complete
`/home/taylor/Projects/mudcrab/modern_assets` directory, explicitly confirmed
by the user. The old partial Riverwood package is superseded as an asset
source and remains untouched. Historical findings and evidence below describe
that earlier package, not the current source.

The complete source has converter schema 16, world/integration schema 4,
76,213 recorded outputs (25,388 GLBs, 35,663 KTX2s, 15,162 Luau files), and
13,084,061,483 recorded asset bytes. All output hashes matched in the source
audit, with no extra asset paths or symlinks. All 80 database-recorded plugin
name/priority/checksum tuples matched the build-host non-VR SE originals.
The integration report passes; its 28 unbounded effect/empty-model issues
remain separate from missing-asset and rendering acceptance.

Native schema 16 includes collision annotations and a configuration hash with
`texture_zstd_level=6`; it is not the same producer contract as LOD schema 16.
Metadata rebuilds preserve `retained_asset_configuration_hash`, verify the
known configuration projection, copy unchanged asset bytes into a new
disjoint output, and prevent normal conversion from reusing incompatible
producer cache entries. Ten metadata tests cover this and repeated rebuilds.

The previous native capture failed: radius-2 P95 was 17.331213 ms against
the unchanged 16.67 ms gate. Radius-0 P95 was 12.886 ms; unequal full-cell
coverage means these are handoff smoke results, not an equal-quality speedup.
Both runs had zero pending/failing work, but white terrain and visible
boundary cracks prevented visual acceptance. New work bakes LAND diffuse
layers, preserves full source boundary heights, and avoids rebuilding terrain
LOD selection on unchanged frames. Native acceptance must be rerun after
these changes; no human-testing release is approved by unit tests alone.

Current local proof: 316 compiler/shared/dummy-content tests pass, with 13
existing installed-asset/performance skips; 196 engine library tests pass;
34 Python capture/audit tests pass. Strict all-target Clippy, formatting,
shell syntax, and whitespace checks pass. Atlas tests cover linear-light
downsampling, isolated padded tile sampling at each emitted mip, and a
three-level sRGB KTX2 chain. The bake intentionally omits normal/specular
maps; slope/material parity and far-distance aliasing remain visual checks.
The empty-layer white finding was qualified against near terrain's same
neutral diffuse behavior; missing nonzero texture references remain errors.
The converter's immutable binary and builder source hashes are recorded.
Formatting the final albedo/texture files with Rust edition 2021 reproduces
their builder hashes exactly: their final changes are formatting only.
The shipped engine snapshot predates only a Clippy argument-count allowance;
the final checkout's scheduled behavior tests and strict lint checks pass.

## Historical Package Gate

**No-go for a Fiji LOD handoff; plugin and metadata gates passed, texture replay pending.** The existing package is converter schema
15/world schema 4; LOD requires 16/5. Its record blobs retain subrecords,
not record-header flags. Defaulting the new flags to zero would invent
enable-state information. A correct rebuild needs the package-matched
non-VR `.esm`/`.esp`/`.esl` files, checked against the plugin SHA-256 values
in its database, or another authoritative source of the missing flags.
The inspected local VFS contains raw asset sources but no plugin files.
On 2026-09-29, all 80 database-recorded plugin SHA-256 values matched the
non-VR originals in `/home/dev/skyrim/Skyrim Special Edition/Data`.
The ordered names, priorities, and matching checksums are recorded in
`lod-fiji-plugin-provenance-20260929.json`; preserve that order during rebuild.
Fiji's corresponding install is
`/home/taylor/.local/share/Steam/steamapps/common/Skyrim Special Edition/Data`;
its plugin bytes have not been separately verified.
Neither a version relabel nor a different game installation is acceptable.
The isolated metadata/LOD rebuild has published converter/world schemas 16/5
without converting any retained assets. Target-hardware quality gates remain
outstanding.
Bounded runtime retry is implemented and covered by focused tests. Checksum
agreement alone does not validate reused assets.

## Resolved findings

- P1: Skyrim `.lod` fields were misread as four `i32` values. The corrected
  parser reads two `i16` origins plus `i32` stride/minimum/maximum levels.
  Installed Tamriel bytes independently confirm `(-96, -96)`, `256`, `4..32`.
  The fixture writer and independent expected-byte test now agree with
  [xEdit's reader](https://github.com/TES5Edit/TES5Edit/blob/dev/wbLOD.pas#L453).
  Native tiers and coverage do not derive a fictional width/height.
- P1: settings bypassed the archive VFS. Resolution now uses canonical staged
  inputs after archive extraction and loose overrides. Tests cover packed
  settings, mixed case, loose precedence, explicit custom origins, and an
  invalid winning sidecar without fallback to lower-priority settings.
- P1: resumed builds retained obsolete chunk rows and old VFS inputs.
  End-to-end reproduction retained 18 chunk rows where a clean changed-origin
  build emitted 12; removing the sidecar archive still emitted 6 chunks.
  Resumes now regenerate the staged DB/cache/reports/LOD set and reconstruct
  the effective VFS. Tests compare resumed keys/hashes against clean output
  and verify removal of chunks, R-tree entries, payloads, and manifest.
- Capture provenance: the script now records the full base commit, actual
  dirty state, source status/diff, and their hashes. Runtime profiles receive
  the dirty flag. Software-rendered fixture smoke is labeled explicitly.
- P1: normal conversion recorded GLB checksums before dangling-texture
  pruning. Manifest and staging journal now record the final bytes. A full
  local manifest audit found 247 mismatches, all recorded pruned GLBs.
  Metadata reuse accepts these only after verifying the source NIF/dependency
  checksum, reproducing the original GLB checksum and size, replaying the
  exact recorded prune set, and reproducing the retained checksum and size.
  Other mutations remain errors; neither source assets nor their manifest
  are changed.
- P1: eight-byte XESP subrecords bypassed the four-byte FormID remapper.
  Parent IDs now resolve through the owning plugin's normal/ESL load order;
  enable-parent flags and record-header flags are preserved. The focused
  normal/light-plugin regression passes.
- P2: failed chunks previously remained cached without retry. Typed I/O
  failures now retain metadata and retry after 1/2/4 seconds, at most three
  times, subject to generation and residency budgets. Missing files, hash
  mismatches, and invalid scenes remain terminal. Synthetic-clock tests and
  a real hash-file fixture pass. Installed Bevy 0.19 source confirms that
  `AssetServer::load` restarts an observed failed asset, including a labeled
  scene; end-to-end reader-fault injection remains unproved.
- P2: capture output could be placed inside the package, and missing
  benchmark JSON did not prevent a successful capture report. The script
  now rejects canonical nested output paths before writes and requires a
  parseable, passing benchmark report for each run. Preflight also verifies
  typed source provenance, exact binary/metadata checksums, integration,
  schemas, and DB/manifest build identity. All 34 audit/capture Python tests
  pass; the script has not yet run on Fiji.
- Asset provenance: every one of Fiji's 759 GLBs differs from its inherited
  conversion manifest. Strict structural comparison also rejected 193 GLBs
  for numeric JSON differences beyond the added collision annotation. Replaying
  the unchanged historical `collision-annotate` at
  `2f58b39f3e8f418df4520dcef208eb18c47ef6d9`, with its locked serde_json 1.0.150,
  from manifest-verified NIFs and pristine GLBs reproduced all 759 installed
  files' raw SHA-256 values and sizes exactly. No float tolerance or rewritten
  expected hashes was used. The replay reports 547 collision annotations,
  202 authored-absent results, and 10 unsupported results. This proves
  reproducibility, not the original runner's identity or complete collision
  correctness. The LOD runtime retains its documented render-proxy collision
  policy and does not consume these authored extras.

## Historical Open Findings

- All 1,930 installed KTX2 files also differ from their inherited conversion
  manifest. Samples contain native BC1/BC3 blocks rather than the baseline's
  UASTC. The unchanged `pack-retexture` helper and native producer at
  `beb8a6f89da52661aae457fc5fe7e8d14189580c` are being replayed in an owned
  disposable tree. Verify each source DDS against the raw digest within the
  manifest's digest-plus-encoding cache identity; preserve that encoding
  separately. Use the exact installed schema-4 DB for semantic inference,
  not the different build-host cache DB. The installed DB SHA-256 is
  `2e824bb7f44c88527f39e5f11487ed12ac58bb5cc54cc7819e0cd7d038ff66f9`.
  Do not approve asset reuse until every output matches the installed size
  and raw SHA-256. A derived manifest must retain baseline and postprocess
  provenance and must not advertise these outputs as current-converter cache
  hits. Preserve the original package and its inherited manifest unchanged.
- Quality gate: coarse terrain emits vertex colors without LAND texture
  layers. Full/coarse edge heights, far-plane corners, real tier transitions,
  movement, teleport, and corrupt-payload recovery need matched captures.
  A flat 3x3 fixture cannot establish these or target-hardware performance.
- Publication audit remains open for same-staging concurrent resumes and
  readers outside the engine's shared asset lock. Passing swap/rollback tests
  does not establish complete reader exclusion.

## Historical Verification

Run through `devenv shell`, opt-in `RUSTC_WRAPPER=kache`, and the LOD-specific
target `/home/dev/.cache/openskyrim-lod-target`:

- `cargo test -p converter -p dummy-content -p shared`: 296 passed,
  13 existing skips for installed-asset/performance tests.
- `cargo test -p engine --lib`: 194 passed. The engine binary target contains
  zero tests; its successful test invocation is not runtime test coverage.
- Strict workspace all-target Clippy (`-D warnings`),
  `cargo fmt --all -- --check`, `git diff --check`, and shell syntax checks
  for both LOD capture scripts pass. The final engine and `world-inspect`
  debug binaries build successfully.
- Fresh capture output:
  `/home/dev/.cache/openskyrim/fiji-lod-20260929/fixture-capture-final`.
  Script completed with 6 ready chunks, 108 ready terrain patches, 32 visible
  LOD patches, zero LOD failures/pending work, and zero origin rebases. Both
  1600x900 images are nonblank and profiles correctly report dirty source.
  Visual inspection confirms texture detail disappears outside the full cell;
  this is a smoke result, not the quality/performance gate.
  The earlier capture's pixel comparison changed 60.25% of pixels at a
  greater-than-4/255 difference in any RGB channel. This measures image
  difference, not quality acceptance; the refreshed pair was separately
  inspected and is nonblank.

Final logs are under `/home/dev/.cache/openskyrim/fiji-lod-20260929/`:
`compiler-tests-final.log`, `engine-tests-final.log`, `clippy-final.log`,
`fmt-final.log`, `engine-build-final.log`, and `world-inspect-build.log`.
The real `--reuse-assets` rebuild completed in `metadata-build-final.log`:
zero converted, 80,208 reused, zero skipped, 4,673 generated chunks. Build
identity: `304259743cba052574bda475d6eed0bdfd03d688252d4ed263aef778ef47d390`.
Integration passes with 52,362 terrain/cache cells and no missing/invalid
models or missing textures. `riverwood-inspection-local.json` independently
reports 25/25 cells and 322/322 ready models at Tamriel 60, grid `(5, -12)`,
radius 2, with all 1,008 Tamriel chunk payloads hash-verified.
`metadata-index-verification.json` confirms 4,673 indexed chunks, no missing
or orphaned entries, matching composite keys, and SQLite integrity `ok`.
R-tree IDs and chunk rowids are independently allocated; verify the composite
worldspace/tier/anchor key, not rowid equality. These checks used the full
build-host assets, not the final Fiji subset or native renderer.

Exact model replay evidence is in `collision-replay-verification.json`;
the original failed structural comparison is retained in
`fiji-asset-reuse-report.json`. The texture replay records its verified
inputs in `texture-replay-inputs.json` and, after completion, its results in
`texture-replay-verification.json`. A successful replay must not overwrite
the earlier failed comparison or be described as collision/visual acceptance.

## Fiji Workflow

Use the SSH delivery method recovered from T3 thread
`0852a717-0846-49b8-8852-835080b04720`:

1. Connect to `taylor@100.85.85.120`. Reuse the complete
   `~/Projects/mudcrab/modern_assets` source; its native schemas are 16/4.
2. Prepare an isolated derived package. Reuse local assets and bundled
   libraries with a copy/reflink, not mutable hardlinks. Never reconvert into
   the original package or silently choose raw Skyrim VR data.
   A fresh space check found 212 GiB free on Fiji's `/home`, compared with
   15 GiB on root; the original package occupies about 5.2 GiB. Prefer the
   user's home filesystem for an isolated copy. Do not rely on the earlier
   nearly-full `/home` snapshot.
3. Rebuild required metadata and LOD only after verifying matching plugin
   checksums. Validate the schema, DB/manifest identity, indexed payload set,
   and asset dependencies before transfer.
4. Transfer the verified binaries and changed/generated files through ordinary
   `scp` or `rsync` over SSH. Check remote SHA-256 values against local values.
   A transport success is not a runnable-package check.
5. Add a separate LOD launcher. Preserve the bundled
   `lib/ld-linux-x86-64.so.2`, `--library-path`, `LD_LIBRARY_PATH`, Fiji's
   `/run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json`, and the
   working Wayland/XKB discovery. Keep the old launcher unchanged.
6. Capture paired views through a script invoking the engine screenshot and
   profiling flags. Save hardware/build provenance and normal target-hardware
   budgets. Do not substitute relaxed software-smoke limits for acceptance.

The old staged partial package has native failure evidence under
`~/riverwood-lod-handoff-20260930/capture-1`. It was not published for human
testing. A new disjoint full-source package must pass refreshed scripted
captures before its launcher is published.
