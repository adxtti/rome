//! Map data: CMapData / CEntityDef placements, LOD hierarchy, instance batches, sector grids.

pub mod error;
pub mod map;
pub mod meta;
mod synth;

pub use error::{MapError, Result};
pub use map::{
    ArchetypeDef, GrassBatch, GrassInstance, LodLevel, MapData, MapExtents, Placement,
    PlacementKind, PropBatch, PropInstance,
};
pub use meta::MetaResource;
