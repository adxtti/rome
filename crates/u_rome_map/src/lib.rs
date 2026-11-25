//! Map data: CMapData / CEntityDef placements, LOD hierarchy, instance
//! batches, sector grids.
//!
//! Decodes GTA VI map placement data from meta resources (`0DRP`, PRD0 type
//! 0x7a, the `.ymap` equivalent) for Studio's World Viewer. Coordinates are
//! RAGE convention: right-handed, Z-up, metres.
//!
//! # Pipeline
//!
//! ```no_run
//! use u_rome_map::{LodTree, MapData, MapStats, MetaResource, raster};
//!
//! # fn entries() -> Vec<Vec<u8>> { Vec::new() }
//! let mut maps = Vec::new();
//! for bytes in entries() {
//!     // One decoded resource: system page followed by graphics page.
//!     // Use `MetaResource::parse_pages` when the pages are known.
//!     let Ok(meta) = MetaResource::parse(&bytes) else { continue };
//!     if let Ok(map) = MapData::from_meta(&meta) {
//!         maps.push(map);
//!     }
//! }
//! let tree = LodTree::build(&maps);
//! let stats = MapStats::from_maps(&maps).with_tree(&tree);
//! if let Some(bounds) = stats.bounds {
//!     let all: Vec<_> = maps.iter().flat_map(|m| m.entities.iter().copied()).collect();
//!     let heat = raster::render_topdown(&all, bounds, 1024, 1024);
//!     # let _ = heat;
//! }
//! ```
//!
//! # Modules
//!
//! * [`meta`]: generic reader for the self-describing container: header,
//!   struct / member / enum schema, instance blocks, pool references and
//!   pointers, typed member reads and arrays.
//! * [`map`]: [`MapData`] (`CMapData`), [`Placement`] (`CEntityDef`,
//!   `CMloInstanceDef`), [`PropBatch`], [`GrassBatch`], [`ArchetypeDef`].
//! * [`lod`]: [`LodTree`], parent / child links across files with
//!   unresolved-link reporting.
//! * [`grid`]: 60 / 512 / 1024 m sector cells and the 32-byte grid records.
//! * [`raster`]: top-down density image for the 2D map view.
//! * [`stats`]: summary counts ([`MapStats`]).
//!
//! # Certainty
//!
//! Facts come from GTA6-Asset-Info (GPL documentation, facts only) and
//! rage9-re (MIT OR Apache-2.0). Field offsets are taken from each resource's
//! own schema by field-name hash; documented offsets are a fallback that sets
//! `provisional`. Inferred parts (graphics page boundary in a single buffer,
//! batch bbox layout, grass scale and colour, parent files chosen without a
//! parent hash) are flagged and explained in `warnings`.

pub mod error;
pub mod grid;
pub mod lod;
pub mod map;
pub mod meta;
pub mod raster;
pub mod stats;
mod synth;

pub use error::{MapError, Result};
pub use grid::{Cell, CellSize, SectorGrid, SectorRecord, cell_bounds, cell_of};
pub use lod::{
    EntityId, LinkSource, LodLink, LodParent, LodTree, UnresolvedLink, UnresolvedReason,
};
pub use map::{
    ArchetypeDef, GrassBatch, GrassInstance, LodLevel, MapData, MapExtents, Placement,
    PlacementKind, PropBatch, PropInstance,
};
pub use meta::MetaResource;
pub use raster::{Bounds2, render_topdown};
pub use stats::MapStats;
