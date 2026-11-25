//! Integration tests over synthetic data only.

use std::path::PathBuf;
use std::time::Instant;

use u_rome_core::{AssetKind, joaat, joaat_lower};
use u_rome_index::*;

/// A temp database path removed (with its WAL/SHM files) on drop.
struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "u_rome_index_{tag}_{}_{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let db = TempDb(p);
        db.cleanup();
        db
    }

    fn cleanup(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut s = self.0.clone().into_os_string();
            s.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(s));
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn entry(chain: Vec<u64>, kind: AssetKind) -> IndexEntry {
    let top = chain[0];
    IndexEntry {
        stored_len: 100 + top,
        size: 200 + top,
        codec: Codec::Zlib,
        pages: 1,
        kind,
        head: b"9FPR\x00\x01".to_vec(),
        ..IndexEntry::new(chain)
    }
}

fn blake(n: u64) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&n.to_be_bytes());
    h[31] = 0x5A;
    h
}

#[test]
fn throughput_200k() {
    let mut index = Index::open_in_memory().unwrap();
    let reg = index.register_archive("x64/big.rpf", 1 << 40, 1).unwrap();
    const N: u64 = 200_000;
    let kinds = [
        AssetKind::TextureStore,
        AssetKind::MeshBlob,
        AssetKind::Audio,
        AssetKind::Pso,
        AssetKind::Unknown,
    ];
    let start = Instant::now();
    let mut w = index.writer(reg.id).unwrap();
    let mut top = 0u64;
    let mut i = 0u64;
    let mut hashed = 0u64;
    let mut distinct = std::collections::HashSet::new();
    while i < N {
        // Every 100th top-level entry is a nested archive with 20 children.
        if i.is_multiple_of(100) {
            w.push(&entry(vec![top], AssetKind::Archive)).unwrap();
            i += 1;
            for c in 0..20u64 {
                let mut e = entry(vec![top, c * 0x1000], kinds[(c % 5) as usize]);
                e.hash = Some(blake(i % 50_000));
                hashed += 1;
                distinct.insert(i % 50_000);
                e.name_hash = Some((i as u32).wrapping_mul(2_654_435_761));
                w.push(&e).unwrap();
                i += 1;
            }
        } else {
            let mut e = entry(vec![top], kinds[(i % 5) as usize]);
            e.hash = Some(blake(i % 50_000));
            hashed += 1;
            distinct.insert(i % 50_000);
            w.push(&e).unwrap();
            i += 1;
        }
        top += 0x10_000;
        if i % 20_000 < 21 && i > 21 {
            w.checkpoint(top).unwrap();
        }
    }
    let summary = w.finish().unwrap();
    let secs = start.elapsed().as_secs_f64();
    let rate = i as f64 / secs;
    println!("inserted {i} entries in {secs:.3}s = {rate:.0} entries/sec");
    assert_eq!(summary.pushed, i);
    assert_eq!(summary.orphans, 0);
    // The target is 100k+/s in release builds; unoptimised test builds of
    // this crate get a looser floor so CI on slow runners stays green.
    let floor = if cfg!(debug_assertions) {
        25_000.0
    } else {
        100_000.0
    };
    assert!(rate >= floor, "{rate:.0} entries/sec below {floor}");

    let stats = index.stats().unwrap();
    assert_eq!(stats.entries, i);
    assert_eq!(stats.archives_done, 1);
    assert_eq!(stats.by_kind.iter().map(|k| k.count).sum::<u64>(), i);
    assert_eq!(stats.duplicates.groups, distinct.len() as u64);
    assert_eq!(
        stats.duplicates.redundant_entries,
        hashed - distinct.len() as u64
    );
    let arch = index.archive(reg.id).unwrap().unwrap();
    assert_eq!(arch.entry_count, i);
    assert_eq!(arch.state, ScanState::Done);
}

#[test]
fn resume_and_change_detection() {
    let db = TempDb::new("resume");
    {
        let mut index = Index::open(&db.0).unwrap();
        let reg = index
            .register_archive("x64\\levels\\a.rpf", 1000, 42)
            .unwrap();
        assert!(reg.needs_scan);
        assert_eq!(reg.resume_offset, 0);
        let mut w = index.writer(reg.id).unwrap();
        assert_eq!(w.resume_offset(), 0);
        for off in 0..50 {
            w.push(&entry(vec![off], AssetKind::Unknown)).unwrap();
        }
        w.checkpoint(50).unwrap();
        for off in 50..80 {
            w.push(&entry(vec![off], AssetKind::Unknown)).unwrap();
        }
        // Simulated crash/cancel: dropped without checkpoint or finish.
        drop(w);
        let a = index.archive_by_path("x64/levels/a.rpf").unwrap().unwrap();
        assert_eq!(a.state, ScanState::Partial);
        assert_eq!(a.last_offset, 50);
        assert_eq!(a.entry_count, 50);
        assert_eq!(index.entries_in(a.id, 0, 1000).unwrap().len(), 50);
    }
    {
        // Reopen from disk and resume.
        let mut index = Index::open(&db.0).unwrap();
        let reg = index
            .register_archive("x64/levels/a.rpf", 1000, 42)
            .unwrap();
        assert!(reg.needs_scan);
        assert!(!reg.changed);
        assert_eq!(reg.resume_offset, 50);
        let existing = index.entries_in(reg.id, 10, 1).unwrap()[0].id;
        let mut w = index.writer(reg.id).unwrap();
        assert_eq!(w.resume_offset(), 50);
        // Re-emitting an already-committed entry is idempotent.
        assert_eq!(
            w.push(&entry(vec![10], AssetKind::Unknown)).unwrap(),
            existing
        );
        for off in 50..100 {
            w.push(&entry(vec![off], AssetKind::Unknown)).unwrap();
        }
        let s = w.finish().unwrap();
        assert_eq!(s.pushed, 50);
        assert_eq!(s.entry_count, 100);
        let rows = index.entries_in(reg.id, 0, 1000).unwrap();
        let offs: Vec<u64> = rows.iter().map(|r| r.top_offset).collect();
        assert_eq!(offs, (0..100).collect::<Vec<_>>());

        let reg = index
            .register_archive("x64/levels/a.rpf", 1000, 42)
            .unwrap();
        assert!(!reg.needs_scan);

        // File changed on disk: invalidated and emptied.
        let reg = index
            .register_archive("x64/levels/a.rpf", 1000, 43)
            .unwrap();
        assert!(reg.needs_scan && reg.changed);
        assert_eq!(reg.resume_offset, 0);
        assert!(index.entries_in(reg.id, 0, 10).unwrap().is_empty());
        let a = index.archive(reg.id).unwrap().unwrap();
        assert_eq!(
            (a.state, a.entry_count, a.mtime),
            (ScanState::Pending, 0, 43)
        );

        index.remove_archive(reg.id).unwrap();
        assert!(index.archives().unwrap().is_empty());
        assert!(matches!(
            index.writer(reg.id).err(),
            Some(IndexError::ArchiveNotFound(_))
        ));
    }
}

#[test]
fn nested_children_and_orphans() {
    let mut index = Index::open_in_memory().unwrap();
    let reg = index.register_archive("a.rpf", 1, 1).unwrap();
    let mut w = index.writer(reg.id).unwrap();
    let parent = w.push(&entry(vec![0x100], AssetKind::Archive)).unwrap();
    let c1 = w
        .push(&entry(vec![0x100, 0x10], AssetKind::Archive))
        .unwrap();
    let c2 = w.push(&entry(vec![0x100, 0x20], AssetKind::Xml)).unwrap();
    let gc = w
        .push(&entry(vec![0x100, 0x10, 0x8], AssetKind::Pso))
        .unwrap();
    let orphan = w.push(&entry(vec![0x999, 0x1], AssetKind::Xml)).unwrap();
    assert!(w.push(&IndexEntry::new(vec![])).is_err());
    assert!(
        w.push(&IndexEntry::new(vec![0; MAX_CHAIN_DEPTH + 1]))
            .is_err()
    );
    let mut big = IndexEntry::new(vec![1]);
    big.size = u64::MAX;
    assert!(w.push(&big).is_err());
    let s = w.finish().unwrap();
    assert_eq!(s.orphans, 1);

    let kids: Vec<EntryId> = index
        .children(parent)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(kids, [c1, c2]);
    let g = index.children(c1).unwrap();
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].id, gc);
    assert_eq!(g[0].chain, [0x100, 0x10, 0x8]);
    assert_eq!(g[0].parent_id, Some(c1));
    assert_eq!(index.entry(orphan).unwrap().unwrap().parent_id, None);
    let roots: Vec<EntryId> = index
        .roots_in(reg.id, 0, 100)
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(roots, [parent, orphan]);
    let e = index.entry(parent).unwrap().unwrap();
    assert_eq!(e.codec, Codec::Zlib);
    assert_eq!(e.head, b"9FPR\x00\x01");
    assert!(index.entry(EntryId(9999)).unwrap().is_none());
}

const CONTENT_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<CDataFileMgr__ContentDef>
  <dataFiles>
    <Item>
      <filename>dlc_pack:/x64/levels/gta6/vehicles.rpf</filename>
      <fileType>RPF_FILE</fileType>
    </Item>
    <Item>
      <filename>dlc_pack:/common/data/Handling.meta</filename>
      <fileType>HANDLING_FILE</fileType>
    </Item>
  </dataFiles>
</CDataFileMgr__ContentDef>"#;

/// Archive with entries carrying embedded name hashes for the manifest names.
fn named_fixture() -> (Index, ArchiveId, Vec<EntryId>) {
    let mut index = Index::open_in_memory().unwrap();
    let reg = index.register_archive("dlc/pack.rpf", 1, 1).unwrap();
    let mut w = index.writer(reg.id).unwrap();
    let mut ids = Vec::new();
    let specs: [(u32, AssetKind); 5] = [
        (joaat_lower("vehicles.rpf").0, AssetKind::Archive),
        // Case-sensitive hash of a mixed-case name.
        (joaat("Handling.meta").0, AssetKind::Pso),
        (joaat_lower("handling").0, AssetKind::Pso),
        (0xDEAD_BEEF, AssetKind::Audio),
        (0x0BAD_F00D, AssetKind::Xml),
    ];
    for (i, (h, kind)) in specs.iter().enumerate() {
        let mut e = entry(vec![i as u64 * 0x100], *kind);
        e.name_hash = Some(*h);
        e.hash = Some(blake(i as u64 % 2));
        ids.push(w.push(&e).unwrap());
    }
    w.finish().unwrap();
    (index, reg.id, ids)
}

#[test]
fn name_recovery_from_xml_manifest() {
    let (mut index, _, ids) = named_fixture();
    let mut reg = NameRegistry::new();
    assert!(reg.add_xml(CONTENT_XML) >= 6);
    let r = reg.load_list(
        "0xDEADBEEF\tmystery_bank\nloose_name\n",
        NameSource::HashList,
    );
    assert_eq!((r.added, r.unverified), (2, 1));
    let added = index.import_names(&reg).unwrap();
    assert_eq!(added, reg.len() as u64);
    // Importing again adds nothing.
    assert_eq!(index.import_names(&reg).unwrap(), 0);

    let rep = index.attach_names_by_hash().unwrap();
    assert_eq!(rep.joaat_lower, 2);
    assert_eq!(rep.joaat, 1);
    assert_eq!(rep.listed_hash, 1);
    // Re-running is a no-op.
    assert_eq!(index.attach_names_by_hash().unwrap().total(), 0);

    let e0 = index.entry(ids[0]).unwrap().unwrap();
    assert_eq!(e0.name.as_deref(), Some("vehicles.rpf"));
    assert_eq!(e0.name_match, Some(NameMatch::JoaatLower));
    assert!(e0.name_match.unwrap().provisional());
    let e1 = index.entry(ids[1]).unwrap().unwrap();
    assert_eq!(e1.name.as_deref(), Some("Handling.meta"));
    assert_eq!(e1.name_match, Some(NameMatch::Joaat));
    // "Handling" (from the manifest) lowercases to the embedded hash.
    let e2 = index.entry(ids[2]).unwrap().unwrap();
    assert_eq!(e2.name.as_deref(), Some("Handling"));
    let e3 = index.entry(ids[3]).unwrap().unwrap();
    assert_eq!(e3.name.as_deref(), Some("mystery_bank"));
    assert_eq!(e3.name_match, Some(NameMatch::ListedHash));
    assert!(index.entry(ids[4]).unwrap().unwrap().name.is_none());

    // Explicit naming wins and is not provisional.
    index
        .attach_name(ids[4], "audio/config.xml", NameSource::Manual)
        .unwrap();
    let e4 = index.entry(ids[4]).unwrap().unwrap();
    assert_eq!(e4.name.as_deref(), Some("audio/config.xml"));
    assert_eq!(e4.name_match, Some(NameMatch::Explicit));
    assert!(!NameMatch::Explicit.provisional());
    assert!(matches!(
        index.attach_name(EntryId(12345), "x", NameSource::Manual),
        Err(IndexError::EntryNotFound(12345))
    ));
    assert!(index.add_name("", NameSource::Manual).is_err());

    // Late hash discovery.
    index.set_name_hash(ids[4], Some(1)).unwrap();
    assert_eq!(index.entry(ids[4]).unwrap().unwrap().name_hash, Some(1));

    let alts = index.names_for_hash(joaat_lower("handling").0).unwrap();
    assert!(alts.iter().any(|n| n.name == "Handling"));
    let stats = index.stats().unwrap();
    assert_eq!(stats.named_entries, 5);
    assert_eq!(stats.duplicates.groups, 2);
    assert_eq!(stats.duplicates.redundant_entries, 3);
}

#[test]
fn fts_search() {
    let (mut index, archive, ids) = named_fixture();
    let mut reg = NameRegistry::new();
    reg.add_xml(CONTENT_XML);
    index.import_names(&reg).unwrap();
    index.attach_names_by_hash().unwrap();
    index.optimize().unwrap();
    let f = SearchFilters::default();
    let ids_of = |rows: Vec<EntryRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();

    // Substring, case-insensitive (trigram).
    assert_eq!(ids_of(index.search("VEHIC", &f, 50, 0).unwrap()), [ids[0]]);
    assert_eq!(
        ids_of(index.search("andling", &f, 50, 0).unwrap()),
        [ids[1], ids[2]]
    );
    assert_eq!(
        ids_of(index.search("handling meta", &f, 50, 0).unwrap()),
        [ids[1]]
    );
    // Short terms use the LIKE fallback.
    assert_eq!(ids_of(index.search("rp", &f, 50, 0).unwrap()), [ids[0]]);
    // Filters, inline and structured.
    assert_eq!(
        ids_of(index.search("handling kind:pso", &f, 50, 0).unwrap()),
        [ids[1], ids[2]]
    );
    let pso = SearchFilters {
        kind: Some(AssetKind::Archive),
        archive: Some(archive),
    };
    assert!(index.search("handling", &pso, 50, 0).unwrap().is_empty());
    // A kind word alone lists that kind.
    assert_eq!(
        ids_of(index.search("pso", &f, 50, 0).unwrap()),
        [ids[1], ids[2]]
    );
    assert_eq!(index.search("", &f, 50, 0).unwrap().len(), 5);
    assert_eq!(
        ids_of(index.search("", &f, 2, 3).unwrap()),
        [ids[3], ids[4]]
    );
    // Quotes and odd characters must not break FTS syntax.
    assert!(index.search("\"pso\"", &f, 50, 0).unwrap().is_empty());
    assert!(index.search("a\"b OR NEAR(", &f, 50, 0).unwrap().is_empty());
    assert!(index.search("100%_", &f, 50, 0).unwrap().is_empty());
    assert!(index.search("kind:bogus", &f, 50, 0).is_err());

    let names = index.search_names("handling", 10, 0).unwrap();
    assert!(names.iter().any(|n| n.name == "common/data/Handling.meta"));
}

#[test]
fn hex_hash_search() {
    let (mut index, _, ids) = named_fixture();
    let mut reg = NameRegistry::new();
    reg.add_xml(CONTENT_XML);
    index.import_names(&reg).unwrap();
    index.attach_names_by_hash().unwrap();
    let f = SearchFilters::default();
    let ids_of = |rows: Vec<EntryRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();

    // Embedded hash with no known name.
    assert_eq!(
        ids_of(index.search("0xdeadbeef", &f, 50, 0).unwrap()),
        [ids[3]]
    );
    // JOAAT of a name: the entry named by it is found through the names table.
    let h = format!("0x{:08X}", joaat_lower("vehicles.rpf").0);
    assert_eq!(ids_of(index.search(&h, &f, 50, 0).unwrap()), [ids[0]]);
    // Case-sensitive JOAAT of a name attached through the lowercase variant.
    let h = format!("0x{:08x}", joaat("VEHICLES.RPF").0);
    assert!(index.search(&h, &f, 50, 0).unwrap().is_empty());
    let h = format!("0x{:08x}", joaat_lower("VEHICLES.RPF").0);
    assert_eq!(ids_of(index.search(&h, &f, 50, 0).unwrap()), [ids[0]]);
    // BLAKE3 prefix: blake(1) starts with 00..01.
    let full: String = blake(1).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        ids_of(index.search(&format!("0x{full}"), &f, 50, 0).unwrap()),
        [ids[1], ids[3]]
    );
    assert_eq!(
        ids_of(index.search("0x00000000000000010", &f, 50, 0).unwrap()),
        [ids[1], ids[3]]
    );
    assert!(index.search("0xffffffffff", &f, 50, 0).unwrap().is_empty());
    // Kind filter combines with hash search.
    let audio = SearchFilters {
        kind: Some(AssetKind::Audio),
        archive: None,
    };
    assert_eq!(
        ids_of(index.search(&format!("0x{full}"), &audio, 50, 0).unwrap()),
        [ids[3]]
    );
    let dups = index.duplicates(10).unwrap();
    assert_eq!(dups.len(), 2);
    assert_eq!(dups[0].count, 3);
}

#[test]
fn migration_from_empty_file() {
    let db = TempDb::new("migrate");
    std::fs::write(&db.0, b"").unwrap();
    {
        let index = Index::open(&db.0).unwrap();
        assert_eq!(index.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(index.archives().unwrap().is_empty());
        assert_eq!(index.stats().unwrap(), Stats::default());
    }
    {
        // Reopening an up-to-date file is a no-op and keeps data.
        let mut index = Index::open(&db.0).unwrap();
        index.register_archive("a.rpf", 1, 1).unwrap();
        assert_eq!(index.schema_version().unwrap(), SCHEMA_VERSION);
    }
    {
        let index = Index::open(&db.0).unwrap();
        assert_eq!(index.archives().unwrap().len(), 1);
    }
    // A file written by a newer build is refused, not misread.
    set_user_version_in_header(&db.0, SCHEMA_VERSION + 1);
    assert!(matches!(
        Index::open(&db.0).err(),
        Some(IndexError::SchemaTooNew { .. })
    ));
}

/// Bump `user_version` behind the index's back. The crate exposes no raw SQL,
/// so write the header field directly (offset 60, big-endian u32, per the
/// SQLite file format). All connections are closed, so the WAL has been
/// checkpointed into the main file.
fn set_user_version_in_header(path: &std::path::Path, v: i64) {
    let mut bytes = std::fs::read(path).unwrap();
    assert!(bytes.len() >= 100, "database header missing");
    bytes[60..64].copy_from_slice(&(v as u32).to_be_bytes());
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn reader_sees_committed_state_while_writer_is_open() {
    let db = TempDb::new("wal");
    let mut writer_ix = Index::open(&db.0).unwrap();
    let reader_ix = Index::open(&db.0).unwrap();
    let reg = writer_ix.register_archive("a.rpf", 1, 1).unwrap();
    let mut w = writer_ix.writer(reg.id).unwrap();
    for off in 0..10 {
        w.push(&entry(vec![off], AssetKind::Unknown)).unwrap();
    }
    // Uncommitted rows are invisible, and reading does not block.
    assert!(reader_ix.entries_in(reg.id, 0, 100).unwrap().is_empty());
    w.checkpoint(10).unwrap();
    assert_eq!(reader_ix.entries_in(reg.id, 0, 100).unwrap().len(), 10);
    let a = reader_ix.archive(reg.id).unwrap().unwrap();
    assert_eq!((a.state, a.last_offset), (ScanState::Partial, 10));
    w.finish().unwrap();
    assert_eq!(
        reader_ix.archive(reg.id).unwrap().unwrap().state,
        ScanState::Done
    );

    // A second writer on another thread waits for the lock instead of failing.
    let path = db.0.clone();
    let mut w = writer_ix.writer(reg.id).unwrap();
    let t = std::thread::spawn(move || {
        let mut other = Index::open(&path).unwrap();
        other.register_archive("b.rpf", 2, 2).map(|r| r.id)
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    w.push(&entry(vec![0], AssetKind::Unknown)).unwrap();
    w.finish().unwrap();
    let other = t.join().unwrap();
    assert!(other.is_ok(), "{other:?}");
}
