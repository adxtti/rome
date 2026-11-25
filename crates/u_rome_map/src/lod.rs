//! LOD hierarchy across map files.
//!
//! Rules from GTA6-Asset-Info `formats/09-placements.md` §9.2 "Resolving LOD
//! parents" (facts only):
//!
//! * A map file's LOD slots are its entities followed by its prop batches.
//! * `parentInstance == -1`: `parentIndex` is an entity of the parent map
//!   file, the file whose name hash equals this file's parent hash
//!   (confirmed). Files without a parent hash share one parent file for all
//!   their children, chosen by an *inferred* rule (candidates whose entity at
//!   each child's index is one level up and has children, ranked by
//!   proximity). Such links are marked [`LinkSource::InferredFile`].
//! * `parentInstance >= 0`: the parent is instance `parentInstance` of prop
//!   batch `parentIndex - <entity count>` in another map file; no field names
//!   that file, but the instance sits on the entity (median 0.0 m), so it is
//!   found by slot and position ([`LinkSource::BatchPosition`]).

use std::collections::HashMap;

use log::{debug, warn};
use u_rome_core::Hash32;

use crate::map::{LodLevel, MapData, Placement};

/// Largest distance (metres) between an entity and a batch instance accepted
/// as its parent. The documented median is 0.0 m; the value is a tolerance
/// chosen here, not a documented constant.
pub const BATCH_MATCH_TOLERANCE: f32 = 2.0;

/// Cell size of the spatial index used to find candidate files.
const INDEX_CELL: f32 = 512.0;
/// Bounding boxes spanning more cells than this per axis are not cell-indexed.
const MAX_INDEX_SPAN: i64 = 32;

/// An entity: index of its map in the slice given to [`LodTree::build`], and
/// its index in that map's `entities`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId {
    pub map: u32,
    pub entity: u32,
}

/// What a LOD link points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LodParent {
    Entity(EntityId),
    /// Instance of a prop batch (a `<name>_batchlod` model).
    PropInstance {
        map: u32,
        batch: u32,
        instance: u32,
    },
}

/// How the parent file of a link was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkSource {
    /// The child file names its parent by hash (confirmed rule).
    ParentHash,
    /// The child file names no parent; the file was chosen by the inferred
    /// candidate rule. Provisional.
    InferredFile,
    /// Batch parent matched by slot and position.
    BatchPosition,
}

/// One resolved child-to-parent link.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LodLink {
    pub child: EntityId,
    pub parent: LodParent,
    pub source: LinkSource,
    /// Distance between child and parent positions, metres.
    pub distance: f32,
    /// Whether the parent is exactly one LOD level up (always true for batch
    /// parents, whose level is not stored).
    pub level_ok: bool,
}

/// Why a link could not be resolved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnresolvedReason {
    /// The child file's parent hash names no loaded file.
    ParentFileNotFound { parent: Hash32 },
    /// The parent file exists but has no entity at `parentIndex`.
    ParentIndexOutOfRange {
        parent_map: u32,
        index: i32,
        entities: usize,
    },
    /// No parent hash and no candidate file fits.
    NoParentFile,
    /// No prop batch at that slot has the instance near the entity.
    BatchInstanceNotFound {
        /// Nearest candidate distance, when any candidate existed.
        nearest: Option<f32>,
    },
}

/// A child whose parent could not be found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnresolvedLink {
    pub child: EntityId,
    pub parent_index: i32,
    pub parent_instance: i32,
    pub reason: UnresolvedReason,
}

/// LOD parents and children across a set of map files.
#[derive(Clone, Debug, Default)]
pub struct LodTree {
    links: Vec<LodLink>,
    parent_of: HashMap<EntityId, usize>,
    children: HashMap<EntityId, Vec<EntityId>>,
    batch_children: HashMap<(u32, u32, u32), Vec<EntityId>>,
    unresolved: Vec<UnresolvedLink>,
    /// (child map, parent map) pairs chosen by the inferred file rule.
    pub inferred_files: Vec<(u32, u32)>,
    /// Parent hashes that name more than one loaded file (state variants);
    /// the best-fitting one was used.
    pub ambiguous_parents: Vec<(u32, Hash32)>,
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn cell_of(x: f32, y: f32) -> Option<(i64, i64)> {
    let cx = (x / INDEX_CELL).floor();
    let cy = (y / INDEX_CELL).floor();
    (cx.is_finite() && cy.is_finite() && cx.abs() < 1.0e9 && cy.abs() < 1.0e9)
        .then_some((cx as i64, cy as i64))
}

/// 2D bounds of a set of points, ignoring non-finite ones.
fn bounds2(points: impl Iterator<Item = [f32; 3]>) -> Option<([f32; 2], [f32; 2])> {
    let mut b: Option<([f32; 2], [f32; 2])> = None;
    for p in points {
        if !(p[0].is_finite() && p[1].is_finite()) {
            continue;
        }
        b = Some(match b {
            None => ([p[0], p[1]], [p[0], p[1]]),
            Some((lo, hi)) => (
                [lo[0].min(p[0]), lo[1].min(p[1])],
                [hi[0].max(p[0]), hi[1].max(p[1])],
            ),
        });
    }
    b
}

/// Cells covered by `bounds` expanded by `pad`, or `None` if too wide.
fn cells(bounds: ([f32; 2], [f32; 2]), pad: f32) -> Option<Vec<(i64, i64)>> {
    let lo = cell_of(bounds.0[0] - pad, bounds.0[1] - pad)?;
    let hi = cell_of(bounds.1[0] + pad, bounds.1[1] + pad)?;
    if hi.0 - lo.0 > MAX_INDEX_SPAN || hi.1 - lo.1 > MAX_INDEX_SPAN {
        return None;
    }
    let mut out = Vec::new();
    for x in lo.0..=hi.0 {
        for y in lo.1..=hi.1 {
            out.push((x, y));
        }
    }
    Some(out)
}

/// A child entity waiting for an entity parent.
#[derive(Clone, Copy)]
struct Pending {
    id: EntityId,
    index: i32,
    instance: i32,
    level: LodLevel,
    position: [f32; 3],
}

/// How well `parent` fits as the parent file of `children`.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
struct Score {
    /// Children whose slot exists, is one level up and has children.
    valid: usize,
    same_region: bool,
    /// Negative mean distance (higher is better).
    closeness: f32,
}

fn score(child_map: &MapData, parent: &MapData, children: &[Pending]) -> Score {
    let mut valid = 0usize;
    let mut total = 0.0f32;
    let mut n = 0usize;
    for c in children {
        let Some(p) = usize::try_from(c.index)
            .ok()
            .and_then(|i| parent.entities.get(i))
        else {
            continue;
        };
        let level_ok = match c.level.parent_level() {
            Some(l) => p.lod_level == l,
            None => matches!(c.level, LodLevel::Unknown(_)),
        };
        if level_ok && p.num_children > 0 {
            valid += 1;
        }
        let d = dist(c.position, p.position);
        if d.is_finite() {
            total += d;
            n += 1;
        }
    }
    let closeness = if n == 0 {
        f32::MIN
    } else {
        -(total / n as f32)
    };
    Score {
        valid,
        same_region: child_map.region_code.is_some() && child_map.region_code == parent.region_code,
        closeness,
    }
}

impl LodTree {
    /// Link every entity with a LOD parent across `maps`.
    ///
    /// Map indices in the result refer to positions in `maps`.
    pub fn build(maps: &[MapData]) -> LodTree {
        let mut tree = LodTree::default();
        let mut by_name: HashMap<Hash32, Vec<u32>> = HashMap::new();
        for (i, m) in maps.iter().enumerate() {
            if m.name != Hash32(0) {
                by_name.entry(m.name).or_default().push(i as u32);
            }
        }
        let file_index = FileIndex::new(maps);
        let batch_index = BatchIndex::new(maps);

        for (ci, child_map) in maps.iter().enumerate() {
            let ci = ci as u32;
            let mut entity_children = Vec::new();
            for (ei, e) in child_map.entities.iter().enumerate() {
                if e.parent_index < 0 {
                    continue;
                }
                let id = EntityId {
                    map: ci,
                    entity: ei as u32,
                };
                if e.parent_instance >= 0 {
                    tree.link_batch(maps, &batch_index, id, e);
                } else {
                    entity_children.push(Pending {
                        id,
                        index: e.parent_index,
                        instance: e.parent_instance,
                        level: e.lod_level,
                        position: e.position,
                    });
                }
            }
            if entity_children.is_empty() {
                continue;
            }
            let (parent, source) = if child_map.parent != Hash32(0) {
                let cands = by_name
                    .get(&child_map.parent)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if cands.len() > 1 {
                    tree.ambiguous_parents.push((ci, child_map.parent));
                }
                let best = cands
                    .iter()
                    .copied()
                    .filter(|&p| p != ci)
                    .filter_map(|p| {
                        Some((p, score(child_map, maps.get(p as usize)?, &entity_children)))
                    })
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                match best {
                    Some((p, _)) => (Some(p), LinkSource::ParentHash),
                    None => {
                        debug!(
                            "lod: map {} names parent {} which is not loaded",
                            child_map.name, child_map.parent
                        );
                        for c in &entity_children {
                            tree.unresolved.push(UnresolvedLink {
                                child: c.id,
                                parent_index: c.index,
                                parent_instance: c.instance,
                                reason: UnresolvedReason::ParentFileNotFound {
                                    parent: child_map.parent,
                                },
                            });
                        }
                        continue;
                    }
                }
            } else {
                let p = infer_parent_file(maps, &file_index, ci, &entity_children);
                if let Some(p) = p {
                    tree.inferred_files.push((ci, p));
                }
                (p, LinkSource::InferredFile)
            };
            let Some(pi) = parent else {
                for c in &entity_children {
                    tree.unresolved.push(UnresolvedLink {
                        child: c.id,
                        parent_index: c.index,
                        parent_instance: c.instance,
                        reason: UnresolvedReason::NoParentFile,
                    });
                }
                continue;
            };
            let Some(parent_map) = maps.get(pi as usize) else {
                continue;
            };
            for c in &entity_children {
                match usize::try_from(c.index)
                    .ok()
                    .and_then(|i| parent_map.entities.get(i).map(|p| (i, p)))
                {
                    Some((i, p)) => {
                        let parent = EntityId {
                            map: pi,
                            entity: i as u32,
                        };
                        tree.push_link(LodLink {
                            child: c.id,
                            parent: LodParent::Entity(parent),
                            source,
                            distance: dist(c.position, p.position),
                            level_ok: c.level.parent_level() == Some(p.lod_level),
                        });
                    }
                    None => tree.unresolved.push(UnresolvedLink {
                        child: c.id,
                        parent_index: c.index,
                        parent_instance: c.instance,
                        reason: UnresolvedReason::ParentIndexOutOfRange {
                            parent_map: pi,
                            index: c.index,
                            entities: parent_map.entities.len(),
                        },
                    }),
                }
            }
        }
        if !tree.unresolved.is_empty() {
            warn!(
                "lod: {} links resolved, {} unresolved",
                tree.links.len(),
                tree.unresolved.len()
            );
        }
        tree
    }

    fn push_link(&mut self, link: LodLink) {
        let idx = self.links.len();
        self.parent_of.insert(link.child, idx);
        match link.parent {
            LodParent::Entity(p) => self.children.entry(p).or_default().push(link.child),
            LodParent::PropInstance {
                map,
                batch,
                instance,
            } => self
                .batch_children
                .entry((map, batch, instance))
                .or_default()
                .push(link.child),
        }
        self.links.push(link);
    }

    fn link_batch(&mut self, maps: &[MapData], index: &BatchIndex, id: EntityId, e: &Placement) {
        let mut best: Option<(u32, u32, f32)> = None;
        if let (Ok(slot), Ok(inst)) = (
            u32::try_from(e.parent_index),
            usize::try_from(e.parent_instance),
        ) {
            for &(m, b) in index.candidates(slot, e.position) {
                let Some(batch) = maps
                    .get(m as usize)
                    .and_then(|mm| mm.prop_batches.get(b as usize))
                else {
                    continue;
                };
                let Some(instance) = batch.instances.get(inst) else {
                    continue;
                };
                let d = dist(instance.translation(), e.position);
                if d.is_finite() && best.is_none_or(|(_, _, bd)| d < bd) {
                    best = Some((m, b, d));
                }
            }
        }
        match best {
            Some((map, batch, d)) if d <= BATCH_MATCH_TOLERANCE => self.push_link(LodLink {
                child: id,
                parent: LodParent::PropInstance {
                    map,
                    batch,
                    instance: e.parent_instance as u32,
                },
                source: LinkSource::BatchPosition,
                distance: d,
                level_ok: true,
            }),
            other => self.unresolved.push(UnresolvedLink {
                child: id,
                parent_index: e.parent_index,
                parent_instance: e.parent_instance,
                reason: UnresolvedReason::BatchInstanceNotFound {
                    nearest: other.map(|(_, _, d)| d),
                },
            }),
        }
    }

    pub fn links(&self) -> &[LodLink] {
        &self.links
    }

    pub fn unresolved(&self) -> &[UnresolvedLink] {
        &self.unresolved
    }

    /// The link to this entity's parent, if resolved.
    pub fn parent(&self, id: EntityId) -> Option<&LodLink> {
        self.links.get(*self.parent_of.get(&id)?)
    }

    /// Entity children of an entity.
    pub fn children(&self, id: EntityId) -> &[EntityId] {
        self.children.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Entity children of a prop batch instance.
    pub fn batch_children(&self, map: u32, batch: u32, instance: u32) -> &[EntityId] {
        self.batch_children
            .get(&(map, batch, instance))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Entities with no resolved parent (top of each LOD chain, orphans and
    /// unresolved children).
    pub fn roots(&self, maps: &[MapData]) -> Vec<EntityId> {
        let mut out = Vec::new();
        for (mi, m) in maps.iter().enumerate() {
            for ei in 0..m.entities.len() {
                let id = EntityId {
                    map: mi as u32,
                    entity: ei as u32,
                };
                if !self.parent_of.contains_key(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Entities whose `numChildren` differs from the number of linked
    /// children: `(entity, numChildren, linked)`.
    pub fn child_count_mismatches(&self, maps: &[MapData]) -> Vec<(EntityId, u32, usize)> {
        let mut out = Vec::new();
        for (mi, m) in maps.iter().enumerate() {
            for (ei, e) in m.entities.iter().enumerate() {
                let id = EntityId {
                    map: mi as u32,
                    entity: ei as u32,
                };
                let linked = self.children(id).len();
                if linked != e.num_children as usize {
                    out.push((id, e.num_children, linked));
                }
            }
        }
        out
    }

    /// Whether every link with a parent resolved.
    pub fn is_complete(&self) -> bool {
        self.unresolved.is_empty()
    }
}

/// Spatial index of map files by the 2D bounds of their entities.
struct FileIndex {
    cells: HashMap<(i64, i64), Vec<u32>>,
    /// Files too wide for the cell index; always candidates.
    wide: Vec<u32>,
}

impl FileIndex {
    fn new(maps: &[MapData]) -> Self {
        let mut cells: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        let mut wide = Vec::new();
        for (i, m) in maps.iter().enumerate() {
            let Some(b) = bounds2(m.entities.iter().map(|e| e.position)) else {
                continue;
            };
            match self::cells(b, 0.0) {
                Some(cs) => {
                    for c in cs {
                        cells.entry(c).or_default().push(i as u32);
                    }
                }
                None => wide.push(i as u32),
            }
        }
        Self { cells, wide }
    }

    fn near(&self, b: ([f32; 2], [f32; 2])) -> Vec<u32> {
        let mut out: Vec<u32> = self.wide.clone();
        match cells(b, INDEX_CELL) {
            Some(cs) => {
                for c in cs {
                    if let Some(v) = self.cells.get(&c) {
                        out.extend_from_slice(v);
                    }
                }
            }
            None => {
                for v in self.cells.values() {
                    out.extend_from_slice(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Inferred parent file for a child file with no parent hash.
fn infer_parent_file(
    maps: &[MapData],
    index: &FileIndex,
    child: u32,
    children: &[Pending],
) -> Option<u32> {
    let child_map = maps.get(child as usize)?;
    let b = bounds2(children.iter().map(|c| c.position))?;
    let n = maps.len() as u32;
    let mut best: Option<(u32, Score, u32)> = None;
    for p in index.near(b) {
        if p == child {
            continue;
        }
        let Some(pm) = maps.get(p as usize) else {
            continue;
        };
        let s = score(child_map, pm, children);
        // Every child's slot must be a plausible parent.
        if s.valid < children.len() {
            continue;
        }
        // Tie-break: nearest following file in load order.
        let follow = (p + n - child) % n.max(1);
        let better = match &best {
            None => true,
            Some((_, bs, bf)) => match s.partial_cmp(bs) {
                Some(std::cmp::Ordering::Greater) => true,
                Some(std::cmp::Ordering::Equal) => follow < *bf,
                _ => false,
            },
        };
        if better {
            best = Some((p, s, follow));
        }
    }
    if let Some((p, s, _)) = best {
        debug!(
            "lod: inferred parent file {} for map {} ({} children, mean distance {:.2} m)",
            p, child, s.valid, -s.closeness
        );
    }
    best.map(|(p, _, _)| p)
}

/// (map, batch) index pair.
type BatchRef = (u32, u32);
/// (LOD slot, index cell).
type SlotCell = (u32, (i64, i64));

/// Index of prop batches by LOD slot and position.
struct BatchIndex {
    cells: HashMap<SlotCell, Vec<BatchRef>>,
    wide: HashMap<u32, Vec<BatchRef>>,
}

impl BatchIndex {
    fn new(maps: &[MapData]) -> Self {
        let mut cells: HashMap<SlotCell, Vec<BatchRef>> = HashMap::new();
        let mut wide: HashMap<u32, Vec<BatchRef>> = HashMap::new();
        for (mi, m) in maps.iter().enumerate() {
            for (bi, b) in m.prop_batches.iter().enumerate() {
                let slot = (m.entities.len() + bi) as u32;
                // Bounds from the instances themselves: robust against an
                // unknown bbox layout.
                let Some(bb) = bounds2(b.instances.iter().map(|i| i.translation())) else {
                    continue;
                };
                let entry = (mi as u32, bi as u32);
                match self::cells(bb, BATCH_MATCH_TOLERANCE) {
                    Some(cs) => {
                        for c in cs {
                            cells.entry((slot, c)).or_default().push(entry);
                        }
                    }
                    None => wide.entry(slot).or_default().push(entry),
                }
            }
        }
        Self { cells, wide }
    }

    fn candidates(&self, slot: u32, pos: [f32; 3]) -> impl Iterator<Item = &(u32, u32)> {
        let cell = cell_of(pos[0], pos[1]);
        let a = cell
            .and_then(|c| self.cells.get(&(slot, c)))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let b = self.wide.get(&slot).map(Vec::as_slice).unwrap_or(&[]);
        a.iter().chain(b.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::tests::{EntitySpec, Fixture, build_fixture};
    use crate::map::{PlacementKind, PropBatch, PropInstance};
    use crate::meta::MetaResource;
    use u_rome_core::joaat;

    fn decode(fx: &Fixture) -> MapData {
        let (sys, gfx) = build_fixture(fx).build_pages();
        MapData::from_meta(&MetaResource::parse_pages(&sys, &gfx).unwrap()).unwrap()
    }

    fn entity(pos: [f32; 3], lod: LodLevel, parent_index: i32, num_children: u32) -> Placement {
        Placement {
            archetype: Hash32(1),
            position: pos,
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale_xy: 1.0,
            scale_z: 1.0,
            flags: 0,
            lod_level: lod,
            parent_index,
            parent_instance: -1,
            num_children,
            lod_dist: -1.0,
            child_lod_dist: 0.0,
            guid: 0,
            parent_dep: -1,
            kind: PlacementKind::Entity,
        }
    }

    #[test]
    fn links_two_files_and_reports_unresolved() {
        let mut lod_a = EntitySpec::new("a_lod", [0.0, 0.0, 0.0], 1);
        lod_a.num_children = 1;
        let mut lod_b = EntitySpec::new("b_lod", [50.0, 0.0, 0.0], 1);
        lod_b.num_children = 1;
        let parent = decode(&Fixture {
            name: "dt1_01_lod",
            parent: "",
            entities: vec![lod_a, lod_b],
            with_batches: false,
            mlo: false,
        });
        let mut hd_a = EntitySpec::new("a", [0.3, 0.0, 0.0], 0);
        hd_a.parent_index = 0;
        let mut hd_b = EntitySpec::new("b", [50.0, 0.4, 0.0], 0);
        hd_b.parent_index = 1;
        let mut hd_bad = EntitySpec::new("c", [10.0, 0.0, 0.0], 0);
        hd_bad.parent_index = 9;
        let child = decode(&Fixture {
            name: "dt1_01",
            parent: "dt1_01_lod",
            entities: vec![hd_a, hd_b, hd_bad, EntitySpec::new("orphan", [1.0; 3], 5)],
            with_batches: false,
            mlo: false,
        });
        let mut lost = EntitySpec::new("lost", [3.0; 3], 0);
        lost.parent_index = 0;
        let stray = decode(&Fixture {
            name: "dt1_02",
            parent: "dt1_02_lod",
            entities: vec![lost],
            with_batches: false,
            mlo: false,
        });
        let maps = vec![child, parent, stray];
        let tree = LodTree::build(&maps);

        assert_eq!(tree.links().len(), 2);
        let a = tree.parent(EntityId { map: 0, entity: 0 }).unwrap();
        assert_eq!(a.parent, LodParent::Entity(EntityId { map: 1, entity: 0 }));
        assert_eq!(a.source, LinkSource::ParentHash);
        assert!(a.level_ok);
        assert!((a.distance - 0.3).abs() < 1e-4);
        assert_eq!(
            tree.children(EntityId { map: 1, entity: 1 }),
            &[EntityId { map: 0, entity: 1 }]
        );

        assert_eq!(tree.unresolved().len(), 2);
        assert!(
            tree.unresolved()
                .iter()
                .any(|u| u.child == EntityId { map: 0, entity: 2 }
                    && u.reason
                        == UnresolvedReason::ParentIndexOutOfRange {
                            parent_map: 1,
                            index: 9,
                            entities: 2
                        })
        );
        assert!(
            tree.unresolved()
                .iter()
                .any(|u| u.child == EntityId { map: 2, entity: 0 }
                    && u.reason
                        == UnresolvedReason::ParentFileNotFound {
                            parent: joaat("dt1_02_lod")
                        })
        );
        assert!(!tree.is_complete());

        let roots = tree.roots(&maps);
        assert!(roots.contains(&EntityId { map: 0, entity: 3 }));
        assert!(!roots.contains(&EntityId { map: 0, entity: 0 }));
        assert!(tree.child_count_mismatches(&maps).is_empty());
    }

    #[test]
    fn infers_parent_file_without_hash() {
        let far = MapData {
            name: Hash32(10),
            entities: vec![entity([5000.0, 0.0, 0.0], LodLevel::Lod, -1, 1)],
            ..MapData::default()
        };
        let near = MapData {
            name: Hash32(11),
            entities: vec![entity([1.0, 0.0, 0.0], LodLevel::Lod, -1, 1)],
            ..MapData::default()
        };
        let wrong_level = MapData {
            name: Hash32(12),
            entities: vec![entity([0.5, 0.0, 0.0], LodLevel::Slod2, -1, 1)],
            ..MapData::default()
        };
        let child = MapData {
            name: Hash32(13),
            entities: vec![entity([0.0, 0.0, 0.0], LodLevel::Hd, 0, 0)],
            ..MapData::default()
        };
        let maps = vec![far, near, wrong_level, child];
        let tree = LodTree::build(&maps);
        let link = tree.parent(EntityId { map: 3, entity: 0 }).unwrap();
        assert_eq!(
            link.parent,
            LodParent::Entity(EntityId { map: 1, entity: 0 })
        );
        assert_eq!(link.source, LinkSource::InferredFile);
        assert_eq!(tree.inferred_files, vec![(3, 1)]);
    }

    #[test]
    fn links_batch_instances_by_slot_and_position() {
        let inst = |t: [f32; 3]| PropInstance {
            rows: [
                [1.0, 0.0, 0.0, t[0]],
                [0.0, 1.0, 0.0, t[1]],
                [0.0, 0.0, 1.0, t[2]],
            ],
            tint: [0; 4],
            unknown_30: 0.0,
            unknown_38: 0.0,
            unknown_3c: 0,
            unknown_3e: [0; 2],
        };
        let batch = PropBatch {
            archetype: joaat("tree_01_batchlod"),
            position: [0.0; 3],
            lod_dist: 300.0,
            bbox_min: [0.0; 3],
            bbox_max: [0.0; 3],
            instances: vec![inst([100.0, 100.0, 0.0]), inst([110.0, 100.0, 0.0])],
            provisional: true,
        };
        let batch_file = MapData {
            name: Hash32(20),
            entities: vec![entity([0.0; 3], LodLevel::Lod, -1, 0)],
            prop_batches: vec![batch],
            ..MapData::default()
        };
        let mut hd = entity([110.0, 100.0, 0.0], LodLevel::Hd, 1, 0);
        hd.parent_instance = 1;
        let mut hd_miss = entity([400.0, 400.0, 0.0], LodLevel::Hd, 1, 0);
        hd_miss.parent_instance = 0;
        let child = MapData {
            name: Hash32(21),
            entities: vec![hd, hd_miss],
            ..MapData::default()
        };
        let maps = vec![batch_file, child];
        let tree = LodTree::build(&maps);
        let link = tree.parent(EntityId { map: 1, entity: 0 }).unwrap();
        assert_eq!(
            link.parent,
            LodParent::PropInstance {
                map: 0,
                batch: 0,
                instance: 1
            }
        );
        assert_eq!(
            tree.batch_children(0, 0, 1),
            &[EntityId { map: 1, entity: 0 }]
        );
        assert_eq!(tree.unresolved().len(), 1);
        assert!(matches!(
            tree.unresolved()[0].reason,
            UnresolvedReason::BatchInstanceNotFound { .. }
        ));
    }

    #[test]
    fn garbage_maps_do_not_panic() {
        let mut e = entity([f32::NAN, f32::INFINITY, 0.0], LodLevel::Unknown(99), 3, 0);
        e.parent_instance = 7;
        let m = MapData {
            name: Hash32(1),
            parent: Hash32(1),
            entities: vec![e, entity([1e30, -1e30, 0.0], LodLevel::Hd, i32::MAX, 0)],
            ..MapData::default()
        };
        let maps = vec![m.clone(), m];
        let tree = LodTree::build(&maps);
        assert!(tree.links().is_empty());
        assert!(!tree.unresolved().is_empty());
    }
}
