//! Hash-to-name resolution.
//!
//! Names are 32-bit JOAAT hashes. Sources disagree on case: rage9-re and
//! GTA6-Asset-Info 08-meta report case-sensitive hashes for field names,
//! rage-tools-vi reports `JOAAT(lowercase name)`. [`HashMapNames::add`]
//! therefore registers both variants, with the case-sensitive one winning a
//! collision.

use std::collections::HashMap;

use u_rome_core::{Hash32, joaat, joaat_lower};

/// Resolves a hash to a readable name.
pub trait NameLookup {
    fn name(&self, h: Hash32) -> Option<&str>;
}

/// Resolves nothing; every hash is printed as `0xHASH`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoNames;

impl NameLookup for NoNames {
    fn name(&self, _h: Hash32) -> Option<&str> {
        None
    }
}

/// A `HashMap`-backed name registry.
#[derive(Clone, Debug, Default)]
pub struct HashMapNames {
    /// hash -> (name, registered from the case-sensitive hash)
    map: HashMap<Hash32, (String, bool)>,
}

impl HashMapNames {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `name` under both `joaat(name)` (as written) and
    /// `joaat_lower(name)` (stored as the lowercased name). An existing
    /// case-sensitive entry is never replaced by a lowercase one.
    pub fn add(&mut self, name: &str) {
        let exact = joaat(name);
        match self.map.get(&exact) {
            Some((_, true)) => {}
            _ => {
                self.map.insert(exact, (name.to_owned(), true));
            }
        }
        let lower = joaat_lower(name);
        if lower != exact {
            self.map
                .entry(lower)
                .or_insert_with(|| (name.to_ascii_lowercase(), false));
        }
    }

    /// Register an explicit hash/name pair (e.g. from a curated TSV),
    /// replacing any existing entry.
    pub fn insert(&mut self, hash: Hash32, name: impl Into<String>) {
        self.map.insert(hash, (name.into(), true));
    }

    /// Add every non-empty line of `text` that does not start with `#`.
    pub fn add_lines(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                self.add(line);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// A registry preloaded with [`KNOWN_NAMES`].
    pub fn with_known_names() -> Self {
        let mut n = Self::new();
        for name in KNOWN_NAMES {
            n.add(name);
        }
        n
    }
}

impl<S: AsRef<str>> FromIterator<S> for HashMapNames {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut n = Self::new();
        for s in iter {
            n.add(s.as_ref());
        }
        n
    }
}

impl NameLookup for HashMapNames {
    fn name(&self, h: Hash32) -> Option<&str> {
        self.map.get(&h).map(|(s, _)| s.as_str())
    }
}

/// Names confirmed by hash in public notes. Each was re-hashed with `joaat`
/// and matches case-sensitively (see the unit test).
///
/// - script.rpf DOM fields: rage9-re `docs/SCRIPT_FORMAT.md` section 3.
/// - `Position`, `Flags`, `Key`, `Item`, `CMapData`: GTA6-Asset-Info
///   `formats/08-meta.md` (facts only).
pub const KNOWN_NAMES: &[&str] = &[
    "attributes",
    "dataNodes",
    "name",
    "parentIndex",
    "childIndex",
    "siblingIndex",
    "previousSiblingIndex",
    "attributeStart",
    "attributeCount",
    "valueHash",
    "x",
    "y",
    "z",
    "root",
    "tag",
    "id",
    "true",
    "false",
    "Position",
    "Flags",
    "Key",
    "Item",
    "CMapData",
];

/// Hashes of the compiled binary-XML DOM schema shared by all 1,012
/// `script.rpf` documents (rage9-re `docs/SCRIPT_FORMAT.md` section 3).
pub mod script_dom {
    use u_rome_core::Hash32;

    /// Root document struct, 0x50 bytes, five `0x5c` arrays.
    pub const ROOT_STRUCT: Hash32 = Hash32(0xde39_6fe2);
    /// Tree node struct, 0x10 bytes.
    pub const NODE_STRUCT: Hash32 = Hash32(0x0139_3259);
    /// Attribute struct, 0x10 bytes.
    pub const ATTRIBUTE_STRUCT: Hash32 = Hash32(0x85b4_c631);
    /// 3-component vector struct (three `0x14` members holding f32 bits).
    pub const VECTOR_STRUCT: Hash32 = Hash32(0xaeb6_23d8);
    /// Attribute value-type enum. rage9-re: 0 string index, 1 vector index,
    /// 2 f32, 3 i32, 4 bool, 5 u64 index. rage-tools-vi labels the same
    /// counts differently (int/float/hash/bool/byte), so the meaning of each
    /// value is provisional.
    pub const VALUE_TYPE_ENUM: Hash32 = Hash32(0x649e_cf8e);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_hash_case_sensitively() {
        let expected: &[(&str, u32)] = &[
            ("attributes", 0x749d_bb9e),
            ("dataNodes", 0x2260_b8be),
            ("name", 0xe6e1_7f14),
            ("parentIndex", 0xd892_31bd),
            ("childIndex", 0x0129_6aed),
            ("siblingIndex", 0x881b_f649),
            ("previousSiblingIndex", 0xf330_47d9),
            ("attributeStart", 0x0334_1d59),
            ("attributeCount", 0x7966_1755),
            ("valueHash", 0x817e_d7a4),
            ("x", 0x9303_a5e5),
            ("y", 0x8095_0108),
            ("z", 0xb660_6c9e),
            ("root", 0xc090_afad),
            ("tag", 0x1aec_de4d),
            ("id", 0x1b60_404d),
            ("true", 0xb2f3_5c25),
            ("false", 0xfe25_ef42),
            ("Position", 0x0c92_8991),
            ("Flags", 0x4b62_1302),
            ("Key", 0x6098_a50e),
            ("Item", 0x063f_a3f2),
            ("CMapData", 0xd359_3fa6),
        ];
        assert_eq!(expected.len(), KNOWN_NAMES.len());
        let names = HashMapNames::with_known_names();
        for &(name, hash) in expected {
            assert_eq!(joaat(name), Hash32(hash), "{name}");
            assert_eq!(names.name(Hash32(hash)), Some(name));
        }
    }

    #[test]
    fn both_case_variants_resolve() {
        let mut n = HashMapNames::new();
        n.add("Position");
        assert_eq!(n.name(joaat("Position")), Some("Position"));
        assert_eq!(n.name(joaat_lower("Position")), Some("position"));
        // A later exact registration of the lowercase spelling wins.
        n.add("position");
        assert_eq!(n.name(joaat("position")), Some("position"));
        // A lowercase variant never overrides an exact entry.
        let mut n = HashMapNames::new();
        n.add("flags");
        n.add("FLAGS");
        assert_eq!(n.name(joaat("flags")), Some("flags"));
        assert_eq!(n.name(joaat("FLAGS")), Some("FLAGS"));
        assert_eq!(NoNames.name(joaat("flags")), None);
    }

    #[test]
    fn lines_and_iter() {
        let mut n = HashMapNames::new();
        n.add_lines("# comment\n\n  alpha \nBeta\n");
        assert_eq!(n.name(joaat("alpha")), Some("alpha"));
        assert_eq!(n.name(joaat("Beta")), Some("Beta"));
        assert_eq!(n.name(joaat("beta")), Some("beta"));
        let m: HashMapNames = ["gamma"].into_iter().collect();
        assert_eq!(m.len(), 1);
    }
}
