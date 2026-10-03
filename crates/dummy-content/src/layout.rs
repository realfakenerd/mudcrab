//! Synthetic Skyrim `Data/` directory layouts.

use crate::{Entry, ba2, bsa, dds, esm, nif, pex, rng::Rng};
use color_eyre::{
    Result,
    eyre::{WrapErr, bail, ensure, eyre},
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Seed used when a caller does not provide one.
pub const DEFAULT_SEED: u64 = 0x5EED_5EED;

/// `TES4` author of the generated plugin, shared by [`generate`], the
/// `dummy-content gen --with-interior` preset and the tests that read them back.
pub const GENERATED_AUTHOR: &str = "OpenSkyrim dummy-content";
/// Editor id of the generated worldspace.
pub const GENERATED_WORLDSPACE: &str = "GeneratedWorld";
/// `MODL` model path of the generated static, and the model both doors of
/// [`esm::PRESET_INTERIOR`] name. It is the only mesh [`generate`] writes, so
/// replacing it means writing the mesh too.
pub const GENERATED_MODEL_PATH: &str = "meshes/generated.nif";
/// Diffuse texture path the generated texture set references.
pub const GENERATED_DIFFUSE_PATH: &str = "textures/generated_color.dds";
/// Normal texture path the generated texture set references.
pub const GENERATED_NORMAL_PATH: &str = "textures/generated_normal.dds";

/// Name of the plugin [`generate`] and [`write_plugin`] publish.
const PLUGIN_FILE_NAME: &str = "Skyrim.esm";

const QUAD_POSITIONS: [[f32; 3]; 4] = [
    [-1.0, -1.0, 0.0],
    [1.0, -1.0, 0.0],
    [1.0, 1.0, 0.0],
    [-1.0, 1.0, 0.0],
];
const QUAD_NORMALS: [[f32; 3]; 4] = [[0.0, 0.0, 1.0]; 4];
const QUAD_UVS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
const QUAD_INDICES: [[u16; 3]; 2] = [[0, 1, 2], [0, 2, 3]];
const ESM_CELLS: [esm::Cell; 9] = [
    esm::Cell {
        grid_x: -1,
        grid_y: -1,
    },
    esm::Cell {
        grid_x: 0,
        grid_y: -1,
    },
    esm::Cell {
        grid_x: 1,
        grid_y: -1,
    },
    esm::Cell {
        grid_x: -1,
        grid_y: 0,
    },
    esm::Cell {
        grid_x: 0,
        grid_y: 0,
    },
    esm::Cell {
        grid_x: 1,
        grid_y: 0,
    },
    esm::Cell {
        grid_x: -1,
        grid_y: 1,
    },
    esm::Cell {
        grid_x: 0,
        grid_y: 1,
    },
    esm::Cell {
        grid_x: 1,
        grid_y: 1,
    },
];

/// File families emitted by [`generate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Formats {
    /// Loose `textures/*.dds` files.
    pub dds: bool,
    /// Loose `scripts/*.pex` files.
    pub pex: bool,
    /// Loose `meshes/*.nif` files.
    pub nif: bool,
    /// `Skyrim - Misc.bsa` and `Skyrim - Meshes.bsa` archives.
    pub bsa: bool,
    /// A `Skyrim - Textures.ba2` archive containing the textures.
    pub ba2: bool,
    /// A `Skyrim.esm` plugin with a generated worldspace.
    pub esm: bool,
    /// `lodsettings/GeneratedWorld.lod` origin/grid sidecar; requires `esm`.
    pub lodsettings: bool,
}

impl Default for Formats {
    fn default() -> Self {
        Self::all()
    }
}

impl Formats {
    /// Enables every format.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            dds: true,
            pex: true,
            nif: true,
            bsa: true,
            ba2: true,
            esm: true,
            lodsettings: true,
        }
    }

    /// Parses a comma-separated list such as `dds,pex,nif,bsa,ba2,esm,lodsettings`.
    pub fn parse(value: &str) -> Result<Self> {
        let mut formats = Self {
            dds: false,
            pex: false,
            nif: false,
            bsa: false,
            ba2: false,
            esm: false,
            lodsettings: false,
        };
        for name in value.split(',') {
            match name.trim() {
                "dds" => formats.dds = true,
                "pex" => formats.pex = true,
                "nif" => formats.nif = true,
                "bsa" => formats.bsa = true,
                "ba2" => formats.ba2 = true,
                "esm" => formats.esm = true,
                "lodsettings" => formats.lodsettings = true,
                "" => bail!("empty format name in {value:?}"),
                other => {
                    bail!(
                        "unknown format {other:?}; expected dds, pex, nif, bsa, ba2, esm or lodsettings"
                    )
                }
            }
        }
        ensure!(
            formats.dds
                || formats.pex
                || formats.nif
                || formats.bsa
                || formats.ba2
                || formats.esm
                || formats.lodsettings,
            "no output formats selected"
        );
        ensure!(
            !formats.lodsettings || formats.esm,
            "lodsettings format requires esm"
        );
        Ok(formats)
    }
}

/// Ensures `root` exists and is empty unless `force` is set.
///
/// With `force`, existing generated files are replaced but unrelated files in
/// the directory are left untouched.
pub fn prepare_directory(root: &Path, force: bool) -> Result<()> {
    if root.exists() {
        ensure!(
            root.is_dir(),
            "output path is not a directory: {}",
            root.display()
        );
        let mut entries =
            fs::read_dir(root).wrap_err_with(|| format!("failed to read {}", root.display()))?;
        if entries.next().is_some() && !force {
            bail!(
                "output directory {} is not empty; pass --force to overwrite generated files",
                root.display()
            );
        }
    } else {
        fs::create_dir_all(root)
            .wrap_err_with(|| format!("failed to create {}", root.display()))?;
    }
    Ok(())
}

/// Generates a synthetic `Data` tree and returns every written path.
///
/// The tree contains loose scripts, textures and meshes, the
/// `Skyrim - Misc.bsa`, `Skyrim - Meshes.bsa` and `Skyrim - Textures.ba2`
/// archives, and a `Skyrim.esm` plugin with a generated worldspace, filtered
/// by `formats`. Output bytes are fully determined by `seed`.
pub fn generate(root: &Path, seed: u64, formats: Formats) -> Result<Vec<PathBuf>> {
    ensure!(
        !formats.lodsettings || formats.esm,
        "lodsettings format requires esm"
    );
    let mut rng = Rng::new(seed);
    let scripts = [
        ("scripts/generated.pex", pex::minimal("Generated")?),
        ("scripts/second.pex", pex::minimal("Second")?),
    ];
    let textures = [
        (
            "textures/generated_color.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 64, 64).with_mip_levels(7),
                &mut rng,
            )?,
        ),
        (
            "textures/generated_normal.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc5Unorm, 64, 64).with_mip_levels(7),
                &mut rng,
            )?,
        ),
        (
            "textures/generated_color_x8.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::X8R8G8B8, 32, 32).with_mip_levels(6),
                &mut rng,
            )?,
        ),
        (
            "textures/generated_cube.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 32, 32)
                    .with_mip_levels(6)
                    .as_cubemap(),
                &mut rng,
            )?,
        ),
        (
            "textures/generated_volume.dds",
            dds::generate(
                &dds::Spec::new(dds::Format::Bc1Unorm, 16, 16)
                    .with_depth(16)
                    .with_mip_levels(5),
                &mut rng,
            )?,
        ),
    ];
    let meshes = [("meshes/generated.nif", generated_mesh()?)];
    let plugin = formats.esm.then(generated_plugin).transpose()?;

    let mut written = Vec::new();
    if formats.pex {
        for (name, bytes) in &scripts {
            written.push(write_file(root, name, bytes)?);
        }
    }
    if formats.dds {
        for (name, bytes) in &textures {
            written.push(write_file(root, name, bytes)?);
        }
    }
    if formats.nif {
        for (name, bytes) in &meshes {
            written.push(write_file(root, name, bytes)?);
        }
    }
    if formats.bsa {
        let entries: Vec<Entry<'_>> = scripts
            .iter()
            .map(|(name, bytes)| Entry::new(name, bytes))
            .collect();
        let archive = bsa::v105(&entries, bsa::Compression::Zlib)?;
        written.push(write_file(root, "Skyrim - Misc.bsa", &archive)?);

        let entries: Vec<Entry<'_>> = meshes
            .iter()
            .map(|(name, bytes)| Entry::new(name, bytes))
            .collect();
        let archive = bsa::v105(&entries, bsa::Compression::Zlib)?;
        written.push(write_file(root, "Skyrim - Meshes.bsa", &archive)?);
    }
    if formats.ba2 {
        let entries: Vec<Entry<'_>> = textures
            .iter()
            .map(|(name, bytes)| Entry::new(name, bytes))
            .collect();
        let archive = ba2::general(&entries, ba2::Compression::Zlib)?;
        written.push(write_file(root, "Skyrim - Textures.ba2", &archive)?);
    }
    if let Some(bytes) = &plugin {
        written.push(write_plugin(root, bytes)?);
    }
    // The generated worldspace spans cells -1..=1 on both axes; its LOD
    // origin sits at the southwest corner with room to grow, the way a real
    // sidecar's grid covers its world's cells.
    if formats.lodsettings && formats.esm {
        written.push(write_file(
            root,
            &format!("lodsettings/{GENERATED_WORLDSPACE}.lod"),
            &lodsettings_file(-4, -4, 32, 4, 32),
        )?);
    }
    Ok(written)
}

/// Skyrim's two `i16` origins and three `i32` grid fields (`xEdit wbLOD.pas`).
fn lodsettings_file(x: i16, y: i16, stride: i32, min_level: i32, max_level: i32) -> Vec<u8> {
    x.to_le_bytes()
        .into_iter()
        .chain(y.to_le_bytes())
        .chain(
            [stride, min_level, max_level]
                .into_iter()
                .flat_map(i32::to_le_bytes),
        )
        .collect()
}

/// Writes `Skyrim.esm` under `root` and returns the path it published.
///
/// This is the write path [`generate`] publishes its own plugin through, so a
/// plugin a caller writes obeys the same fixture policy: the write is refused
/// when any component below `root` is a symlink (ADR-0007) and the file is
/// published atomically, so a failed or interrupted write never replaces a
/// previous `Skyrim.esm` with a partial one. `dummy-content gen --with-interior`
/// uses it to replace the default plugin.
pub fn write_plugin(root: &Path, bytes: &[u8]) -> Result<PathBuf> {
    write_file(root, PLUGIN_FILE_NAME, bytes)
}

fn generated_plugin() -> Result<Vec<u8>> {
    esm::plugin(&esm::Plugin {
        author: GENERATED_AUTHOR,
        worldspace: GENERATED_WORLDSPACE,
        cells: &ESM_CELLS,
        model_path: GENERATED_MODEL_PATH,
        diffuse: GENERATED_DIFFUSE_PATH,
        normal_texture: GENERATED_NORMAL_PATH,
    })
}

fn generated_mesh() -> Result<Vec<u8>> {
    nif::static_shape(&nif::StaticShape {
        name: "GeneratedQuad",
        positions: &QUAD_POSITIONS,
        normals: &QUAD_NORMALS,
        uvs: &QUAD_UVS,
        indices: &QUAD_INDICES,
        diffuse: "textures/generated_color.dds",
        normal_texture: "textures/generated_normal.dds",
    })
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) -> Result<PathBuf> {
    ensure_no_symlink_components(root, relative)?;
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("invalid fixture path {}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let temporary = path.with_file_name(format!("{file_name}.{}.partial", std::process::id()));
    let backup = path.with_file_name(format!("{file_name}.{}.backup", std::process::id()));
    ensure!(
        !temporary.exists() && !backup.exists(),
        "stale fixture temporary exists for {}",
        path.display()
    );

    let mut file = fs::File::create(&temporary)
        .wrap_err_with(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if path.exists() {
        fs::rename(&path, &backup)
            .wrap_err_with(|| format!("failed to preserve {}", path.display()))?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if backup.exists() {
            let _ = fs::rename(&backup, &path);
        }
        return Err(error).wrap_err_with(|| format!("failed to publish {}", path.display()));
    }
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    Ok(path)
}

/// Rejects any symlinked component below `root` so generation can never
/// escape the target directory through a planted link.
fn ensure_no_symlink_components(root: &Path, relative: &str) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing to write through symlink {}", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .wrap_err_with(|| format!("failed to inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_FILES: [&str; 13] = [
        "scripts/generated.pex",
        "scripts/second.pex",
        "textures/generated_color.dds",
        "textures/generated_normal.dds",
        "textures/generated_color_x8.dds",
        "textures/generated_cube.dds",
        "textures/generated_volume.dds",
        "meshes/generated.nif",
        "Skyrim - Misc.bsa",
        "Skyrim - Meshes.bsa",
        "Skyrim - Textures.ba2",
        "Skyrim.esm",
        "lodsettings/GeneratedWorld.lod",
    ];

    #[test]
    fn parses_format_lists() {
        assert_eq!(
            Formats::parse("dds").unwrap(),
            Formats {
                dds: true,
                pex: false,
                nif: false,
                bsa: false,
                ba2: false,
                esm: false,
                lodsettings: false,
            }
        );
        assert_eq!(
            Formats::parse("dds, pex,nif,bsa ,ba2,esm,lodsettings").unwrap(),
            Formats::all()
        );
        for value in ["", "dds,", "foo", "dds,foo", "lodsettings"] {
            assert!(Formats::parse(value).is_err(), "{value:?} was accepted");
        }
    }

    #[test]
    fn test_v1_lodsettings_selector_requires_generated_worldspace() {
        let temp = tempfile::tempdir().unwrap();
        let mut formats = Formats::all();
        formats.esm = false;
        let error = generate(&temp.path().join("Data"), DEFAULT_SEED, formats).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("lodsettings format requires esm")
        );
    }

    #[test]
    fn refuses_non_empty_directories_without_force() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        prepare_directory(&root, false).unwrap();
        assert!(prepare_directory(&root, false).is_ok());
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        assert!(prepare_directory(&root, false).is_err());
        prepare_directory(&root, true).unwrap();
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn generates_the_expected_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        prepare_directory(&root, false).unwrap();
        let written = generate(&root, DEFAULT_SEED, Formats::all()).unwrap();
        assert_eq!(written.len(), EXPECTED_FILES.len());
        for relative in EXPECTED_FILES {
            assert!(root.join(relative).is_file(), "missing {relative}");
        }
        assert_eq!(
            fs::read(root.join("lodsettings/GeneratedWorld.lod")).unwrap(),
            vec![0xfc, 0xff, 0xfc, 0xff, 32, 0, 0, 0, 4, 0, 0, 0, 32, 0, 0, 0]
        );
    }

    #[test]
    fn generation_is_deterministic_per_seed() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_root = first.path().join("Data");
        let second_root = second.path().join("Data");
        generate(&first_root, DEFAULT_SEED, Formats::all()).unwrap();
        generate(&second_root, DEFAULT_SEED, Formats::all()).unwrap();
        for relative in EXPECTED_FILES {
            assert_eq!(
                fs::read(first_root.join(relative)).unwrap(),
                fs::read(second_root.join(relative)).unwrap(),
                "{relative} differs for the same seed"
            );
        }

        let other = tempfile::tempdir().unwrap();
        let other_root = other.path().join("Data");
        generate(&other_root, DEFAULT_SEED + 1, Formats::all()).unwrap();
        assert_ne!(
            fs::read(first_root.join("textures/generated_color.dds")).unwrap(),
            fs::read(other_root.join("textures/generated_color.dds")).unwrap(),
            "a different seed produced identical textures"
        );
        assert_eq!(
            fs::read(first_root.join("scripts/generated.pex")).unwrap(),
            fs::read(other_root.join("scripts/generated.pex")).unwrap(),
            "script bytes must not depend on the seed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn refuses_to_write_through_symlinked_directories() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("scripts")).unwrap();

        let error = generate(&root, DEFAULT_SEED, Formats::all()).unwrap_err();
        assert!(
            error.to_string().contains("symlink"),
            "unexpected error: {error:#}"
        );
        assert!(
            fs::read_dir(outside.path()).unwrap().next().is_none(),
            "generation wrote through the symlink"
        );
    }

    #[test]
    #[cfg(unix)]
    fn refuses_to_write_the_plugin_through_a_symlinked_file() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        fs::create_dir_all(&root).unwrap();
        let target = outside.path().join("Skyrim.esm");
        fs::write(&target, b"outside").unwrap();
        std::os::unix::fs::symlink(&target, root.join("Skyrim.esm")).unwrap();

        let error = write_plugin(&root, b"plugin").unwrap_err();
        assert!(
            error.to_string().contains("symlink"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"outside",
            "the plugin was written through the symlink"
        );
    }

    /// The plugin goes through the same writer as every other generated file,
    /// so the policy can be observed without a symlink: the writer refuses a
    /// stale temporary left behind by an interrupted run, where a plain
    /// `fs::write` would ignore it and publish the plugin anyway.
    #[test]
    fn plugin_writes_go_through_the_fixture_write_policy() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        fs::create_dir_all(&root).unwrap();
        let stale = root.join(format!("Skyrim.esm.{}.partial", std::process::id()));
        fs::write(&stale, b"stale").unwrap();

        let error = write_plugin(&root, b"plugin").unwrap_err();
        assert!(
            error.to_string().contains("stale fixture temporary"),
            "unexpected error: {error:#}"
        );
        assert!(
            !root.join("Skyrim.esm").exists(),
            "the plugin was written despite the stale temporary"
        );
    }

    #[test]
    fn publishes_the_plugin_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        fs::create_dir_all(&root).unwrap();
        let path = write_plugin(&root, b"first").unwrap();
        assert_eq!(path, root.join(PLUGIN_FILE_NAME));
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_plugin(&root, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [PLUGIN_FILE_NAME],
            "a write left a temporary or backup behind"
        );
    }

    #[test]
    fn formats_filter_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Data");
        let formats = Formats {
            dds: false,
            pex: true,
            nif: false,
            bsa: true,
            ba2: false,
            esm: false,
            lodsettings: false,
        };
        let written = generate(&root, DEFAULT_SEED, formats).unwrap();
        assert_eq!(written.len(), 4);
        assert!(root.join("scripts/generated.pex").is_file());
        assert!(root.join("Skyrim - Misc.bsa").is_file());
        assert!(root.join("Skyrim - Meshes.bsa").is_file());
        assert!(!root.join("meshes/generated.nif").exists());
        assert!(!root.join("textures").exists());
        assert!(!root.join("Skyrim - Textures.ba2").exists());
    }
}
