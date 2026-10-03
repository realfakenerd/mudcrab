# ADR-0011: Fixed 4/8/16 tiers first; projected-size selection later

- **Status:** Proposed
- **Date:** 2026-09-27

## Context

DynDOLOD allows fixed spatial tiers for Skyrim parity and projected-size
selection as a native extension. This engine streams on a cell grid with
an R-tree, a per-frame commit budget, and radius-derived camera far and
shadow cascades. A size-based policy needs per-frame projected-size
evaluation and a second eviction discipline; a tier policy maps directly
onto the grid and existing streaming machinery.

## Decision

Select LOD by fixed 4/8/16-cell tiers past the full-detail grid. Keep
block size, mesh source, distance policy, and variant as separate
dimensions so a projected-size policy can replace the distance mapping
later without touching chunk contents.

## Consequences

- Chunk keys, budgets, and handoff tests stay simple and grid-aligned.
- Small-near vs large-far tradeoffs are coarser than a size-based policy;
  per-tier source choice (landmarks persist, clutter drops) covers the
  common cases.
- Revisit when tier captures show size-driven popping the tier policy
  cannot fix.
