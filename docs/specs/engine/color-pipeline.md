# L1 material and color pipeline

Tracks [L1 #131](https://github.com/Mudcrab-Team/mudcrab/issues/131), under [vanilla lighting parity #129](https://github.com/Mudcrab-Team/mudcrab/issues/129). L0 reference capture and L1 implementation can proceed concurrently. L0 evidence gates retail parity acceptance, not synthetic tests or implementation.

## Scope and evidence

Default target: unmodded Skyrim SE. Interior and exterior cases have equal priority. Mod research informs native behavior; addon lighting remains out of scope. This first L1 slice fixes output-domain inconsistencies and supplies controlled probes. It does not establish Skyrim's image-space equations or select its final exposure/tone curve.

Code baseline: `a9f2310ccfc691eebb97fde18df1e8d334b7d744`; Bevy 0.19. The source trace below describes actual runtime behavior. Claims in older sky notes about encoded weather interpolation and fog equations still require the L0/L2 retail evidence; this change preserves those inputs.

## Pipeline trace and ownership

| Stage / owner | Input → output | Current limit / next owner |
|---|---|---|
| `converter/src/texture.rs`, `material.rs` | DDS channels → semantic KTX2 transfer format; diffuse/glow use sRGB aliases, normals/data use linear views | Shared source bytes may have distinct views; preserve alpha as data. Existing round-trip tests cover encoded channels. |
| `converter/src/material.rs` | Validated per-shape NIF values → glTF factors, extensions, source extras | #81: specular enable, normal-alpha mask and gloss exponent approximation. #82: glow-slot eligibility and emission energy. #83: alpha/UV. #84: effect/editor surfaces. These issues are open, not integrated by this slice. |
| Bevy glTF / `StandardMaterial` | sRGB textures decoded once; material factors and data textures remain linear → material response | Bevy's PBR BRDF is an approximation, not recovered Skyrim shading. Tangent/model-space normals require distinct treatment. |
| `shaders/terrain.wgsl` | Linear layer samples and normal data → weighted material response → PBR lighting | Authored normal conventions, layer semantics and specular response remain separate material probes. |
| Bevy `pbr_functions.wgsl` | Lights + material → exposure-scaled linear RGB | `Exposure.ev100` uses Bevy's `2^-EV100 / 1.2`. Current 9.7 is pinned from the prior default, not a Skyrim value. Emission's exposure weight is material-owned; default emission is exposure-independent. |
| `sky.rs`, `shaders/sky.wgsl` | Encoded weather-row mixing → one sRGB decode → linear palette brightness | Existing palette is a fixed clear-day approximation. Sky/fog palette values already occupy the composition domain; do not multiply them by camera exposure a second time. L2 owns authored state and its units. |
| `DistanceFog` | Exposed surface RGB + linear fog palette → fogged linear RGB | Existing exponential/Fog Far approximation stays explicit. Interior fog removal is current behavior, not complete Skyrim interior semantics. L2 owns the correction. |
| `render.rs` reflection camera | Scene at main-view exposure → `Rgba16Float`, tone mapping disabled | Floating linear target preserves values above 1. Reflection gate copies exposure even while inactive. Existing reflected layers and geometry selection remain unchanged. |
| `shaders/water.wgsl` | Exposed water lighting + exposed linear reflection → blend → surface fog | Reflection receives neither a second lighting evaluation nor a display transform. Existing Fresnel/waves and reflection coverage are approximations. |
| `color_pipeline.rs` / scene cameras | HDR composition → one full-view `TonyMcMapface` transform → display encoding | Includes sky, background, fog, opaque and transparent surfaces. Tone curve remains a pinned diagnostic baseline pending IMGS evidence; L4 owns adaptation/record-driven image-space behavior. |

The previous non-HDR mesh path tone-mapped in each material shader. The custom sky shader bypassed that operation, and reflection RGB passed through tone mapping in both the reflection and water passes. HDR composition moves the transform after composition without adding a new shader implementation. It changes images and consumes more render-target memory; target-hardware performance remains an acceptance requirement.

## Invariants

- V1: Every production scene camera and existing visual fixture uses explicit `SceneColorPipeline`: HDR, EV100 9.7, TonyMcMapface. These values remain provisional; defaults are not evidence of vanilla parity.
- V2: Reflection storage preserves linear values above 1; no tone map or sRGB target view before water sampling. Reflection exposure matches the main camera before rendering, including after exposure changes while water is invisible. Missing reflection exposure ! restore before rendering; pose/visibility updates continue.
- V3: Sky, unlit mesh, fully fogged mesh, terrain emission and unit-reflecting water given equal composition-domain RGB produce matching output within 2/255 per channel. Exterior includes sky; interior has a black background. Test neutral gray and saturated HDR inputs.
- V4: Diagnostic inputs, camera and output settings, samples and verdict are recorded. Probe failure returns a nonzero status; stale reports are removed at startup. Synthetic consistency is not retail parity.
- V5: Preserve NIF source values and declared unsupported families. Do not compensate for pending material errors with global tint, exposure, ambient or emission changes.

## Supported response and remaining material work

| Family / path | Current representation | L1 acceptance status |
|---|---|---|
| Ordinary lighting / opaque | glTF `StandardMaterial`, PBR lighting | Output consistency can be tested now; NIF specular and emission fixes pending #81/#82. |
| Alpha-tested / blended | glTF alpha modes + existing prepass | #83 and PR #99 remain separate dependencies. Do not declare caster silhouettes accepted before they are integrated and tested. |
| Tangent-space normal maps | Linear normal samples through glTF; terrain has its own tangent frame | Direction, handedness and specular-alpha probe still required. |
| Model-space normals | No established compatibility path in this slice | Named L1 gap; generic tangent interpretation cannot count as acceptance. |
| Environment-map / parallax / skin / hair / other NIF lighting variants | Raw source contract/extras plus generic approximation | Runtime compatibility inventory and per-family probes still required; retained metadata alone is not shader support. |
| Effect shader surfaces | Converted material approximation | #84 visibility and shader semantics unresolved. No general effect parity claim. |
| Water / sky | Custom Bevy shader paths | Output-domain test only; authored behavior and full-scene parity remain open. |

## Verification

- `cargo test -p engine --lib`: explicit scene settings, production reflection format, reflection exposure lifecycle, existing renderer/sky regressions.
- `cargo run -p engine --example color_pipeline_probe -- --output <dir> [--interior] [--gray]`: real GPU shader path and pixel comparison; uses headless GPU readback and requires a Vulkan adapter with at least 32 sampled-texture and sampler slots. The probe requests WebGPU features with terrain limits explicitly raised; it does not benchmark the full production device feature set. Software Vulkan is sufficient for functional validation, not performance acceptance.
- Run all four combinations: exterior/interior × gray/HDR. Each writes `probe.png` and `probe.json`. The probe fixes 800×600, orthographic camera `(0,0,10)`, EV100 9.7, TonyMcMapface, no dither/MSAA, full diagnostic fog on one swatch, unit reflectivity on water, and constant sky rows. All are isolated diagnostic settings, not shipping values.
- The probe uses the production reflection allocation and camera setup; its unlit source geometry, render layer and visibility are diagnostic. A clear-only reflection would not exercise material tone mapping. The HDR case includes blue = 2.0 to detect clipping and duplicate display transforms.
- `--legacy-output` is a negative control: restore the previous non-HDR cameras and 8-bit reflection target inside the probe. It must fail the consistency check, with a nonzero status and saved pixel differences. This option does not exist on the game CLI.
- Complete L1 acceptance additionally needs NIF-to-runtime material probes, integrated dependency fixes and matched vanilla neutral/material captures from L0. Leave #131 open until those gates pass.

## Local verification, 2026-10-02

Headless Vulkan on llvmpipe / Mesa 26.2.2, LLVM 21.1.8. No renderer errors in the six final runs. These are functional shader checks, not target-hardware performance or Skyrim visual acceptance.

| Case | Largest channel difference (8-bit) | Expected verdict |
|---|---:|---|
| Exterior, HDR `(0.18, 0.4, 2.0)` | 1 | Pass |
| Interior, HDR | 1 | Pass |
| Exterior, gray `(0.18, 0.18, 0.18)` | 0 | Pass |
| Interior, gray | 0 | Pass |
| Previous output path, HDR | 39 | Fail (negative control) |
| Previous output path, gray | 3 | Fail (negative control) |

HDR mesh/sky/fog/water samples: `(114,151,239)`; terrain: `(114,152,239)`. Previous-path sky: `(118,170,255)`; water: `(107,136,200)`. All new-path gray samples: `(115,115,115)`. Interior background: black. Pixel tolerance was fixed at 2/255 before running; the negative controls returned exit code 1.

## Review follow-up (2026-10-03)

Reflection recovery: optional component query restores missing `Exposure` before
rendering; pose/visibility continue. Shared `DEFAULT_SCENE_EV100` owns baseline.
V1 tests execute five world/visual setup systems plus physics fixture startup;
V2 regression fails before fix, then passes with observer exposure and fallback.
232 engine tests pass; strict engine Clippy and formatting pass.

[Existing terrain/water fixture comparison](../../images/l1-color-fixture-comparison.png)
and [capture conditions](../../evidence/lighting-l1-color-fixture-20261003.json):
identical camera, materials, lighting, fixed water time; previous output path vs
HDR/linear reflection. Fixture has no sky; existing composition probe covers sky.
Private capture-only patch uses WebGPU features and 32 texture/sampler limits on
llvmpipe, omits production renderer gate from screenshot readiness. Both runs emit
Xvfb/Vulkan swapchain diagnostics and fail software performance gates. Images are
review evidence, not production-device or retail-parity acceptance. Capture-only
changes reverted; shipping renderer requirements unchanged.

## Tasks

id|status|task|cites
T1|x|Trace existing color/material owners and name unsupported paths|V5
T2|x|Use explicit HDR scene composition and linear reflection storage; synchronize exposure|V1,V2
T3|x|Render paired synthetic probes and record pixel evidence|V3,V4
T4|.|Integrate existing material/prepass/sampler fixes, add converted-NIF response probes|V5
T5|.|Compare both scene types against L0 references; accept declared tolerances|V3,V5

## Bugs

id|date|cause|fix
B1|2026-10-02|Non-HDR mesh shaders tone-map while sky bypasses transform|V1,V3
B2|2026-10-02|Reflection is display-mapped before water applies another transform|V2,V3
B3|2026-10-02|Xvfb launch lacked `libxkbcommon-x11` runtime path; local software Vulkan rejected optional features; baseline WebGPU limits excluded terrain bindings|Headless readback; explicit 32 texture/sampler slots; no game renderer fallback change
B4|2026-10-02|Probe used unboxed `WgpuSettings` for Bevy 0.19 `RenderCreation::Automatic`|Box settings; compile-only correction, no new invariant

B27|2026-10-03|Required reflection Exposure query silently drops camera after component removal|V2; restore missing component; regression checks pose, activation and observer/default exposure
B28|2026-10-03|Review test helper followed test module; strict Clippy rejects item order|Move helper before module; mechanical, no new invariant
