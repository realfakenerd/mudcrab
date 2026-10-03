use color_eyre::{Result, eyre::bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Texture,
    Mesh,
    Script,
    LodSettings,
}

impl AssetKind {
    const fn folder(self) -> &'static str {
        match self {
            Self::Texture => "textures",
            Self::Mesh => "meshes",
            Self::Script => "scripts",
            Self::LodSettings => "lodsettings",
        }
    }

    const fn accepted_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Texture => &["dds", "ktx2"],
            Self::Mesh => &["nif", "glb"],
            Self::Script => &["pex", "luau"],
            Self::LodSettings => &["lod"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedAsset {
    pub key: String,
    pub path: PathBuf,
    pub source_root: PathBuf,
    pub relative_source: String,
    pub priority: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetOverride {
    pub key: String,
    pub lower_priority_source: PathBuf,
    pub winning_source: PathBuf,
}

#[derive(Debug, Default)]
pub struct AssetSourceIndex {
    entries: BTreeMap<String, IndexedAsset>,
    overrides: Vec<AssetOverride>,
}

impl AssetSourceIndex {
    /// Builds a case-insensitive index. Roots are ordered from lowest to highest
    /// priority, matching load-order overlay semantics.
    pub fn build(roots: &[PathBuf], kind: AssetKind) -> Result<Self> {
        let mut index = Self::default();
        for (priority, root) in roots.iter().enumerate() {
            let mut within_root = BTreeMap::<String, PathBuf>::new();
            for entry in WalkDir::new(root).follow_links(false) {
                let entry = entry?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let relative = entry.path().strip_prefix(root)?;
                let Some(extension) = relative.extension().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !kind
                    .accepted_extensions()
                    .iter()
                    .any(|accepted| extension.eq_ignore_ascii_case(accepted))
                {
                    continue;
                }
                let key = canonical_asset_path(&relative.to_string_lossy(), kind, extension)?;
                if let Some(previous) = within_root.insert(key.clone(), entry.path().to_owned()) {
                    bail!(
                        "normalized asset collision for {key}: {} and {}",
                        previous.display(),
                        entry.path().display()
                    );
                }
                let indexed = IndexedAsset {
                    key: key.clone(),
                    path: entry.path().to_owned(),
                    source_root: root.clone(),
                    relative_source: normalize_separators(relative),
                    priority,
                };
                if let Some(previous) = index.entries.insert(key.clone(), indexed) {
                    index.overrides.push(AssetOverride {
                        key,
                        lower_priority_source: previous.path,
                        winning_source: entry.path().to_owned(),
                    });
                }
            }
        }
        index.overrides.sort_by(|left, right| {
            left.key
                .cmp(&right.key)
                .then_with(|| left.winning_source.cmp(&right.winning_source))
        });
        Ok(index)
    }

    pub fn get(&self, canonical_key: &str) -> Option<&IndexedAsset> {
        self.entries.get(canonical_key)
    }

    pub fn overrides(&self) -> &[AssetOverride] {
        &self.overrides
    }
}

/// Canonicalizes a Bethesda asset path into a lowercase, root-relative key.
/// Repeated `data/<kind>` or `<kind>` prefixes are collapsed by selecting the
/// last kind component, which repairs paths leaked from authoring workspaces.
pub fn canonical_asset_path(
    input: &str,
    kind: AssetKind,
    output_extension: &str,
) -> Result<String> {
    let decoded = percent_decode(input.trim())?;
    if decoded.is_empty() {
        bail!("asset path is empty");
    }
    if decoded.chars().any(char::is_control) {
        bail!("asset path contains a control character");
    }
    let normalized = decoded.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        bail!("asset path is absolute: {input}");
    }
    let raw_components = normalized
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>();
    if raw_components.contains(&"..") {
        bail!("asset path contains traversal: {input}");
    }
    let folder = kind.folder();
    let start = raw_components
        .iter()
        .rposition(|component| component.eq_ignore_ascii_case(folder))
        .map_or(0, |index| index + 1);
    let mut relative = PathBuf::new();
    for component in &raw_components[start..] {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        bail!("asset path has no file name: {input}");
    }
    if let Some(extension) = relative.extension().and_then(|value| value.to_str())
        && !kind
            .accepted_extensions()
            .iter()
            .any(|accepted| extension.eq_ignore_ascii_case(accepted))
    {
        bail!("unsupported {} extension in {input}", kind.folder());
    }
    relative.set_extension(output_extension);
    Ok(format!("{folder}/{}", normalize_separators(&relative)).to_ascii_lowercase())
}

/// Resolves a relative glTF URI lexically and rejects paths escaping the asset root.
pub fn resolve_asset_uri(assets_root: &Path, document: &Path, uri: &str) -> Result<PathBuf> {
    if uri.starts_with("data:") {
        bail!("embedded data URI has no filesystem path");
    }
    let decoded = percent_decode(uri)?;
    if decoded.chars().any(char::is_control) || decoded.contains(['?', '#']) {
        bail!("asset URI contains unsupported characters: {uri}");
    }
    let joined = document
        .parent()
        .unwrap_or(assets_root)
        .join(decoded.replace('\\', "/"));
    let candidate = lexical_normalize(&joined);
    if !is_within(&candidate, assets_root) {
        bail!("asset URI escapes assets root: {uri}");
    }
    Ok(candidate)
}

pub fn normalize_separators(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(hex) = bytes.get(index + 1..index + 3) else {
                bail!("asset path contains invalid percent encoding: {input}");
            };
            let value = std::str::from_utf8(hex)
                .ok()
                .and_then(|value| u8::from_str_radix(value, 16).ok())
                .ok_or_else(|| {
                    color_eyre::eyre::eyre!("asset path contains invalid percent encoding: {input}")
                })?;
            output.push(value);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output)
        .map_err(|_| color_eyre::eyre::eyre!("asset path is not valid UTF-8 after decoding"))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

fn is_within(path: &Path, root: &Path) -> bool {
    let path = normalize_separators(path).to_ascii_lowercase();
    let mut root = normalize_separators(root).to_ascii_lowercase();
    if !root.ends_with('/') {
        root.push('/');
    }
    path == root.trim_end_matches('/') || path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn canonicalizes_bethesda_texture_variants() {
        assert_eq!(
            canonical_asset_path(r"Textures\Landscape\Rocks 01", AssetKind::Texture, "dds")
                .unwrap(),
            "textures/landscape/rocks 01.dds"
        );
        assert_eq!(
            canonical_asset_path(
                "textures/skyrimhd/build/pc/data/textures/landscape/rock_n.dds",
                AssetKind::Texture,
                "ktx2"
            )
            .unwrap(),
            "textures/landscape/rock_n.ktx2"
        );
        assert_eq!(
            canonical_asset_path("landscape/rock%2001.DDS", AssetKind::Texture, "ktx2").unwrap(),
            "textures/landscape/rock 01.ktx2"
        );
    }

    #[test]
    fn rejects_unsafe_or_unsupported_texture_paths() {
        for path in [
            "../secret.dds",
            "C:/textures/a.dds",
            "textures/a.tga",
            "textures/a.bmp",
            "textures/a\u{8}b.dds",
            "textures/a%XX.dds",
        ] {
            assert!(
                canonical_asset_path(path, AssetKind::Texture, "dds").is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn later_roots_override_and_same_root_collisions_fail() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().join("base");
        let modded = directory.path().join("modded");
        fs::create_dir_all(base.join("Landscape")).unwrap();
        fs::create_dir_all(modded.join("landscape")).unwrap();
        fs::write(base.join("Landscape/Rock.DDS"), b"base").unwrap();
        fs::write(modded.join("landscape/rock.dds"), b"override").unwrap();
        let index =
            AssetSourceIndex::build(&[base.clone(), modded.clone()], AssetKind::Texture).unwrap();
        assert_eq!(index.overrides().len(), 1);
        assert!(
            index
                .get("textures/landscape/rock.dds")
                .unwrap()
                .path
                .starts_with(modded)
        );

        if !cfg!(windows) {
            fs::write(base.join("Landscape/ROCK.dds"), b"collision").unwrap();
            assert!(AssetSourceIndex::build(&[base], AssetKind::Texture).is_err());
        }
    }

    #[test]
    fn resolves_uri_without_allowing_escape() {
        let root = Path::new("C:/assets");
        let glb = root.join("meshes/architecture/a.glb");
        assert_eq!(
            resolve_asset_uri(root, &glb, "../../textures/a%20b.ktx2").unwrap(),
            root.join("textures/a b.ktx2")
        );
        assert!(resolve_asset_uri(root, &glb, "../../../secret.ktx2").is_err());
    }
}
