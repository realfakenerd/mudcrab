# ADR-0012: Per-worldspace billboard atlases with gutters and layout versioning

- **Status:** Proposed
- **Date:** 2026-09-27

## Context

Tree and object billboards need atlas packing: allocation policy,
gutter size for mip fringes, power-of-two rules, and the relationship
between pixel updates and UV remeshing. Without a contract, an atlas
repack silently invalidates every chunk UV that references it.

## Decision

One atlas family per worldspace, built by the converter: diffuse and
normal atlases with synchronized transforms, transparent-edge dilation,
gutters sized for the deepest consumed mip, and power-of-two dimensions.
Atlas layout carries its own version; pixel-only updates reuse UVs, while
any layout change invalidates dependent chunks by content hash.

## Consequences

- Chunk rebuilds trigger on layout change, not on every billboard tweak.
- Atlas capacity is a budgeted, reported quantity; overflow is a
  predictable build error naming the assets that exceed it.
- Diffuse/normal sync and color-space separation are build-time checks,
  not runtime hopes.
