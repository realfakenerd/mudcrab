# SPEC

## §G GOAL

Build Skyrim-derived player movement in phases. First milestone: retain working Riverwood WALK/NOCLIP controller, drive walk/run speeds from resolved game records in Creation units, and keep basic jump working while resolving its record-backed parameters. Expand record conversion and behavior only when later phases need them; full controller parity remains long-term goal.

## §C CONSTRAINTS

- Initial target: Skyrim Special Edition master records in current converted asset set; preserve plugin order and winning override. Runtime oracle capture is optional, not a gate for basic walk/run/jump.
- CommonLibSSE-NG headers establish native structures, names, and record links; they do not establish update equations or prove behavioral use of every field. Derive expected flat-ground distance from verified `MOVT` speed units and elapsed fixed-step time; use selected `GMST` values only after identifying their units and role. Label project-derived formulas as such, without claiming exact executable parity.
- Engine/Rust owns input dispatch, camera, actor state, movement solver, collision, animation state requests, fixed-step integration, and native Papyrus implementations. Records supply immutable definitions; runtime state supplies mutable actor values, active effects, inventory, control locks, and save data. Papyrus invokes native operations and reacts to events; script source does not implement contact resolution or per-frame locomotion.
- FormID/load-order resolution, overrides, sex/race selection, missing-form policy, and record provenance precede tuning. Retail Player `NordRace` has no `WKMV`/`RNMV`; Q2 selects `NPC_Default_MT` FormID `0x0003580D` as explicit first-pass profile pending native selection-path evidence. Never infer a value from an unrelated GMST name or copy a header offset as a record layout.
- Legacy P1–P3, V1–V34, and T1–T15 document prototype behavior/history. `MovementTuning` remains responsible for working WALK physics; Q2 replaces only verified walk/run/jump inputs. Capsule, eye height, slopes, and other guesses stay provisional for later phases.
- Riverwood primary manual physics test area; primitive fixture retained for automated regression only.
- Prototype stack: Bevy `0.19.0`; Rapier 3D `0.36.x`. Q2 reuses current Rapier controller and fixed-step integration; later replacement requires a concrete mismatch and Riverwood regression gate.
- Linux build/test → `devenv shell` supplies Bevy Wayland pkg-config libraries.
- Rust release upgrades deliberate: `rust-toolchain.toml`, CI toolchain inputs & `RUSTUP_TOOLCHAIN` ! same fixed version; `devenv` reads toolchain file.
- Former P3 moves first, merged with thin WALK/tankard fixture from former P4; phases renumbered in execution order. Every collision gate uses same player capsule & dynamic tankard, not probes alone.
- Render terrain/collision terrain share validated 33×33 quadrant geometry, transforms, & streamed lifetime.
- Static collision source explicit per converted asset. Prefer original NIF/Havok collision for placed `STAT`/`TREE`/`FURN`; `FURN` gets no render proxy. Where unavailable, use declared render-triangle proxy only for verified `STAT`/`TREE` solids. Never use broad bounds boxes or treat all visuals as solid. Record proxy/skipped coverage.
- Converted GLB scene extras carry NIF collision provenance and supported Havok geometry; authored absence and unsupported extraction remain distinct from legacy GLBs.
- Database `statics` table also holds `MISC`/other movable model records; fixed collider eligibility requires base record type or equivalent authoritative metadata. Movable clutter cannot receive both fixed & dynamic colliders.
- Debug tankard uses visible cup/handle geometry & compound convex collider; converted tankard model absent in inspected asset set. Label it as physics fixture, not Skyrim asset parity. P1 auto-spawns in fixture; P2+ `T` spawns bounded debug tankards in loaded interactive world.
- Existing benchmark, visual fixtures, screenshot, headless, & `--auto-fly-speed` paths retain current camera behavior; interactive Riverwood owns controller.
- Physics uses Creation units throughout; WALK & dynamic tankards share gravity constant. Origin rebase moves Rapier body poses with Bevy transforms, preserving velocity; shifting visual transforms alone insufficient.
- First milestone excludes new camera states, Papyrus bindings, actor-value/effect stacking, mounts, swimming, sneak, combat, animation, and general movable clutter. `Alt` sprint uses a provisional 1.5× directional run factor with unlimited duration; debug tankards and pickup stay available. Later phases add only records needed by their behavior.
- Phases remain planned until gate evidence passes. Later phase work waits for predecessor acceptance.

id|state|capability|gate
Q0|accepted|record/value map|Player race, selected `MOVT`, candidate `GMST`, source hashes, unit assumptions and project formulas in `docs/player-movement-record-map.md`
Q1|accepted|minimal typed conversion|`NPC_` link + winning `RACE` validation, selected `MOVT`, candidate `GMST` fields load with overrides/provenance; converter/runtime tests pass
Q2|planned|record-backed walk/run + basic jump|existing WALK controller uses Q1 values where verified; flat-distance, jump, Riverwood collision/regression gates pass
Q3|deferred|modifiers and added gaits|add `AVIF`, `SPEL`, `MGEF`, `PERK` only for supported speed/jump/sprint/sneak rules
Q4|deferred|water and movable world|add `WRLD`, `CELL`, `LAND`, `REFR`, `WATR`, and required base object records for swim/contact
Q5|deferred|camera, animation, scripts|add equipment/animation forms and `QUST`/VMAD as corresponding behavior ships
Q6|deferred|special player states|mount, furniture, transformations, bleedout, dragon; identify record dependencies per state

Q0–Q2 form current delivery. Q3–Q6 are parked scope; none blocks basic WALK release.

id|state|capability|depends_on|gate
P1|planned|NOCLIP + WALK + dynamic fixture|-|mouse camera, `V`, walking capsule, debug tankards pass primitive slope/wall tests
P2|planned|streamed terrain collision|P1 accepted|player walks/jumps across hill/seams; tankards roll/rest on hill; unload/rebase pass
P3|planned|fixed static collision|P2 accepted|player blocked by rock/wall, passes doorway; tankards hit statics without tunneling

## §I INTERFACES

- I.launcher_lod: implemented launcher LOD reporting; planned existing-assets build action; contract and gates in `docs/specs/modding/launcher-lod.md`. Fiji RC excludes build action.
- cmd: `--physics-fixture` → interactive primitive hill/wall arena, WALK/NOCLIP toggle, auto-spawned debug tankards; no Skyrim asset install required.
- runtime: P1+ interactive exterior → first-person NOCLIP at start-cell view; fixture supports both modes from P1. P2 enables WALK over streamed terrain.
- key: NOCLIP `W/A/S/D` fly relative to view; `Space` rise; `Ctrl` descend; `Shift` accelerate.
- input: mouse look; click viewport captures pointer; `Escape` or focus loss releases pointer; uncaptured pointer → no movement/look.
- debug: `NOCLIP: ON  [V]` or `NOCLIP: OFF  [V]`; `[V]` identifies toggle key, no player glyph; blocked WALK entry shows reason.
- debug: interactive Riverwood `[F3]` toggles Rapier collision outlines; status reports ON/OFF.
- key: `V` → toggle NOCLIP ↔ WALK once per press; NOCLIP default ON on interactive start.
- key: WALK `W/A/S/D` move relative to yaw; mouse look; `Space` jump; `Shift` walk slowly; `Alt` press while moving toggles sprint; no movement input, focus loss, or NOCLIP switch clears sprint. No stamina cost.
- key: P2+ `T` → spawn one debug tankard ahead of camera only when local terrain collider ready; bounded live count; fixture auto-spawns several in P1.
- key: P2+ `E` → pick aimed nearby debug tankard; next `E` drops it. Held item follows view with collision/gravity suspended; release restores dynamic physics.
- mode: NOCLIP→WALK in free space keeps position; overlap → bounded upward search for free capsule placement; missing collision/no safe placement → remain NOCLIP & show reason.

### Movement data flow

- source: winning `NPC_` Player record → `RACE`; absent `WKMV`/`RNMV` → explicit first-pass `NPC_Default_MT` `MOVT` selection. Separately inspect named `GMST`. Export typed consumed fields plus source FormID/plugin and raw record. Existing `MovementTuning` remains runtime owner; Q2 fills selected walk/run fields after asset load.
- unit: document raw field type, evidenced or unknown source unit, project conversion factor, and resulting Creation units/s or units/s² for each consumed value. First pass treats `MOVT` speed as Creation units/s with factor 1; unit remains inferred. At steady speed on flat ground, expected distance = effective speed × elapsed fixed-step time; acceleration/contact makes start-from-rest distance different.
- jump: select record-backed height/impulse/gravity only with established field semantics and units. If height + gravity are confirmed and no impulse field exists, project model uses `launch_speed = sqrt(2 × gravity × target_height)`; this is a derived initial rule, not a recovered Skyrim update equation. Keep current jump values labeled provisional until source mapping is established.
- runtime: reuse current WALK input, grounded check, fixed-step integration, camera, collision, `V` NOCLIP, `F3`, and tankard debug path. No new player state object, Papyrus binding, or native solver for Q2.
- later: when Q3+ needs actor values, effects, scripts, or save state, keep their mutable runtime state separate from immutable record definitions. Papyrus invokes engine-owned operations; scripts do not solve contacts.

### Record extraction contract

Each phase adds typed conversion for consumed fields only. Retain raw subrecords, winning override, source plugin/FormID, and actionable errors for malformed required links or values.

phase|record types|minimum new fields|consumer
Q1|`NPC_`|reuse existing Player race FormID; height/sex only if chosen value rule needs them|player-to-race link
Q1|`RACE`|inspect optional walk/run movement FormIDs (`WKMV`/`RNMV`); retail Player `NordRace` leaves both absent|race profile evidence
Q1|`MOVT`|directional walk/run speed data (`SPED`) and fields actually used by Q2|flat movement targets
Q1|`GMST`|allowlisted candidate names, type, value; consume only after role/units confirmed|jump parameters if source identified
Q3|`AVIF`, `SPEL`, `MGEF`, `PERK`|actor-value IDs and effect/condition links required for speed/jump modifiers|modified gaits
Q4|`WRLD`, `CELL`, `LAND`, `REFR`, `WATR`, `STAT`, `TREE`, `FURN`, `ACTI`, `DOOR`, `CONT`, `MISC`, `FLOR`|water/contact and placed-object fields beyond existing collision export|swim and movable contacts
Q5|`WEAP`, `ARMO`, `ARMA`, `KYWD`, `EQUP`, `ENCH`, `ALCH`, `INGR`, `IDLE`, `QUST`/VMAD|equip/effect/graph/script fields only as implemented|camera, animation, scripts
Q6|state-specific forms from earlier rows|identify exact links for mount, furniture, transformation, bleedout, dragon|special states

`CLAS`, `MATO`, and `MATT` remain conditional on a demonstrated movement/contact dependency. Core input mapping and INI values are external to plugin record extraction.

## §R RESEARCH

id|topic|finding|src
R1|dependency|`bevy_rapier3d 0.36.0` declares Bevy `0.19.0` dependency|https://docs.rs/crate/bevy_rapier3d/0.36.0/source/Cargo.toml
R2|controller|Rapier kinematic controller exposes translation, slope climb/slide angles, autostep, & ground snap|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/control/struct.KinematicCharacterController.html
R3|controller result|Rapier output exposes grounded, effective translation, collisions, & slope sliding|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/control/struct.KinematicCharacterControllerOutput.html
R4|terrain collider|Rapier `Collider::trimesh` consumes vertices + triangle indices & returns `Result`; handle failure during cell commit|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/geometry/struct.Collider.html#method.trimesh
R5|Skyrim controller|reverse-engineered `bhkCharacterController` exposes gravity, jumpHeight, supportNorm, collisionBound, & step/jump flags; no native update body|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/B/bhkCharacterController.h
R6|Skyrim input|`hkpCharacterInput` includes movement, jump intent, gravity, velocity, & surface info; exact player values unmeasured|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/H/hkpCharacterContext.h
R7|Skyrim slope|`hkpCharacterProxy` names `maxSlopeCosine` & friction fields; player use/value of each field unmeasured|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/H/hkpCharacterProxy.h
R8|dynamic shape|Rapier supports compound colliders for multi-part debug tankard shape|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/geometry/struct.Collider.html#method.compound
R9|dynamic body|Rapier supports `RigidBody::Dynamic` for gravity-driven tankards|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/dynamics/enum.RigidBody.html
R10|contact|Rapier exposes CCD for fast dynamic bodies; enable only if gate observes tunneling|https://docs.rs/bevy_rapier3d/0.36.0/bevy_rapier3d/dynamics/struct.Ccd.html
R11|native input|`PlayerControls` holds movement/look/sprint/sneak/jump/POV handlers; `PlayerControlsData` stores move/look vectors and running/auto-move flags; handler algorithms absent|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/P/PlayerControls.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/P/PlayerControlsData.h
R12|native camera|`PlayerCamera` enumerates first person, third person, free, furniture, mount, transition, bleedout, dragon states; offsets/transition rules unproven by enum|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/P/PlayerCamera.h
R13|native state|`hkpCharacterState` enumerates ground, jump, air, climb, fly, swim and virtual `Update`/`Change`; `hkpCharacterInput` carries directional input, jump, surface, gravity, velocity, and step info|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/H/hkpCharacterState.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/H/hkpCharacterContext.h
R14|native controller|`bhkCharacterController` holds `gravity`, `jumpHeight`, `waterHeight`, `swimFloatHeight`, `actorHeight`, `supportNorm`, `collisionBound`, and step/support flags; no formula for deriving these from records|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/B/bhkCharacterController.h
R15|race|`TESRace::RACE_DATA` has sex-specific height, mass, acceleration/deceleration; race links six `BGSMovementType` forms and behavior graphs|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/T/TESRace.h
R16|movement form|`BGSMovementType` contains `Movement::TypeData` with directional walk/run speed table, rotation, and movement-speed fields; header alone does not establish units or update equation|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/B/BGSMovementType.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/M/Movement.h
R17|actor values|native `ActorValue` includes `SpeedMult` and `JumpingBonus`; `ActorValueOwner` has base/current/modification methods; `ActorValueInfo` is a form with enum name and abbreviation|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/A/ActorValues.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/A/ActorValueOwner.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/A/ActorValueInfo.h
R18|player base|`TESNPC` has height field; converter currently extracts NPC race/class/flags and retains raw records; runtime has no typed RACE/MOVT/GMST player resolver|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/T/TESNPC.h ; crates/converter/src/esm/exporter.rs
R19|Papyrus boundary|vanilla `Game.psc` declares native control/POV/settings functions; `Actor.psc` declares native actor-value/race functions; signatures do not implement movement physics|https://github.com/ianpatt/skse64/blob/25b72352adb6543fa6d0bd3795780672b2e238e0/scripts/vanilla/Game.psc ; https://github.com/ianpatt/skse64/blob/25b72352adb6543fa6d0bd3795780672b2e238e0/scripts/vanilla/Actor.psc
R20|effects|`EffectSetting` carries effect flags, associated form/skill and archetype; `BGSPerk` has entries/conditions; behavioral influence requires record and runtime evidence|https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/E/EffectSetting.h ; https://github.com/CharmedBaryon/CommonLibSSE-NG/blob/b93280e832f263dbef44e44cbe2936622a02f91a/include/RE/B/BGSPerk.h
R21|prototype implementation|`MovementTuning` currently hardcodes movement, gravity, capsule and eye values; `PlayerControlsPlugin` uses Rapier KCC; converter stores all raw records but typed `NPC_` subset only|crates/engine/src/physics.rs ; crates/converter/src/esm/exporter.rs
R22|retail movement values|winning Player `0x00000007` from `ccbgssse018-shadowrend.esl` RNAM `NordRace` `0x00013746`; no `WKMV`/`RNMV`; `NPC_Default_MT` `0x0003580D` SPED left 80.09/370, right 79.75/370, forward 80.10/370, back 71.93/205.25; unit/s inferred|docs/player-movement-record-map.md ; https://github.com/Mutagen-Modding/Mutagen/blob/dev/Mutagen.Bethesda.Skyrim/Records/Major%20Records/MovementType.xml
R23|retail GMST candidates|`fMoveCharWalkBase` `0x0001EC72` = 100; `fJumpHeightMin` `0x000ABEF6` = 76; no proven player-speed/impulse mapping or gravity GMST|docs/player-movement-record-map.md

## §V INVARIANTS

V1: P2 ∀ valid visible terrain quadrants → matching fixed collider from same positions/indices & parent transform; invalid/hidden terrain → no collision; unload/reload → no stale/duplicate collider; player & tankard cross seams.
V2: P2 collider build error → reported asset/cell failure, no panic or invented floor; existing render/streaming acceptance behavior preserved.
V3: P3 ∀ eligible fixed placements → collider follows full instance/node transform & cell lifetime; source marked original or proxy; unsupported assets skipped & counted. Base record type distinguishes fixed statics from `MISC`/other movable clutter; no duplicate fixed/dynamic collider.
V4: P3 player blocks at rock/wall, passes doorway/opening & excluded visual; tankard hits same rock/wall without tunneling, falls through opening as geometry permits; unload/reload leaves no stale collider; P2 gate remains green.
V5: P1 interactive exterior → one streaming camera, NOCLIP ON, walking collider/gravity disabled; camera crosses fixture/world geometry; noninteractive paths unchanged.
V6: P1 mouse yaw/pitch controls view, pitch bounded; uncaptured/unfocused input cannot move or look; focus loss clears held keys; render-origin rebase preserves position & view.
V7: P1 debug overlay reports actual mode every frame with adjacent `[V]` key hint; overlay never obscures center view or screenshot path.
V8: P1 `V` press toggles once; NOCLIP→WALK at any height when capsule fits; failed overlap search leaves NOCLIP ON with visible reason; WALK→NOCLIP removes collision/gravity immediately.
V9: P1 WALK capsule upright independent of camera pitch; camera follows body at eye offset; yaw moves body, pitch moves view. P2+ render-origin rebase shifts Rapier body poses & Bevy transforms together; body/camera/tankards/world colliders stay aligned without added velocity.
V10: P1 WALK input normalized & camera-relative; acceleration/deceleration bounded; WALK < RUN < SPRINT; velocity frame-rate independent. Initial guesses: walk 160, run 300 Creation units/s, sprint 1.5× directional run; horizontal acceleration 1800 units/s².
V11: P1 gravity applies while airborne; jump requires grounded contact & new `Space` press; no air repeat. Initial guesses: gravity 900 units/s² downward, jump launch 340 units/s upward; fixture records actual apex/time.
V12: P1 capsule radius 28, standing height 100.8, eye height 89.6 Creation units; slope climb 50°, slide 55°, autostep height 36 units, ground snap 12 units; provisional values centralized. Fixture ground, wall, step, slope outcomes match configured behavior.
V13: NOCLIP→WALK requires free capsule placement, independent of ground distance; failed entry keeps NOCLIP with reason. Active airborne WALK applies gravity through absent ground collision; no synthetic floor or loading claim from ray miss. NOCLIP remains available.
V14: P1 toggling clears stale flight/walk velocities & jump state; mode/overlay/collision response agree after switch in air, on ground, or near obstacle.
V15: P1 dynamic debug tankards use 60 Hz Rapier fixed step, same downward 900 Creation units/s² gravity as WALK, & compound convex colliders; fixture cups tumble/roll downhill, contact ground/wall, settle without persistent penetration; P2+ `T` spawns only near loaded collider, capped at 32 live bodies; switch to NOCLIP leaves tankard physics active.
V16: ∀ collision phase gate P1–P3 → exercise actual WALK capsule & dynamic tankard against phase geometry; probe-only success insufficient. Record position/contact outcomes, frame-step settings, asset/collision provenance, & manual playtest result.
V17: Player WALK, terrain, fixed statics, & tankards share one Rapier physics context with matching collision groups; player & tankards contact surfaces, tankards contact player, NOCLIP camera contacts none.
V19: P1 character controller keeps `apply_impulse_to_dynamic_bodies=false` until upstream Rapier manifold-transfer panic fixed; hill gate walks slope with tankards nearby & asserts zero Rapier panics.
V18: Input sampled once/frame, movement integrated once/60 Hz physics tick; streamed collider commits & origin rebase reach Rapier before next physics tick. Same fixture inputs at 30/60/120 render fps → positions/contact outcomes within declared tolerance.
V20: Interactive Riverwood uses one controller camera; WALK yaw & pitch consume current `LookIntent`; mouse up/down changes view while capsule stays upright. Benchmark/screenshot/headless/auto-fly camera behavior preserved.
V21: P2 `E` picks aimed `DebugTankard` within 240 units only; held body follows camera, collision & gravity suspended; second `E` releases dynamic body without stale velocity. Unload/rebase/mode toggle leaves no dangling hold. Overlay shows `T`/`E` controls.
V22: Fiji Riverwood launcher with unset display vars + live `/run/user/<uid>/wayland-N` socket → set `XDG_RUNTIME_DIR` + `WAYLAND_DISPLAY` before engine; preserve explicit display vars; no socket → clear launcher error.
V23: P3 `RockCliff` `BLEND` rock faces → render proxy; `MASK` detail & unrelated blended visuals → no proxy. Fiji warning log ! `RockCliff` skipped for no eligible triangles.
V24: P3 legacy GLBs: `TREE` pine models & `STAT` pine logs/stumps, firewood piles, road ramps → fixed render proxy from supported primitives; `FLOR`, plant `TREE`, movable records → no fixed collider.
V25: P3 lumbermill walkway `MASK` primitive → proxy; masked roof/rope & unrelated masked primitives → no proxy.
V26: WALK crosses adjacent 15-unit floor rises without jump while existing wall/24-unit step tests remain green.
V27: P3 converted `STAT`/`TREE` with physical NIF Havok layer + supported shape → authored collider; absent collision or `NONCOLLIDABLE` layer → passable; unsupported shape → counted reason. Legacy GLBs alone use narrow proxy policy.
V28: P3 bridge/stair compressed mesh follows authored triangles, chunk transforms, body and node transforms; WALK crosses deck/treads without render-beam snag; tankard contacts deck. Invalid refs, indices, transforms → skipped reason, no invented collider.
V29: Converter collision-contract change bumps cache schema; engine accepts complete converter schema 15–17 packages, schema 15 via proxy fallback; older/newer/incomplete assets fail startup.
V30: Converter schema 12–15 → 16 migration marks manifest incomplete, invalidates every GLB cache entry, preserves unchanged texture/script cache entries.
V31: Packaged Riverwood 5×5 and wider grid `x=-1..11,y=-18..-6` audits → zero unsupported fixed `STAT`/`TREE` models; bridge, stairs, lumbermill, pine solid & clover passable.
V32: P3 authored multi-shape placement → each mesh/primitive attached as child collider to one fixed body; ⊥ nested Rapier compounds; all child colliders use world groups and answer contact queries without panic.
V33: P3 placed `FURN` with supported authored NIF collision → fixed streamed collider; authored absence → passable; `FURN` gets no render proxy. MillLogPile contact blocks WALK and tankards. `MISC`/`FLOR` remain outside fixed path.
V34: Interactive Riverwood `F3` → toggle actual Rapier collider outlines; startup OFF, status matches toggle; entirely behind-camera collider bounds hidden, bounds crossing view plane shown; noninteractive camera paths retain prior behavior.
V35: Q0 source manifest ! master/plugin files and hashes, load order, winning Player `NPC_` FormID, linked `RACE` FormID, explicitly selected `MOVT` FormID and absent race links, selected `GMST` names and values. No runtime sampler required for Q0–Q2.
V36: Q0 field map distinguishes CommonLib header field, ESM subrecord, value type, source unit, conversion factor, resulting Creation unit, and Q2 consumer. Only mapped fields enter tuning.
V37: Q0 walk/run steady-state target for flat unobstructed motion = resolved effective `MOVT` speed in Creation units/s × elapsed seconds. Acceleration phase and contacts evaluated separately; header layout alone does not establish conversion factor or update formula.
V38: Q0 jump map identifies source of target height/impulse and gravity. When height and gravity have known units, `sqrt(2 × gravity × height)` defines project launch-speed calculation; no claim this is Skyrim's internal formula. If no confirmed record source exists, retain provisional jump tuning and report gap.
V39: Q1 typed extraction reuses `NPC_` race link, validates winning `RACE` and optional walk/run links, adds selected `MOVT` speed fields and allowlisted `GMST` candidates. `AVIF`/effects/quests/animation remain later-phase work.
V40: Q1 resolver applies winning override and FormID remap, preserves plugin/FormID provenance, validates Player `NPC_`→`RACE`, and rejects missing selected `MOVT` with record/field error. Absent retail `WKMV`/`RNMV` ! remain explicit; existing raw records remain available.
V41: Q1 decoded speed, gravity, jump values finite and in valid range; invalid type/length/value names exact source. No silent zero, guessed setting name, or arbitrary fallback in a package claiming record-backed movement.
V42: Q2 existing `MovementTuning`/Rapier fixed-step WALK path consumes resolved walk/run parameters; existing input, grounded jump guard, camera, collision, and NOCLIP remain authoritative. No second solver or speculative `PlayerRuntime` layer.
V43: Q2 forward/back/strafe speeds use resolved direction-specific `MOVT` fields where established; diagonal input keeps bounded normalization. Any movement-mode/direction without a confirmed field keeps labeled provisional tuning.
V44: Q2 deterministic empty-fixture steady walk/run displacement matches Q0 `speed × time` within max(1 Creation unit, 1% target) over 2 seconds at 60 Hz; start-from-rest checks current acceleration integration separately.
V45: Q2 jump still requires grounded press and no air repeat. When record-backed height/gravity exist, free-flight apex matches derived project target within max(2 Creation units, 2% height); otherwise current jump remains functional and marked provisional.
V46: Q2 Riverwood WALK traverses existing bridge, lumbermill, stairs, terrain seams and blocks at solid trees/buildings; mode toggle, origin rebase, tankard contact, and 30/60/120 render-fps fixture regressions stay green.
V47: Q2 record-backed values enter physics before fixed step; changing render fps does not alter movement distance, jump result, or contact category for equal simulated time. No per-frame tuning mutation or duplicate integration.
V48: Q2 diagnostic reports effective walk/run/jump/gravity values and source record/FormID or provisional constant once at startup; warning+ log records rejected/missing fields without flooding each tick.
V49: Q2 new complete packages with missing required walk/run records fail with actionable error; legacy packages may use existing provisional tuning only when explicitly identified as legacy. NOCLIP recovery remains available.
V50: Q3 adds actor-value/effect records and sprint/sneak only when their chosen behavior is implemented; immutable record definitions and mutable actor state remain separate.
V51: Q4 adds water/world and movable-object fields only as needed by swimming and contacts; existing authored collision/proxy provenance rules still apply.
V52: Q5 adds camera, animation and Papyrus records/functions only when corresponding behavior ships; CommonLib state names are not movement equations.
V53: Vanilla capture/measurement is optional diagnostic for unresolved formulas or later parity claims, never prerequisite to Q0–Q2 implementation. Static record calculation and engine regression are sufficient for this first milestone.
V54: Developer `V` NOCLIP, `F3`, `T`, `E`, fixture, overlay, benchmark and headless paths retain behavior while Q2 tuning changes; debug flight excluded from walk/run distance checks.
V55: Full Skyrim controller parity remains unclaimed until later phases validate every supported gait, camera, water, script-modified and special state. First milestone reports exactly which values are record-backed and which remain provisional.
V56: Fiji Riverwood package bundles glibc-matched `libdl.so.2` beside `libvulkan.so.1`; missing library → named launcher error; target startup → Vulkan `AdapterInfo` and runtime initialization.
V57: WALK `AltLeft` or `AltRight` new press with movement input toggles sprint latch; key release preserves latch; second press turns it off; zero movement input, uncaptured focus, or mode switch clears latch. Sprint has no stamina cost; provisional multiplier and fixed-step integration remain.
V58: Jump fixture measures `PlayerBody` pose from grounded rest through launch apex and landing; camera and other transforms cannot satisfy or fail the jump gate.
V59: Portable Riverwood launcher leaves host Vulkan ICD discovery intact and preserves caller `VK_ICD_FILENAMES`; bundled ALSA config hook resolves through package `lib` on target. Target smoke reaches `AdapterInfo` and runtime initialization.
V60: WALK overlay reports `Alt` latch and actual horizontal speed from Rapier `effective_translation` per fixed step, never target speed; under 5 Creation units/s contact jitter reads 0, vertical jump excluded. Controlled perspective gains 8° FOV only during moving WALK sprint, with 0.12 s exponential half-life back to base on stop/mode change; noninteractive cameras retain base FOV.
V61: Provisional sprint target = 1.5× selected directional run speed, including winning `MOVT` fields; packaged forward 370 → 555 Creation units/s. Collision-free fixed-step actual speed reaches targets; blocked speed remains 0 despite nonzero target.
V62: WALK entry has no ground-distance limit; active WALK keeps falling when ground lies far below or no ground ray hits.
V63: Compressed Havok mesh vertices scale by finite positive serialized quantization error; alternate valid scales preserve geometry.
V64: Selected `MOVT SPED` accepts 40- or 44-byte layouts; rejects other lengths and invalid speed fields.
V65: Package launcher resolves XKB data on Nix and standard Linux paths or reports missing data before engine start; configured valid `XKB_CONFIG_ROOT` survives.
V66: WALK view pitch follows `LookIntent.pitch`; body pitch stays zero.
V67: `Space` press remains latched across Update frames until WALK fixed tick consumes it; no repeat after consumption.
V68: Exactly one fixture mode selected per run; physics fixture benchmark passes only after validation with zero fixture failures.
V69: Active airborne WALK with no downward collision ray continues descending across fixed ticks; ground-ray miss alone never resets velocity or reports terrain loading.
V70: NOCLIP→WALK from far above ground enters WALK & descends when capsule fits; overlap rejection keeps NOCLIP with reason after bounded upward search.
V71: Runtime accepts passed integration report & world database schema 3 through 5 inclusive; schema 3 legacy & additive schemas 4/5 supported; older/newer schemas rejected; supported Riverwood package reaches world loading without `--allow-incomplete-assets`.

V72: Launcher conversion runs existing terrain LOD stage exactly once before final validation/publication; LOD failure cannot report successful conversion. No second compiler or duplicate post-publication job.
V73: Launcher retains LOD chunk count & LOD warnings; zero chunks, partial world coverage & compiler failure remain distinct. LOD stage label/count accurate; no invented ETA or benchmark acceptance.
V74: Existing-assets LOD build verifies source manifest, retained asset hashes & matching plugin order/checksums; publishes schema-17/5 derived output to new disjoint directory. Source assets remain unchanged; metadata version relabeling forbidden.
V75: Existing-assets LOD build unavailable until cancellable metadata API & worker/state tests pass; cancellation checked between retained files & worlds and before publication; in-flight file/world allowed to finish; never reports resumable staging without journal support. Engine-running/path/asset-lock guards retained.
V76: Publication backups identified by full output name; record-owned recovery restores supported manifests with verified retained output sizes/hashes; incomplete status retained; invalid or symlink backups remain untouched.
V77: Equivalent existing asset paths share one canonical lock; acquisition fails on resolution errors; missing-tail paths normalized before report containment checks.
V78: LOD readiness counts unique queued, loading & scheduled retry work; current unrecovered failures block screenshot; recovered failures retain cumulative diagnostics; smoke capture still rejects historical failures.
V79: Shared commit cap unchanged; continuous near-cell work cannot starve queued LOD & LOD cannot starve near cells; center changes retain only in-world/in-range immutable metadata; queries retry at most three times with 1/2/4-second backoff.
V80: Publication recovery ! recorded destination/backup ownership, unchanged manifest & sealed generated artifacts; ambiguous legacy backups untouched. Missing/corrupt DB/cache/LOD prevents recovery. Symlink output rejected; one exclusive output guard spans prior-pack reads through publication.
V81: Shared lock opens existing read-only descriptor; missing lock on read-only parent fails closed. Terminal metadata errors never retry; transient SQLite busy/locked/IO errors retain bounded retries.
V82: Schema-16 compatible mesh producers reused with verified source/output/configuration; schema 12-15 collision producers invalidated. Tier residency bounded by tier distance + margin across query/admission/queue/unload; coarse inner fallback retained.
V83: Near/baked terrain use shared repeats-per-cell; tiling change participates in LOD build identity. World-local compiler content failure skips world with explicit warning; input mutation/cancellation/publication/DB errors fatal.
V84: Completed launcher summary & bounded conversion notices survive; post-run notices bounded separately. Generic smoke rejects historical query failures & pending queries. Atlas documentation states actual UASTC encoding & native loader limits.
V85: Interrupted publication cleanup ! validated replacement plus sealed generated files before deleting owned prior package; ownership record atomically published before directory rename.

V86: Record-owned publication recovery accepts sealed structurally valid incomplete packages at every directory-swap boundary; completeness/failures/integration pass status retained, never promoted; schema/hash/cache/DB/LOD checks unchanged.
V87: Legacy pre-prune GLB replay ! manifest-matched raw NIF & dependencies; unavailable raw source → actionable rejection before publication; retained source bytes unchanged.
V88: `--ini` retains strict CLI missing/option-shaped value rejection, help precedence & file-layer/CLI override order; INI loaded-grid radius ≤ `MAX_STREAM_RADIUS`.

V89: Script CLI drift checks cover utility commands, excluding script-test assertions; non-engine flags classified by owner; comma-separated values remain whole and PowerShell list delimiters still split.
V90: Serialized legacy `PipelineConfig` without `lod_origins` → empty map, no invented world origin; explicit signed origins retained; existing config fields/defaults unchanged.

## §T TASKS

id|status|task|cites
T1|x|P1 add Rapier fixed-step setup, `--physics-fixture` primitive slope/wall arena, debug tankard mesh + compound dynamic collider; capture existing camera regression baseline|V5,V15,V17,R1,R8,R9
T2|x|P1 add upright WALK capsule, camera follow, movement/jump/slope settings, & collision-safe toggle against fixture geometry|V8,V9,V10,V11,V12,V13,V14,V18
T3|x|P1 add mouse-look NOCLIP, `V` toggle, `NOCLIP: ON/OFF  [V]` overlay, cursor lifecycle; preserve noninteractive camera paths|V5,V6,V7,V8
T4|x|P1 test controller + tankards on primitive hill/wall, toggle/focus/rebase, 30/60/120 render fps, & camera regressions; record gate evidence|V5,V6,V7,V8,V9,V10,V11,V12,V14,V15,V16,V18,V19
T5|~|P2 attach validated terrain trimesh to quadrant lifetime; handle failures & missing-ground transition|V1,V2,V13,R4
T6|~|P2 enable bounded `T` tankard spawn; rebase Rapier poses with world; test player + tankards on real hill, seams, stream unload/reload; record gate evidence|V1,V2,V9,V13,V15,V16,V18
T7|x|P3 inventory NIF/Havok collision support, base record types, & representative statics; record original/proxy/skip policy|V3
T8|x|P3 convert/load eligible fixed colliders with full transforms, streamed lifetime, & source/skip counts; exclude movable records|V3
T9|~|P3 test player + tankards at rocks, walls, openings, excluded visuals & unload/reload; adjust CCD/contact only from measured failures|V3,V4,V15,V16,V17,V23,V24,V25,V26,R10
T10|.|P3 rerun P1/P2 gates, interactive playtest, screenshot/benchmark regression; record evidence, controls, static proxy limits|V1,V2,V3,V4,V5,V7,V15,V16
T11|x|P2 mount controller in Riverwood, fix WALK pitch, add `E` tankard pickup/drop; preserve noninteractive camera paths|V5,V6,V9,V20,V21
T12|x|Package Fiji launcher with display discovery; verify from shell with display vars unset|V22
T13|~|Extract NIF Havok collision into GLB, use it for fixed `STAT`/`TREE`, audit Riverwood coverage, package Fiji build and run WALK/tankard gate|V3,V4,V16,V27,V28,V29,V30,V31,V32
T14|x|Load authored furniture collision, shrink player height, add F3 collision view, package Fiji and test mill log pile|V12,V33,V34
T15|~|Cull F3 outlines fully behind camera, keep crossing bounds, package Fiji and check debug frame rate|V34
T16|x|Q0 inspect winning Player `NPC_`, linked `RACE`, selected walk/run `MOVT`, and candidate jump/gravity `GMST`; record FormIDs, absent links, overrides, field types and values|V35,V36,R15,R16,R18
T17|x|Q0 document speed/acceleration/jump units, conversion factors and project formulas; mark unknown GMST mapping rather than inventing it|V36,V37,V38,R14,R16,R21
T18|x|Q1 reuse NPC race link; validate winning `RACE`, optional `WKMV`/`RNMV`, and explicit retail absence before selected profile|V39,V40,V41,R15,R18
T19|x|Q1 convert needed `MOVT` directional speed fields; validate source values, unit conversion and override resolution|V36,V39,V40,V41,R16
T20|x|Q1 export allowlisted `GMST` candidates with type/value provenance; consume jump/gravity only when Q0 establishes role and units, otherwise retain provisional values|V38,V39,V41,R14
T21|x|Q1 validate Player→RACE, select `NPC_Default_MT` as first-pass profile when race links absent, keep GMST candidates unconsumed until mapped; test missing-form errors and legacy-package fallback|V40,V41,V42,V49
T22|x|Q2 feed resolved walk/run directional speeds into current fixed-step WALK path; keep input normalization, collision and debug modes|V42,V43,V47,V54
T23|x|Q2 feed confirmed jump/gravity values and derived launch speed when available; keep grounded/no-air-repeat behavior and report provisional gap otherwise|V38,V42,V45,V48
T24|~|Q2 audit packaged GLB collision coverage before handoff; package as soon as build passes for Riverwood manual test; run deterministic distance/jump and bridge/stairs/terrain/tankard regressions at 30/60/120 render fps in background, then report results|V31,V44,V45,V46,V47,V54,V56
T25|.|Q2 report effective values with source IDs, conversion formulas, unsupported mappings and first-milestone pass/fail; release only supported subset|V35,V36,V48,V49,V55
T26|.|Deferred Q3: add `AVIF`/`SPEL`/`MGEF`/`PERK` fields needed by selected speed/jump modifiers|V50
T27|.|Deferred Q3: add record-backed sprint/sneak gait rules after modifier/state dependencies are known|V50,R11,R15,R16
T28|.|Deferred Q4: add water/world fields and swimming transition behavior|V51,R13,R14
T29|.|Deferred Q4: add movable-object record types and authored dynamic contact beyond debug tankards|V51,V54
T30|.|Deferred Q5: add third-person camera and race-linked animation fields when implemented|V52,R12,R15
T31|.|Deferred Q5: bind relevant Papyrus native controls and script records to engine state|V52,R19
T32|.|Deferred Q6: resolve state-specific records and implement mount/furniture/transformation/bleedout/dragon paths; report scoped parity|V55,R12,R13
T33|x|Latch provisional unlimited WALK sprint on `Alt` press until movement stops; reset on focus loss and mode switch; test key edge and gait changes|V10,V14,V57
T34|~|Make sprint perceptible; display actual speed and latch; ease controlled camera FOV; package laptop and verify manual launch|V10,V57,V60,V61
T35|x|Launcher LOD A: retain chunk count/warnings in `RunReport`; label LOD world progress; preserve final summary & warning visibility; document automatic conversion-stage build|V72,V73,I.launcher_lod
T36|~|Launcher LOD A: focused report/status/worker tests; tiny conversion verifies payload/DB/manifest identities; stop/failure never enables new incomplete output; scripted launcher capture|V72,V73,I.launcher_lod
T37|.|Launcher LOD B: add cooperative metadata cancellation with typed failure & no-resume contract; tests cover preflight, retained-file copy, world boundary, pre-publication, source preservation|V74,V75,I.launcher_lod
T38|.|Launcher LOD B after T37: `Build LOD` action for selected converted source & new destination; reuse worker/messages/state ownership; verify reuse hashes, plugin mismatch, busy/engine/path guards & publication|V74,V75,I.launcher_lod
T39|x|PR105 review batch: destination-bound recovery seals/locking, schema16 mesh reuse, tier residency, query classification, world-content failure policy, capture/launcher fixes, shared tiling & contract docs; 830 workspace tests, strict clippy/fmt, 37 Python tests, 4 release perf tests; frozen-source software captures|V73,V76,V77,V78,V79,V80,V81,V82,V83,V84,V85
T40|.|Deferred by owner 2026-10-03: PR105 final-head Fiji stationary/moving/recovery/launcher captures & wider native performance; no pre-merge native campaign required. PR95 merged & converter-17/world-5 range tests pass. Prior candidate approval ≠ final-head native evidence; native limits retained|V73,V78,V79,V80,V82,V83,V84

## §B BUGS

id|date|cause|fix
B1|2026-09-26|probe-only collision phases deferred WALK/dynamic validation until too late|V16
B2|2026-09-26|Rapier 0.35 controller panics slicing empty manifold vec when pushing dynamic bodies on slope|V19
B3|2026-09-26|WALK follow reused prior view pitch; interactive world omitted controller plugin|V20
B4|2026-09-27|Fiji launcher assumed graphical display variables inherited by terminal; winit panicked before startup|V22
B5|2026-09-26|direct Cargo test lacked Wayland pkg-config path|§C build env
B6|2026-09-27|static proxy draft used wrong Bevy iterator and Parry ray types; Clippy found non-idiomatic loops|compile + Clippy fixes
B7|2026-09-27|opaque-only proxy skipped `RockCliff` rock faces exported as `BLEND`|V23
B8|2026-09-27|fixed proxy eligibility omitted Riverwood pine, woodpile, & road-ramp models|V24
B9|2026-09-27|opaque-only proxy skipped lumbermill walkway exported as `MASK`|V25
B10|2026-09-27|24-unit autostep clearance blocked adjoining 15-unit floor rises|V26
B11|2026-09-27|path and render-material proxies missed bridge and made stair support beams physical|V27,V28
B12|2026-09-27|converter schema bump would reject existing Fiji schema-15 package at startup|V29
B13|2026-09-27|schema-16 bump bypassed earlier GLB-only cache migration, dropping reusable texture/script entries|V30
B14|2026-09-27|unsupported `bhkNiTriStripsShape` left two wider-area uprooted pines without collision|V31
B15|2026-09-27|multi-shape authored model wrapped trimesh in Rapier compound; Parry panicked on nested composite|V32
B16|2026-09-27|mill log pile was placed `FURN`; runtime fixed-collider gate admitted only `STAT`/`TREE` despite nine authored shapes|V33
B17|2026-09-27|frame-rate gate compared unequal physics durations; Bevy FixedUpdate retained 64 Hz default despite 60 Hz Rapier/player tuning|V18
B18|2026-09-28|Riverwood bundle omitted `libdl.so.2`; wgpu failed Vulkan entry-point loading and reported no GPU|V56
B19|2026-09-28|movement package copied legacy GLBs instead of authored collision GLBs; 239 Riverwood fixed models lost collision provenance|V31,V46
B20|2026-09-28|jump test sampled an arbitrary Transform, which selected camera height and falsely failed the apex gate|V58
B21|2026-09-28|Fiji launcher forced Radeon ICD on Intel laptop; bundled ALSA hook searched build-host plugin path and crashed|V59
B22|2026-09-28|fixed 420-unit sprint gave only 13.5% gain over packaged 370-unit forward run, with no FOV or actual-speed feedback|V60,V61
B23|2026-09-28|Rapier wall contact produced about 4 units/s lateral jitter, making a stationary speed indicator nonzero|V60
B24|2026-09-28|FOV ECS test used ambiguous generic `Time::default()` and failed to compile|explicit `Time<()>` fixture resource
B25|2026-09-29|legacy conversion test copied schema 4 integration report into schema 3 checkout|fixture uses `WORLD_DATABASE_SCHEMA_VERSION`
B26|2026-09-29|400-unit WALK entry search reused for active ground presence and froze long falls|V62
B27|2026-09-29|compressed mesh decoder ignored serialized quantization error and assumed 0.001|V63
B28|2026-09-29|MOVT parser rejected legacy 40-byte SPED despite same eight speed fields|V64
B29|2026-09-29|launcher searched XKB only in Nix store; standard Linux hosts could use stale build path|V65
B30|2026-09-29|collision audit compared normalized model key but reported raw-key status|normalize report status
B31|2026-09-29|launcher test expected `/bin/sh` to depend on libdl after glibc 2.34|readable preflight fixture
B32|2026-09-29|new converter loops failed CI Clippy under Rust 1.98|collapse node condition and use `as_chunks`
B33|2026-09-29|fixture WALK camera reused prior pitch instead of `LookIntent.pitch`|V66
B34|2026-09-29|Update overwrote jump press before next fixed tick at high render fps|V67
B35|2026-09-29|fixture flags selected conflicting owners; benchmark ignored physics validation|V68
B36|2026-09-29|fixture test reassigned field after Default; CI Clippy rejected it|struct initializer
B37|2026-09-29|record tuning and scene helper used patterns Clippy rejects under workspace warning gate|struct initializer and scoped argument allows
B38|2026-09-29|active WALK treated any downward ray miss as loading and reset fall state every tick|V69
B39|2026-09-29|NOCLIP→WALK rejected free high-altitude capsule because ground lay beyond 400-unit cast|V70
B40|2026-09-29|schema 3 runtime binary paired with schema 4 Riverwood assets; report & database gates rejected valid package|V71
B41|2026-10-01|floating CI `stable` upgraded 1.98.1→1.99.0; new macro warnings failed unchanged workspace under `-D warnings`|§C fixed Rust toolchain; restore 1.98.1
B42|2026-10-01|PR #102 merge retained local LAND tiling constant alongside shared import; engine failed E0255|reuse shared constant; workspace compile + Clippy gates
B43|2026-09-30|launcher LOD report fields omitted from Play-availability fixture outside conversion module|migrate fixture; launcher compile/test gate
B44|2026-09-30|ignored layout performance test retained 12-file count after LOD sidecar addition|migrate count; assert sidecar output; preserve 10-second budget; no new invariant
B45|2026-10-01|world-inspect queried absent legacy LOD table; capture scripts passed unsupported log flag|legacy table guard; preserve malformed-table errors; existing stdout/stderr redirection
B46|2026-10-01|publication recovery adopted newest backup by name without validating completeness or outputs|V76
B47|2026-10-01|dotted outputs shared backup namespace; raw path aliases bypassed asset locks; completed notices trimmed results|V76,V77,V73
B48|2026-10-01|near cells exhausted shared budget; center changes dropped relevant LOD metadata; pending counts omitted queue/retry work; failed queries stayed requested|V78,V79
B49|2026-10-01|metadata LOD extraction let unmatched archives override package-matched plugin sources|filter unmatched archives before settings/diffuse extraction; record omission; preserve loose precedence
B50|2026-10-01|screenshot readiness treated recovered LOD failures as current errors|V78
B51|2026-10-01|first fair-budget draft derived reservation from shrinking remainder and released it after a near commit|V79; reserve from frame limit before collectors
B52|2026-10-01|backup validation omitted generated artifacts & destination ownership; output alias changed lock identity; publication lock checked too late|V80,V81
B53|2026-10-01|schema17 invalidated unchanged schema16 GLBs; all tiers loaded largest radius; deterministic metadata errors retried|V81,V82
B54|2026-10-01|generic capture omitted query failures; launcher replaced progress notices then allowed unbounded post-run growth; ADR misstated UASTC encoding; PR91 tiling diverged|V83,V84
B55|2026-10-01|new preflight IO used `?` across `PipelineFailure` boundary without `Report` conversion|named `WrapErr` context; compile oracle catches mechanical error; no new invariant
B56|2026-10-01|interrupted replacement cleanup trusted next manifest alone; missing generated files could discard last-good backup|V85
B57|2026-10-03|record-owned recovery applied Play-readiness completeness gate to structurally valid incomplete packs|V86
B58|2026-10-03|legacy prune replay assumed published packs retained raw `vfs/` NIFs; missing source produced opaque hash IO error|V87
B59|2026-10-03|INI integration bypassed strict CLI value contract & loaded-grid overflow bound; GPU result channel omitted fatal cleanup flag|V88,V83; preserve upstream parser & shared cleanup semantics
B60|2026-10-03|metadata payload validation retained old three-argument API after main parallelized validation; new recovery test used absent runner; compiler fixture retained removed export helper|pass configured CPU jobs; reuse existing runners/merged-record exporter; compile oracle, no new invariant
B61|2026-10-03|script drift scanner mistook audit/Cargo/Git flags & negative test assertions for engine arguments; comma splitting turned camera CSV into scalar|V89
B62|2026-10-03|conflict splice placed `notices` before LOD keys in sorted JSON snapshot|restore observed sorted order; fixture oracle, no new invariant
B63|2026-10-03|new `lod_origins` lacked serde default, rejecting legacy serialized configs|V90
B64|2026-10-03|schema docs claimed legacy `lod` removal while exporter retained unused table|document retained placeholder and external GLB payloads; documentation correction, no new invariant
B65|2026-10-03|V29/V71 retained pre-LOD current schema limits after runtime range expanded to converter 17/world 5|align existing invariants with supported ranges; existing runtime/launcher range tests, no new invariant
