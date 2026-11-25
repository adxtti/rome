//! Recognising one entry at an offset. Shared by [`scan`](crate::scan) and
//! [`read`](crate::read) so both agree on extents.

use crate::Codec;
use crate::header::{Header, nested_header_at};
use crate::inflate::{Fail, Inflater, align16, is_zlib_header};
use crate::stored::{adat_len, bink_len};
use std::sync::atomic::AtomicBool;

/// Smallest raw-DEFLATE output accepted. Raw DEFLATE has no checksum, so tiny
/// streams that happen to decode from random bytes must be rejected.
const RAW_MIN_OUTPUT: u64 = 64;

/// An entry recognised at some offset.
#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub stored_len: u64,
    pub size: u64,
    pub codec: Codec,
    pub pages: u32,
    pub first_page_size: u64,
    pub provisional: bool,
    /// Output limit hit while inflating; `size` is a lower bound.
    pub truncated: bool,
    /// Set for a nested archive found in place (its front region only).
    pub nested: Option<Header>,
}

/// Why nothing was recognised.
#[derive(Debug)]
pub(crate) enum Miss {
    Cancelled,
    /// Nothing that looks like an entry.
    Nothing(String),
    /// A signature was present but the data behind it is broken.
    Corrupt(String),
}

/// Look for an entry at `off`.
///
/// `sink` receives the decoded bytes of the entry (decompressed output or the
/// stored slice), in order. For a nested archive found in place the sink gets
/// the front region (header and encrypted tables). If `Err` is returned the
/// sink may have received partial output, which the caller must discard.
pub(crate) fn probe_at(
    data: &[u8],
    off: usize,
    inf: &mut Inflater,
    limit: u64,
    raw_deflate: bool,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Hit, Miss> {
    let rest = data
        .get(off..)
        .ok_or_else(|| Miss::Nothing("offset past end".to_owned()))?;
    if is_zlib_header(data, off) {
        return match inf.chain(data, off, limit, cancel, sink) {
            Ok(c) => Ok(Hit {
                stored_len: c.stored_len,
                size: c.size,
                codec: Codec::Zlib,
                pages: c.pages,
                first_page_size: c.first_size,
                provisional: c.truncated,
                truncated: c.truncated,
                nested: None,
            }),
            Err(Fail::Cancelled) => Err(Miss::Cancelled),
            Err(e) => Err(Miss::Corrupt(format!("zlib stream: {}", e.describe()))),
        };
    }
    match rest.get(..4) {
        Some(b"ADAT") => {
            if let Some((len, provisional)) = adat_len(rest) {
                return Ok(stored(rest, len, provisional, sink));
            }
        }
        Some(b"9FPR") => {
            if let Some(h) = nested_header_at(data, off) {
                let len = u64::from(h.payload_start);
                let mut hit = stored(rest, len, true, sink);
                hit.nested = Some(h);
                return Ok(hit);
            }
        }
        Some([b'K', b'B', b'2', _]) => {
            if let Some(len) = bink_len(rest) {
                return Ok(stored(rest, len, false, sink));
            }
        }
        _ => {}
    }
    if raw_deflate {
        return match inf.raw(rest, limit, cancel, sink) {
            Ok((consumed, produced)) => {
                let end = off.saturating_add(consumed as usize);
                let padded = data
                    .get(end..align16(end).min(data.len()))
                    .is_some_and(|s| s.iter().all(|&b| b == 0));
                if produced >= RAW_MIN_OUTPUT && consumed >= 4 && padded {
                    // Raw DEFLATE carries no checksum: the decode is unverified.
                    Ok(Hit {
                        stored_len: consumed,
                        size: produced,
                        codec: Codec::RawDeflate,
                        pages: 1,
                        first_page_size: produced,
                        provisional: true,
                        truncated: false,
                        nested: None,
                    })
                } else {
                    Err(Miss::Nothing(format!(
                        "raw DEFLATE candidate rejected ({produced} bytes out, {consumed} in, padded {padded})"
                    )))
                }
            }
            Err(Fail::Cancelled) => Err(Miss::Cancelled),
            Err(Fail::Limit { consumed, produced }) => Ok(Hit {
                stored_len: consumed,
                size: produced,
                codec: Codec::RawDeflate,
                pages: 1,
                first_page_size: produced,
                provisional: true,
                truncated: true,
                nested: None,
            }),
            Err(e) => Err(Miss::Nothing(format!("raw DEFLATE: {}", e.describe()))),
        };
    }
    let sig: Vec<u8> = rest.iter().take(4).copied().collect();
    Err(Miss::Nothing(format!(
        "no known signature (bytes {sig:02x?})"
    )))
}

fn stored(rest: &[u8], len: u64, provisional: bool, sink: &mut dyn FnMut(&[u8])) -> Hit {
    let slice = rest
        .get(..usize::try_from(len).unwrap_or(usize::MAX))
        .unwrap_or(rest);
    sink(slice);
    Hit {
        stored_len: slice.len() as u64,
        size: slice.len() as u64,
        codec: Codec::Stored,
        pages: 1,
        first_page_size: slice.len() as u64,
        provisional,
        truncated: false,
        nested: None,
    }
}
