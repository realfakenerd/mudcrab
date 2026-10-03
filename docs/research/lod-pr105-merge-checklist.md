# PR #105 Merge Checklist

Review snapshot: 2026-10-01 04:24 UTC.
PR: https://github.com/Mudcrab-Team/mudcrab/pull/105
Reviewed head: `91e74013d63452bf9c4dd26cc4f359c09a76e2b0`.
Base: `a896b903472324d2613c7d530dfde1fef12351e4`.

The original inventory below records findings at the reviewed head; its source
locations are historical. Implementation and verification results appear in
the batch update at the end. A completed code fix does not imply native
acceptance or merge approval.

## Current GitHub State

- Pulled all 22 review threads, including replies; 15 resolved, 7 unresolved.
  Neither thread nor comment pagination reported another page.
- Read 11 PR conversation comments and all nonempty review bodies, including
  the latest BimingtonBill, Sonnet 5.5, and Opus 5.5 reviews.
- Format, Clippy, tests, security, performance, `tests_pass`, and CodeRabbit
  checks all pass. GitHub reports `MERGEABLE` but merge state `BLOCKED`.
- Main's rules require review-thread resolution, current passing `tests_pass`,
  linear history, and squash/rebase merging. The seven open threads are an
  observed merge requirement; passing CI does not settle the other findings.
- PRs #91 and #95 are both open. Their integration requirements are below.

## Safety And Validation Fixes

### F1. Recovery Does Not Validate Generated Artifacts [High]

Confirmed at `crates/converter/src/pipeline.rs:2491`:
`validate_publication_backup` checks converted `manifest.entries`, but not the
generated database, cell cache, LOD manifest, or chunk payloads. Those outputs
are added to `PipelineReport.artifacts`. An empty entries map passes the loop.

- Fix: establish which generated artifacts the package promises, then verify
  their presence and validity before recovery. Check supported database schema
  and integrity, readable cell cache, DB/LOD-manifest identity, and chunk hashes.
  Preserve valid legacy and intentionally plugin-free package contracts; do
  not infer completeness solely from a nonempty asset list.
- Proof: missing/corrupt database, cache, LOD manifest, and LOD payload cases
  remain untouched; valid current and supported legacy backups recover.
- Source: [BimingtonBill follow-up](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924543418).

### F2. Backup Ownership Is Not Bound To Its Destination [High]

Static risk confirmed at `pipeline.rs:2427`: matching a sibling name prefix
and a self-consistent manifest does not establish that the folder belongs to
this publication. A separately owned `assets.backup-other` can qualify and
later be deleted during replacement. No destructive reproduction was run.

- Fix: record destination and publication-transaction ownership before the
  first rename. Adopt/delete only backups belonging to that transaction and
  destination; leave ambiguous legacy candidates for manual recovery.
- Proof: an unrelated valid sibling package is neither renamed nor deleted;
  interruption at each publication boundary restores only the owned backup.
- Source: [CodeRabbit security architecture review](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5923511692).

### F3. Symlink Publication Can Change Lock Identity [High]

Confirmed at `pipeline.rs:2341` and `crates/shared/src/asset_lock.rs:92`:
the lock resolves an output alias to its target, but publication renames the
unresolved alias. The replacement can subsequently resolve to a different
lock file while the first writer still holds the original lock.

- Fix: use one resolved destination consistently for checking, locking,
  recovery, renaming, and cleanup, or reject symlinked output directories.
  Check metadata rebuilding as well as normal conversion.
- Proof: real-path and alias readers/writers cannot obtain conflicting locks
  during replacement; rejected aliases leave the link and target untouched.
- Source: [BimingtonBill follow-up](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924543418).

### F4. Shared Readers Require A Writable Lock File [Medium]

Confirmed at `asset_lock.rs:98`: both modes open the lock read/write/create.
An existing read-only package lock cannot be opened by the runtime even when
the package itself is fully readable.

- Fix: use a read-only descriptor for an existing shared lock where supported;
  retain safe initialization for writable legacy installs. Define the missing
  lock-file case in a read-only location without silently bypassing locking.
- Proof: shared reads work with a provisioned read-only lock; exclusive/shared
  exclusion still holds. Exercise permissions as an unprivileged user and
  check platform-specific locking requirements.
- Source: [Sonnet review, item 1](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924668570).

### F5. Normal Conversion Checks An Existing Output Lock Too Late [Medium]

Confirmed at `pipeline.rs:224` and `pipeline.rs:2342`: an existing output
skips recovery's lock acquisition; conversion first acquires its exclusive
output lock when publishing. It can do expensive work before discovering an
engine reader, and reads the prior package without a session-long guard.

- Fix: settle the lock lifetime at conversion entry, before reading/reusing
  the previous output. Holding the exclusive guard through publication is
  the simplest serial policy, but prevents opening that package during the
  build. A preflight-only probe improves early errors but does not protect
  later cache reads or guarantee publication if a reader starts afterwards.
  Report the directory/lock conflict accurately; advisory locks currently
  do not identify the holder's PID or name.
- Proof: an existing shared reader rejects conversion before expensive work;
  concurrent converters cannot race cache reads, recovery, or publication.
- Source: [open review thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812144).

### F6. Generic Smoke Capture Omits Query Failures [Medium]

Confirmed at `scripts/capture-lod-phase1.sh:131`: the validator checks
historical chunk failures but omits historical query failures. The screenshot
gate now permits capture after recovery, so it no longer covers this omission.

- Fix: add zero `failed_lod_queries` and zero `pending_lod_queries` checks to
  the generic smoke validator. Keep cumulative counters for fault-free smoke;
  recovery tests must remain a separate acceptance path.
- Proof: nonzero historical query failure, pending query, and missing metric
  cases fail; a clean profile passes. Cover both capture scripts.
- Source: [BimingtonBill follow-up](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924543418).

### F7. Deterministic Invalid Metadata Gets Transient Retries [Medium]

Confirmed at `crates/engine/src/world/database.rs:663` and
`crates/engine/src/streaming/lod.rs:352`: one invalid row fails the whole
query, and the database worker reduces errors to strings. The streamer retries
validation failures as if they were temporary database errors.

- Fix: preserve a typed terminal/transient failure classification at the
  database boundary. Keep invalid metadata fail-closed; do not silently accept
  partial tier coverage. Record terminal failure immediately and retain valid
  coarse/full-detail fallback. Per-row isolation is a separate design choice.
- Proof: malformed paths, hashes, bounds, and identities are not retried;
  transient failures recover within the existing retry budget; readiness stays
  false for unresolved required work and no invalid payload becomes visible.
- Source: [Sonnet review, item 2](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924668570).

## Performance And Compatibility Fixes

### F8. Schema 17 Invalidates Unchanged Schema-16 Meshes [Medium]

Confirmed at `crates/converter/src/cache.rs:227`: schema 12 through 16 GLB
entries are discarded. The schema-16 mesh-output producer is unchanged by
this PR; the mesh diff extracts helpers and refactors texture pruning.
Configuration hashes also include the package schema, so simply retaining
entries is not a sufficient end-to-end compatibility proof.

- Fix: reuse verified schema-16 mesh outputs and configuration-compatible
  retained schema-16 meshes while preserving pre-collision invalidation for
  schema 12-15. Regenerate database/cache/LOD metadata normally. Keep the
  explicitly requested snapshot-rebuild route; removing it is not required
  to fix cache compatibility.
- Proof: unchanged schema-16 inputs reuse GLBs byte-for-byte in a normal
  schema-17 conversion. Modified NIF/dependencies/options reconvert, old
  collision producers invalidate, and generated metadata is current.
- Source: [open review thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812138).

### F9. Every Tier Loads To The Largest Radius [Medium]

Confirmed at `streaming/lod.rs:262`: all tiers query to 18 cells and use the
same unload radius; selection prefers tier 4 through distance 4 and tier 8
through distance 8. This loads higher-detail chunks outside their useful band
and consumes shared cell/LOD commits. Reviewer chunk-count estimates have
not been independently benchmarked.

- Fix: use tier-specific outer radii plus unload margin consistently for
  querying, retention, queued work, retry work, and response admission. Keep
  coarser chunks overlapping inner bands for ready fallback; do not exclude
  all overlapping tiers or exceed the shared commit cap.
- Proof: stationary and moving cameras maintain coverage at positive and
  negative chunk boundaries, while ready/queued chunk counts and startup IO
  fall. Recheck near-cell latency, commit fairness, memory, and frame times.
- Source: [open review thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812141).

### F10. LOD Compile-Failure Policy Needs A Clear Boundary [Medium]

Confirmed at `pipeline.rs:2161`: material-input errors skip a world with a
warning, but geometry/atlas compilation and chunk publication errors abort
the run. The nearby comment explicitly promises fallback for invalid
settings, not for every possible compiler or filesystem failure. The review
therefore identifies inconsistent failure treatment, not proof that all
publication errors should be swallowed.

- Fix: classify world-local unsupported/invalid content versus cancellation,
  changed sources, database corruption, and filesystem/publication errors.
  Skip eligible world-local failures without leaving partial chunks or rows;
  keep consistency and publication failures fatal. Align specs and messaging.
- Proof: one unsupported world leaves other worlds/full-detail output usable
  and reports omission; partial writes cannot survive as a ready world;
  cancellation, changed inputs, and IO/DB faults never publish a complete pack.
- Source: [open review thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812150).

### F11. Atlas Encoding And glTF Contract Disagree [Medium]

Confirmed at `crates/converter/src/lod/albedo.rs:414`,
`crates/converter/src/texture.rs:156` and `texture.rs:1347`: the encoder emits
UASTC Basis KTX2. ADR 0010 incorrectly says these are not Basis textures.
`lod/terrain.rs:426` embeds KTX2 using an ordinary texture source.

- Fix: correct the ADR and emit/test the actual `KHR_texture_basisu` texture
  source declaration and applicable extension-use/requirement lists. Merely
  adding a name to `extensionsUsed` does not define the extension's source.
  Verify Bevy's loader before changing emitted GLBs.
- Proof: inspect KTX2 encoding, validate the generated glTF extension structure,
  and load/render it with the project's native loader without a texture loss.
- Source: [BimingtonBill follow-up](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924543418).

### F12. Launcher Notice Lifecycle Loses Warnings And Grows Unbounded [Medium]

Confirmed at `crates/launcher/src/conversion/status.rs:170` and `:207`:
`finish_run` replaces progress-channel notices with the report; subsequent
notices bypass trimming. Some pruning warnings are intentionally absent from
`report.warnings`, so replacing the pane removes their visible record.

- Fix: distinguish completed report lines from bounded progress/post-run
  notices. Preserve the report and relevant conversion warnings, bound later
  notices, and reset both at the next run without duplicating summary warnings.
- Proof: a pruning notice survives completion; repeated readiness checks stay
  bounded and preserve all report lines; a new run clears old state.
- Sources: [open thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812163),
  [Sonnet review, item 4](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924668570).

## Documentation And Integration Closure

### F13. Correct The Documented Chunk Filename [Low]

`crates/shared/src/lod.rs:205` omits the actual `cell_` prefix in its example.
Correct the comment; the function and existing filename test already agree.
Source: [open thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812152).

### F14. Document The Added Reference Columns [Low]

`crates/converter/src/esm/exporter.rs:118` adds `header_flags`,
`enable_parent_id`, and `enable_parent_flags`, but the DB schema document
omits them. Document types, nullability/defaults, raw-record provenance, and
that current terrain LOD does not consume them. Preserve tested metadata
unless deliberately moving it to #106; unused by this phase is not a bug.
Source: [open thread](https://github.com/Mudcrab-Team/mudcrab/pull/105#discussion_r4151812155).

### F15. Coordinate Texture Tiling With PR #91 [Integration]

PR #105 and its current base both use 8 repeats per cell. PR #91 is open at
`1b422f93cb361f7b6538899f75164cacfd806ba2`; its diff changes near terrain to
24 through `LAND_TEXTURE_REPEATS_PER_CELL`. The earlier claim that no 24
exists applies only to #105's current path, not the proposed combined result.

- Fix: establish one shared tiling contract for runtime and offline baking,
  with the approved #91 value. Coordinate merge order and atlas invalidation;
  an engine-only constant change leaves already baked chunks at the old scale.
- Proof: near and baked terrain sample the same spatial frequency; changed
  tiling rebuilds atlases/build identity; matched native handoff captures pass.
- Source: [BimingtonBill follow-up](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5924543418)
  and [PR #91](https://github.com/Mudcrab-Team/mudcrab/pull/91).

### F16. Coordinate Legacy Launcher Readiness With PR #95 [Integration]

Confirmed at `crates/launcher/src/conversion/mod.rs:382`: readiness requires
exact current converter/world schemas. PR #95 is open at
`3c7533dd67e44d57a77cc8e10f49f82ceafd14e9` and owns accepted-range behavior.

- Fix: agree merge order and update #95's range tests for converter 17/world 5.
  Do not duplicate its schema-range implementation in #105. If #105 lands
  first, explicitly accept or prevent the intervening legacy-readiness regression.
- Proof: supported legacy and current packs agree between launcher, runtime,
  and inspection; future/unsupported/incomplete packs still fail readiness.
- Source: [initial collaborator review](https://github.com/Mudcrab-Team/mudcrab/pull/105#issuecomment-5922007532)
  and [PR #95](https://github.com/Mudcrab-Team/mudcrab/pull/95).

## Additional Fix Candidate

### F17. Parse And Merge Plugins Once [Nonblocking Performance Cleanup]

Confirmed at `crates/converter/src/esm/mod.rs:36`, `pipeline.rs:613-615`,
and `crates/converter/src/metadata.rs:305-307`: `convert_plugins` merges
internally, then callers merge again for cell-cache generation. The normal
conversion duplication predates #105; metadata rebuilding repeats it.

- Fix: let database export and cell-cache generation share the same merged
  records, preserving checksum verification, load order, and deletion rules.
- Proof: identical DB/cache contents for override/deletion/light-plugin
  fixtures; only one full parse/merge per conversion route. Measure time and
  peak memory before claiming a performance improvement.
- Source: local trace from this thread; not a GitHub review blocker.

## Already Settled Or Not Required For This Fix

- Original CodeRabbit inline findings are resolved, including the snapshot
  reuse contract and zero-cumulative-failure smoke contract. The bot explicitly
  withdrew those two objections. Do not reverse either contract to satisfy an
  older summary; request correction of its stale merge-risk summary instead.
- Legacy `world-inspect`, alias lock acquisition, bounded retries, pending
  accounting, moving-camera queued-work retention, commit fairness, report-path
  handling, unsupported `--log-file`, cancellation messaging, archive filtering,
  and superseded Fiji evidence received fixes in `91e74013`. F1-F3 and F6 are
  remaining gaps around those fixes, not requests to redo them wholesale.
- The outside-diff `lodsettings` help finding is already fixed in
  `crates/dummy-content/src/main.rs:188`.
- CodeRabbit's docstring-coverage warning is advisory; the repository's required
  check is `tests_pass`. Document changed public contracts, not every private
  helper solely to chase the bot's percentage.
- Object/tree LOD, dynamic proxies, cooperative metadata-rebuild cancellation,
  and splitting the PR are not required behavior changes for this terrain fix.

## Fix Order And Merge Evidence

1. Fix recovery completeness/ownership and destination/lock identity together
   (F1-F5), with fault-injection and concurrency/permission tests.
2. Fix mesh migration and bounded tier residency (F8-F9); share parsed records
   in the same batch only if F17 remains contained and verifiable.
3. Fix error classification, atlas contract, capture checks, and launcher state
   (F6-F7, F10-F12). Correct the two documentation comments (F13-F14).
4. Settle #91/#95 integration and test the resulting source (F15-F16).
5. Run focused regressions, the workspace suite, all-target/all-feature strict
   Clippy, formatting, Python capture tests, shell syntax, and unchanged release
   performance budgets. Record results from the final source, not prior CI.
6. Build and deploy a new Fiji candidate. Run scripted stationary and moving
   captures, tier handoffs, startup/load measurements, and fault-recovery tests;
   check launcher warning retention. Current native evidence is from
   `412fdac3`, not the reviewed head, and proves only the documented smoke slice.
   Do not require future full-world production acceptance for this initial PR.
7. Reply to each review item with evidence, resolve all seven current open
   threads after addressing them, request a final review, and recheck required
   GitHub checks and merge state. No merge is authorized by this checklist.

## Review Fix Batch

Changes in `lod/main-consolidated-20260930`, based on `91e74013`:

| Finding | Implementation | Proof |
| --- | --- | --- |
| F1-F3, F5 | Destination-owned publication record, seals for generated old/new files, validated recovery/cleanup, symlink rejection, session-long exclusive guard | `recovery_handles_each_owned_publication_boundary`, `recovery_rejects_record_copied_from_another_destination`, `recovery_v85_preserves_backup_when_replacement_generated_file_is_missing`, `recovery_verifies_generated_database_cache_manifest_and_chunk_bytes`, reader/alias tests |
| F4 | Existing shared locks opened read-only; missing lock still requires safe initialization | `existing_shared_lock_uses_a_read_only_descriptor`, `read_only_parent_allows_provisioned_readers_but_missing_lock_fails_closed` (UID 1000), shared/exclusive conflict tests |
| F6-F7 | Strict historical query smoke checks; typed transient/terminal database failures | Python validator tests, `terminal_query_failure_never_schedules_a_retry`, SQLite classification and bounded-retry tests |
| F8 | Verified compatible schema-16 GLBs reused; pre-collision producers still invalidated | `schema16_meshes_reuse_but_changed_mesh_source_reconverts`, cache migration and retained-producer fixtures |
| F9 | Tier query/admission/retention/unload radii 6/10/18; coarse inner fallback preserved | `tier_residency_covers_moving_positive_and_negative_boundaries_with_less_work`, movement and commit-fairness regressions |
| F10 | Invalid world compiler content omitted before writes; source mutation, cancellation, DB/publication errors remain fatal | `invalid_world_compiler_content_is_omitted_without_partial_publication` verifies both an omitted world and a valid second world |
| F11 | Actual UASTC encoding documented; extension source emitted alongside native Bevy source | Structural GLB validation; software fixture captures load/render through Bevy with no asset/material/LOD failures; Fiji confirmation pending |
| F12-F14 | Bounded retained notices and post-run notices; corrected filename and reference/schema docs | Completed-notice lifecycle and canonical filename tests |
| F15 | One `shared::LAND_TEXTURE_REPEATS_PER_CELL = 24` used by near terrain and baker; build identity/manifest record scale; stale scale rejected | Renderer settings and runtime stale-scale tests; matched native captures pending |
| F16 | #95's shared minimum/range API names used; raw launcher manifest reads accept converter 15-17/world 3-5 | `readiness_accepts_the_same_schema_ranges_as_the_runtime`, shared range and legacy DB tests; merge order still needs coordination |
| F17 | One merged plugin record set supplies DB export and cell cache in both routes | Override/deletion/plugin remapping and metadata-rebuild fixtures; no timing improvement claimed |

Recovery deliberately refuses unowned legacy backups and damaged/ambiguous
transactions instead of guessing which directory can be deleted. The record
is atomically published before the first rename. Supported legacy recovery
is covered by the schema-16/world-3 variation of the generated-artifact test.
An exclusive conversion guard means an existing package cannot be opened by
the engine during a conversion targeting that same output.

#91's current head is `f8c53ebf` and places the tiling constant in `shared`;
this batch uses that location and value. Its anisotropic-filtering change
remains owned by #91. #95 remains open at `3c7533dd`; this batch uses its
contract names and extended schema ranges, rather than a competing API.
Maintainers must preserve those contracts when choosing merge order.

Native acceptance remains open: Fiji authenticated successfully, then went
offline in Tailscale and subsequent SSH attempts timed out. Old `412fdac3`
captures do not validate this source or newly baked 24-repeat atlases.
Publication-boundary unit tests are not GPU reader-fault recovery captures.

### Verification

Final runtime source, 2026-10-01:

- `cargo test --locked --workspace --no-fail-fast`: 830 passed, 15 expected
  ignored, zero failures.
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`:
  passed. The existing `proc-macro-error2` future-incompatibility notice remains.
- `cargo fmt --all -- --check`, `git diff --check`, and both capture scripts'
  shell syntax: passed.
- Python capture/audit suite: 37 passed.
- Release dummy-content performance suite: all four unchanged budgets passed.
- `scripts/capture-lod-phase1.sh`: frozen runtime source recorded in
  `/home/dev/.cache/openskyrim/lod-pr105-fixes-20261001/capture-2`.
  Base commit `91e74013` plus dirty diff SHA-256
  `dc952b3347002f82231da5af9bce1e4e9d9f1cacf328e0942214b4d66798c75b`.
  Completion documentation was added afterwards; no runtime source changed.
  Capture 1 predates the source freeze and is superseded as source evidence.

The frozen-source LOD capture has six ready chunks, 32 visible LOD terrain
patches, and zero pending/failed LOD queries/chunks, failed cells/assets,
material/terrain validation failures, diagnostic fallbacks or streaming
invariant failures. Both 1600x900 PNGs were inspected. Full-detail and LOD
images contain 144,924 and 142,057 distinct RGB colors; whole-image mean
absolute RGB difference is 3.1226/255, with 11.4343% of pixels differing by
more than eight in at least one channel. PNG hashes match capture 1 exactly.
These figures describe a deterministic synthetic fixture with unequal full
cell coverage; unchanged sky/neutral terrain dominate the comparison. They
are not real-world visual parity or equal-quality performance evidence.

Mesa software rendering reports Vulkan swapchain image-layout/semaphore
validation errors in both scenarios. The screenshots and streaming gates
pass, but those driver/backend errors prevent clean renderer acceptance.
Fiji is still offline. T39's local fix verification is complete; T40's native
stationary/moving/recovery/launcher campaign and merge-order confirmation
remain open. No merge or production-readiness claim is made.

Local logs: `/home/dev/.cache/openskyrim/lod-pr105-fixes-20261001/`
(`workspace-tests.log`, `clippy.log`, `performance.log`, `capture-2/`).
