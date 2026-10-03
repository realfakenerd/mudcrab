//! Census of the rigid bodies the converter extracts from a folder of NIFs.
//!
//! `OPENSKYRIM_NIF_DIR=<folder> cargo run --release -p converter --example physics_census`
//! (or pass the folder as the first argument). Every file under the folder that starts with
//! the Gamebryo signature is parsed, whatever its name, so content-addressed blobs work.
//! An optional second argument (or `OPENSKYRIM_NIF_LIST`) is a text file of `<path>\t<blob>`
//! lines; with it the census also reports the subset whose path starts with
//! `meshes/clutter/`. Read only.
//!
//! The counts are extraction-stage: they come from `MeshConverter::extract_collision`, which
//! never runs the converter's node verification. A body counted here can still be moved to
//! `skipped` when it is written, because the rule depends on the exported GLB's node names
//! (a missing or duplicated name, or a node at another index, is not visible from the NIF
//! alone). The one failure that is visible from the NIF alone, an empty target name, is
//! reported separately.

use converter::mesh::MeshConverter;
use shared::collision::{BodyKind, CollisionAsset};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Default)]
struct Census {
    files: usize,
    unparsable: usize,
    meshes_with_bodies: usize,
    bodies: usize,
    per_kind: BTreeMap<&'static str, usize>,
    per_system: BTreeMap<(u8, u8), usize>,
    per_system_mass_positive: BTreeMap<(u8, u8), usize>,
    dynamic_mass: Option<(f32, f32)>,
    meshes_with_skipped: usize,
    skipped_reasons: BTreeMap<String, usize>,
    bodies_with_empty_target: usize,
    convex: usize,
}

impl Census {
    fn add(&mut self, asset: &CollisionAsset) {
        if !asset.bodies.is_empty() {
            self.meshes_with_bodies += 1;
        }
        if !asset.skipped.is_empty() {
            self.meshes_with_skipped += 1;
        }
        for reason in &asset.skipped {
            let reason = reason.split_once(": ").map_or(reason.as_str(), |(_, r)| r);
            let reason = if std::env::var_os("OPENSKYRIM_CENSUS_FULL_REASONS").is_some() {
                reason
            } else {
                reason.trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ')
            };
            *self.skipped_reasons.entry(reason.to_owned()).or_default() += 1;
        }
        for body in &asset.bodies {
            self.bodies += 1;
            let kind = match body.kind {
                BodyKind::Fixed => "fixed",
                BodyKind::Keyframed => "keyframed",
                BodyKind::Dynamic => "dynamic",
            };
            *self.per_kind.entry(kind).or_default() += 1;
            let key = (body.havok.motion_system, body.havok.quality_type);
            *self.per_system.entry(key).or_default() += 1;
            if body.mass > 0.0 {
                *self.per_system_mass_positive.entry(key).or_default() += 1;
            }
            if body.target.is_empty() {
                self.bodies_with_empty_target += 1;
            }
            self.convex += usize::from(body.convex);
            if body.kind == BodyKind::Dynamic {
                let (lo, hi) = self.dynamic_mass.get_or_insert((body.mass, body.mass));
                *lo = lo.min(body.mass);
                *hi = hi.max(body.mass);
            }
        }
    }

    fn print(&self, title: &str) {
        println!("== {title} ==");
        println!(
            "files parsed: {} (unparsable: {})",
            self.files, self.unparsable
        );
        println!("meshes with at least one body: {}", self.meshes_with_bodies);
        println!("total bodies: {}", self.bodies);
        println!("per kind: {:?}", self.per_kind);
        println!("convex bodies: {}", self.convex);
        println!("per (motion_system, quality_type): total / with mass > 0");
        for (key, count) in &self.per_system {
            println!(
                "  {key:?}: {count} / {}",
                self.per_system_mass_positive.get(key).copied().unwrap_or(0)
            );
        }
        match self.dynamic_mass {
            Some((lo, hi)) => println!("dynamic mass range: {lo} .. {hi} kg"),
            None => println!("dynamic mass range: none"),
        }
        println!(
            "bodies with an empty target name (skipped by node verification): {}",
            self.bodies_with_empty_target
        );
        println!("meshes with skipped entries: {}", self.meshes_with_skipped);
        let mut reasons: Vec<_> = self.skipped_reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in reasons.into_iter().take(12) {
            println!("  skipped x{count}: {reason}");
        }
    }
}

/// Walks `dir` without following symlinked folders. Every folder or entry that cannot be read
/// is pushed to `errors`, so the caller can say the census is incomplete.
fn collect(dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            errors.push(format!("{}: {err}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                errors.push(format!("{}: {err}", dir.display()));
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => collect(&path, out, errors),
            Ok(_) => out.push(path),
            Err(err) => errors.push(format!("{}: {err}", path.display())),
        }
    }
}

/// True when an install path (either separator, any case) is under `meshes/clutter/`.
fn is_clutter_path(path: &str) -> bool {
    path.to_ascii_lowercase()
        .replace('\\', "/")
        .starts_with("meshes/clutter/")
}

fn is_nif(path: &Path) -> bool {
    let mut head = [0_u8; 20];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && &head == b"Gamebryo File Format"
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .or_else(|| std::env::var("OPENSKYRIM_NIF_DIR").ok())
        .expect("pass a folder or set OPENSKYRIM_NIF_DIR");
    assert!(
        Path::new(&dir).is_dir(),
        "{dir} is not a readable folder (check the argument or OPENSKYRIM_NIF_DIR)"
    );
    let clutter_only = std::env::var_os("OPENSKYRIM_CENSUS_CLUTTER_ONLY").is_some();
    let list = args
        .next()
        .or_else(|| std::env::var("OPENSKYRIM_NIF_LIST").ok());
    // blob file name -> install path, for the optional subset report.
    let mut paths: HashMap<String, String> = HashMap::new();
    assert!(
        !clutter_only || list.is_some(),
        "OPENSKYRIM_CENSUS_CLUTTER_ONLY needs a list (second argument or OPENSKYRIM_NIF_LIST)"
    );
    if let Some(list) = list {
        for line in fs::read_to_string(list).expect("read list").lines() {
            if let Some((path, blob)) = line.split_once('\t') {
                // A blob shared by several paths counts as clutter if any of them is.
                let entry = paths
                    .entry(blob.to_owned())
                    .or_insert_with(|| path.to_owned());
                if is_clutter_path(path) {
                    *entry = path.to_owned();
                }
            }
        }
    }
    let mut files = Vec::new();
    let mut errors = Vec::new();
    if clutter_only {
        // Fast path: go straight to the listed clutter blobs (`<dir>/sha256/<ab>/<hash>`).
        for (blob, path) in &paths {
            if is_clutter_path(path) && blob.len() > 2 {
                files.push(Path::new(&dir).join("sha256").join(&blob[..2]).join(blob));
            }
        }
    } else {
        collect(Path::new(&dir), &mut files, &mut errors);
    }
    files.sort();
    let (mut all, mut clutter) = (Census::default(), Census::default());
    for file in files.iter().filter(|file| is_nif(file)) {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        let in_clutter = paths.get(&name).is_some_and(|path| is_clutter_path(path));
        let parsed = MeshConverter::extract_collision(file).ok();
        for census in [Some(&mut all), in_clutter.then_some(&mut clutter)]
            .into_iter()
            .flatten()
        {
            census.files += 1;
            match &parsed {
                Some(asset) => census.add(asset),
                None => census.unparsable += 1,
            }
        }
    }
    all.print("whole folder");
    if !paths.is_empty() {
        clutter.print("meshes/clutter/ subset");
    }
    if !errors.is_empty() {
        eprintln!(
            "census INCOMPLETE: {} folder(s) or entries could not be read:",
            errors.len()
        );
        for error in &errors {
            eprintln!("  {error}");
        }
        std::process::exit(1);
    }
}
