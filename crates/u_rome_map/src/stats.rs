//! Summary counts over decoded map files.

use crate::lod::{LinkSource, LodTree};
use crate::map::{MapData, PlacementKind};
use crate::raster::{Bounds2, map_points};

/// Counts over a set of map files, for Studio's map panel and logs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapStats {
    pub map_files: usize,
    pub entities: usize,
    /// Entities by LOD level: index = raw level 0..=6 (HD, LOD, SLOD1-3,
    /// ORPHAN_HD, SLOD4), 7 = unknown.
    pub entities_by_lod: [usize; 8],
    pub mlo_instances: usize,
    pub prop_batches: usize,
    pub prop_instances: usize,
    pub grass_batches: usize,
    pub grass_instances: usize,
    /// Files naming a parent map file.
    pub files_with_parent: usize,
    /// Files whose decode relied on an uncertain rule.
    pub provisional_files: usize,
    /// Total warnings across files.
    pub warnings: usize,
    /// Entities whose archetype hash is 0.
    pub empty_archetypes: usize,
    /// 2D bounds of every placed point.
    pub bounds: Option<Bounds2>,
    /// LOD links by source, when computed with [`MapStats::with_tree`].
    pub links_parent_hash: usize,
    pub links_inferred_file: usize,
    pub links_batch: usize,
    pub unresolved_links: usize,
}

impl MapStats {
    pub fn from_maps(maps: &[MapData]) -> Self {
        let mut s = MapStats::default();
        for m in maps {
            s.add(m);
        }
        s
    }

    /// Add one map file to the counts.
    pub fn add(&mut self, map: &MapData) {
        self.map_files += 1;
        self.entities += map.entities.len();
        for e in &map.entities {
            if let Some(c) = self.entities_by_lod.get_mut(e.lod_level.slot()) {
                *c += 1;
            }
            if e.kind == PlacementKind::MloInstance {
                self.mlo_instances += 1;
            }
            if e.archetype.0 == 0 {
                self.empty_archetypes += 1;
            }
        }
        self.prop_batches += map.prop_batches.len();
        self.prop_instances += map
            .prop_batches
            .iter()
            .map(|b| b.instances.len())
            .sum::<usize>();
        self.grass_batches += map.grass_batches.len();
        self.grass_instances += map
            .grass_batches
            .iter()
            .map(|b| b.instances.len())
            .sum::<usize>();
        if map.parent.0 != 0 {
            self.files_with_parent += 1;
        }
        if map.provisional {
            self.provisional_files += 1;
        }
        self.warnings += map.warnings.len();
        if let Some(b) = Bounds2::from_points(map_points(map)) {
            self.bounds = Some(match self.bounds {
                None => b,
                Some(a) => Bounds2::new(
                    [a.min[0].min(b.min[0]), a.min[1].min(b.min[1])],
                    [a.max[0].max(b.max[0]), a.max[1].max(b.max[1])],
                ),
            });
        }
    }

    /// Fill in the LOD link counts from a tree.
    pub fn with_tree(mut self, tree: &LodTree) -> Self {
        for l in tree.links() {
            match l.source {
                LinkSource::ParentHash => self.links_parent_hash += 1,
                LinkSource::InferredFile => self.links_inferred_file += 1,
                LinkSource::BatchPosition => self.links_batch += 1,
            }
        }
        self.unresolved_links = tree.unresolved().len();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::tests::{build_fixture, sample};
    use crate::meta::MetaResource;

    #[test]
    fn counts_fixture() {
        let (sys, gfx) = build_fixture(&sample()).build_pages();
        let map = MapData::from_meta(&MetaResource::parse_pages(&sys, &gfx).unwrap()).unwrap();
        let maps = vec![map];
        let tree = LodTree::build(&maps);
        let s = MapStats::from_maps(&maps).with_tree(&tree);
        assert_eq!(s.map_files, 1);
        assert_eq!(s.entities, 2);
        assert_eq!(s.entities_by_lod[0], 1);
        assert_eq!(s.entities_by_lod[1], 1);
        assert_eq!(s.prop_batches, 1);
        assert_eq!(s.prop_instances, 2);
        assert_eq!(s.grass_batches, 1);
        assert_eq!(s.grass_instances, 2);
        assert_eq!(s.files_with_parent, 1);
        assert_eq!(s.provisional_files, 1);
        // The parent "dt1_01_lod" is not loaded.
        assert_eq!(s.unresolved_links, 1);
        let b = s.bounds.unwrap();
        assert_eq!(b.min, [-10.0, -10.0]);
        assert_eq!(b.max, [11.0, 12.0]);
    }
}
