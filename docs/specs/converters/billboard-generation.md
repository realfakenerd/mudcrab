# Billboard generation (TexGen equivalent)

Renders billboards and LOD textures from converted assets. Depends on the
GLB/KTX2 pipeline, never on generated outputs (TEX-08).

## Pipeline

Resolve full model and materials, apply texture-set substitutions per
reference (TEX-01: shared geometry with different substitutions may need
different products), choose a recipe (stitched object texture, rendered
object texture, tree render, grass render), compute bounds and framing,
render linear intermediates, generate normals and transparency metadata,
apply output brightness/material conversion, build a coverage-aware mip
chain, compress, and store dimensions, pivot, hashes, recipe, and
dependencies.

## Framing and metadata

Deterministic framing and lighting only: no editor camera state, random
wind phase, or environment exposure in a build (TEX-03). Store crop
rectangles, projection, physical dimensions, and pivot explicitly; a
cropped billboard stays planted at the source base (TEX-02). Resolution
may scale with object dimensions and reference scale inside a configured
cap; rectangular and power-of-two policies follow backend need (TEX-05).

## Alpha and mips

Silhouette coverage must survive minification: transparent-edge color
dilation and atlas gutters (TEX-06). The runtime cutout default is 128
(`material.rs`), matching the binary-alpha convention. ADR-0009's vertex
alpha normalization exists because distant cutout foliage collapsed; this
pipeline is the long-term home for that fix, scoped by material semantics
instead of applied converter-wide. Diffuse and normal atlas transforms
stay synchronized; normals never take diffuse color-space conversion
(MAT-05). Contact sheets compare full model vs billboard across front,
side, scale, silhouette, and lighting (TEX-07).

## Trees

Object-route billboards first: all-billboard tree LOD is a valid low-cost
configuration (TREE-03). Tilted, fallen, and rotated trees keep their
transforms (TREE-04); 3D or hybrid crowns come only after
signature-matched assets exist, with fallback reported rather than
implied (TREE-05). Validate silhouette, trunk anchoring, crown lighting,
and transitions independently (TREE-06).
