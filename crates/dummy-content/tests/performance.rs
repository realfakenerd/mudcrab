//! Release-mode performance budgets.
//!
//! These tests are ignored by default because they assert wall-clock budgets
//! with generous headroom. Run them with:
//!
//! ```text
//! cargo test --release -p dummy-content -- --ignored performance
//! ```

use dummy_content::{Entry, bsa, layout};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn payloads(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|index| (format!("assets/file{index}.bin"), vec![index as u8; 256]))
        .collect()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn timed(generate: impl Fn()) -> Duration {
    median(
        (0..3)
            .map(|_| {
                let start = Instant::now();
                generate();
                start.elapsed()
            })
            .collect(),
    )
}

#[test]
#[ignore = "performance"]
fn performance_bsa_generation_stays_within_budget() {
    let owned = payloads(10_000);
    let entries: Vec<Entry<'_>> = owned
        .iter()
        .map(|(name, data)| Entry::new(name, data))
        .collect();

    let elapsed = timed(|| {
        let archive = bsa::v105(&entries, bsa::Compression::None).unwrap();
        assert!(archive.len() > 1_000_000);
        black_box(archive);
    });
    assert!(
        elapsed < Duration::from_secs(10),
        "10k-entry BSA generation took {elapsed:?}"
    );
}

#[test]
#[ignore = "performance"]
fn performance_bsa_generation_scales_subquadratically() {
    let small = payloads(1_000);
    let small_entries: Vec<Entry<'_>> = small
        .iter()
        .map(|(name, data)| Entry::new(name, data))
        .collect();
    let large = payloads(10_000);
    let large_entries: Vec<Entry<'_>> = large
        .iter()
        .map(|(name, data)| Entry::new(name, data))
        .collect();

    let baseline = Duration::from_micros(500);
    let small_time = timed(|| {
        black_box(bsa::v105(&small_entries, bsa::Compression::None).unwrap());
    })
    .max(baseline);
    let large_time = timed(|| {
        black_box(bsa::v105(&large_entries, bsa::Compression::None).unwrap());
    });

    assert!(
        large_time < small_time * 50,
        "BSA generation grew from {small_time:?} to {large_time:?}"
    );
}

#[test]
#[ignore = "performance"]
fn performance_esm_generation_stays_within_budget() {
    let cells: Vec<dummy_content::esm::Cell> = (0..81)
        .map(|index| dummy_content::esm::Cell {
            grid_x: (index % 9) - 4,
            grid_y: (index / 9) - 4,
        })
        .collect();
    let spec = dummy_content::esm::Plugin {
        author: "OpenSkyrim dummy-content",
        worldspace: "BenchWorld",
        cells: &cells,
        model_path: "meshes/generated.nif",
        diffuse: "textures/generated_color.dds",
        normal_texture: "textures/generated_normal.dds",
    };
    let elapsed = timed(|| {
        let plugin = dummy_content::esm::plugin(&spec).unwrap();
        assert!(plugin.len() > 10_000);
        black_box(plugin);
    });
    assert!(
        elapsed < Duration::from_secs(10),
        "81-cell plugin generation took {elapsed:?}"
    );
}

#[test]
#[ignore = "performance"]
fn performance_layout_generation_stays_within_budget() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("Data");
    let elapsed = timed(|| {
        layout::prepare_directory(&root, true).unwrap();
        let written =
            layout::generate(&root, layout::DEFAULT_SEED, layout::Formats::all()).unwrap();
        assert_eq!(written.len(), 13);
        let lod_settings = root.join(format!("lodsettings/{}.lod", layout::GENERATED_WORLDSPACE));
        assert!(written.contains(&lod_settings));
        assert!(lod_settings.is_file());
        black_box(written);
    });
    assert!(
        elapsed < Duration::from_secs(10),
        "layout generation took {elapsed:?}"
    );
}
