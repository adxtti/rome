//! World sector grids.
//!
//! GTA6-Asset-Info `formats/09-placements.md` §9.6 (facts only): cell =
//! floor(world position / cell size), origin at the cell corner, sizes 60 m
//! (high confidence), 512 m and 1024 m (medium: fits peak at 510-512 m and
//! 1020-1024 m, the powers of two are taken). The grids are stored as arrays
//! of 32-byte records (struct `9c834886`) in PSO files; the record's "bundle
//! key" meaning is inferred.

use std::collections::BTreeMap;

use u_rome_core::Reader;

use crate::error::{MapError, Result};
use crate::map::Placement;

/// The three documented cell sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CellSize {
    /// The 60 m world grid (high confidence).
    M60,
    /// 512 m (medium confidence; 510 m also fits).
    M512,
    /// 1024 m (medium confidence; 1020 m also fits).
    M1024,
}

impl CellSize {
    pub const ALL: [CellSize; 3] = [CellSize::M60, CellSize::M512, CellSize::M1024];

    pub fn metres(self) -> f32 {
        match self {
            CellSize::M60 => 60.0,
            CellSize::M512 => 512.0,
            CellSize::M1024 => 1024.0,
        }
    }

    /// True for the sizes whose exact value is not settled.
    pub fn provisional(self) -> bool {
        !matches!(self, CellSize::M60)
    }
}

/// A grid cell coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
}

/// The cell containing a world position; `None` for non-finite or
/// out-of-range coordinates.
pub fn cell_of(position: [f32; 3], size: CellSize) -> Option<Cell> {
    let s = size.metres();
    let x = (position[0] / s).floor();
    let y = (position[1] / s).floor();
    let ok = |v: f32| v.is_finite() && v >= i32::MIN as f32 && v <= i32::MAX as f32;
    (ok(x) && ok(y)).then_some(Cell {
        x: x as i32,
        y: y as i32,
    })
}

/// World-space 2D bounds `(min, max)` of a cell.
pub fn cell_bounds(cell: Cell, size: CellSize) -> ([f32; 2], [f32; 2]) {
    let s = size.metres();
    let x0 = cell.x as f32 * s;
    let y0 = cell.y as f32 * s;
    ([x0, y0], [x0 + s, y0 + s])
}

/// Placements bucketed by cell.
#[derive(Clone, Debug)]
pub struct SectorGrid {
    pub size: CellSize,
    cells: BTreeMap<Cell, Vec<u32>>,
    /// Placements with non-finite positions, left out of every cell.
    pub skipped: usize,
}

impl SectorGrid {
    /// Bucket placements (by index into `placements`) into cells.
    pub fn build(placements: &[Placement], size: CellSize) -> Self {
        Self::from_positions(placements.iter().map(|p| p.position), size)
    }

    /// Bucket arbitrary positions (index = iteration order).
    pub fn from_positions(positions: impl IntoIterator<Item = [f32; 3]>, size: CellSize) -> Self {
        let mut cells: BTreeMap<Cell, Vec<u32>> = BTreeMap::new();
        let mut skipped = 0;
        for (i, p) in positions.into_iter().enumerate() {
            match cell_of(p, size) {
                Some(c) => cells.entry(c).or_default().push(i as u32),
                None => skipped += 1,
            }
        }
        Self {
            size,
            cells,
            skipped,
        }
    }

    /// Indices in a cell.
    pub fn get(&self, cell: Cell) -> &[u32] {
        self.cells.get(&cell).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Occupied cells and their indices, in (x, y) order.
    pub fn iter(&self) -> impl Iterator<Item = (Cell, &[u32])> {
        self.cells.iter().map(|(c, v)| (*c, v.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Occupied cells overlapping a 2D world rectangle.
    pub fn cells_in_rect(&self, min: [f32; 2], max: [f32; 2]) -> Vec<Cell> {
        let (Some(lo), Some(hi)) = (
            cell_of([min[0], min[1], 0.0], self.size),
            cell_of([max[0], max[1], 0.0], self.size),
        ) else {
            return Vec::new();
        };
        self.cells
            .range(
                Cell {
                    x: lo.x,
                    y: i32::MIN,
                }..=Cell {
                    x: hi.x,
                    y: i32::MAX,
                },
            )
            .map(|(c, _)| *c)
            .filter(|c| c.y >= lo.y && c.y <= hi.y)
            .collect()
    }
}

/// One 32-byte sector grid record (struct `9c834886`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectorRecord {
    /// +0x00 i32 `x`.
    pub x: i32,
    /// +0x04 i32 `y`.
    pub y: i32,
    /// +0x08, always 0 so far.
    pub reserved: u32,
    /// +0x0C: most likely a streamed bundle file key (inferred, low).
    pub bundle_key: u32,
    /// +0x10: index in the bundle.
    pub bundle_index: u32,
    /// +0x18: 0 for a cell list, else a map group id.
    pub map_group: u64,
}

impl SectorRecord {
    pub const SIZE: usize = 32;
    /// Cap on the number of records accepted from one buffer.
    pub const MAX_RECORDS: usize = 1 << 20;

    pub fn cell(&self) -> Cell {
        Cell {
            x: self.x,
            y: self.y,
        }
    }

    /// Parse a packed array of records (e.g. the element bytes of the PSO
    /// array). Trailing bytes that do not fill a record are an error.
    pub fn parse_all(bytes: &[u8]) -> Result<Vec<SectorRecord>> {
        if !bytes.len().is_multiple_of(Self::SIZE) {
            return Err(MapError::bytes(
                format!(
                    "sector records: {} bytes is not a multiple of {}",
                    bytes.len(),
                    Self::SIZE
                ),
                u_rome_core::ByteError::OutOfBounds {
                    offset: bytes.len() - bytes.len() % Self::SIZE,
                    len: Self::SIZE,
                    size: bytes.len(),
                },
            ));
        }
        let n = bytes.len() / Self::SIZE;
        if n > Self::MAX_RECORDS {
            return Err(MapError::TooLarge {
                what: "sector record count",
                count: n,
                limit: Self::MAX_RECORDS,
            });
        }
        let r = Reader::new(bytes);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let o = i * Self::SIZE;
            let ctx = || format!("sector record {i} at 0x{o:X}");
            let e = |e| MapError::bytes(ctx(), e);
            out.push(SectorRecord {
                x: r.i32_at(o).map_err(e)?,
                y: r.i32_at(o + 4).map_err(e)?,
                reserved: r.u32_at(o + 8).map_err(e)?,
                bundle_key: r.u32_at(o + 0x0C).map_err(e)?,
                bundle_index: r.u32_at(o + 0x10).map_err(e)?,
                map_group: r.u64_at(o + 0x18).map_err(e)?,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_floor_with_corner_origin() {
        assert_eq!(
            cell_of([0.0, 0.0, 0.0], CellSize::M60),
            Some(Cell { x: 0, y: 0 })
        );
        assert_eq!(
            cell_of([59.9, 60.0, 0.0], CellSize::M60),
            Some(Cell { x: 0, y: 1 })
        );
        assert_eq!(
            cell_of([-0.1, -60.1, 0.0], CellSize::M60),
            Some(Cell { x: -1, y: -2 })
        );
        assert_eq!(
            cell_of([1023.0, 1024.0, 0.0], CellSize::M1024),
            Some(Cell { x: 0, y: 1 })
        );
        assert_eq!(
            cell_of([513.0, -1.0, 0.0], CellSize::M512),
            Some(Cell { x: 1, y: -1 })
        );
        assert_eq!(cell_of([f32::NAN, 0.0, 0.0], CellSize::M60), None);
        assert_eq!(cell_of([1e30, 0.0, 0.0], CellSize::M60), None);
        assert_eq!(
            cell_bounds(Cell { x: -1, y: 2 }, CellSize::M60),
            ([-60.0, 120.0], [0.0, 180.0])
        );
        assert!(CellSize::M512.provisional() && !CellSize::M60.provisional());
    }

    #[test]
    fn grid_buckets_and_queries() {
        let pts = [
            [1.0, 1.0, 0.0],
            [2.0, 2.0, 0.0],
            [70.0, 1.0, 0.0],
            [f32::NAN, 0.0, 0.0],
            [-5.0, 130.0, 0.0],
        ];
        let g = SectorGrid::from_positions(pts, CellSize::M60);
        assert_eq!(g.len(), 3);
        assert_eq!(g.skipped, 1);
        assert_eq!(g.get(Cell { x: 0, y: 0 }), &[0, 1]);
        assert_eq!(g.get(Cell { x: 1, y: 0 }), &[2]);
        assert_eq!(
            g.cells_in_rect([0.0, 0.0], [100.0, 50.0]),
            vec![Cell { x: 0, y: 0 }, Cell { x: 1, y: 0 }]
        );
        assert_eq!(g.cells_in_rect([-10.0, 0.0], [10.0, 200.0]).len(), 2);
    }

    #[test]
    fn parses_records() {
        let mut b = Vec::new();
        for (x, y, key, idx, group) in [
            (3i32, -4i32, 0xAABBu32, 2u32, 0u64),
            (-1, 7, 5, 0, 0x1122_3344_5566_7788),
        ] {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&key.to_le_bytes());
            b.extend_from_slice(&idx.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&group.to_le_bytes());
        }
        let r = SectorRecord::parse_all(&b).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].cell(), Cell { x: 3, y: -4 });
        assert_eq!(r[0].bundle_key, 0xAABB);
        assert_eq!(r[1].map_group, 0x1122_3344_5566_7788);
        assert!(SectorRecord::parse_all(&b[..40]).is_err());
        assert!(SectorRecord::parse_all(&[]).unwrap().is_empty());
    }
}
