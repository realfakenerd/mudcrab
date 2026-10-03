# LOD runtime proxies

Individually managed distant representations for references that static
batches cannot serve: enable-state followers, window/glow overlays, and
approved animated scenery. Visual-only unless a compatibility rule says
otherwise; never duplicate scripts, inventory, collision, quests, or AI
(RUN-08).

## State model

Explicit states: unavailable world, owner disabled, out of range, proxy
visible, full representation active, transition pending. Transition state
is operational, never persisted (RUN-01). Visibility per component:

```text
want_proxy = runtime_enabled
    AND world_context_allowed
    AND effective_enabled(owner)
    AND within_policy_distance
    AND handoff_policy_allows_proxy
```

Handoff policy is not "cell unloaded": full geometry may already show
outside attached cells, and keep/copy/replacement rules can override.
Enable-parent inversion evaluates correctly; cycles and broken parents
are compile-time errors, never infinite runtime traversal (RUN-02).

## Distance classes

Near, Far, NeverFade. Grid diameter vs radius stays explicit: for odd
diameter `d`, nominal radius is `(d - 1) / 2`, with inclusion boundaries
confirmed against the engine adapter (RUN-04). Proxies bucket spatially;
state owners keep reverse dependencies to affected proxies; no per-frame
full-scene scan (RUN-05). Polling fallback, if ever needed, carries a
configurable budget and latency.

## Epochs and persistence

Fast travel, world change, load, or reinit increments a context epoch;
asset completions from an old epoch are ignored (RUN-06). Build IDs and
schema versions validate before activation; mixed generations fail with
an actionable error, never partial scenery (RUN-09). Saves hold minimum
persistent state; spatial caches rebuild after load; stale handles never
reuse without identity validation (RUN-10).

## Glow and windows

Building bodies compile separately from controllable window overlays,
both bound to the same source reference (GLOW-01). Fixed emissive color
and external region-driven emittance stay distinct through batching
(GLOW-02); flame geometry, light sprites, glow volumes, and window
surfaces get separate asset/rule choices (GLOW-03). Fake lights simulate
luminous air only, never real light sources (GLOW-05). Preview across
day/night and weather fixtures with bounded distance multipliers
(GLOW-04).

## Animation

Each binding declares one of: autonomous loop, engine-managed effect,
explicit state channel, or unsupported. Approved list only (windmill,
waterfall, flame pattern). No door pose, movement, phase, destruction,
or script-variable sync is implied by enable/disable sync.
