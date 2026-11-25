mod common;

use common::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use u_rome_core::AssetKind;
use u_rome_rpf9::*;

static NO: AtomicBool = AtomicBool::new(false);

fn opts() -> ScanOptions {
    ScanOptions {
        hash: true,
        threads: Some(4),
        recurse_nested: true,
        max_entry_size: 64 << 20,
    }
}

fn run(archive: &Archive, opts: &ScanOptions, extras: &ScanExtras) -> (Vec<Found>, ScanSummary) {
    let found = Mutex::new(Vec::new());
    let cancel = &NO;
    let summary = scan_with(
        archive,
        opts,
        extras,
        cancel,
        &|f| found.lock().unwrap().push(f),
        &|_| {},
    )
    .unwrap();
    let mut v = found.into_inner().unwrap();
    v.sort_by(|a, b| a.location.cmp(&b.location));
    (v, summary)
}

/// Inner archive embedded in a decoded stream.
fn inner_archive(id: u64) -> Builder {
    let mut b = Builder::archive(2, id);
    b.zlib(b"<?xml version=\"1.0\"?><Item>inner</Item>", AssetKind::Xml);
    let mut psin = b"PSIN".to_vec();
    psin.extend_from_slice(&text(3000, id));
    b.zlib(&psin, AssetKind::Pso);
    b.align(16);
    b
}

/// Builds the full synthetic archive and its expectations.
fn build() -> (Vec<u8>, Vec<Expect>) {
    let mut b = Builder::archive(5, 0x5EED);
    b.align(0x1000);
    // Plain compressible stream.
    b.zlib(&text(1000, 1), AssetKind::Unknown);
    // Incompressible stream (stored deflate blocks inside zlib).
    b.zlib(&noise(5000, 2), AssetKind::Unknown);
    // Corrupt stream: Adler-32 mismatch. Must not be reported.
    b.align(16);
    let mut bad = zlib(&text(2000, 3), 6);
    let n = bad.len();
    bad[n - 1] ^= 0x5A;
    b.bytes(&bad);
    // Fake zlib header followed by noise. Must not be reported.
    b.align(16);
    b.bytes(&[0x78, 0x9C]).bytes(&noise(300, 4));
    // Truncated stream: first half of a valid stream, then zeros.
    b.align(16);
    let t = zlib(&text(4000, 5), 6);
    b.bytes(&t[..t.len() / 2]);
    b.align(0x1000);
    // 3-page resource (64 KiB + 64 KiB + tail).
    let mut big = b"RDS5".to_vec();
    big.extend_from_slice(&text(2 * PAGE + 5000 - 4, 6));
    b.paged(&big, AssetKind::TextureStore);
    // Exactly one full page followed by a non-continuation (non-zero gap).
    let mut one = b"ETPF".to_vec();
    one.extend_from_slice(&text(PAGE - 4, 7));
    b.paged(&one, AssetKind::TextureTable);
    b.bytes(&[0xEE; 16]);
    // Virtual resource.
    let mut meta = b"0DRP".to_vec();
    meta.extend_from_slice(&text(700, 8));
    b.virtual_resource(&meta, AssetKind::MetaResource);
    // Stored audio bank and video.
    b.stored(&adat(3, 2, 0x300, 9), AssetKind::Audio);
    b.stored(&bink(0x900, 10), AssetKind::Video);
    // Nested archive inside a zlib stream (archive at decoded offset 0).
    b.zlib_container(inner_archive(0x1111), AssetKind::Archive);
    // Nested archive at decoded offset 0xaa0 inside a stream.
    let mut wrapper = Builder::blob();
    wrapper.bytes(&text(0xaa0, 11));
    wrapper.prefix = Vec::new();
    let inner = inner_archive(0x2222);
    wrapper.raw_nested(inner);
    b.zlib_container(wrapper, AssetKind::Unknown);
    // Raw nested archive on a 4 KiB boundary in the top-level bytes.
    b.align(0x1000);
    b.raw_nested(inner_archive(0x3333));
    // A last stream after the nested archive.
    b.zlib(&text(321, 12), AssetKind::Unknown);
    let bytes = b.finish();
    (bytes, b.expect)
}

fn check(found: &[Found], expect: &[Expect], archive: &Archive) {
    let mut exp = expect.to_vec();
    exp.sort_by(|a, b| a.chain.cmp(&b.chain));
    let got: Vec<_> = found.iter().map(|f| f.location.chain.clone()).collect();
    let want: Vec<_> = exp.iter().map(|e| e.chain.clone()).collect();
    assert_eq!(got, want, "locations differ");
    for (f, e) in found.iter().zip(&exp) {
        let ctx = format!("{} ({:?})", f.location, e.kind);
        assert_eq!(f.kind, e.kind, "{ctx}");
        assert_eq!(f.codec, e.codec, "{ctx}");
        assert_eq!(f.pages, e.pages, "{ctx}");
        assert_eq!(f.provisional, e.provisional, "{ctx}");
        assert_eq!(f.size, e.data.len() as u64, "{ctx}");
        assert_eq!(f.head, e.data[..e.data.len().min(64)], "{ctx}");
        assert_eq!(f.hash, Some(*blake3::hash(&e.data).as_bytes()), "{ctx}");
        let back = read(archive, &f.location, 64 << 20).unwrap();
        assert_eq!(back, e.data, "{ctx}");
    }
}

#[test]
fn scan_finds_every_entry() {
    let (bytes, expect) = build();
    let archive = Archive::from_vec(bytes);
    let h = archive.header().expect("top header");
    assert_eq!(h.payload_start, 0x340);
    assert_eq!(h.entry_count, 5);
    assert_eq!(h.archive_id, 0x5EED);
    let (found, summary) = run(&archive, &opts(), &ScanExtras::default());
    check(&found, &expect, &archive);
    assert_eq!(summary.found, expect.len() as u64);
    assert!(!summary.cancelled);
    assert_eq!(summary.bytes_scanned, archive.bytes().len() as u64);
    assert_eq!(summary.by_kind.get(&AssetKind::Archive), Some(&3));
    assert_eq!(summary.by_kind.get(&AssetKind::Xml), Some(&3));
    let paged = found.iter().find(|f| f.pages == 3).unwrap();
    assert_eq!(paged.kind, AssetKind::TextureStore);
    let virt = found
        .iter()
        .find(|f| f.kind == AssetKind::MetaResource)
        .unwrap();
    assert!(virt.zero_prefix);
    // Nested-in-stream entries carry two-level chains.
    assert!(
        found
            .iter()
            .any(|f| f.location.chain.len() == 2 && f.location.chain[1] == 0xaa0)
    );
}

#[test]
fn range_boundaries_do_not_change_results() {
    let (bytes, expect) = build();
    let archive = Archive::from_vec(bytes);
    for range in [16usize, 48, 1000, 4096, 0x8000, 0x10010, 1 << 20] {
        for threads in [1usize, 3, 8] {
            let o = ScanOptions {
                threads: Some(threads),
                ..opts()
            };
            let extras = ScanExtras {
                range_size: Some(range),
                ..ScanExtras::default()
            };
            let (found, _) = run(&archive, &o, &extras);
            let got: Vec<_> = found
                .iter()
                .map(|f| (f.location.clone(), f.size, f.pages))
                .collect();
            let mut want: Vec<_> = expect
                .iter()
                .map(|e| {
                    (
                        Location {
                            chain: e.chain.clone(),
                        },
                        e.data.len() as u64,
                        e.pages,
                    )
                })
                .collect();
            want.sort();
            assert_eq!(got, want, "range {range} threads {threads}");
        }
    }
}

#[test]
fn no_recursion_and_no_hash() {
    let (bytes, expect) = build();
    let archive = Archive::from_vec(bytes);
    let o = ScanOptions {
        hash: false,
        recurse_nested: false,
        ..opts()
    };
    let (found, _) = run(&archive, &o, &ScanExtras::default());
    let top = expect.iter().filter(|e| e.chain.len() == 1).count();
    assert_eq!(found.len(), top);
    assert!(found.iter().all(|f| f.hash.is_none()));
}

#[test]
fn progress_and_cancellation() {
    let (bytes, expect) = build();
    let archive = Archive::from_vec(bytes);
    let extras = ScanExtras {
        range_size: Some(4096),
        ..ScanExtras::default()
    };
    // Progress is monotonic per call and ends at the total.
    let last = Mutex::new(None);
    let s = scan_with(&archive, &opts(), &extras, &NO, &|_| {}, &|p| {
        *last.lock().unwrap() = Some(p);
    })
    .unwrap();
    let p = last.into_inner().unwrap().unwrap();
    assert_eq!(p.bytes_done, p.bytes_total);
    assert_eq!(p.found, s.found);

    // Cancelled before starting.
    let cancel = AtomicBool::new(true);
    let s = scan(
        &archive,
        &opts(),
        &cancel,
        &|_| panic!("nothing expected"),
        &|_| {},
    )
    .unwrap();
    assert!(s.cancelled);
    assert_eq!(s.found, 0);

    // Cancelled from inside the callback after the first entry.
    let cancel = AtomicBool::new(false);
    let o = ScanOptions {
        threads: Some(1),
        ..opts()
    };
    let s = scan_with(
        &archive,
        &o,
        &extras,
        &cancel,
        &|_| cancel.store(true, Ordering::Relaxed),
        &|_| {},
    )
    .unwrap();
    assert!(s.cancelled);
    assert!(
        s.found >= 1 && (s.found as usize) < expect.len(),
        "{}",
        s.found
    );
}

#[test]
fn size_limit_marks_entries_provisional() {
    let mut b = Builder::archive(1, 9);
    b.zlib(&text(200_000, 1), AssetKind::Unknown);
    let archive = Archive::from_vec(b.finish());
    let o = ScanOptions {
        max_entry_size: 10_000,
        ..opts()
    };
    let (found, _) = run(&archive, &o, &ScanExtras::default());
    assert!(!found.is_empty());
    let f = &found[0];
    assert!(f.provisional && f.size <= 10_000 && f.hash.is_none());
    assert!(matches!(
        read(&archive, &f.location, 10_000),
        Err(Rpf9Error::TooLarge { .. })
    ));
    assert_eq!(
        read(&archive, &f.location, 1 << 20).unwrap(),
        text(200_000, 1)
    );
}

#[test]
fn broken_page_ends_chain() {
    let mut b = Builder::archive(1, 10);
    let data = text(3 * PAGE, 2);
    let first = b.pos();
    b.bytes(&zlib(&data[..PAGE], 6)).align(16);
    let second = b.pos();
    let mut p2 = zlib(&data[PAGE..2 * PAGE], 6);
    let n = p2.len();
    p2[n - 2] ^= 1;
    b.bytes(&p2).align(16);
    let third = b.pos();
    b.bytes(&zlib(&data[2 * PAGE..], 6)).align(16);
    let archive = Archive::from_vec(b.finish());
    let (found, _) = run(&archive, &opts(), &ScanExtras::default());
    let locs: Vec<_> = found
        .iter()
        .map(|f| (f.location.chain[0], f.pages, f.size))
        .collect();
    assert_eq!(locs, vec![(first, 1, PAGE as u64), (third, 1, PAGE as u64)]);
    let _ = second;
}

#[test]
fn raw_deflate_is_optional_and_provisional() {
    let mut b = Builder::archive(1, 11);
    b.align(0x1000);
    b.bytes(&[0u8; 16]);
    let at = b.pos();
    let data = text(5000, 3);
    b.bytes(&raw_deflate(&data)).align(16);
    b.bytes(&[0u8; 32]);
    let archive = Archive::from_vec(b.finish());
    let (found, _) = run(&archive, &opts(), &ScanExtras::default());
    assert!(found.is_empty());
    let extras = ScanExtras {
        raw_deflate: true,
        ..ScanExtras::default()
    };
    let (found, _) = run(&archive, &opts(), &extras);
    assert_eq!(found.len(), 1);
    let f = &found[0];
    assert_eq!(
        (f.location.chain[0], f.codec, f.provisional),
        (at, Codec::RawDeflate, true)
    );
    assert_eq!(read(&archive, &f.location, 1 << 20).unwrap(), data);
}

#[test]
fn read_errors_are_descriptive() {
    let (bytes, _) = build();
    let archive = Archive::from_vec(bytes);
    assert!(matches!(
        read(&archive, &Location { chain: vec![] }, 1 << 20),
        Err(Rpf9Error::EmptyLocation)
    ));
    let e = read(&archive, &Location::top(u64::MAX), 1 << 20).unwrap_err();
    assert!(matches!(e, Rpf9Error::OutOfRange { depth: 0, .. }), "{e}");
    let e = read(&archive, &Location::top(0x1000 + 3), 1 << 20).unwrap_err();
    assert!(e.to_string().contains("0x1003"), "{e}");
    let e = read(
        &archive,
        &Location {
            chain: vec![0x1000, 1 << 40],
        },
        1 << 20,
    )
    .unwrap_err();
    assert!(matches!(e, Rpf9Error::OutOfRange { depth: 1, .. }), "{e}");
}

#[test]
fn open_from_disk() {
    let (bytes, expect) = build();
    let dir = std::env::temp_dir().join(format!("u_rome_rpf9_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("synthetic.rpf");
    std::fs::write(&path, &bytes).unwrap();
    let empty = dir.join("empty.rpf");
    std::fs::write(&empty, b"").unwrap();
    {
        let archive = Archive::open(&path).unwrap();
        assert_eq!(archive.bytes(), &bytes[..]);
        let (found, _) = run(&archive, &opts(), &ScanExtras::default());
        assert_eq!(found.len(), expect.len());
        let e = Archive::open(&empty).unwrap();
        assert!(e.header().is_none());
        assert_eq!(run(&e, &opts(), &ScanExtras::default()).0.len(), 0);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(matches!(
        Archive::open(&dir.join("missing")),
        Err(Rpf9Error::Io { .. })
    ));
}

#[test]
fn garbage_never_panics() {
    let mut rng = Rng::new(42);
    let small = ScanExtras {
        raw_deflate: true,
        range_size: Some(256),
        ..ScanExtras::default()
    };
    // Pure noise, sprinkled with signatures.
    for round in 0..40u64 {
        let len = rng.below(20_000) as usize;
        let mut data = noise(len, round);
        for _ in 0..rng.below(50) {
            if len < 80 {
                break;
            }
            let at = (rng.below((len - 64) as u64) as usize) & !15;
            let sig: &[u8] = match rng.below(6) {
                0 => b"9FPR",
                1 => b"ADAT\x01\x00",
                2 => b"KB2j",
                3 => &[0x78, 0x9C],
                4 => &[0x78, 0xDA],
                _ => &header(0x60, 1, 0, 1),
            };
            data[at..at + sig.len()].copy_from_slice(sig);
        }
        let archive = Archive::from_vec(data);
        let (found, _) = run(&archive, &opts(), &small);
        for f in &found {
            let _ = read(&archive, &f.location, 1 << 20);
        }
        for _ in 0..50 {
            let off = rng.below(len as u64 + 10);
            let _ = read(&archive, &Location::top(off), 1 << 16);
            let _ = read(
                &archive,
                &Location {
                    chain: vec![off, rng.next()],
                },
                1 << 16,
            );
        }
    }
    // A valid archive with random byte flips.
    let (bytes, _) = build();
    for round in 0..12u64 {
        let mut data = bytes.clone();
        let mut r = Rng::new(round);
        for _ in 0..(1 + r.below(200)) {
            let i = r.below(data.len() as u64) as usize;
            data[i] = r.next() as u8;
        }
        if round % 3 == 0 {
            let cut = r.below(data.len() as u64) as usize;
            data.truncate(cut);
        }
        let archive = Archive::from_vec(data);
        let (found, _) = run(&archive, &opts(), &small);
        for f in found.iter().take(30) {
            let _ = read(&archive, &f.location, 1 << 20);
        }
    }
    // Headers with extreme fields.
    for v in [0u32, 1, 0x40, 0xFFFF_FFF0, u32::MAX] {
        let mut h = header(0x340, 3, 0x90, 1).to_vec();
        for off in [4usize, 8, 12, 0x24] {
            h[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let _ = parse_header(&h);
        let archive = Archive::from_vec(h);
        let _ = run(&archive, &opts(), &small);
    }
}

/// Throughput on a few hundred MB of synthetic data:
/// `cargo test -p u_rome_rpf9 --release -- --ignored --nocapture bench_throughput`
#[test]
#[ignore]
fn bench_throughput() {
    let target = 300usize << 20;
    let mut b = Builder::archive(1000, 77);
    b.align(0x1000);
    let mut i = 0u64;
    let mut big = b"RDS5".to_vec();
    big.extend_from_slice(&text(4 * PAGE, 1));
    let small = text(30_000, 2);
    let rnd = noise(40_000, 3);
    let (zs, zr) = (zlib(&small, 6), zlib(&rnd, 6));
    let pages: Vec<Vec<u8>> = big.chunks(PAGE).map(|p| zlib(p, 6)).collect();
    while b.buf.len() < target {
        match i % 4 {
            0 => {
                for p in &pages {
                    b.align(16).bytes(p);
                }
            }
            1 => {
                b.align(16).bytes(&zs);
            }
            2 => {
                b.align(16).bytes(&zr);
            }
            _ => {
                b.align(0x1000);
            }
        }
        i += 1;
    }
    let archive = Archive::from_vec(b.finish());
    for hash in [false, true] {
        let o = ScanOptions {
            hash,
            threads: None,
            ..ScanOptions::default()
        };
        let t = std::time::Instant::now();
        let s = scan(&archive, &o, &NO, &|_| {}, &|_| {}).unwrap();
        let secs = t.elapsed().as_secs_f64();
        println!(
            "hash={hash}: {} MB, {} entries in {secs:.2}s = {:.0} MB/s ({} threads)",
            archive.bytes().len() >> 20,
            s.found,
            archive.bytes().len() as f64 / secs / 1e6,
            rayon::current_num_threads()
        );
    }
}
