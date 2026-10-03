# Launcher LOD Follow-Up

Status: A implemented; 112 launcher tests, 5 converter LOD pipeline tests, and
strict launcher Clippy pass. Native launcher screenshot gate still pending.
B planned. Published Fiji candidate `412fdac3` remains unchanged.

## Existing Behavior

Conversion already builds terrain LOD. Launcher worker calls
`AssetPipeline::run_async_with_cancel` in
`crates/launcher/src/conversion/runner.rs:95`; pipeline calls
`compile_lod_chunks_with_cancel` before final integration/publication in
`crates/converter/src/pipeline.rs:781`.

Progress already travels through `RunMessage::Progress` and `ConversionStatus`.
Before A, `RunReport::from_pipeline` dropped `PipelineReport::lod_chunks` and
`lod_warnings`. A retains both. `LodChunks` has zero
uncalibrated progress weight; its long first world makes overall progress
unhelpful. Do not invent timing estimates to hide this.

## A: Small Launcher Change

- Keep conversion's automatic LOD build; no toggle, duplicate subprocess,
  second compiler, or schema bump.
- Preserve chunk count and LOD-specific warnings in launcher `RunReport`.
- Show `Building terrain LOD` with completed/total worldspaces and elapsed
  time. Zero completed worlds does not mean zero CPU work. Leave ETA unknown
  where measurements cannot support it; retain shared monotonic overall bar.
- On completion, show terrain chunk count and skipped-world warning count
  alongside conversion outcome. A zero-chunk result says no terrain LOD was
  generated. Conversion completeness is not proof of full-world LOD coverage.
- Keep final outcome visible even when warnings exceed five notice lines.
  Retain full warnings in `RunReport`; reuse scrolling whole-result notice
  treatment used by checks rather than truncating through `push_notice`.
- Preserve Start/Stop/Resume, check actions, locks, engine-running guards,
  and final validation/publication. World-local unsupported/invalid compiler
  content omits that world's LOD with a warning; full-detail output remains
  usable. Cancellation, changed sources, database and publication failures
  follow the ordinary failure path.

Acceptance: report projection covers nonzero/zero chunks and warnings;
status tests cover LOD stage and unknown ETA; tiny conversion produces matching
payload/DB/manifest identities; cancellation/failure cannot enable Play for
new incomplete output. Capture launcher with scripted screenshot, not manual
screen grabbing. Existing CLI progress and launcher suites remain green.

## B: Build LOD From Converted Assets

Separate follow-up after A. Needed to use `modern_assets` without reconverting
its models/textures. Do not expose current non-cancellable metadata helper as
ordinary resumable conversion.

- Action: `Build LOD`, selected converted-source folder, selected new derived
  destination, and matching Skyrim Data folder. Reject existing destinations,
  overlaps, missing manifests, plugin mismatch, busy jobs, and running engine
  conflicts before work starts; converter revalidates under asset locks.
- Extend converter-owned metadata rebuild with cooperative cancellation and
  typed failure. Check cancellation before expensive stages, between retained
  file hash/copy operations, between compiled worlds, and before publication.
  No promise of immediate interruption within
  one terrain world; no Resume until metadata journaling exists.
- Reuse existing launcher worker, messages, and state machine with explicit
  job kind. Hide unsupported Resume for this kind; failure must not invent
  resumable staging or point Delete staging at source assets.
- Verify native source producer/configuration, all retained bytes, winning
  plugin order/checksums, regenerated LOD identity and schema-17/5 integration.
  Publish new directory atomically. Never overwrite source or relabel old LOD.
- On success, select new derived output for Play. On failure/cancellation,
  preserve previous playable output and show actionable error.

Acceptance: worker/state/path tests; cancellation during retained-file copy,
at world boundary and before
publication; mismatch fails before output; source hashes unchanged; tiny
metadata-reuse fixture checks retained bytes and fresh LOD identity; native
Fiji reuse and scripted launcher capture before claiming test readiness.

Excluded: object/tree LOD, quality presets, bespoke compiler orchestration,
performance threshold changes, source-package mutation, and automatic rebuild
on every launch. Tasks T35-T38; invariants V72-V75 in root `SPEC.md`.
