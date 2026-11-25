//! Reading one entry back by its [`Location`].

use crate::carve::{Miss, probe_at};
use crate::error::Rpf9Error;
use crate::inflate::Inflater;
use crate::{Archive, Location};
use std::borrow::Cow;
use std::sync::atomic::AtomicBool;

/// Decode the entry at `location`.
///
/// Each step of the chain is decoded with the same rules the scanner uses
/// (zlib with 64 KiB page stitching, ADAT, Bink, nested archive front region,
/// and raw DEFLATE as a last resort). `max_size` caps the decoded size of
/// every step, including intermediate nested archives.
pub fn read(archive: &Archive, location: &Location, max_size: u64) -> Result<Vec<u8>, Rpf9Error> {
    if location.chain.is_empty() {
        return Err(Rpf9Error::EmptyLocation);
    }
    let mut inf = Inflater::new();
    let mut buf: Cow<'_, [u8]> = Cow::Borrowed(archive.bytes());
    for (depth, &off) in location.chain.iter().enumerate() {
        let next = decode_entry(&buf, off, max_size, depth, &mut inf)?;
        buf = Cow::Owned(next);
    }
    Ok(buf.into_owned())
}

pub(crate) fn decode_entry(
    data: &[u8],
    off: u64,
    max_size: u64,
    depth: usize,
    inf: &mut Inflater,
) -> Result<Vec<u8>, Rpf9Error> {
    let len = data.len() as u64;
    let start = usize::try_from(off)
        .ok()
        .filter(|&o| o < data.len())
        .ok_or(Rpf9Error::OutOfRange {
            depth,
            offset: off,
            len,
        })?;
    let never = AtomicBool::new(false);
    let mut out = Vec::new();
    let mut over = false;
    let res = probe_at(data, start, inf, max_size, true, &never, &mut |b| {
        if over || out.len() as u64 + b.len() as u64 > max_size {
            over = true;
        } else {
            out.extend_from_slice(b);
        }
    });
    match res {
        Ok(hit) if hit.truncated || over => Err(Rpf9Error::TooLarge {
            depth,
            offset: off,
            limit: max_size,
        }),
        Ok(hit) => {
            if hit.provisional {
                log::debug!(
                    "read: entry at depth {depth} offset {off:#x} is provisional ({:?})",
                    hit.codec
                );
            }
            Ok(out)
        }
        Err(Miss::Cancelled) => Err(Rpf9Error::Corrupt {
            depth,
            offset: off,
            reason: "cancelled".to_owned(),
        }),
        Err(Miss::Corrupt(reason)) => {
            log::warn!("read: corrupt entry at depth {depth} offset {off:#x}: {reason}");
            Err(Rpf9Error::Corrupt {
                depth,
                offset: off,
                reason,
            })
        }
        Err(Miss::Nothing(reason)) => Err(Rpf9Error::NoEntry {
            depth,
            offset: off,
            reason,
        }),
    }
}
