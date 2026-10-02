# NIF to glTF 2.0 / GLB Transformation Specification

This document details the technical specification for converting Bethesda NetImmerse (`.nif`) 3D mesh files into modern, GPU-ready **glTF 2.0 (`.glb`)** binary files.

---

## 1. Overview & Objectives

- **Input:** Skyrim `.nif` file (NiHeader, BSTriShape / NiTriShape, BSLightingShaderProperty).
- **Output:** Standalone glTF 2.0 binary file (`.glb`).
- **Goal:** Convert legacy proprietary 3D geometry into standard PBR-compatible glTF primitives that render zero-copy inside Bevy.

---

## 2. Block Mapping Reference Table

| Skyrim NIF Block Type                            | glTF 2.0 Equivalent     | Conversion Logic                                                                  |
| :----------------------------------------------- | :---------------------- | :-------------------------------------------------------------------------------- |
| **`NiHeader`**                                   | `asset` metadata        | Copy generator & version tags                                                     |
| **`NiNode` / `BSFadeNode`**                      | `nodes`                 | Convert local transform matrix (`translation`, `rotation` quaternion, `scale`)    |
| **`BSTriShape` / `NiTriShape`**                  | `meshes` + `primitives` | Extract vertex positions, normals, UVs, tangents, and index buffers               |
| **`BSLightingShaderProperty`**                   | `materials`             | Map Bethesda shader flags to glTF PBR Metallic Roughness properties               |
| **`BSShaderTextureSet`**                         | `textures` + `images`   | Map Skyrim texture slots (`_d.dds`, `_n.dds`, `_s.dds`) to glTF URIs/KTX2 handles |
| **`NiSkinInstance` / `BSDismemberSkinInstance`** | `skins`                 | Map bone indices (`JOINTS_0`) and vertex weights (`WEIGHTS_0`)                    |

---

## 3. Detailed Data Extraction Steps

```
┌──────────────────────┐
│  Skyrim NIF File     │
└──────────┬───────────┘
           │
           ▼ (Binary Reader / nom)
┌─────────────────────────────────────────────────────────────────────────────┐
│ 1. Extract Geometry Buffers (BSTriShape)                                    │
│    - Positions:  Vec3<f32>  ➔ glTF Accessor "POSITION"                     │
│    - UV Map:     Vec2<f32>  ➔ glTF Accessor "TEXCOORD_0"                   │
│    - Normals:    Vec3<f32>  ➔ glTF Accessor "NORMAL"                       │
│    - Tangents:   Vec4<f32>  ➔ glTF Accessor "TANGENT"                      │
│    - Indices:    u16 / u32  ➔ glTF Accessor "ELEMENT_ARRAY_BUFFER"         │
└──────────────────────────┬──────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ 2. Map Material & Textures (BSLightingShaderProperty)                       │
│    - Slot 0 (Diffuse)       ➔ baseColorTexture                             │
│    - Slot 1 (Normal Map)    ➔ normalTexture                                │
│    - Slot 2 (Subsurface/Env)➔ metallicRoughnessTexture                     │
│    - Alpha Flags            ➔ alphaMode ("OPAQUE" / "MASK" / "BLEND")      │
└──────────────────────────┬──────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ 3. Build & Write glTF 2.0 Binary (.glb)                                     │
│    - Write JSON Chunk (Nodes, Meshes, Materials, Accessors, Views)          │
│    - Write BIN Chunk  (Interleaved Vertex & Index Buffers)                  │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Material Parameter Conversion Matrix

Before glTF publication, OpenSkyrim builds a validated material contract for every reachable shape.
The contract follows the shape's explicit shader, texture-set and alpha-property block references;
block order and filename suffixes are not used to associate or classify materials. Unsupported
properties are recorded as explicit exclusions, while invalid references and non-finite values fail
conversion with the source file, shape block and shader block in the diagnostic.

| Skyrim Shader Feature    | Skyrim Flag / Value                           | glTF PBR Property                                                                                       |
| :----------------------- | :-------------------------------------------- | :------------------------------------------------------------------------------------------------------ |
| **Base Color**           | Diffuse texture (`Slot 0`) + material alpha   | `pbrMetallicRoughness.baseColorTexture` + `baseColorFactor`, interpreted by glTF as sRGB color + alpha |
| **Normal Map**           | Normal texture (`Slot 1`)                     | `normalTexture`, interpreted by glTF as linear data                                                     |
| **Roughness / Specular** | Glossiness value + specular texture (`Slot 7`)| `roughnessFactor = 1.0 - clamp(glossiness / 100.0)` + `KHR_materials_specular`                          |
| **Metallic Factor**      | No validated metalness input in the SSE IR    | Fixed to `0.0`; environment mapping is not misclassified as metalness                                   |
| **Emissive / Glow**      | Glow map (`Slot 2`) or emissive color/strength| `emissiveTexture`, `emissiveFactor` and `KHR_materials_emissive_strength`                               |
| **Two-Sided Rendering**  | `SLSF2_Double_Sided` flag                     | `doubleSided: true` only when the flag is set                                                            |
| **Alpha Transparency**   | `NiAlphaProperty` and alpha-related flags     | `alphaMode: "MASK"` with normalized threshold, or `"BLEND"`                                           |

Height/detail, environment, environment-mask, inner-layer and greyscale slots remain in the
`OPEN_SKYRIM_material` extension because core glTF has no equivalent Skyrim shader semantics.
The extension also records premultiplied-alpha and screen-door-alpha requirements. Texture URIs
always target the canonical KTX2 hierarchy; the semantic DDS-to-KTX2 encoding itself is closed by
the following conversion stage. Once that stage has published the hierarchy, the pipeline prunes
from every GLB the texture URIs whose source texture is absent from the installed game data -
including a base-color URI - together with every core or `OPEN_SKYRIM_material` slot that referenced
them, so a NIF naming a texture Bethesda never shipped still publishes and renders with its
remaining maps. The dropped paths are recorded per mesh under `pruned_texture_references` in
`conversion-manifest.json` and do not make the conversion incomplete.

---

## 5. Rust Implementation (`mesh_tools` Builder Architecture)

We use the **`mesh_tools`** crate (`GltfBuilder`), which provides an incredibly clean, ergonomic API for assembling vertices, normals, UVs, and PBR materials into binary `.glb` files.

```rust
use mesh_tools::GltfBuilder;

pub struct NifToGltfConverter;

impl NifToGltfConverter {
    /// Converts a parsed Skyrim NIF structure into a binary GLB file
    pub fn convert_and_export(nif: &SkyrimNif, output_path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = GltfBuilder::new();

        // 1. Create PBR Material
        let material = builder.add_pbr_material(
            Some("SkyrimMaterial".to_string()),
            Some([1.0, 1.0, 1.0, 1.0]), // Base Color (RGBA)
            Some(nif.material.roughness),
            Some(nif.material.metallic),
        );

        // 2. Add Mesh Primitives (Positions, Normals, UVs, Indices)
        let mesh_index = builder.add_custom_mesh(
            Some("SkyrimMesh".to_string()),
            &nif.positions, // Vec<[f32; 3]>
            &nif.normals,   // Vec<[f32; 3]>
            &nif.uvs,       // Vec<[f32; 2]>
            &nif.indices,   // Vec<u32>
            Some(material),
        );

        // 3. Create Scene Node with Transform
        let node_index = builder.add_node(
            Some("RootNode".to_string()),
            Some(mesh_index),
            Some(nif.translation), // [x, y, z]
            Some(nif.rotation),    // Quaternion [x, y, z, w]
            Some(nif.scale),       // [sx, sy, sz]
        );

        builder.add_scene(
            Some("SkyrimScene".to_string()),
            Some(vec![node_index]),
        );

        // 4. Export binary GLB directly to disk
        builder.export_glb(output_path)?;

        Ok(())
    }
}
```

## 6. Collision extras and rigid-body dynamics

Scene extras carry `openSkyrimCollision` (`shared::collision::CollisionAsset`): the authored
collision `shapes`, the `skipped` blocks, and, since version 2, a `bodies` array (#104 phase a).
A version 1 asset has no `bodies`; readers treat every shape in it as fixed and ignore body
fields they do not know.

Each `bhkCollisionObject` whose rigid body (`bhkRigidBody` or `bhkRigidBodyT`) yields shapes
produces one `CollisionBody`:

| Field | Source (byte offset in the Skyrim SE body block) | Conversion |
| --- | --- | --- |
| `node`, `target` | the collision object's target `NiNode` | glTF node index and NIF node name |
| `shapes` | the shapes extracted for this body | indices into `shapes` |
| `havok.collision_layer` | havok filter layer (4) | raw |
| `havok.motion_system`, `deactivator_type`, `quality_type` | 224, 225, 227 | raw (`hkMotionType`, `hkDeactivatorType`, `hkQualityType`) |
| `mass` | 180 | unchanged (kg) |
| `inertia` | `hkMatrix3`, three rows of four floats (116) | R I R^T for the shapes' transform, x 70^2, then the Creation-to-runtime basis |
| `center_of_mass` | `Vector4` (164) | x 70, same transform as the shapes, then the runtime basis |
| `linear_damping`, `angular_damping` | 184, 188 | unchanged |
| `friction`, `restitution` | 200, 208 | unchanged |
| `max_linear_velocity` | 212 | x 70 |
| `max_angular_velocity` | 216 | unchanged (rad/s) |

Shapes are read from the body's shape block, through `bhkMoppBvTreeShape`,
`bhkTransformShape`/`bhkConvexTransformShape` and `bhkListShape` wrappers, and map onto
`shared::collision::CollisionShape`:

| NIF block | Shape | Conversion |
| --- | --- | --- |
| `bhkCompressedMeshShape` | `Mesh` | decompressed vertices and triangles |
| `bhkNiTriStripsShape` | `Mesh` | strip data as triangles |
| `bhkBoxShape` | `Hull` | the eight transformed half-extent corners (16) |
| `bhkConvexVerticesShape` | `Hull` | vertices from 36 |
| `bhkCylinderShape` | `Hull` | two 16-point rings around the A-B axis (A @16, B @32), radius @48 |
| `bhkCapsuleShape` | `Capsule` | A @16 and B @32, radius max(Radius 1 @28, Radius 2 @44) |
| `bhkSphereShape` | `Capsule` | zero-length capsule (a = b = the shape origin), radius @4 |
| `bhkMultiSphereShape` | `Capsule` per sphere | count @16 (1..=8), `NiBound {centre, radius}` from @20 |

Points are Havok units scaled by 70 before the Creation-to-runtime basis; radii are scaled by
70 and the transform scale, like the capsule arm. A sphere or multi-sphere under a non-uniform
or sheared transform has no single radius and is skipped. A degenerate cylinder (A = B, radius
below or equal to zero, non-finite), an out-of-range multi-sphere count and any malformed or
unsupported shape land in `skipped` instead of becoming collision.

Only bodies on physical Skyrim layers are read (`SkyrimLayer` in nif.xml): 0 UNIDENTIFIED,
1 STATIC, 2 ANIMSTATIC, 3 TRANSPARENT, 4 CLUTTER, 5 WEAPON, 9 TREES, 10 PROPS, 13 TERRAIN,
17 GROUND, 26 TRANSPARENT_SMALL, 27 INVISIBLE_WALL, 28 TRANSPARENT_SMALL_ANIM, 31 STAIRHELPER
and 35 COLLISIONBOX. Layer 15 NONCOLLIDABLE (harvestable flora and other nonphysical objects)
is dropped silently; layer 12 TRIGGER is skipped with the reason "trigger volume (layer 12) is
not physical"; every other layer is reported as unsupported.

`kind` is `dynamic` when the motion system is a simulated one (dynamic, sphere or box inertia,
plain or stabilized, thin box: 1, 2, 3, 4, 5, 8), the quality type is a moving one (debris,
moving, critical, bullet: 3, 4, 5, 6) and the mass is finite and above zero; `keyframed` for
`MO_SYS_KEYFRAMED` (6) with mass 0; otherwise `fixed`. `convex` is true when every shape of the
body is a box, capsule or hull (a compressed or strip mesh makes it false).

The `node` index and `target` name are checked against the GLB that is actually written, not the
source NIF's static node list (skeletal and effect NIFs lay their nodes out differently): a body
is kept only when `target` is non-empty and the GLB's node at `node` has that name. A name
used by more than one node is trusted at its index only when the GLB's nodes start in the
same order as the NIF's static scene, which the index was predicted from; otherwise, and
always when the collision is added to a GLB built elsewhere (`annotate`), the name must be
unique.

A body on an unsupported layer, a trigger, a non-colliding body (a `HavokFilter` with the
"No Collision" flag, or a collision response of 2 RESPONSE_REPORTING or 3 RESPONSE_NONE, in
either of the two copies a body stores) or a body whose target node cannot be resolved is listed
in `skipped` and contributes no collision. A body whose shapes cannot all be read is listed in
`skipped`; the shapes it did read stay as fixed collision. A body whose dynamics cannot be read,
or whose glTF node fails the check above, is listed in `skipped` but its shapes stay as fixed
collision.

A cylinder's 16-point rings are inscribed in the true circle (about 2% inside at the chord
midpoints); like the box and hull arms, the Havok convex radius (a thin shell) is not added.

There is no converter schema bump: conversions made before this change keep their cached GLBs,
which have no `bodies`, until they are reconverted.
