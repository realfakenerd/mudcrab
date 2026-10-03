//! Readers for Skyrim's `lodsettings/<WorldspaceEDID>.lod` sidecars.
//!
//! Each file is exactly 16 bytes: little-endian `i16` X/Y origins followed
//! by `i32` stride, minimum level, and maximum level (`xEdit wbLOD.pas`).
//! The origins anchor every native LOD tier (GEOM-02). The remaining fields
//! describe Skyrim's grid; they are not a rectangular width/height and do
//! not restrict the native compiler's source-cell coverage or tier policy.
//! A file that is missing, truncated, or overlong is not an origin:
//! the caller skips that world's LOD with an actionable error rather than
//! assuming zero.

use color_eyre::{Result, eyre::WrapErr};
use shared::lod::LodOrigin;
use std::path::Path;

/// A parsed Skyrim `.lod` sidecar. Native tiers remain independent of its levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LodSettings {
    pub origin: LodOrigin,
    pub stride: i32,
    pub min_level: i32,
    pub max_level: i32,
}

impl LodSettings {
    /// Reads and validates one sidecar file. The path identifies the file in
    /// error messages, so a skip-the-world error can name it.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        Self::parse(&bytes).wrap_err_with(|| format!("invalid LOD settings {}", path.display()))
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        color_eyre::eyre::ensure!(
            bytes.len() == 16,
            "expected a 16-byte Skyrim [i16 X, i16 Y, i32 stride, i32 min level, i32 max level] file, found {} bytes",
            bytes.len()
        );
        let field =
            |offset: usize| i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let grid_x = i32::from(i16::from_le_bytes(bytes[0..2].try_into().unwrap()));
        let grid_y = i32::from(i16::from_le_bytes(bytes[2..4].try_into().unwrap()));
        let (stride, min_level, max_level) = (field(4), field(8), field(12));
        color_eyre::eyre::ensure!(
            stride > 0 && min_level > 0 && max_level >= min_level,
            "LOD stride and levels must be positive and ordered, found stride {stride}, levels {min_level}..{max_level}"
        );
        Ok(Self {
            origin: LodOrigin::new(grid_x, grid_y),
            stride,
            min_level,
            max_level,
        })
    }
}

/// The sidecar path for a worldspace editor id inside a Skyrim data layout:
/// `lodsettings/<WorldspaceEDID>.lod`, lowercased to match the staged VFS.
///
/// The id is untrusted input at a trust boundary: it must be exactly one
/// ordinary filename component. Empty ids, absolute paths, `.`/`..`, and any
/// id containing `/`, `\`, or `:` is rejected rather than joined.
pub fn sidecar_path(data_dir: &Path, worldspace_editor_id: &str) -> Result<std::path::PathBuf> {
    color_eyre::eyre::ensure!(
        !worldspace_editor_id.is_empty(),
        "worldspace editor id must not be empty"
    );
    color_eyre::eyre::ensure!(
        !worldspace_editor_id.contains('/')
            && !worldspace_editor_id.contains('\\')
            && !worldspace_editor_id.contains(':'),
        "worldspace editor id {worldspace_editor_id:?} must not contain `/`, `\\`, or `:`"
    );
    color_eyre::eyre::ensure!(
        worldspace_editor_id != "." && worldspace_editor_id != "..",
        "worldspace editor id {worldspace_editor_id:?} must not be `.` or `..`"
    );
    color_eyre::eyre::ensure!(
        !Path::new(worldspace_editor_id).is_absolute(),
        "worldspace editor id {worldspace_editor_id:?} must not be an absolute path"
    );
    {
        let mut components = Path::new(worldspace_editor_id).components();
        let is_single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        color_eyre::eyre::ensure!(
            is_single_normal,
            "worldspace editor id {worldspace_editor_id:?} must be exactly one ordinary filename component"
        );
    }
    Ok(data_dir
        .join("lodsettings")
        .join(format!("{worldspace_editor_id}.lod").to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_tamriel_origin() {
        // Captured from the installed Skyrim sidecar, independent of our writer.
        let bytes = [
            0xa0, 0xff, 0xa0, 0xff, 0x00, 0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x20, 0x00,
            0x00, 0x00,
        ];
        let settings = LodSettings::parse(&bytes).unwrap();
        assert_eq!(settings.origin, LodOrigin::new(-96, -96));
        assert_eq!(
            (settings.stride, settings.min_level, settings.max_level),
            (256, 4, 32)
        );
    }

    fn settings_bytes(x: i16, y: i16, stride: i32, min_level: i32, max_level: i32) -> Vec<u8> {
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

    #[test]
    fn parses_origin_and_grid_metadata() {
        let settings = LodSettings::parse(&settings_bytes(-64, -48, 256, 4, 32)).unwrap();
        assert_eq!(settings.origin, LodOrigin::new(-64, -48));
        assert_eq!(
            (settings.stride, settings.min_level, settings.max_level),
            (256, 4, 32)
        );
    }

    #[test]
    fn rejects_wrong_lengths() {
        for len in [0, 4, 12, 15, 17, 32] {
            assert!(
                LodSettings::parse(&vec![0u8; len]).is_err(),
                "{len} bytes must not parse"
            );
        }
    }

    #[test]
    fn rejects_invalid_grid_metadata() {
        for (stride, min_level, max_level) in [
            (0, 4, 32),
            (-1, 4, 32),
            (256, 0, 32),
            (256, 4, 0),
            (256, 32, 4),
        ] {
            assert!(
                LodSettings::parse(&settings_bytes(0, 0, stride, min_level, max_level)).is_err(),
                "stride {stride}, levels {min_level}..{max_level} must not parse"
            );
        }
    }

    #[test]
    fn sidecar_path_uses_the_worldspace_editor_id() {
        assert_eq!(
            sidecar_path(Path::new("/data"), "Tamriel").unwrap(),
            Path::new("/data/lodsettings/tamriel.lod")
        );
    }

    #[test]
    fn sidecar_path_accepts_ordinary_editor_ids() {
        for id in [
            "Tamriel",
            "WhiterunWorld",
            "DLC01Hearthfire",
            "My_World01",
            "a.b",
        ] {
            assert!(
                sidecar_path(Path::new("/data"), id).is_ok(),
                "{id:?} must be accepted"
            );
        }
    }

    #[test]
    fn sidecar_path_rejects_untrusted_editor_ids() {
        for id in [
            "",
            ".",
            "..",
            "Tamriel/evil",
            "../Tamriel",
            "/Tamriel",
            "/etc/passwd",
            "Tamriel\\evil",
            "..\\Tamriel",
            "C:\\Tamriel",
            "C:Tamriel",
            "a:b",
            "a/b",
        ] {
            assert!(
                sidecar_path(Path::new("/data"), id).is_err(),
                "{id:?} must be rejected"
            );
        }
    }
}
