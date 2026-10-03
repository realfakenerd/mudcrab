# LOD Consolidation On Main

This report records the consolidation checkpoint before the fresh release
candidate. The later [RC handoff](lod-rc-20260930.md) supersedes its failed
capture and no-publication status: radius-2 P95 is 14.148632 ms and both smoke
gates pass. Broader visual, moving-camera, recovery and long-session gates
remain open. The checkpoint evidence below is retained unchanged.

Feature request: [mudcrab #103](https://github.com/realfakenerd/mudcrab/issues/103),
assigned to `TaylorTurnerIT`.

Working branch: `lod/main-consolidated-20260930`.
Base: `origin/main` at `a896b903472324d2613c7d530dfde1fef12351e4`.
This branch transplants LOD-only changes; it does not replay the old
`integration/phase2-rendering` branch or replace current main's controller,
launcher, collision export, cancellation, or progress implementation.

## Preserved Evidence

The original `OpenSkyrim-lod` worktree is unchanged. Its tracked and selected
untracked source files are also preserved in local archival commit
`a4389239e7cce8f5d0a9d223ecf2a17c311c8598` on
`archive/lod-before-main-20260930`. Generated Python bytecode is excluded.
That archival branch is evidence, not a proposed merge into main.

Retained LOD work includes contracts and spatial indexing; load-order/VFS
settings resolution; reference header/enable-parent metadata; deterministic
terrain payload validation; isolated metadata reuse with asset/plugin
checksums; resume cleanup; bounded transient retries; dirty-driven visibility;
LAND diffuse atlases; and fixture/capture/audit tests. Existing main behavior
takes precedence where older prototype plumbing has been replaced.

Converter schema **17** and world schema **5** distinguish this branch from
main's schema-16 collision producer. Metadata reuse keeps the original asset
producer schema/configuration rather than relabeling retained bytes. Native
schema-16 inputs remain explicitly verified; normal conversion regenerates
older-contract meshes. Historical schema-16 LOD evidence is not a schema-17
release validation.

Legacy pre-prune checksum replay still requires the verified raw VFS retained
by historical packs. Current-main runtime packs omit that build workspace;
their clean manifest hashes use the normal verified-copy path. A historical
mismatch without replay inputs remains an error, not permission to trust or
rewrite the expected hash. The metadata-only route also remains non-resumable;
its cancellation behavior has not been upgraded to main's normal conversion
stop/resume contract in this consolidation.

## Native Gate Remains Open

The latest Fiji evidence is under
`/home/dev/.cache/openskyrim/fiji-lod-20260929/modern-fiji-capture-1`.
The source is the user's complete non-VR
`/home/taylor/Projects/mudcrab/modern_assets`; it was not changed.

The derived package verifies 80,208 retained files and 1,693 generated chunks,
including all 1,008 expected Tamriel chunks. Riverwood inspection finds
169/169 cells and 759/759 ready models. The native radius-2 run has 134 ready
chunks, 4,256 visible terrain patches, and no pending/failing streaming work.
Its average rate is 79.74 FPS, but P95 is **16.982165 ms**, above the unchanged
**16.67 ms** gate. The capture report fails. Radius-0 uses different full-cell
coverage and cannot establish an equal-quality speedup.

Both actual screenshots were reviewed. LAND baking removes the earlier white
terrain, but radius-0 exposes coarse angular interior geometry. The current
full-perimeter/single-center fan preserves boundary samples; it does not prove
interior shape fidelity. Keep it as a testable prototype, not an accepted
quality/performance solution. Static-object and tree/billboard LOD are absent.
Solstheim and Deadlands also had unresolved LAND material references in the
old native rebuild; current-main fixes may change this, but it needs rerunning.

No package was published for human testing. This consolidation does not ship
another Fiji build, relax thresholds, or claim production readiness.

## Local Verification

The consolidated source passes the workspace-wide all-targets check and strict
all-targets Clippy. Converter/shared/dummy-content/engine suites pass 674 tests,
with 14 existing installed-asset/performance skips; engine library coverage is
242 tests. Capture/audit Python coverage passes 34 tests. Formatting, shell
syntax, and whitespace checks pass. The final converter rerun after Clippy-only
expression cleanups passes 287 tests, with 9 existing skips.

Verification logs are under `/home/dev/.cache/openskyrim/`:
`lod-main-check-20260930-4.log`, `lod-main-tests-20260930-4.log`,
`lod-main-clippy-20260930-2.log`, and
`lod-main-final-converter-tests-20260930.log`. Earlier failed runs remain there
as evidence. These are local source checks, not a native Fiji approval.

## Next Gate

Verify the rebased contracts and fixture workflows locally. Then address
terrain interior fidelity and measured runtime cost with matched native
captures. Retain failed runs, require existing acceptance budgets, and only
publish a human-testing package after both visual and performance checks pass.
