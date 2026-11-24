//! Synthetic AWC-style `ADAT` audio banks with an Opus-like unit layout.
//!
//! Layout (little-endian), modelled on the public GTA V AWC notes:
//!
//! ```text
//! 0x00  [u8;4] "ADAT"
//! 0x04  u16    version (1)
//! 0x06  u16    flags (0xFF00: no chunk-index table)
//! 0x08  u32    stream count
//! 0x0C  u32    data offset (first chunk, 16-byte aligned)
//! 0x10  u32 x streams      stream info: id (low 29 bits) | tag count << 29
//! ....  u64 x total tags   chunk info: offset (bits 0-27) | size (bits 28-55) | tag (bits 56-63)
//! ....  zero padding to the data offset
//! data  per stream, each chunk 16-byte aligned: format, seek table, data
//! ```
//!
//! Chunks per stream:
//!
//! - [`TAG_FORMAT`] (0x18 bytes): u32 samples, i32 loop point (-1), u16
//!   sample rate, i16 headroom, u16 loop begin, u16 loop end, u16 play end,
//!   u8 play begin, u8 codec, 4 zero bytes.
//! - [`TAG_SEEK`]: one u32 per unit, the first sample of that unit.
//! - [`TAG_DATA`]: `units` fixed-size units of `unit_size` bytes. Each unit is
//!   u16 frame count, then per frame u16 frame length, a TOC byte (0xF8,
//!   CELT fullband 20 ms) and noise, then zero padding to `unit_size`. Every
//!   frame stands for 960 samples.
//!
//! The codec id and unit framing are synthetic; they mirror the shape of an
//! Opus-in-AWC stream, not a confirmed GTA VI layout.

use crate::{ALIGN, Prng, align_up, pad_to_align};

/// Chunk tag of the sample data.
pub const TAG_DATA: u8 = 0x55;
/// Chunk tag of the format block.
pub const TAG_FORMAT: u8 = 0xFA;
/// Chunk tag of the per-unit seek table.
pub const TAG_SEEK: u8 = 0xA3;
/// Placeholder codec id written into the format block.
pub const DEFAULT_CODEC: u8 = 0x0D;

const SAMPLES_PER_FRAME: u32 = 960;
const OPUS_TOC: u8 = 0xF8;

/// One stream to place in the bank.
#[derive(Clone, Debug)]
pub struct AdatStream {
    /// Stream id (29 bits are stored).
    pub id: u32,
    pub sample_rate: u16,
    pub units: usize,
    /// Bytes per unit, rounded up to a multiple of 16 and to fit the frames.
    pub unit_size: usize,
    pub frames_per_unit: usize,
}

impl AdatStream {
    /// 48 kHz, 1024-byte units with 3 frames each.
    pub fn new(id: u32, units: usize) -> Self {
        Self {
            id,
            sample_rate: 48_000,
            units,
            unit_size: 1024,
            frames_per_unit: 3,
        }
    }

    fn effective_unit_size(&self) -> usize {
        align_up(self.unit_size.max(2 + 3 * self.frames_per_unit), ALIGN)
    }
}

/// One chunk as placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdatChunk {
    pub tag: u8,
    /// Offset from the start of the bank.
    pub offset: u64,
    pub size: u64,
}

/// One stream as placed.
#[derive(Clone, Debug)]
pub struct AdatStreamInfo {
    pub id: u32,
    pub samples: u32,
    /// Format, seek, data, in that order.
    pub chunks: Vec<AdatChunk>,
    /// Offset of every unit, from the start of the bank.
    pub unit_offsets: Vec<u64>,
    pub unit_size: usize,
}

#[derive(Clone, Debug)]
pub struct AdatManifest {
    pub data_offset: u64,
    pub streams: Vec<AdatStreamInfo>,
}

#[derive(Clone, Debug)]
pub struct BuiltAdat {
    pub bytes: Vec<u8>,
    pub manifest: AdatManifest,
}

/// Builds an `ADAT` bank. See the module docs for the layout.
#[derive(Clone, Debug)]
pub struct AdatBuilder {
    seed: u64,
    codec: u8,
    streams: Vec<AdatStream>,
}

impl AdatBuilder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            codec: DEFAULT_CODEC,
            streams: Vec::new(),
        }
    }

    pub fn codec(mut self, codec: u8) -> Self {
        self.codec = codec;
        self
    }

    pub fn stream(mut self, stream: AdatStream) -> Self {
        self.streams.push(stream);
        self
    }

    pub fn build(&self) -> BuiltAdat {
        let mut noise = Prng::new(self.seed);
        let n = self.streams.len();
        let table_len = 16 + 4 * n + 8 * 3 * n;
        let data_offset = align_up(table_len, ALIGN);
        let mut out = vec![0u8; data_offset];

        let mut infos = Vec::with_capacity(n);
        for s in &self.streams {
            let unit_size = s.effective_unit_size();
            let frames = s.frames_per_unit as u32;
            let samples_per_unit = frames * SAMPLES_PER_FRAME;
            let samples = samples_per_unit.saturating_mul(s.units as u32);

            pad_to_align(&mut out);
            let fmt_off = out.len();
            out.extend_from_slice(&samples.to_le_bytes());
            out.extend_from_slice(&(-1i32).to_le_bytes());
            out.extend_from_slice(&s.sample_rate.to_le_bytes());
            out.extend_from_slice(&[0u8; 2 + 2 + 2 + 2 + 1]);
            out.push(self.codec);
            out.extend_from_slice(&[0u8; 4]);
            let fmt = chunk(TAG_FORMAT, fmt_off, out.len());

            pad_to_align(&mut out);
            let seek_off = out.len();
            for u in 0..s.units as u32 {
                out.extend_from_slice(&u.saturating_mul(samples_per_unit).to_le_bytes());
            }
            let seek = chunk(TAG_SEEK, seek_off, out.len());

            pad_to_align(&mut out);
            let data_off = out.len();
            let mut unit_offsets = Vec::with_capacity(s.units);
            for _ in 0..s.units {
                let unit_start = out.len();
                unit_offsets.push(unit_start as u64);
                out.extend_from_slice(&(s.frames_per_unit as u16).to_le_bytes());
                let per_frame = (unit_size - 2) / s.frames_per_unit.max(1);
                for _ in 0..s.frames_per_unit {
                    let body = per_frame - 2 - noise.below(per_frame as u64 / 4) as usize;
                    out.extend_from_slice(&(body as u16).to_le_bytes());
                    out.push(OPUS_TOC);
                    noise.push_noise(&mut out, body - 1);
                }
                out.resize(unit_start + unit_size, 0);
            }
            let data = chunk(TAG_DATA, data_off, out.len());

            infos.push(AdatStreamInfo {
                id: s.id & 0x1FFF_FFFF,
                samples,
                chunks: vec![fmt, seek, data],
                unit_offsets,
                unit_size,
            });
        }

        let mut header = Vec::with_capacity(table_len);
        header.extend_from_slice(b"ADAT");
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0xFF00u16.to_le_bytes());
        header.extend_from_slice(&(n as u32).to_le_bytes());
        header.extend_from_slice(&(data_offset as u32).to_le_bytes());
        for info in &infos {
            let v = info.id | ((info.chunks.len() as u32) << 29);
            header.extend_from_slice(&v.to_le_bytes());
        }
        for info in &infos {
            for c in &info.chunks {
                let v = (u64::from(c.tag) << 56)
                    | ((c.size & 0x0FFF_FFFF) << 28)
                    | (c.offset & 0x0FFF_FFFF);
                header.extend_from_slice(&v.to_le_bytes());
            }
        }
        out[..header.len()].copy_from_slice(&header);

        BuiltAdat {
            bytes: out,
            manifest: AdatManifest {
                data_offset: data_offset as u64,
                streams: infos,
            },
        }
    }
}

fn chunk(tag: u8, start: usize, end: usize) -> AdatChunk {
    AdatChunk {
        tag,
        offset: start as u64,
        size: (end - start) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::{AssetKind, Reader, detect};

    #[test]
    fn header_tables_match_manifest() {
        let built = AdatBuilder::new(1)
            .stream(AdatStream::new(0x0ABC_DEF1, 4))
            .stream(AdatStream {
                unit_size: 500,
                frames_per_unit: 2,
                ..AdatStream::new(0x2000_0007, 2)
            })
            .build();
        let b = &built.bytes;
        let m = &built.manifest;
        assert_eq!(detect(b), AssetKind::Audio);
        let r = Reader::new(b);
        assert_eq!(r.u16_at(4).unwrap(), 1);
        assert_eq!(r.u16_at(6).unwrap(), 0xFF00);
        assert_eq!(r.u32_at(8).unwrap(), 2);
        assert_eq!(r.u32_at(12).unwrap() as u64, m.data_offset);
        assert_eq!(m.data_offset % 16, 0);
        assert_eq!(m.streams[1].id, 7, "id masked to 29 bits");

        let mut tag_pos = 16 + 4 * m.streams.len();
        for (i, s) in m.streams.iter().enumerate() {
            let info = r.u32_at(16 + 4 * i).unwrap();
            assert_eq!(info & 0x1FFF_FFFF, s.id);
            assert_eq!(info >> 29, 3);
            for c in &s.chunks {
                let v = r.u64_at(tag_pos).unwrap();
                tag_pos += 8;
                assert_eq!((v >> 56) as u8, c.tag);
                assert_eq!((v >> 28) & 0x0FFF_FFFF, c.size);
                assert_eq!(v & 0x0FFF_FFFF, c.offset);
                assert_eq!(c.offset % 16, 0);
                assert!(c.offset + c.size <= b.len() as u64);
            }
            let fmt = s.chunks[0];
            assert_eq!(fmt.size, 0x18);
            assert_eq!(r.u32_at(fmt.offset as usize).unwrap(), s.samples);
            assert_eq!(b[fmt.offset as usize + 0x13], DEFAULT_CODEC);

            let seek = s.chunks[1];
            assert_eq!(seek.size as usize, 4 * s.unit_offsets.len());
            let data = s.chunks[2];
            assert_eq!(data.size as usize, s.unit_size * s.unit_offsets.len());
            assert_eq!(s.unit_size % 16, 0);
            for &u in &s.unit_offsets {
                let mut ur = Reader::new(b);
                ur.seek(u as usize);
                let frames = ur.u16().unwrap() as usize;
                let mut total = 2;
                for _ in 0..frames {
                    let len = ur.u16().unwrap() as usize;
                    assert_eq!(ur.u8().unwrap(), OPUS_TOC);
                    ur.skip(len - 1);
                    total += 2 + len;
                }
                assert!(total <= s.unit_size);
            }
        }
        assert_eq!(
            built.bytes,
            AdatBuilder::new(1)
                .stream(AdatStream::new(0x0ABC_DEF1, 4))
                .stream(AdatStream {
                    unit_size: 500,
                    frames_per_unit: 2,
                    ..AdatStream::new(0x2000_0007, 2)
                })
                .build()
                .bytes
        );
    }

    #[test]
    fn empty_bank() {
        let built = AdatBuilder::new(0).build();
        assert_eq!(built.bytes.len(), 16);
        assert_eq!(detect(&built.bytes), AssetKind::Audio);
    }
}
