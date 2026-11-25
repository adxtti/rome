//! Parallel carving of an archive into entries.
//!
//! The archive is split into 16-byte aligned ranges scanned in parallel. A
//! range owns every entry whose first byte lies inside it and follows that
//! entry (including all of its 64 KiB pages) past the range end. The first
//! zlib stream a range finds may be a continuation page of a chain owned by
//! an earlier range; it is detected by looking back for a 65536-byte page that
//! ends right before it, and skipped. After the first entry, a worker's view
//! is self-consistent, so no further look-back is needed.

use crate::carve::{Hit, Miss, probe_at};
use crate::header::nested_header_at;
use crate::inflate::{Inflater, PAGE, align16, is_zlib_header};
use crate::read::decode_entry;
use crate::{Archive, Codec, Found, Location, ScanOptions, ScanProgress, ScanSummary};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use u_rome_core::{AssetKind, detect};

/// Bytes of decoded output kept in [`Found::head`].
pub const HEAD_LEN: usize = 64;
const MAX_NESTED_HITS: usize = 256;

/// Less common scan settings, kept out of [`ScanOptions`] so its field list stays stable.
#[derive(Clone, Debug)]
pub struct ScanExtras {
    /// Also carve headerless raw DEFLATE streams. They carry no checksum, so
    /// candidates are only tried right after at least 16 zero bytes, must
    /// decode to at least 64 bytes and be followed by zero padding, and are
    /// always reported as provisional. Off by default.
    pub raw_deflate: bool,
    /// Bytes per parallel work unit (rounded up to 16). `None` picks a size
    /// from the archive length and thread count.
    pub range_size: Option<usize>,
    /// Maximum nesting depth followed when `recurse_nested` is set.
    pub max_depth: usize,
}

impl Default for ScanExtras {
    fn default() -> Self {
        Self {
            raw_deflate: false,
            range_size: None,
            max_depth: 8,
        }
    }
}

struct Ctx<'a> {
    opts: &'a ScanOptions,
    extras: &'a ScanExtras,
    cancel: &'a AtomicBool,
    on_found: &'a (dyn Fn(Found) + Sync),
    on_progress: &'a (dyn Fn(ScanProgress) + Sync),
    found: AtomicU64,
    bytes_done: AtomicU64,
    bytes_total: u64,
}

impl Ctx<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn progress(&self) -> ScanProgress {
        ScanProgress {
            bytes_done: self.bytes_done.load(Ordering::Relaxed),
            bytes_total: self.bytes_total,
            found: self.found.load(Ordering::Relaxed),
        }
    }
}

type Stats = BTreeMap<AssetKind, u64>;

fn merge(mut a: Stats, b: Stats) -> Stats {
    for (k, v) in b {
        *a.entry(k).or_default() += v;
    }
    a
}

/// Scan with default [`ScanExtras`]. See [`scan_with`].
pub fn scan(
    archive: &Archive,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    on_found: &(dyn Fn(Found) + Sync),
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Result<ScanSummary, crate::Rpf9Error> {
    scan_with(
        archive,
        opts,
        &ScanExtras::default(),
        cancel,
        on_found,
        on_progress,
    )
}

/// Carve every entry out of `archive`.
///
/// `on_found` is called from worker threads as entries are found, in no
/// particular order. `on_progress` is called after each work unit. Setting
/// `cancel` stops the scan promptly; the summary then has `cancelled = true`
/// and counts only what was reported. Data errors never fail the scan: they
/// are logged and the bytes are skipped.
pub fn scan_with(
    archive: &Archive,
    opts: &ScanOptions,
    extras: &ScanExtras,
    cancel: &AtomicBool,
    on_found: &(dyn Fn(Found) + Sync),
    on_progress: &(dyn Fn(ScanProgress) + Sync),
) -> Result<ScanSummary, crate::Rpf9Error> {
    let data = archive.bytes();
    let start = archive
        .header()
        .map_or(0, |h| (h.payload_start as usize).min(data.len()));
    let ctx = Ctx {
        opts,
        extras,
        cancel,
        on_found,
        on_progress,
        found: AtomicU64::new(0),
        bytes_done: AtomicU64::new(align16(start).min(data.len()) as u64),
        bytes_total: data.len() as u64,
    };
    log::debug!(
        "scan: {} bytes from {start:#x}, hash {}, nested {}, limit {}",
        data.len(),
        opts.hash,
        opts.recurse_nested,
        opts.max_entry_size
    );
    let run = || scan_buffer(&ctx, data, &[], 0, start, true);
    let by_kind = match opts.threads.filter(|&n| n > 0) {
        Some(n) => rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build()
            .map_err(|e| crate::Rpf9Error::ThreadPool(e.to_string()))?
            .install(run),
        None => run(),
    };
    (ctx.on_progress)(ctx.progress());
    let summary = ScanSummary {
        found: ctx.found.load(Ordering::Relaxed),
        by_kind,
        bytes_scanned: ctx.bytes_done.load(Ordering::Relaxed),
        cancelled: ctx.cancelled(),
    };
    log::debug!(
        "scan done: {} entries, {} bytes, cancelled {}",
        summary.found,
        summary.bytes_scanned,
        summary.cancelled
    );
    Ok(summary)
}

fn scan_buffer(
    ctx: &Ctx<'_>,
    data: &[u8],
    prefix: &[u64],
    depth: usize,
    region_start: usize,
    top: bool,
) -> Stats {
    let region_start = align16(region_start).min(data.len());
    let span = data.len() - region_start;
    let threads = rayon::current_num_threads().max(1);
    let range = match ctx.extras.range_size {
        Some(r) => r,
        None if top => (span / (threads * 16)).clamp(1 << 20, 64 << 20),
        None => (span / (threads * 4)).clamp(256 << 10, 16 << 20),
    };
    let range = align16(range.max(16));
    let ranges: Vec<(usize, usize)> = (region_start..data.len())
        .step_by(range)
        .map(|s| (s, s.saturating_add(range).min(data.len())))
        .collect();
    ranges
        .par_iter()
        .map(|&(s, e)| {
            let stats = if ctx.cancelled() {
                Stats::new()
            } else {
                scan_range(ctx, data, prefix, depth, region_start, s, e)
            };
            if top && !ctx.cancelled() {
                ctx.bytes_done.fetch_add((e - s) as u64, Ordering::Relaxed);
                (ctx.on_progress)(ctx.progress());
            }
            stats
        })
        .reduce(Stats::new, merge)
}

/// Collects what the scanner needs from an entry's decoded bytes without
/// keeping them: the head, an optional hash, and aligned `9FPR` positions.
struct ScanSink {
    head: Vec<u8>,
    hasher: Option<blake3::Hasher>,
    want_hash: bool,
    probe: bool,
    hits: Vec<u64>,
    carry: Vec<u8>,
    total: u64,
}

impl ScanSink {
    fn new(want_hash: bool, probe: bool) -> Self {
        Self {
            head: Vec::with_capacity(HEAD_LEN),
            hasher: None,
            want_hash,
            probe,
            hits: Vec::new(),
            carry: Vec::with_capacity(3),
            total: 0,
        }
    }

    fn reset(&mut self) {
        self.head.clear();
        self.hasher = self.want_hash.then(blake3::Hasher::new);
        self.hits.clear();
        self.carry.clear();
        self.total = 0;
    }

    fn feed(&mut self, b: &[u8]) {
        if self.head.len() < HEAD_LEN {
            let n = (HEAD_LEN - self.head.len()).min(b.len());
            self.head.extend_from_slice(b.get(..n).unwrap_or_default());
        }
        if let Some(h) = self.hasher.as_mut() {
            h.update(b);
        }
        if self.probe && self.hits.len() < MAX_NESTED_HITS {
            self.find_magic(b);
        }
        self.total += b.len() as u64;
    }

    /// Record every 16-aligned decoded offset holding `9FPR`, including
    /// signatures split across output chunks.
    fn find_magic(&mut self, b: &[u8]) {
        const M: &[u8; 4] = b"9FPR";
        let base = self.total;
        let clen = self.carry.len();
        for i in 0..clen {
            let p = base - (clen - i) as u64;
            let need = 4 - (clen - i);
            if p.is_multiple_of(16)
                && let Some(tail) = b.get(..need)
                && self.carry.get(i..).is_some_and(|c| c == &M[..clen - i])
                && tail == &M[clen - i..]
            {
                self.hits.push(p);
            }
        }
        let mut j = (align16(base as usize) as u64 - base) as usize;
        while let Some(w) = b.get(j..j + 4) {
            if w == M {
                self.hits.push(base + j as u64);
            }
            j += 16;
        }
        if b.len() >= 3 {
            self.carry.clear();
            self.carry
                .extend_from_slice(b.get(b.len() - 3..).unwrap_or_default());
        } else {
            self.carry.extend_from_slice(b);
            let extra = self.carry.len().saturating_sub(3);
            self.carry.drain(..extra);
        }
    }
}

fn is_candidate(data: &[u8], pos: usize, raw: bool) -> (bool, bool) {
    if is_zlib_header(data, pos) {
        return (true, false);
    }
    match data.get(pos..pos + 4) {
        Some(b"ADAT" | b"9FPR") | Some([b'K', b'B', b'2', _]) => return (true, false),
        None => return (false, false),
        _ => {}
    }
    let raw_ok = raw
        && pos >= 16
        && data
            .get(pos - 16..pos)
            .is_some_and(|s| s.iter().all(|&b| b == 0))
        && data
            .get(pos..pos + 16)
            .is_some_and(|s| s.iter().any(|&b| b != 0));
    (raw_ok, raw_ok)
}

fn scan_range(
    ctx: &Ctx<'_>,
    data: &[u8],
    prefix: &[u64],
    depth: usize,
    region_start: usize,
    start: usize,
    end: usize,
) -> Stats {
    let opts = ctx.opts;
    let mut stats = Stats::new();
    let mut inf = Inflater::new();
    let probe_nested = opts.recurse_nested && depth < ctx.extras.max_depth;
    let mut sink = ScanSink::new(opts.hash, probe_nested);
    let mut pos = start;
    let mut first = true;
    while pos < end {
        if ctx.cancelled() {
            break;
        }
        let (candidate, raw) = is_candidate(data, pos, ctx.extras.raw_deflate);
        if !candidate {
            pos += 16;
            continue;
        }
        sink.reset();
        let res = probe_at(
            data,
            pos,
            &mut inf,
            opts.max_entry_size,
            raw,
            ctx.cancel,
            &mut |b| sink.feed(b),
        );
        let hit = match res {
            Ok(hit) => hit,
            Err(Miss::Cancelled) => break,
            Err(Miss::Nothing(why) | Miss::Corrupt(why)) => {
                log::trace!("depth {depth} offset {pos:#x}: {why}");
                pos += 16;
                continue;
            }
        };
        let next = match &hit.nested {
            Some(h) => pos.saturating_add(h.payload_start as usize),
            None => pos.saturating_add(hit.stored_len as usize),
        };
        let next = align16(next).max(pos + 16);
        if std::mem::take(&mut first)
            && hit.codec == Codec::Zlib
            && start > region_start
            && hit.first_page_size <= PAGE as u64
            && inf.is_continuation(data, pos, region_start, start, ctx.cancel)
        {
            log::trace!(
                "depth {depth} offset {pos:#x}: continuation page owned by an earlier range"
            );
            pos = next;
            continue;
        }
        report(ctx, data, prefix, pos, &hit, &mut sink, &mut stats);
        if probe_nested
            && !sink.hits.is_empty()
            && !hit.truncated
            && matches!(hit.codec, Codec::Zlib | Codec::RawDeflate)
        {
            let nested = recurse(ctx, data, prefix, depth, pos, &mut inf, &sink.hits);
            stats = merge(stats, nested);
        }
        pos = next;
    }
    stats
}

fn report(
    ctx: &Ctx<'_>,
    data: &[u8],
    prefix: &[u64],
    pos: usize,
    hit: &Hit,
    sink: &mut ScanSink,
    stats: &mut Stats,
) {
    let mut chain = Vec::with_capacity(prefix.len() + 1);
    chain.extend_from_slice(prefix);
    chain.push(pos as u64);
    let kind = if hit.nested.is_some() {
        AssetKind::Archive
    } else {
        detect(&sink.head)
    };
    if hit.truncated {
        log::warn!(
            "entry at {chain:x?} exceeds the {}-byte limit; size and extent are lower bounds",
            ctx.opts.max_entry_size
        );
    }
    let hash = sink
        .hasher
        .take()
        .filter(|_| !hit.truncated)
        .map(|h| *h.finalize().as_bytes());
    let zero_prefix = hit.codec != Codec::Stored
        && pos >= 16
        && data
            .get(pos - 16..pos)
            .is_some_and(|s| s.iter().all(|&b| b == 0));
    let found = Found {
        location: Location { chain },
        stored_len: hit.stored_len,
        size: hit.size,
        codec: hit.codec,
        pages: hit.pages,
        kind,
        head: std::mem::take(&mut sink.head),
        hash,
        provisional: hit.provisional,
        zero_prefix,
    };
    *stats.entry(kind).or_default() += 1;
    ctx.found.fetch_add(1, Ordering::Relaxed);
    (ctx.on_found)(found);
}

fn recurse(
    ctx: &Ctx<'_>,
    data: &[u8],
    prefix: &[u64],
    depth: usize,
    pos: usize,
    inf: &mut Inflater,
    hits: &[u64],
) -> Stats {
    let decoded = match decode_entry(data, pos as u64, ctx.opts.max_entry_size, depth, inf) {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "cannot re-decode entry at depth {depth} offset {pos:#x} for nested scan: {e}"
            );
            return Stats::new();
        }
    };
    let valid: Vec<u64> = hits
        .iter()
        .copied()
        .filter(|&h| nested_header_at(&decoded, h as usize).is_some())
        .collect();
    if valid.is_empty() {
        return Stats::new();
    }
    log::debug!(
        "depth {depth} offset {pos:#x}: nested RPF9 headers at {valid:x?}, scanning {} decoded bytes",
        decoded.len()
    );
    let start = nested_header_at(&decoded, 0).map_or(0, |h| h.payload_start as usize);
    let mut child = prefix.to_vec();
    child.push(pos as u64);
    scan_buffer(ctx, &decoded, &child, depth + 1, start, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_split_magic() {
        let mut data = [0u8; 96];
        data[16..20].copy_from_slice(b"9FPR");
        data[48..52].copy_from_slice(b"9FPR");
        data[50 + 16..54 + 16].copy_from_slice(b"9FPR"); // unaligned, ignored
        for split in [1usize, 2, 3, 5, 7] {
            let mut s = ScanSink::new(false, true);
            s.reset();
            for c in data.chunks(split) {
                s.feed(c);
            }
            assert_eq!(s.hits, vec![16, 48], "chunk size {split}");
            assert_eq!(s.head.len(), 64);
        }
    }
}
