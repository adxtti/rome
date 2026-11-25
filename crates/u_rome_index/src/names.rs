//! In-memory name registry: hash lists and XML-derived names, looked up by
//! either JOAAT variant. Persist it with [`crate::Index::import_names`].

use std::collections::HashMap;

use u_rome_core::{joaat, joaat_lower};

use crate::types::NameSource;
use crate::xml::names_from_xml;

/// Longest name accepted (bytes). Longer lines are skipped.
pub const MAX_NAME_LEN: usize = 1024;
/// Upper bound on names held by one registry.
pub const MAX_REGISTRY_NAMES: usize = 20_000_000;

/// One known name with both JOAAT variants precomputed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRecord {
    pub name: String,
    pub joaat: u32,
    pub joaat_lower: u32,
    /// Hash given by a list that matched neither JOAAT variant. Kept because
    /// the GTA VI hashing convention is not settled; matches through it are
    /// reported as [`crate::NameMatch::ListedHash`] (provisional).
    pub listed_hash: Option<u32>,
    pub source: NameSource,
}

/// Counts from loading a name list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// Names newly added to the registry.
    pub added: usize,
    /// `HASH<TAB>name` lines whose hash equals one of the JOAAT variants.
    pub verified: usize,
    /// `HASH<TAB>name` lines whose hash matched neither variant.
    pub unverified: usize,
    /// Lines that could not be parsed (or exceeded limits).
    pub skipped: usize,
}

/// Hash -> names lookup built from lists and XML payloads.
#[derive(Clone, Debug, Default)]
pub struct NameRegistry {
    records: Vec<NameRecord>,
    by_name: HashMap<String, usize>,
    by_hash: HashMap<u32, Vec<usize>>,
}

impl NameRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &NameRecord> {
        self.records.iter()
    }

    /// Add a name. Returns true if it was new.
    pub fn insert(&mut self, name: &str, source: NameSource) -> bool {
        self.insert_inner(name, None, source)
    }

    /// Add a name with a hash from a list. If the hash equals neither JOAAT
    /// variant it is stored as `listed_hash`. Returns true if the name was new.
    pub fn insert_with_hash(&mut self, name: &str, hash: u32, source: NameSource) -> bool {
        self.insert_inner(name, Some(hash), source)
    }

    fn insert_inner(&mut self, name: &str, hash: Option<u32>, source: NameSource) -> bool {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return false;
        }
        let j = joaat(name).0;
        let jl = joaat_lower(name).0;
        let listed = hash.filter(|h| *h != j && *h != jl);
        if let Some(&idx) = self.by_name.get(name) {
            if let (Some(h), Some(rec)) = (listed, self.records.get_mut(idx))
                && rec.listed_hash.is_none()
            {
                rec.listed_hash = Some(h);
                self.by_hash.entry(h).or_default().push(idx);
            }
            return false;
        }
        if self.records.len() >= MAX_REGISTRY_NAMES {
            log::warn!("name registry full ({MAX_REGISTRY_NAMES} names), dropping {name:?}");
            return false;
        }
        let idx = self.records.len();
        self.records.push(NameRecord {
            name: name.to_owned(),
            joaat: j,
            joaat_lower: jl,
            listed_hash: listed,
            source,
        });
        self.by_name.insert(name.to_owned(), idx);
        self.by_hash.entry(j).or_default().push(idx);
        if jl != j {
            self.by_hash.entry(jl).or_default().push(idx);
        }
        if let Some(h) = listed {
            self.by_hash.entry(h).or_default().push(idx);
        }
        true
    }

    /// All names whose case-sensitive JOAAT, lowercase JOAAT or listed hash
    /// equals `hash`, in insertion order.
    pub fn lookup(&self, hash: u32) -> impl Iterator<Item = &NameRecord> {
        self.by_hash
            .get(&hash)
            .into_iter()
            .flatten()
            .filter_map(|&i| self.records.get(i))
    }

    /// Load a name list. Accepted line forms (after trimming):
    ///
    /// - `0xHASH<TAB>name` (also `HASH<TAB>name` with 8 bare hex digits, and
    ///   `name<TAB>0xHASH`)
    /// - `name`
    ///
    /// Blank lines and lines starting with `#` or `//` are ignored.
    pub fn load_list(&mut self, text: &str, source: NameSource) -> LoadReport {
        let mut report = LoadReport::default();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim_matches(|c: char| c == '\r' || c == ' ' || c == '\u{feff}');
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            if let Some((a, b)) = line.split_once('\t') {
                let (hash, name) = match (parse_hash(a), parse_hash(b)) {
                    (Some(h), _) => (h, b.trim()),
                    (None, Some(h)) => (h, a.trim()),
                    (None, None) => {
                        log::debug!("name list line {}: no hash in {line:?}", lineno + 1);
                        report.skipped += 1;
                        continue;
                    }
                };
                if name.is_empty() || name.len() > MAX_NAME_LEN || name.contains('\t') {
                    report.skipped += 1;
                    continue;
                }
                if hash == joaat(name).0 || hash == joaat_lower(name).0 {
                    report.verified += 1;
                } else {
                    report.unverified += 1;
                }
                if self.insert_with_hash(name, hash, source) {
                    report.added += 1;
                }
            } else if line.len() > MAX_NAME_LEN {
                report.skipped += 1;
            } else if self.insert(line, source) {
                report.added += 1;
            }
        }
        if report.unverified > 0 {
            log::warn!(
                "name list: {} of {} hashed lines match neither JOAAT variant; \
                 kept as unverified listed hashes",
                report.unverified,
                report.verified + report.unverified
            );
        }
        report
    }

    /// [`Self::load_list`] over raw bytes (invalid UTF-8 replaced).
    pub fn load_list_bytes(&mut self, bytes: &[u8], source: NameSource) -> LoadReport {
        self.load_list(&String::from_utf8_lossy(bytes), source)
    }

    /// Add every name [`names_from_xml`] finds in `bytes`. Returns how many were new.
    pub fn add_xml(&mut self, bytes: &[u8]) -> usize {
        names_from_xml(bytes)
            .iter()
            .filter(|n| self.insert(n, NameSource::Xml))
            .count()
    }
}

/// Parse `0x1234ABCD` or a bare 8-digit hex hash.
pub(crate) fn parse_hash(s: &str) -> Option<u32> {
    let s = s.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| (s.len() == 8).then_some(s))?;
    if hex.is_empty() || hex.len() > 8 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_lists() {
        let mut reg = NameRegistry::new();
        let list = "# comment\n0xB779A091\tadder\nADDER\n0x12345678\tmystery\n\
                    weird line\twithout hash\nzentorno\t0x0\n\n";
        let r = reg.load_list(list, NameSource::HashList);
        assert_eq!(r.verified, 1);
        assert_eq!(r.unverified, 2);
        assert_eq!(r.skipped, 1);
        assert_eq!(r.added, 4);
        let hits: Vec<_> = reg.lookup(0xB779_A091).map(|r| r.name.as_str()).collect();
        assert_eq!(hits, ["adder", "ADDER"]);
        let m: Vec<_> = reg.lookup(0x1234_5678).collect();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].listed_hash, Some(0x1234_5678));
        assert_eq!(reg.lookup(joaat("ADDER").0).count(), 1);
        assert!(!reg.insert("adder", NameSource::Manual));
        assert!(!reg.insert("", NameSource::Manual));
        assert!(!reg.insert(&"x".repeat(MAX_NAME_LEN + 1), NameSource::Manual));
    }

    #[test]
    fn parses_hashes() {
        assert_eq!(parse_hash("0xb779a091"), Some(0xB779_A091));
        assert_eq!(parse_hash("B779A091"), Some(0xB779_A091));
        assert_eq!(parse_hash("adder"), None);
        assert_eq!(parse_hash("0x123456789"), None);
        assert_eq!(parse_hash("0x"), None);
    }
}
