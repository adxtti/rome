//! Streaming zlib / raw-DEFLATE inflation and 64 KiB page stitching.
//!
//! Nothing here allocates in proportion to header fields: output is pushed
//! through a caller-supplied sink in fixed-size chunks, and continuation pages
//! are buffered only up to one page.

use flate2::{Decompress, FlushDecompress, Status};
use std::sync::atomic::{AtomicBool, Ordering};

/// Page size of split resources (all three community sources).
pub(crate) const PAGE: usize = 0x10000;
/// Upper bound on the stored size of one 64 KiB page: worst case is stored
/// DEFLATE blocks (5 bytes per 65535-byte block) plus the 2-byte zlib header,
/// 4-byte Adler-32 and up to 15 bytes of padding. Rounded up generously.
pub(crate) const MAX_PAGE_STORED: usize = PAGE + 0x400;

const OUT_CHUNK: usize = 0x10000;
const IN_STEP: usize = 1 << 20;

/// Why a stream did not inflate.
#[derive(Debug)]
pub(crate) enum Fail {
    /// Not valid DEFLATE / bad zlib header / Adler-32 mismatch.
    Invalid(String),
    /// Input ended before the stream did.
    Truncated,
    /// Output exceeded the limit. `produced` bytes were emitted to the sink.
    Limit {
        consumed: u64,
        produced: u64,
    },
    Cancelled,
}

impl Fail {
    pub(crate) fn describe(&self) -> String {
        match self {
            Fail::Invalid(s) => s.clone(),
            Fail::Truncated => "stream truncated by end of data".to_owned(),
            Fail::Limit { produced, .. } => format!("output limit reached after {produced} bytes"),
            Fail::Cancelled => "cancelled".to_owned(),
        }
    }
}

/// The result of following a (possibly paged) zlib chain.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chain {
    /// From the first stream start to the end of the last stream (includes
    /// inter-page padding, excludes trailing padding).
    pub stored_len: u64,
    pub size: u64,
    pub pages: u32,
    /// Decoded size of the first stream.
    pub first_size: u64,
    /// Output limit hit; `size` is a lower bound.
    pub truncated: bool,
}

/// `78 01`, `78 5E`, `78 9C`, `78 DA`: 32 KiB window, no preset dictionary.
pub(crate) fn is_zlib_header(data: &[u8], off: usize) -> bool {
    matches!(
        data.get(off..off.saturating_add(2)),
        Some([0x78, 0x01 | 0x5E | 0x9C | 0xDA])
    )
}

pub(crate) fn align16(v: usize) -> usize {
    v.saturating_add(15) & !15
}

fn all_zero(data: &[u8], from: usize, to: usize) -> bool {
    data.get(from..to)
        .is_some_and(|s| s.iter().all(|&b| b == 0))
}

/// Per-thread inflate state, reused across candidates.
pub(crate) struct Inflater {
    zlib: Decompress,
    raw: Decompress,
    out: Box<[u8]>,
    page: Vec<u8>,
}

impl Inflater {
    pub(crate) fn new() -> Self {
        Self {
            zlib: Decompress::new(true),
            raw: Decompress::new(false),
            out: vec![0u8; OUT_CHUNK].into_boxed_slice(),
            page: Vec::with_capacity(PAGE),
        }
    }

    /// Inflate one raw DEFLATE stream (no header, no checksum).
    pub(crate) fn raw(
        &mut self,
        input: &[u8],
        limit: u64,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<(u64, u64), Fail> {
        inflate_one(
            &mut self.raw,
            false,
            &mut self.out,
            input,
            limit,
            cancel,
            sink,
        )
    }

    /// Inflate the zlib stream at `start` and every 64 KiB continuation page.
    ///
    /// Continuation rule (0x52504639 docs, rage-tools-vi `format.md`): a stream
    /// that inflates to exactly 65536 bytes continues when the bytes up to the
    /// next 16-byte boundary are zero and a valid zlib stream starts there. We
    /// additionally require a continuation page to inflate to at most 65536
    /// bytes, since pages never exceed the page size; a larger stream is an
    /// independent entry.
    pub(crate) fn chain(
        &mut self,
        data: &[u8],
        start: usize,
        limit: u64,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<Chain, Fail> {
        let input = data.get(start..).ok_or(Fail::Truncated)?;
        let (consumed, produced) = match inflate_one(
            &mut self.zlib,
            true,
            &mut self.out,
            input,
            limit,
            cancel,
            sink,
        ) {
            Ok(v) => v,
            Err(Fail::Limit { consumed, produced }) => {
                return Ok(Chain {
                    stored_len: consumed,
                    size: produced,
                    pages: 1,
                    first_size: produced,
                    truncated: true,
                });
            }
            Err(e) => return Err(e),
        };
        let mut chain = Chain {
            stored_len: consumed,
            size: produced,
            pages: 1,
            first_size: produced,
            truncated: false,
        };
        let mut end = start.saturating_add(consumed as usize);
        let mut last = produced;
        while last == PAGE as u64 {
            let next = align16(end);
            if !all_zero(data, end, next) || !is_zlib_header(data, next) {
                break;
            }
            let Some(input) = data.get(next..) else { break };
            let Inflater {
                zlib, out, page, ..
            } = self;
            page.clear();
            let res = inflate_one(zlib, true, out, input, PAGE as u64, cancel, &mut |b| {
                page.extend_from_slice(b)
            });
            let (c2, p2) = match res {
                Ok(v) => v,
                Err(Fail::Cancelled) => return Err(Fail::Cancelled),
                Err(e) => {
                    log::trace!(
                        "page after {:#x} at {next:#x} not a continuation: {}",
                        start,
                        e.describe()
                    );
                    break;
                }
            };
            if chain.size.saturating_add(p2) > limit {
                chain.truncated = true;
                break;
            }
            sink(page);
            chain.size += p2;
            chain.pages = chain.pages.saturating_add(1);
            end = next.saturating_add(c2 as usize);
            chain.stored_len = (end - start) as u64;
            last = p2;
        }
        Ok(chain)
    }

    /// Whether the stream at `pos` is a continuation page of a stream that
    /// starts in `[lo, hi)` (16-byte aligned offsets only). `hi` is clamped to
    /// `pos`. Used to resynchronise a scan range that starts mid-chain.
    pub(crate) fn is_continuation(
        &mut self,
        data: &[u8],
        pos: usize,
        lo: usize,
        hi: usize,
        cancel: &AtomicBool,
    ) -> bool {
        let lo = align16(lo.max(pos.saturating_sub(MAX_PAGE_STORED)));
        let hi = hi.min(pos);
        let mut p = lo;
        while p < hi {
            if is_zlib_header(data, p)
                && let Some(input) = data.get(p..pos)
            {
                let r = inflate_one(
                    &mut self.zlib,
                    true,
                    &mut self.out,
                    input,
                    PAGE as u64,
                    cancel,
                    &mut |_| {},
                );
                if let Ok((c, n)) = r {
                    let end = p + c as usize;
                    if n == PAGE as u64 && align16(end) == pos && all_zero(data, end, pos) {
                        return true;
                    }
                }
            }
            p += 16;
        }
        false
    }
}

/// Inflate a single stream from the start of `input`. Returns
/// `(bytes consumed, bytes produced)`.
fn inflate_one(
    d: &mut Decompress,
    zlib: bool,
    out: &mut [u8],
    input: &[u8],
    limit: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<(u64, u64), Fail> {
    d.reset(zlib);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Fail::Cancelled);
        }
        let in_pos = usize::try_from(d.total_in()).map_err(|_| Fail::Truncated)?;
        let out_before = d.total_out();
        let chunk = input
            .get(in_pos..input.len().min(in_pos.saturating_add(IN_STEP)))
            .ok_or(Fail::Truncated)?;
        let status = d
            .decompress(chunk, out, FlushDecompress::None)
            .map_err(|e| Fail::Invalid(e.to_string()))?;
        let produced = d.total_out() - out_before;
        let consumed = d.total_in() - in_pos as u64;
        if produced > 0 {
            if d.total_out() > limit {
                return Err(Fail::Limit {
                    consumed: d.total_in(),
                    produced: out_before,
                });
            }
            sink(out.get(..produced as usize).unwrap_or_default());
        }
        match status {
            Status::StreamEnd => return Ok((d.total_in(), d.total_out())),
            Status::Ok | Status::BufError => {
                if produced == 0 && consumed == 0 {
                    return if in_pos >= input.len() {
                        Err(Fail::Truncated)
                    } else {
                        Err(Fail::Invalid("inflate made no progress".to_owned()))
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn z(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn inflates_and_checks_adler() {
        let no = AtomicBool::new(false);
        let src: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut s = z(&src);
        let mut inf = Inflater::new();
        let mut got = Vec::new();
        let c = inf
            .chain(&s, 0, u64::MAX, &no, &mut |b| got.extend_from_slice(b))
            .unwrap();
        assert_eq!(got, src);
        assert_eq!(c.stored_len as usize, s.len());
        let n = s.len();
        s[n - 1] ^= 0xFF;
        assert!(inf.chain(&s, 0, u64::MAX, &no, &mut |_| {}).is_err());
        assert!(matches!(
            inf.chain(&s[..n / 2], 0, u64::MAX, &no, &mut |_| {}),
            Err(Fail::Truncated)
        ));
    }

    #[test]
    fn limit_and_cancel() {
        let src = vec![0u8; 300_000];
        let s = z(&src);
        let mut inf = Inflater::new();
        let no = AtomicBool::new(false);
        let c = inf.chain(&s, 0, 1000, &no, &mut |_| {}).unwrap();
        assert!(c.truncated);
        assert!(c.size <= 1000);
        let yes = AtomicBool::new(true);
        assert!(matches!(
            inf.chain(&s, 0, u64::MAX, &yes, &mut |_| {}),
            Err(Fail::Cancelled)
        ));
    }
}
