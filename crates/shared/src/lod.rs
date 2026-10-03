//! Native LOD chunk contracts shared by the offline compiler and the runtime.
//!
//! Fixed spatial tiers past the full-detail grid (ADR-0011): 4-, 8-, and
//! 16-cell blocks. Chunk payloads are GLB files; the database holds the
//! spatial index, never blobs (ADR-0010). Anchoring uses per-worldspace
//! origins with floor division toward negative infinity, so a position just
//! west of zero lands in cell -1 (GEOM-02).

/// A fixed spatial LOD tier: the side length of a chunk in exterior cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum LodTier {
    /// 4x4-cell chunks, the nearest LOD ring past the full-detail grid.
    Tier4 = 4,
    /// 8x8-cell chunks.
    Tier8 = 8,
    /// 16x16-cell chunks, the farthest compiled tier.
    Tier16 = 16,
}

/// Full LAND sample intervals along one terrain quadrant edge.
pub const TERRAIN_QUADRANT_INTERVALS: usize = 16;
/// Unique boundary samples plus one interior center vertex.
pub const TERRAIN_QUADRANT_VERTEX_COUNT: usize = 4 * TERRAIN_QUADRANT_INTERVALS + 1;
/// Triangle fan indices joining the perimeter to the interior center.
pub const TERRAIN_QUADRANT_INDEX_COUNT: usize = 3 * (TERRAIN_QUADRANT_VERTEX_COUNT - 1);

impl LodTier {
    /// All tiers from nearest to farthest.
    pub const ALL: [LodTier; 3] = [LodTier::Tier4, LodTier::Tier8, LodTier::Tier16];

    /// Chunk side length in exterior cells.
    pub const fn side_cells(self) -> i32 {
        self as i32
    }

    /// Parses a tier from its side length. Anything else is a corrupt index
    /// row, not a tier the compiler emits.
    pub const fn from_side_cells(side: i32) -> Option<LodTier> {
        match side {
            4 => Some(LodTier::Tier4),
            8 => Some(LodTier::Tier8),
            16 => Some(LodTier::Tier16),
            _ => None,
        }
    }

    /// The next coarser tier, if any. Tier selection walks outward; there is
    /// nothing past [`LodTier::Tier16`].
    pub const fn coarser(self) -> Option<LodTier> {
        match self {
            LodTier::Tier4 => Some(LodTier::Tier8),
            LodTier::Tier8 => Some(LodTier::Tier16),
            LodTier::Tier16 => None,
        }
    }

    /// The next finer tier, if any. Tier 4 is the finest compiled tier; full
    /// cells are finer still but are not LOD chunks.
    pub const fn finer(self) -> Option<LodTier> {
        match self {
            LodTier::Tier4 => None,
            LodTier::Tier8 => Some(LodTier::Tier4),
            LodTier::Tier16 => Some(LodTier::Tier8),
        }
    }
}

/// A chunk's position on its tier grid: which `side_cells`-wide block of the
/// worldspace it covers. The anchor is in chunk units, not cell units; the
/// covered cells run from `anchor * side` to `anchor * side + side - 1`
/// relative to the worldspace LOD origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkAnchor {
    pub x: i32,
    pub y: i32,
}

impl ChunkAnchor {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// The lowest cell coordinate this chunk covers along one axis, relative
    /// to the worldspace LOD origin.
    pub const fn min_cell(self, tier: LodTier) -> (i32, i32) {
        let side = tier.side_cells();
        (self.x * side, self.y * side)
    }

    /// The highest cell coordinate this chunk covers along one axis, relative
    /// to the worldspace LOD origin.
    pub const fn max_cell(self, tier: LodTier) -> (i32, i32) {
        let side = tier.side_cells();
        (self.x * side + side - 1, self.y * side + side - 1)
    }

    /// Whether a cell (relative to the same origin) falls inside this chunk.
    pub const fn contains_cell(self, tier: LodTier, cell_x: i32, cell_y: i32) -> bool {
        let (min_x, min_y) = self.min_cell(tier);
        let side = tier.side_cells();
        cell_x >= min_x && cell_x < min_x + side && cell_y >= min_y && cell_y < min_y + side
    }
}

/// Identity of one compiled chunk: the worldspace it belongs to, its tier,
/// and its anchor on that tier's grid (ADR-0010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkKey {
    pub worldspace_id: u32,
    pub tier: LodTier,
    pub anchor: ChunkAnchor,
}

impl ChunkKey {
    pub const fn new(worldspace_id: u32, tier: LodTier, anchor: ChunkAnchor) -> Self {
        Self {
            worldspace_id,
            tier,
            anchor,
        }
    }
}

/// Per-worldspace LOD grid origin in cell units: the cell coordinate that maps
/// to chunk anchor (0, 0) at every tier. Read from the authoritative
/// `lodsettings/<worldspace>.lod` sidecar for installed worlds, or supplied
/// explicitly for custom worlds. There is no default origin: a world without
/// a valid origin gets no LOD (GEOM-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LodOrigin {
    pub grid_x: i32,
    pub grid_y: i32,
}

impl LodOrigin {
    pub const fn new(grid_x: i32, grid_y: i32) -> Self {
        Self { grid_x, grid_y }
    }

    /// Which chunk of `tier` covers the exterior cell `(grid_x, grid_y)`.
    /// Division floors toward negative infinity, so a cell just west or south
    /// of the origin still anchors inside its own chunk (GEOM-02).
    pub fn chunk_for_cell(self, tier: LodTier, grid_x: i32, grid_y: i32) -> ChunkAnchor {
        let side = tier.side_cells();
        ChunkAnchor::new(
            floor_div(grid_x - self.grid_x, side),
            floor_div(grid_y - self.grid_y, side),
        )
    }
}

/// Integer division rounding toward negative infinity. Rust's `/` truncates
/// toward zero, which would fold cells just west/south of the origin into the
/// wrong chunk.
const fn floor_div(value: i32, divisor: i32) -> i32 {
    debug_assert!(divisor > 0);
    let quotient = value / divisor;
    let remainder = value % divisor;
    if remainder != 0 && (remainder < 0) != (divisor < 0) {
        quotient - 1
    } else {
        quotient
    }
}

/// Stable node names inside a chunk GLB, shared by the compiler that writes
/// them and the runtime that hides them. Lookup is by name, never by
/// traversal order: one file per chunk, one node per source cell, separately
/// hideable `terrain` and `objects` groups beneath it, material-compatible
/// batches under each group (GEOM-05).
pub mod nodes {
    /// Node holding one source cell's share of the chunk. `grid_x`/`grid_y`
    /// are absolute exterior cell coordinates, so the runtime derives the
    /// same name from its own cell grid without consulting the chunk.
    pub fn source_cell(grid_x: i32, grid_y: i32) -> String {
        format!("cell_{grid_x}_{grid_y}")
    }

    /// Group beneath a source-cell node holding that cell's terrain batches.
    /// Hidden only when the matching full cell's terrain (or a nearer tier's)
    /// is drawable.
    pub const fn terrain_group() -> &'static str {
        "terrain"
    }

    /// Group beneath a source-cell node holding that cell's object batches.
    /// Hidden only when the matching full cell's objects (or a nearer tier's)
    /// are drawable.
    pub const fn objects_group() -> &'static str {
        "objects"
    }

    /// Parses a [`source_cell`](source_cell) name back into its cell
    /// coordinates. Returns `None` for group names, batch names, and anything
    /// else the compiler did not emit as a source-cell node.
    pub fn parse_source_cell(name: &str) -> Option<(i32, i32)> {
        let rest = name.strip_prefix("cell_")?;
        let (x, y) = rest.split_once('_')?;
        Some((x.parse().ok()?, y.parse().ok()?))
    }
}

/// Canonical chunk payload path inside the published asset set, relative to
/// the assets root: `lod/<worldspace-id>/<tier>/cell_<ax>_<ay>.glb`. Lowercase hex
/// worldspace id, decimal anchors (which may be negative). Both the compiler
/// that writes the file and the runtime that loads it derive this path from
/// the same [`ChunkKey`], so a chunk is never found by directory scan.
pub fn chunk_payload_path(key: ChunkKey) -> String {
    format!(
        "lod/{:08x}/{}/cell_{}_{}.glb",
        key.worldspace_id,
        key.tier.side_cells(),
        key.anchor.x,
        key.anchor.y
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_order_nearest_to_farthest() {
        assert_eq!(LodTier::ALL.map(LodTier::side_cells), [4, 8, 16]);
        assert_eq!(LodTier::Tier4.coarser(), Some(LodTier::Tier8));
        assert_eq!(LodTier::Tier8.coarser(), Some(LodTier::Tier16));
        assert_eq!(LodTier::Tier16.coarser(), None);
        assert_eq!(LodTier::Tier4.finer(), None);
        assert_eq!(LodTier::Tier16.finer(), Some(LodTier::Tier8));
        assert_eq!(LodTier::from_side_cells(4), Some(LodTier::Tier4));
        assert_eq!(LodTier::from_side_cells(8), Some(LodTier::Tier8));
        assert_eq!(LodTier::from_side_cells(16), Some(LodTier::Tier16));
        assert_eq!(LodTier::from_side_cells(32), None);
        assert_eq!(LodTier::from_side_cells(0), None);
        assert_eq!(LodTier::from_side_cells(-4), None);
    }

    #[test]
    fn anchors_cover_exactly_their_cells() {
        let anchor = ChunkAnchor::new(2, -1);
        assert_eq!(anchor.min_cell(LodTier::Tier4), (8, -4));
        assert_eq!(anchor.max_cell(LodTier::Tier4), (11, -1));
        assert!(anchor.contains_cell(LodTier::Tier4, 8, -4));
        assert!(anchor.contains_cell(LodTier::Tier4, 11, -1));
        assert!(!anchor.contains_cell(LodTier::Tier4, 12, -1));
        assert!(!anchor.contains_cell(LodTier::Tier4, 8, -5));
    }

    #[test]
    fn chunk_lookup_floors_toward_negative_infinity() {
        let origin = LodOrigin::new(0, 0);
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, 0, 0),
            ChunkAnchor::new(0, 0)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, 3, 3),
            ChunkAnchor::new(0, 0)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, 4, 4),
            ChunkAnchor::new(1, 1)
        );
        // A cell just west/south of the origin lands in chunk -1, not 0:
        // truncation toward zero would put it in the wrong chunk (GEOM-02).
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, -1, -1),
            ChunkAnchor::new(-1, -1)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, -4, -4),
            ChunkAnchor::new(-1, -1)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, -5, 0),
            ChunkAnchor::new(-2, 0)
        );
    }

    #[test]
    fn chunk_lookup_respects_a_nonzero_origin() {
        let origin = LodOrigin::new(2, -6);
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, 2, -6),
            ChunkAnchor::new(0, 0)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier4, 1, -6),
            ChunkAnchor::new(-1, 0)
        );
        assert_eq!(
            origin.chunk_for_cell(LodTier::Tier8, 10, -6),
            ChunkAnchor::new(1, 0)
        );
    }

    #[test]
    fn chunk_anchors_are_stable_across_tier_boundaries() {
        // Adjacent cells on either side of a chunk edge anchor to adjacent
        // chunks, with no gap and no overlap.
        let origin = LodOrigin::new(0, 0);
        for tier in LodTier::ALL {
            let side = tier.side_cells();
            let west = origin.chunk_for_cell(tier, -1, 0);
            let east = origin.chunk_for_cell(tier, 0, 0);
            assert_eq!((west.x + 1, west.y), (east.x, east.y));
            assert!(west.contains_cell(tier, -1, 0));
            assert!(!west.contains_cell(tier, 0, 0));
            assert!(east.contains_cell(tier, 0, 0));
            assert!(!east.contains_cell(tier, -1, 0));
            assert_eq!(east.min_cell(tier).0 - west.max_cell(tier).0, 1);
            assert_eq!(side, east.max_cell(tier).0 - east.min_cell(tier).0 + 1);
        }
    }

    #[test]
    fn source_cell_names_round_trip_including_negatives() {
        for (x, y) in [(0, 0), (7, -3), (-1, -1), (-128, 64)] {
            let name = nodes::source_cell(x, y);
            assert_eq!(nodes::parse_source_cell(&name), Some((x, y)));
        }
        assert_eq!(nodes::parse_source_cell("terrain"), None);
        assert_eq!(nodes::parse_source_cell("objects"), None);
        assert_eq!(nodes::parse_source_cell("cell_1"), None);
        assert_eq!(nodes::parse_source_cell("cell_a_b"), None);
        assert_eq!(nodes::parse_source_cell("other_1_2"), None);
        assert_eq!(nodes::terrain_group(), "terrain");
        assert_eq!(nodes::objects_group(), "objects");
    }

    #[test]
    fn chunk_payload_paths_are_stable_and_unique() {
        let key = ChunkKey::new(0x3c, LodTier::Tier4, ChunkAnchor::new(2, -1));
        assert_eq!(chunk_payload_path(key), "lod/0000003c/4/cell_2_-1.glb");
        let other = ChunkKey::new(0x3c, LodTier::Tier8, ChunkAnchor::new(2, -1));
        assert_eq!(chunk_payload_path(other), "lod/0000003c/8/cell_2_-1.glb");
        assert_ne!(chunk_payload_path(key), chunk_payload_path(other));
    }
}
