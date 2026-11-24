//! Deterministic synthetic test data for u_rome.
//!
//! Nothing in this crate is, or is derived from, game data. Every builder
//! produces bytes from a seed with a fixed PRNG ([`Prng`]), so tests across the
//! workspace can assert exact offsets and sizes without shipping fixtures.
//!
//! Builders:
//!
//! - [`Rpf9Builder`]: a whole RPF9 archive (header, front tables, zlib entries
//!   paged at 64 KiB, virtual resources, stored blobs, nested archives,
//!   trailer) plus an [`Rpf9Manifest`] of exactly what was placed where.
//! - [`AdatBuilder`]: an AWC-style `ADAT` audio bank with an Opus-like unit
//!   layout.
//! - [`PsinBuilder`]: a `PSIN` section chain (tag + big-endian size headers).
//! - [`Rds5Builder`]: an `RDS5` chunk store header and chunk table.
//! - [`kb2_stub`]: a minimal Bink 2 (`KB2j`) header with noise payload.
//!
//! The layouts of ADAT, PSIN, RDS5 and KB2 here are intentionally minimal and
//! documented on each builder. They model what scanners and dispatchers look
//! at (magic, framing, sizes), not every field a full decoder needs.
//!
//! Alignment guarantee: random-looking regions (front tables, trailers,
//! payload noise) never have a byte at a 16-byte-aligned file offset that
//! could begin a zlib header or a known magic, so a scanner walking aligned
//! offsets finds exactly the streams and blobs listed in the manifest.

mod adat;
mod prng;
mod psin;
mod rds5;
mod rpf9;

pub use adat::{
    AdatBuilder, AdatChunk, AdatManifest, AdatStream, AdatStreamInfo, BuiltAdat, DEFAULT_CODEC,
    TAG_DATA, TAG_FORMAT, TAG_SEEK,
};
pub use prng::Prng;
pub use psin::{BuiltPsin, PSIN_CHAIN, PsinBuilder, PsinSection};
pub use rds5::{BuiltRds5, Rds5Builder, Rds5Chunk};
pub use rpf9::{
    BuiltRpf9, EntryKind, FLAGS, GENERATION, HEADER_0X1C, HEADER_0X30, HEADER_LEN, MAGIC, Page,
    PlacedEntry, Rpf9Builder, Rpf9Manifest, VERSION,
};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

/// Alignment of every entry, page and blob inside an archive.
pub const ALIGN: usize = 16;

/// Decoded size of every zlib page except the last one of an entry.
pub const PAGE_SIZE: usize = 0x1_0000;

/// Round `n` up to a multiple of `align` (a power of two).
pub const fn align_up(n: usize, align: usize) -> usize {
    (n + align - 1) & !(align - 1)
}

/// Pad `out` with zero bytes up to the next multiple of [`ALIGN`].
pub fn pad_to_align(out: &mut Vec<u8>) {
    let target = align_up(out.len(), ALIGN);
    out.resize(target, 0);
}

/// True if `b` could be the first byte of a zlib header (CM = 8, CINFO <= 7)
/// or of a magic recognised by [`u_rome_core::detect`]. Noise regions never
/// carry such a byte at an aligned offset.
pub const fn can_start_signature(b: u8) -> bool {
    let zlib_cmf = (b & 0x0F) == 0x08 && (b >> 4) <= 7;
    zlib_cmf
        || matches!(
            b,
            b'9' | b'R' | b'E' | b'P' | b'0' | b'A' | b'K' | b'B' | b'<' | 0xEF
        )
}

/// True if `data` starts with a valid zlib header (CMF/FLG check).
pub fn is_zlib_header(data: &[u8]) -> bool {
    match data {
        [cmf, flg, ..] => {
            (cmf & 0x0F) == 0x08
                && (cmf >> 4) <= 7
                && ((u16::from(*cmf) << 8) | u16::from(*flg)) % 31 == 0
        }
        _ => false,
    }
}

/// Compress `data` into one zlib stream at `level` (0-9).
pub fn zlib_compress(data: &[u8], level: u32) -> Vec<u8> {
    let mut c = Compress::new(Compression::new(level.min(9)), true);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(64));
        }
        let consumed = c.total_in() as usize;
        let input = data.get(consumed..).unwrap_or_default();
        match c.compress_vec(input, &mut out, FlushCompress::Finish) {
            Ok(Status::StreamEnd) => return out,
            Ok(_) => {}
            // In-memory compression with a growing buffer cannot fail; stop
            // rather than loop forever if the backend ever reports an error.
            Err(_) => return out,
        }
    }
}

/// Inflate the zlib stream at the start of `data`.
///
/// Returns the decoded bytes and the number of input bytes the stream
/// occupied, or `None` if `data` does not start with a complete, valid stream
/// or the output would exceed `max_out` bytes.
pub fn inflate_prefix(data: &[u8], max_out: usize) -> Option<(Vec<u8>, usize)> {
    if !is_zlib_header(data) {
        return None;
    }
    let mut d = Decompress::new(true);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        if out.len() == out.capacity() {
            if out.len() >= max_out {
                return None;
            }
            out.reserve(out.capacity().min(max_out - out.len()).max(1));
        }
        let (in_before, out_before) = (d.total_in(), d.total_out());
        let input = data.get(d.total_in() as usize..)?;
        let status = d
            .decompress_vec(input, &mut out, FlushDecompress::None)
            .ok()?;
        if status == Status::StreamEnd {
            return Some((out, d.total_in() as usize));
        }
        let stalled = d.total_in() == in_before && d.total_out() == out_before;
        if stalled && out.len() < out.capacity() {
            // No progress with room to spare: the input is truncated.
            return None;
        }
    }
}

/// A minimal Bink 2 header (`KB2j`) followed by noise, `len` bytes in total
/// (at least 44).
///
/// Layout (all little-endian u32 after the magic): file size - 8, frame
/// count, largest frame size, frame count again, width, height, fps
/// numerator (30), fps denominator (1), flags (0), audio track count (0).
pub fn kb2_stub(seed: u64, width: u32, height: u32, frames: u32, len: usize) -> Vec<u8> {
    let len = len.max(44);
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(b"KB2j");
    let size = u32::try_from(len - 8).unwrap_or(u32::MAX);
    let body = size.saturating_sub(36);
    let largest = body.checked_div(frames).unwrap_or(0);
    for v in [size, frames, largest, frames, width, height, 30, 1, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let rest = len - out.len();
    Prng::new(seed).push_noise(&mut out, rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::{AssetKind, detect};

    #[test]
    fn zlib_round_trip_and_trailing_bytes() {
        let data = Prng::new(3).text(10_000);
        for level in [0, 1, 6, 9] {
            let mut z = zlib_compress(&data, level);
            assert!(is_zlib_header(&z));
            let stream_len = z.len();
            z.extend_from_slice(&[0; 32]);
            let (dec, used) = inflate_prefix(&z, 1 << 20).expect("inflates");
            assert_eq!(dec, data);
            assert_eq!(used, stream_len);
        }
    }

    #[test]
    fn inflate_rejects_truncated_and_oversized() {
        let z = zlib_compress(&Prng::new(4).bytes(5000), 6);
        assert!(inflate_prefix(&z[..z.len() - 4], 1 << 20).is_none());
        assert!(inflate_prefix(&z, 100).is_none());
        assert!(inflate_prefix(&[], 100).is_none());
        assert!(inflate_prefix(&[0x78], 100).is_none());
    }

    #[test]
    fn signature_bytes_cover_known_starts() {
        for b in [0x08u8, 0x78, 0x58] {
            assert!(can_start_signature(b));
        }
        assert!(!can_start_signature(0x88));
        assert!(!can_start_signature(0xFF));
        assert!(!can_start_signature(0x00));
        for sig in [
            &b"9FPR"[..],
            b"RDS5",
            b"ETPF",
            b"PSIN",
            b"0DRP",
            b"ADAT",
            b"KB2",
            b"BIK",
        ] {
            assert!(can_start_signature(sig[0]));
        }
        assert!(can_start_signature(0x4042u32.to_le_bytes()[0]));
    }

    #[test]
    fn kb2_stub_header() {
        let v = kb2_stub(1, 1920, 1080, 10, 4096);
        assert_eq!(v.len(), 4096);
        assert_eq!(detect(&v), AssetKind::Video);
        assert_eq!(u32::from_le_bytes(v[4..8].try_into().unwrap()), 4088);
        assert_eq!(u32::from_le_bytes(v[20..24].try_into().unwrap()), 1920);
        assert_eq!(kb2_stub(1, 1920, 1080, 10, 4096), v);
        assert_eq!(kb2_stub(1, 1, 1, 0, 0).len(), 44);
    }
}
