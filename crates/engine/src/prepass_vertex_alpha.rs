//! Vertex-colour alpha in the depth prepass's alpha test.
//!
//! Bevy 0.19's main pass multiplies a mesh's `COLOR_0` into the base colour before an
//! `AlphaMode::Mask` material tests its alpha against the cutoff (`pbr_fragment.wgsl`), but the
//! prepass tests only the material colour and its texture (`prepass_alpha_discard` in
//! `pbr_prepass_functions.wgsl`). A masked shape whose vertex alpha fades it out therefore writes
//! depth in the prepass where the main pass later discards the fragment, and whatever lies behind
//! it fails the depth test: the clear colour shows through in the fragment's place. The shadow
//! passes use the same function, so the same fragments cast shadows they do not draw.
//!
//! Skyrim's alpha-tested shapes fade this way wherever their shader reads vertex alpha: the gravel
//! skirts around rocks and the moss and plaster decals on walls. The converter currently forces
//! vertex alpha to opaque on alpha-tested shapes, so its own output does not reach this yet; an
//! asset set that keeps the fade does. So the prepass is made to test the same alpha as the main
//! pass: one line is inserted into Bevy's shader library when it loads
//! ([`patch_prepass_functions`]). If a Bevy upgrade moves the anchor, the patch logs an error and
//! the prepass keeps Bevy's test.

use bevy::{prelude::*, shader::Source};

/// Where Bevy 0.19 embeds the shader library that holds `prepass_alpha_discard()`.
pub const PREPASS_FUNCTIONS_PATH: &str = "embedded://bevy_pbr/render/pbr_prepass_functions.wgsl";

/// The line of `prepass_alpha_discard()` in Bevy 0.19.0 that follows the base colour's texture
/// sample and precedes the alpha test. The vertex colour is multiplied in just before it.
pub const ANCHOR_LINE: &str =
    "let alpha_mode = flags & pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_RESERVED_BITS;";

/// The multiply that makes the prepass test the alpha the main pass tests.
pub const VERTEX_COLOUR_MULTIPLY: &str = "output_color = output_color * in.color;";

/// The lines inserted on their own before the anchor's line, so `#ifdef` and `#endif` sit at
/// column 0 like Bevy's own directives; [`patched_source`] puts the anchor line back with its
/// original indent. The prepass's `VertexOutput` carries `color` exactly when the mesh has
/// `COLOR_0` (`VERTEX_COLORS`).
pub const VERTEX_COLOUR_LINES: &str = "#ifdef VERTEX_COLORS
    output_color = output_color * in.color; // test the alpha the main pass tests
#endif
";

/// How many frames the plugin waits for Bevy's prepass functions before it stops trying.
const LOOKUP_FRAMES: u32 = 600;

/// The shader source with the vertex colour multiplied in, or `None` when the anchor is not there
/// or the source is already patched.
pub fn patched_source(source: &str) -> Option<String> {
    if source.contains(VERTEX_COLOUR_LINES) {
        return None;
    }
    let anchor = source.find(ANCHOR_LINE)?;
    let line_start = source[..anchor]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let indent = &source[line_start..anchor];
    if !indent.trim().is_empty() {
        return None;
    }
    Some(format!(
        "{}{VERTEX_COLOUR_LINES}{indent}{}",
        &source[..line_start],
        &source[anchor..]
    ))
}

/// What [`patch_shader`] did, or why it left Bevy's own alpha test in place.
#[derive(Debug, PartialEq, Eq)]
enum PatchOutcome {
    /// The vertex-colour multiply was inserted.
    Patched,
    /// The shader was already patched.
    AlreadyPatched,
    /// The anchor is gone: a Bevy upgrade moved or rewrote the alpha test.
    AnchorMissing,
    /// The asset behind the handle is not a WGSL shader.
    NotWgsl,
    /// Nothing is loaded behind the handle yet.
    NotLoaded,
    /// The loaded asset could not be edited in place.
    MutateFailed,
}

/// Patches the loaded shader behind `handle` in place. Changing the asset keeps its id and import
/// path, so every prepass and shadow pipeline that imports it recompiles on the `Modified` event.
fn patch_shader(shaders: &mut Assets<Shader>, handle: &Handle<Shader>) -> PatchOutcome {
    let source = match shaders.get(handle).map(|shader| &shader.source) {
        Some(Source::Wgsl(source)) => source.clone(),
        Some(_) => return PatchOutcome::NotWgsl,
        None => return PatchOutcome::NotLoaded,
    };
    match patched_source(&source) {
        Some(patched) => {
            let Some(mut shader) = shaders.get_mut(handle) else {
                return PatchOutcome::MutateFailed;
            };
            shader.source = Source::Wgsl(patched.into());
            PatchOutcome::Patched
        }
        None if source.contains(VERTEX_COLOUR_LINES) => PatchOutcome::AlreadyPatched,
        None => PatchOutcome::AnchorMissing,
    }
}

pub struct PrepassVertexAlphaPlugin;

impl Plugin for PrepassVertexAlphaPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, patch_prepass_functions);
    }
}

/// Patches the prepass library once it is loaded, and stops after [`LOOKUP_FRAMES`] frames so a
/// renamed embedded path cannot leave it retrying unnoticed forever.
fn patch_prepass_functions(
    asset_server: Res<AssetServer>,
    mut shaders: ResMut<Assets<Shader>>,
    mut done: Local<bool>,
    mut frames_without_source: Local<u32>,
) {
    if *done {
        return;
    }
    let handle: Handle<Shader> = asset_server.load(PREPASS_FUNCTIONS_PATH);
    match patch_shader(&mut shaders, &handle) {
        PatchOutcome::Patched => {
            *done = true;
            info!("the depth prepass tests vertex-colour alpha like the main pass");
        }
        PatchOutcome::AlreadyPatched => *done = true,
        PatchOutcome::NotWgsl => {
            *done = true;
            error!(
                path = PREPASS_FUNCTIONS_PATH,
                "Bevy's prepass functions are not WGSL; masked shapes keep Bevy's prepass alpha test"
            );
        }
        PatchOutcome::AnchorMissing => {
            *done = true;
            error!(
                path = PREPASS_FUNCTIONS_PATH,
                "the prepass alpha test's anchor was not found (a Bevy upgrade?); masked shapes keep Bevy's prepass alpha test"
            );
        }
        PatchOutcome::MutateFailed => {
            *done = true;
            error!(
                path = PREPASS_FUNCTIONS_PATH,
                "Bevy's prepass functions could not be edited in place; masked shapes keep Bevy's prepass alpha test"
            );
        }
        PatchOutcome::NotLoaded => {
            *frames_without_source += 1;
            if *frames_without_source >= LOOKUP_FRAMES {
                *done = true;
                error!(
                    path = PREPASS_FUNCTIONS_PATH,
                    "Bevy's prepass functions were not loaded after {LOOKUP_FRAMES} frames (a renamed embedded path?); masked shapes keep Bevy's prepass alpha test"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// The Bevy version the anchor was written against.
    const PATCHED_BEVY_PBR: &str = "0.19.0";

    /// The `bevy_pbr` version `Cargo.lock` pins.
    fn locked_bevy_pbr_version() -> String {
        let lock = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"),
        )
        .expect("the workspace has a Cargo.lock");
        let entry = lock
            .split("[[package]]")
            .find(|entry| {
                entry
                    .lines()
                    .any(|line| line.trim() == "name = \"bevy_pbr\"")
            })
            .expect("Cargo.lock pins bevy_pbr");
        entry
            .lines()
            .find_map(|line| line.trim().strip_prefix("version = \""))
            .and_then(|version| version.strip_suffix('"'))
            .expect("the bevy_pbr entry has a version")
            .to_owned()
    }

    /// The unpinned `bevy_pbr` source directory in the local cargo registry, or `None` when there
    /// is none: a vendored build, or a machine without `CARGO_HOME`/`USERPROFILE`/`HOME`.
    fn bevy_pbr_source_dir(version: &str) -> Option<PathBuf> {
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .or_else(|| std::env::var_os("HOME"))
                    .map(|home| Path::new(&home).join(".cargo"))
            })?;
        // Low risk, documented: a git- or patch-pinned bevy_pbr of the same version number could
        // differ from this registry copy, so the check might read the wrong source.
        let registry = cargo_home.join("registry").join("src");
        std::fs::read_dir(registry)
            .ok()?
            .filter_map(Result::ok)
            .map(|index| index.path().join(format!("bevy_pbr-{version}")))
            .find(|dir| dir.join("src/render/pbr_prepass_functions.wgsl").is_file())
    }

    fn registry_source(dir: &Path, relative: &str) -> Option<String> {
        let path = dir.join(relative);
        match std::fs::read_to_string(&path) {
            Ok(source) => Some(source),
            Err(error) => {
                eprintln!("{} is unreadable ({error}); skipped", path.display());
                None
            }
        }
    }

    /// A miniature `prepass_alpha_discard()` with the same shape the patch relies on.
    fn synthetic_prepass_functions() -> String {
        format!(
            "fn prepass_alpha_discard(in: VertexOutput) {{\n    \
             var output_color: vec4<f32> = vec4<f32>(1.0);\n    \
             output_color = output_color * textureSampleBias(base_color_texture, base_color_sampler, uv, 0.0);\n    \
             {ANCHOR_LINE}\n}}\n"
        )
    }

    /// The patch edits the loaded `Shader` asset, so pipelines that import it recompile, and it
    /// leaves an already-patched asset alone.
    #[test]
    fn the_patch_replaces_the_shader_asset() {
        let mut shaders = Assets::<Shader>::default();
        let handle = shaders.add(Shader::from_wgsl(
            synthetic_prepass_functions(),
            "pbr_prepass_functions.wgsl",
        ));
        assert_eq!(patch_shader(&mut shaders, &handle), PatchOutcome::Patched);
        let Source::Wgsl(source) = &shaders.get(&handle).expect("the asset is there").source else {
            panic!("the patched shader is still WGSL");
        };
        assert!(source.contains(VERTEX_COLOUR_MULTIPLY));
        assert!(
            source.contains(&format!("\n{VERTEX_COLOUR_LINES}    {ANCHOR_LINE}\n")),
            "the block sits at column 0 and the anchor keeps its indent: {source:?}"
        );
        assert_eq!(
            patch_shader(&mut shaders, &handle),
            PatchOutcome::AlreadyPatched
        );
    }

    /// A Bevy upgrade fails here, not silently at runtime: the pinned version must be the one the
    /// anchor was written for, that version's shader must contain the anchor, and its
    /// `VertexOutput` must carry the `color` and `output_color` the inserted line needs. The check
    /// is skipped, with a note, when the cargo registry has no sources (a vendored build) or the
    /// files the anchor and the inserted line depend on are not readable.
    #[test]
    fn the_anchor_is_in_bevys_prepass_functions() {
        let version = locked_bevy_pbr_version();
        assert_eq!(
            version, PATCHED_BEVY_PBR,
            "bevy_pbr {version} is locked: re-check the prepass anchor against its \
             pbr_prepass_functions.wgsl, then update PATCHED_BEVY_PBR"
        );
        let Some(dir) = bevy_pbr_source_dir(&version) else {
            eprintln!(
                "no bevy_pbr {version} source in the cargo registry (a vendored build, or no \
                 CARGO_HOME/USERPROFILE/HOME); skipped"
            );
            return;
        };
        let Some(functions) = registry_source(&dir, "src/render/pbr_prepass_functions.wgsl") else {
            return;
        };
        let prepass_io = registry_source(&dir, "src/prepass/prepass_io.wgsl");

        assert!(
            functions.contains("var output_color: vec4<f32>"),
            "the inserted line multiplies a vec4 into output_color"
        );
        let patched =
            patched_source(&functions).expect("the anchor is in Bevy's prepass functions");
        let discard = patched
            .find("fn prepass_alpha_discard")
            .expect("prepass_alpha_discard exists");
        let texture = patched
            .find("output_color = output_color * textureSampleBias(")
            .expect("the base colour texture is sampled into output_color");
        let multiply = patched
            .find(VERTEX_COLOUR_MULTIPLY)
            .expect("the multiply was inserted");
        let test = patched.find(ANCHOR_LINE).expect("the anchor is kept");
        assert!(
            discard < texture && texture < multiply && multiply < test,
            "the multiply comes after the texture sample and before the alpha test"
        );
        assert!(
            patched.contains(&format!("\n{VERTEX_COLOUR_LINES}    {ANCHOR_LINE}\n")),
            "the block sits at column 0 and the anchor keeps its indent"
        );
        assert!(
            patched_source(&patched).is_none(),
            "a patched shader is not patched twice"
        );

        let Some(prepass_io) = prepass_io else {
            return;
        };

        let vertex_output = prepass_io
            .split("struct VertexOutput")
            .nth(1)
            .expect("VertexOutput exists");
        let body = vertex_output
            .split("\n}")
            .next()
            .expect("the struct has a body");
        let mut fields = body
            .lines()
            .skip_while(|line| line.trim() != "#ifdef VERTEX_COLORS");
        assert_eq!(
            fields.next().map(str::trim),
            Some("#ifdef VERTEX_COLORS"),
            "VertexOutput has a VERTEX_COLORS-gated field"
        );
        let field = fields.next().expect("the VERTEX_COLORS field is declared");
        assert!(
            field.contains("color:") && field.contains("vec4<f32>"),
            "VertexOutput's VERTEX_COLORS field is `color: vec4<f32>`, found {field:?}"
        );
    }

    #[test]
    fn a_source_without_the_anchor_is_left_alone() {
        assert!(patched_source("fn prepass_alpha_discard(in: VertexOutput) {}").is_none());
    }
}
