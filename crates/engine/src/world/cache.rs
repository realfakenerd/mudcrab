use bevy::prelude::Resource;
use color_eyre::{Result, eyre::WrapErr};
use memmap2::Mmap;
use rkyv::rancor::Error;
use std::{collections::HashMap, fs::File, path::Path};

#[derive(Debug, Clone)]
pub struct TerrainSnapshot {
    pub cell_id: u32,
    pub width: u16,
    pub height: u16,
    pub heights: Vec<f32>,
    pub normals: Vec<i8>,
    pub vertex_colors: Vec<u8>,
    pub layers: Vec<TerrainLayerSnapshot>,
    pub water_height: Option<f32>,
    pub water_type_form_id: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct TerrainLayerSnapshot {
    pub texture_form_id: u32,
    pub quadrant: u8,
    pub layer: u16,
    pub is_base: bool,
    pub weights: Vec<(u16, f32)>,
}

#[derive(Resource)]
pub struct CellCache {
    mmap: Mmap,
    index: HashMap<u32, usize>,
}

impl CellCache {
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
        let mmap = unsafe { Mmap::map(&file) }.wrap_err("failed to map cell cache")?;
        let archived = rkyv::access::<shared::ArchivedCellCache, Error>(&mmap)
            .wrap_err("invalid cell cache")?;
        color_eyre::eyre::ensure!(
            archived.version == shared::CELL_CACHE_VERSION,
            "cell cache version {} is unsupported; reconvert assets for version {}",
            archived.version,
            shared::CELL_CACHE_VERSION
        );
        let index = archived
            .cells
            .iter()
            .enumerate()
            .map(|(index, cell)| (cell.cell_id.into(), index))
            .collect();
        Ok(Self { mmap, index })
    }

    pub fn terrain(&self, cell_id: u32) -> Option<TerrainSnapshot> {
        // SAFETY: `open` already validated these exact bytes with `rkyv::access`
        // (which runs bytecheck over the whole `ArchivedCellCache`) before this
        // `CellCache` was constructed. `self.mmap` is never remapped or mutated
        // after `open` returns, so the bytes still describe a valid
        // `ArchivedCellCache` at the default root position, and re-validating on
        // every lookup (this is called once per committed cell) would be wasted
        // work.
        let archived = unsafe { rkyv::access_unchecked::<shared::ArchivedCellCache>(&self.mmap) };
        let cell = archived.cells.get(*self.index.get(&cell_id)?)?;
        Some(TerrainSnapshot {
            cell_id: cell.cell_id.into(),
            width: cell.width.into(),
            height: cell.height.into(),
            heights: cell.heights.iter().copied().map(Into::into).collect(),
            normals: cell.normals.iter().copied().collect(),
            vertex_colors: cell.vertex_colors.iter().copied().collect(),
            layers: cell
                .layers
                .iter()
                .map(|layer| TerrainLayerSnapshot {
                    texture_form_id: layer.texture_form_id.into(),
                    quadrant: layer.quadrant,
                    layer: layer.layer.into(),
                    is_base: layer.is_base,
                    weights: layer
                        .weights
                        .iter()
                        .map(|weight| (weight.vertex.into(), weight.opacity.into()))
                        .collect(),
                })
                .collect(),
            water_height: cell.water_height.as_ref().copied().map(Into::into),
            water_type_form_id: cell.water_type_form_id.as_ref().copied().map(Into::into),
        })
    }

    /// Height of the cell's centre sample (the one the start position and `coc`/`coe` stand on),
    /// or `None` for a cell with no terrain in the cache.
    pub fn centre_height(&self, cell_id: u32) -> Option<f32> {
        let terrain = self.terrain(cell_id)?;
        let width = usize::from(terrain.width);
        let height = usize::from(terrain.height);
        let index = (height / 2)
            .checked_mul(width)
            .and_then(|row| row.checked_add(width / 2))?;
        terrain.heights.get(index).copied()
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centre_height_reads_the_centre_sample_and_is_none_without_terrain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cell_cache.rkyv");
        let land = |cell_id, side: u16, heights: Vec<f32>| shared::CachedLand {
            cell_id,
            width: side,
            height: side,
            heights,
            normals: vec![0; usize::from(side) * usize::from(side) * 3],
            vertex_colors: vec![255; usize::from(side) * usize::from(side) * 3],
            layers: vec![],
            water_height: None,
            water_type_form_id: None,
        };
        let source = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![
                land(1, 3, vec![0.0, 1.0, 2.0, 3.0, 40.0, 5.0, 6.0, 7.0, 8.0]),
                land(2, 2, vec![1.0, 2.0, 3.0, 4.0]),
            ],
        };
        std::fs::write(&path, rkyv::to_bytes::<Error>(&source).unwrap()).unwrap();
        let cache = CellCache::open(&path).unwrap();
        assert_eq!(cache.centre_height(1), Some(40.0));
        // An even-sized grid's centre is index (height / 2) * width + width / 2.
        assert_eq!(cache.centre_height(2), Some(4.0));
        assert_eq!(cache.centre_height(99), None);
    }

    #[test]
    fn maps_and_reads_versioned_terrain() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cell_cache.rkyv");
        let source = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![shared::CachedLand {
                cell_id: 42,
                width: 2,
                height: 2,
                heights: vec![1.0, 2.0, 3.0, 4.0],
                normals: vec![0; 12],
                vertex_colors: vec![255; 12],
                layers: vec![],
                water_height: Some(8.0),
                water_type_form_id: Some(7),
            }],
        };
        let bytes = rkyv::to_bytes::<Error>(&source).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let cache = CellCache::open(&path).unwrap();
        let terrain = cache.terrain(42).unwrap();
        assert_eq!(terrain.heights, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(terrain.water_height, Some(8.0));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn repeated_terrain_lookups_return_the_right_cells() {
        // `terrain` used to re-validate the whole mapped file (`rkyv::access`)
        // on every call; it now trusts the one-time validation done in `open`
        // and reads with `rkyv::access_unchecked` instead. Multiple cells and
        // repeated, out-of-order lookups exercise that the unchecked access
        // still indexes the right cell every time, not just once.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cell_cache.rkyv");
        let source = shared::CellCache {
            version: shared::CELL_CACHE_VERSION,
            cells: vec![
                shared::CachedLand {
                    cell_id: 42,
                    width: 2,
                    height: 2,
                    heights: vec![1.0, 2.0, 3.0, 4.0],
                    normals: vec![0; 12],
                    vertex_colors: vec![255; 12],
                    layers: vec![],
                    water_height: Some(8.0),
                    water_type_form_id: Some(7),
                },
                shared::CachedLand {
                    cell_id: 99,
                    width: 2,
                    height: 2,
                    heights: vec![10.0, 20.0, 30.0, 40.0],
                    normals: vec![0; 12],
                    vertex_colors: vec![255; 12],
                    layers: vec![],
                    water_height: None,
                    water_type_form_id: None,
                },
            ],
        };
        let bytes = rkyv::to_bytes::<Error>(&source).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let cache = CellCache::open(&path).unwrap();
        assert_eq!(cache.terrain(99).unwrap().heights, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(cache.terrain(42).unwrap().heights, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(cache.terrain(42).unwrap().water_height, Some(8.0));
        assert!(cache.terrain(7).is_none());
        assert_eq!(cache.terrain(99).unwrap().heights, [10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn rejects_previous_cache_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.rkyv");
        let bytes = rkyv::to_bytes::<Error>(&shared::CellCache {
            version: shared::CELL_CACHE_VERSION - 1,
            cells: vec![],
        })
        .unwrap();
        std::fs::write(&path, bytes).unwrap();
        assert!(CellCache::open(&path).is_err());
    }

    #[test]
    fn rejects_truncated_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("truncated.rkyv");
        std::fs::write(&path, [0_u8; 7]).unwrap();
        assert!(CellCache::open(&path).is_err());
    }
}
