//! Flat entry index and query filtering behind the Search panel.
//!
//! Query syntax (terms are AND-ed, case-insensitive):
//! - `word`        substring of the entry path
//! - `-word`       path must not contain `word`
//! - `0x1A2B3C4D`  name hash or offset equals the value (or path contains it)
//! - `kind:tex`    kind label or tag starts with `tex`
//! - `>1mb` `<10k` decoded size bounds (`b`, `k`/`kb`, `m`/`mb`, `g`/`gb`)
//! - `codec:zstd`  storage codec
//! - `is:provisional`, `is:unnamed`, `is:container`

use std::collections::HashMap;

use u_rome_core::AssetKind;

use crate::backend::{EntryId, EntryInfo};
use crate::jobs::JobContext;

/// Short fixed-width tag shown next to entries.
pub fn kind_tag(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Archive => "ARC",
        AssetKind::TextureStore => "TXS",
        AssetKind::TextureTable => "TXT",
        AssetKind::Pso => "PSO",
        AssetKind::MetaResource => "META",
        AssetKind::Audio => "AUD",
        AssetKind::Video => "VID",
        AssetKind::MeshBlob => "MESH",
        AssetKind::Xml => "XML",
        AssetKind::Unknown => "BIN",
    }
}

/// All kinds in display order.
pub const ALL_KINDS: [AssetKind; 10] = [
    AssetKind::Archive,
    AssetKind::TextureStore,
    AssetKind::TextureTable,
    AssetKind::Pso,
    AssetKind::MetaResource,
    AssetKind::Audio,
    AssetKind::Video,
    AssetKind::MeshBlob,
    AssetKind::Xml,
    AssetKind::Unknown,
];

/// Every entry of a backend, flattened, with precomputed search keys.
#[derive(Default)]
pub struct SearchIndex {
    rows: Vec<EntryInfo>,
    paths: Vec<String>,
    lower: Vec<String>,
    by_id: HashMap<EntryId, u32>,
}

/// Cap on indexed rows so a corrupt archive cannot exhaust memory.
pub const MAX_INDEX_ROWS: usize = 20_000_000;

impl SearchIndex {
    /// Builds from entries listed parents-first (as `Backend::walk` yields).
    pub fn build(entries: Vec<EntryInfo>) -> Self {
        let mut idx = Self {
            rows: Vec::with_capacity(entries.len()),
            paths: Vec::with_capacity(entries.len()),
            lower: Vec::with_capacity(entries.len()),
            by_id: HashMap::with_capacity(entries.len()),
        };
        for e in entries.into_iter().take(MAX_INDEX_ROWS) {
            let name = e.display_name();
            let path = match e
                .parent
                .and_then(|p| idx.by_id.get(&p))
                .and_then(|&i| idx.paths.get(i as usize))
            {
                Some(parent) => format!("{parent}/{name}"),
                None => name,
            };
            idx.by_id.insert(e.id, idx.rows.len() as u32);
            idx.lower.push(path.to_lowercase());
            idx.paths.push(path);
            idx.rows.push(e);
        }
        idx
    }

    /// Walks a backend into an index (worker thread).
    pub fn from_backend(
        backend: &dyn crate::backend::Backend,
        ctx: &JobContext,
    ) -> Result<Self, crate::backend::BackendError> {
        let expected = backend.stats().entries.max(1);
        let mut rows = Vec::with_capacity(expected.min(MAX_INDEX_ROWS as u64) as usize);
        backend.walk(ctx, &mut |e| {
            if rows.len() < MAX_INDEX_ROWS {
                rows.push(e.clone());
                if rows.len().is_multiple_of(4096) {
                    ctx.set_progress(rows.len() as u64, expected);
                }
            }
        })?;
        if rows.len() >= MAX_INDEX_ROWS {
            log::warn!("search index truncated at {MAX_INDEX_ROWS} rows");
        }
        ctx.set_status("building search keys");
        Ok(Self::build(rows))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, i: u32) -> Option<&EntryInfo> {
        self.rows.get(i as usize)
    }

    pub fn path(&self, i: u32) -> Option<&str> {
        self.paths.get(i as usize).map(String::as_str)
    }

    pub fn path_of(&self, id: EntryId) -> Option<&str> {
        self.by_id.get(&id).and_then(|&i| self.path(i))
    }

    /// Entry counts per kind, in [`ALL_KINDS`] order.
    pub fn kind_counts(&self) -> Vec<(AssetKind, u64)> {
        ALL_KINDS
            .iter()
            .map(|k| (*k, self.rows.iter().filter(|r| r.kind == *k).count() as u64))
            .collect()
    }
}

/// One parsed query term.
#[derive(Clone, Debug, PartialEq)]
pub enum Term {
    Text(String),
    NotText(String),
    Hex(u64, String),
    Kind(String),
    MinSize(u64),
    MaxSize(u64),
    Codec(String),
    Provisional,
    Unnamed,
    Container,
}

/// Parses a size like `10k`, `1.5mb`, `4096`.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mul: f64 = match unit {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = n * mul;
    (v.is_finite() && v >= 0.0 && v < u64::MAX as f64).then_some(v as u64)
}

/// Splits query text into terms. Unparseable special terms fall back to text.
pub fn parse_query(text: &str) -> Vec<Term> {
    text.split_whitespace()
        .map(|raw| {
            let t = raw.to_lowercase();
            if let Some(h) = t.strip_prefix("0x")
                && !h.is_empty()
                && let Ok(v) = u64::from_str_radix(h, 16)
            {
                return Term::Hex(v, t);
            }
            if let Some(k) = t.strip_prefix("kind:").filter(|k| !k.is_empty()) {
                return Term::Kind(k.to_owned());
            }
            if let Some(c) = t.strip_prefix("codec:").filter(|c| !c.is_empty()) {
                return Term::Codec(c.to_owned());
            }
            match t.as_str() {
                "is:provisional" => return Term::Provisional,
                "is:unnamed" => return Term::Unnamed,
                "is:container" | "is:archive" => return Term::Container,
                _ => {}
            }
            if let Some(v) = t.strip_prefix('>').and_then(parse_size) {
                return Term::MinSize(v);
            }
            if let Some(v) = t.strip_prefix('<').and_then(parse_size) {
                return Term::MaxSize(v);
            }
            if let Some(n) = t.strip_prefix('-').filter(|n| !n.is_empty()) {
                return Term::NotText(n.to_owned());
            }
            Term::Text(t)
        })
        .collect()
}

/// A search request from the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchQuery {
    pub text: String,
    pub kind: Option<AssetKind>,
}

fn kind_matches(kind: AssetKind, prefix: &str) -> bool {
    kind_tag(kind).to_lowercase().starts_with(prefix)
        || kind.label().to_lowercase().starts_with(prefix)
        || format!("{kind:?}").to_lowercase().starts_with(prefix)
}

fn term_matches(t: &Term, e: &EntryInfo, lower_path: &str) -> bool {
    match t {
        Term::Text(s) => lower_path.contains(s.as_str()),
        Term::NotText(s) => !lower_path.contains(s.as_str()),
        Term::Hex(v, raw) => {
            e.name_hash.is_some_and(|h| u64::from(h.0) == *v)
                || e.offset == *v
                || lower_path.contains(raw.as_str())
        }
        Term::Kind(k) => kind_matches(e.kind, k),
        Term::MinSize(v) => e.size >= *v,
        Term::MaxSize(v) => e.size <= *v,
        Term::Codec(c) => e.codec.to_string().starts_with(c.as_str()),
        Term::Provisional => e.provisional,
        Term::Unnamed => e.name.is_none(),
        Term::Container => e.is_container(),
    }
}

/// Indices of matching rows, in index order, at most `limit`.
pub fn filter(index: &SearchIndex, q: &SearchQuery, limit: usize) -> Vec<u32> {
    let terms = parse_query(&q.text);
    let mut out = Vec::new();
    for (i, (e, lower)) in index.rows.iter().zip(&index.lower).enumerate() {
        if out.len() >= limit {
            break;
        }
        if q.kind.is_some_and(|k| k != e.kind) {
            continue;
        }
        if terms.iter().all(|t| term_matches(t, e, lower)) {
            out.push(i as u32);
        }
    }
    out
}

/// Result table sort column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Index,
    Kind,
    Name,
    Size,
    Offset,
}

/// Sorts result indices in place (stable).
pub fn sort_results(index: &SearchIndex, results: &mut [u32], key: SortKey, descending: bool) {
    let get = |i: &u32| index.row(*i);
    match key {
        SortKey::Index => results.sort(),
        SortKey::Kind => results.sort_by_key(|i| get(i).map(|e| kind_tag(e.kind))),
        SortKey::Name => results.sort_by(|a, b| {
            let pa = index.lower.get(*a as usize);
            let pb = index.lower.get(*b as usize);
            pa.cmp(&pb)
        }),
        SortKey::Size => results.sort_by_key(|i| get(i).map(|e| e.size)),
        SortKey::Offset => results.sort_by_key(|i| get(i).map(|e| e.offset)),
    }
    if descending {
        results.reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Codec;
    use u_rome_core::Hash32;

    fn e(
        id: u64,
        parent: Option<u64>,
        name: Option<&str>,
        kind: AssetKind,
        size: u64,
    ) -> EntryInfo {
        EntryInfo {
            id,
            parent,
            name: name.map(str::to_owned),
            name_hash: Some(Hash32(0xDEAD_0000 + id as u32)),
            kind,
            codec: if kind == AssetKind::Archive {
                Codec::Stored
            } else {
                Codec::Zstd
            },
            offset: id * 0x100,
            stored_size: size,
            size,
            child_count: (kind == AssetKind::Archive).then_some(1),
            provisional: id == 3,
        }
    }

    fn index() -> SearchIndex {
        SearchIndex::build(vec![
            e(
                0,
                None,
                Some("x64/levels.rpf"),
                AssetKind::Archive,
                10 << 20,
            ),
            e(
                1,
                Some(0),
                Some("Palm_Beach.mesh"),
                AssetKind::MeshBlob,
                2 << 20,
            ),
            e(2, Some(0), Some("nested.rpf"), AssetKind::Archive, 4096),
            e(3, Some(2), None, AssetKind::TextureTable, 512),
            e(4, Some(2), Some("palm_road.pso"), AssetKind::Pso, 100),
        ])
    }

    fn q(text: &str) -> Vec<u32> {
        filter(
            &index(),
            &SearchQuery {
                text: text.into(),
                kind: None,
            },
            usize::MAX,
        )
    }

    #[test]
    fn paths_are_built() {
        let i = index();
        assert_eq!(i.path(4), Some("x64/levels.rpf/nested.rpf/palm_road.pso"));
        assert_eq!(
            i.path_of(3).unwrap(),
            "x64/levels.rpf/nested.rpf/0xDEAD0003 @0x00000300"
        );
        assert_eq!(i.len(), 5);
    }

    #[test]
    fn text_terms() {
        assert_eq!(q(""), vec![0, 1, 2, 3, 4]);
        assert_eq!(q("PALM"), vec![1, 4]);
        assert_eq!(q("palm nested"), vec![4]);
        assert_eq!(q("palm -road"), vec![1]);
    }

    #[test]
    fn special_terms() {
        assert_eq!(q("0xdead0003"), vec![3]);
        assert_eq!(q("0x400"), vec![4]);
        assert_eq!(q("kind:arc"), vec![0, 2]);
        assert_eq!(q("kind:mesh"), vec![1]);
        assert_eq!(q(">1mb"), vec![0, 1]);
        assert_eq!(q("<1k"), vec![3, 4]);
        assert_eq!(q("is:provisional"), vec![3]);
        assert_eq!(q("is:unnamed"), vec![3]);
        assert_eq!(q("is:container"), vec![0, 2]);
        assert_eq!(q("codec:zstd palm"), vec![1, 4]);
    }

    #[test]
    fn kind_filter_and_limit() {
        let i = index();
        let r = filter(
            &i,
            &SearchQuery {
                text: String::new(),
                kind: Some(AssetKind::Archive),
            },
            usize::MAX,
        );
        assert_eq!(r, vec![0, 2]);
        assert_eq!(filter(&i, &SearchQuery::default(), 2), vec![0, 1]);
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("1k"), Some(1024));
        assert_eq!(parse_size("1.5MB"), Some(1_572_864));
        assert_eq!(parse_size("2g"), Some(2 << 30));
        assert_eq!(parse_size("x"), None);
        assert_eq!(parse_size("1q"), None);
        assert_eq!(parse_size("99999999999999999999999g"), None);
        // Malformed special terms degrade to text search.
        assert_eq!(parse_query(">zz"), vec![Term::Text(">zz".into())]);
        assert_eq!(parse_query("0x"), vec![Term::Text("0x".into())]);
    }

    #[test]
    fn sorting() {
        let i = index();
        let mut r = vec![0, 1, 2, 3, 4];
        sort_results(&i, &mut r, SortKey::Size, true);
        assert_eq!(r, vec![0, 1, 2, 3, 4]);
        sort_results(&i, &mut r, SortKey::Size, false);
        assert_eq!(r, vec![4, 3, 2, 1, 0]);
        sort_results(&i, &mut r, SortKey::Index, false);
        assert_eq!(r, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn large_index_filters() {
        let rows: Vec<EntryInfo> = (0..150_000u64)
            .map(|id| {
                e(
                    id,
                    None,
                    Some(&format!("entry_{id:06}")),
                    AssetKind::Unknown,
                    id,
                )
            })
            .collect();
        let i = SearchIndex::build(rows);
        let r = filter(
            &i,
            &SearchQuery {
                text: "entry_1499".into(),
                kind: None,
            },
            usize::MAX,
        );
        assert_eq!(r.len(), 100);
    }
}
