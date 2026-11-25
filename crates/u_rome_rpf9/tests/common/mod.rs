//! Synthetic archive builder. No game data: everything is generated here.
#![allow(dead_code)]

use flate2::Compression;
use flate2::write::{DeflateEncoder, ZlibEncoder};
use std::io::Write;
use u_rome_core::AssetKind;
use u_rome_rpf9::Codec;

pub const PAGE: usize = 0x10000;

/// Deterministic xorshift64* generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }
}

pub fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; n];
    Rng::new(seed).fill(&mut v);
    v
}

/// Compressible pseudo-text.
pub fn text(n: usize, seed: u64) -> Vec<u8> {
    const WORDS: [&str; 12] = [
        "vice ", "city ", "ocean ", "drive ", "neon ", "palm ", "leonida ", "keys ", "mesh ",
        "bone ", "lod ", "shader ",
    ];
    let mut r = Rng::new(seed);
    let mut v = Vec::with_capacity(n + 16);
    while v.len() < n {
        v.extend_from_slice(WORDS[r.below(WORDS.len() as u64) as usize].as_bytes());
        if r.below(7) == 0 {
            v.extend_from_slice(format!("{} ", r.next() % 100_000).as_bytes());
        }
    }
    v.truncate(n);
    v
}

pub fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub fn raw_deflate(data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// A 64-byte header with fields in the observed ranges. Table 2 holds
/// `count` 9-byte records, so `table_split = payload_start - align16(9 * count)`.
pub fn header(payload_start: u32, count: u32, trailer: u32, id: u64) -> [u8; 64] {
    let mut h = [0u8; 64];
    let put =
        |h: &mut [u8; 64], off: usize, v: u32| h[off..off + 4].copy_from_slice(&v.to_le_bytes());
    h[0..4].copy_from_slice(b"9FPR");
    put(&mut h, 0x04, payload_start);
    put(&mut h, 0x08, payload_start - (9 * count).div_ceil(16) * 16);
    put(&mut h, 0x0C, trailer);
    h[0x10..0x18].copy_from_slice(&id.to_le_bytes());
    put(&mut h, 0x18, 0x4_7A00);
    put(&mut h, 0x1C, 0x150);
    put(&mut h, 0x20, 0x1_0005);
    put(&mut h, 0x24, count);
    put(&mut h, 0x28, 1);
    put(&mut h, 0x2C, 0x9_0F01);
    put(&mut h, 0x30, 0x10);
    h
}

/// ADAT bank: `streams` streams of `chunks` chunks each, with a chunk-index
/// table (flags bit 0) and payload filled with noise.
pub fn adat(streams: u32, chunks: u32, chunk_size: u32, seed: u64) -> Vec<u8> {
    let total = (streams * chunks) as usize;
    let tables_end = 16 + 2 * streams as usize + 4 * streams as usize + 8 * total;
    let data_offset = tables_end.div_ceil(16) * 16;
    let mut v = Vec::new();
    v.extend_from_slice(b"ADAT");
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0xFF01u16.to_le_bytes());
    v.extend_from_slice(&streams.to_le_bytes());
    v.extend_from_slice(&(data_offset as u32).to_le_bytes());
    for s in 0..streams {
        v.extend_from_slice(&((s * chunks) as u16).to_le_bytes());
    }
    for s in 0..streams {
        v.extend_from_slice(&((chunks << 29) | (0x1000 + s)).to_le_bytes());
    }
    let mut off = data_offset as u64;
    for _ in 0..total {
        let raw = off | (u64::from(chunk_size) << 28) | (0x55u64 << 56);
        v.extend_from_slice(&raw.to_le_bytes());
        off += u64::from(chunk_size);
    }
    v.resize(data_offset, 0);
    v.extend_from_slice(&noise(total * chunk_size as usize, seed));
    v
}

pub fn bink(len: usize, seed: u64) -> Vec<u8> {
    let mut v = noise(len, seed);
    v[..4].copy_from_slice(b"KB2j");
    v[4..8].copy_from_slice(&((len - 8) as u32).to_le_bytes());
    v[8..12].copy_from_slice(&42u32.to_le_bytes());
    v
}

/// What the scanner should report.
#[derive(Clone, Debug)]
pub struct Expect {
    pub chain: Vec<u64>,
    pub kind: AssetKind,
    pub codec: Codec,
    pub pages: u32,
    pub provisional: bool,
    /// Decoded bytes (for stored nested archives: the front region).
    pub data: Vec<u8>,
}

/// Appends entries to a buffer and records expectations.
pub struct Builder {
    pub buf: Vec<u8>,
    pub expect: Vec<Expect>,
    pub prefix: Vec<u64>,
}

impl Builder {
    /// Start an archive with a header and noise-filled "encrypted" tables.
    pub fn archive(count: u32, id: u64) -> Self {
        let payload_start = 0x340u32;
        let mut buf = header(payload_start, count, 0x90, id).to_vec();
        buf.extend_from_slice(&noise(payload_start as usize - 64, id ^ 0xABCD));
        Self {
            buf,
            expect: Vec::new(),
            prefix: Vec::new(),
        }
    }

    pub fn blob() -> Self {
        Self {
            buf: Vec::new(),
            expect: Vec::new(),
            prefix: Vec::new(),
        }
    }

    pub fn pos(&self) -> u64 {
        self.buf.len() as u64
    }

    pub fn align(&mut self, n: usize) -> &mut Self {
        let len = self.buf.len().div_ceil(n) * n;
        self.buf.resize(len, 0);
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    fn expect(
        &mut self,
        at: u64,
        kind: AssetKind,
        codec: Codec,
        pages: u32,
        prov: bool,
        data: Vec<u8>,
    ) {
        let mut chain = self.prefix.clone();
        chain.push(at);
        self.expect.push(Expect {
            chain,
            kind,
            codec,
            pages,
            provisional: prov,
            data,
        });
    }

    /// One zlib stream, 16-byte aligned, followed by padding.
    pub fn zlib(&mut self, data: &[u8], kind: AssetKind) -> u64 {
        self.align(16);
        let at = self.pos();
        self.buf.extend_from_slice(&zlib(data, 6));
        self.align(16);
        self.expect(at, kind, Codec::Zlib, 1, false, data.to_vec());
        at
    }

    /// A resource split into 64 KiB pages, each its own zlib stream.
    pub fn paged(&mut self, data: &[u8], kind: AssetKind) -> u64 {
        self.align(16);
        let at = self.pos();
        for page in data.chunks(PAGE) {
            self.align(16);
            self.buf.extend_from_slice(&zlib(page, 6));
        }
        self.align(16);
        let pages = data.len().div_ceil(PAGE) as u32;
        self.expect(at, kind, Codec::Zlib, pages, false, data.to_vec());
        at
    }

    /// 16 zero bytes, then a zlib stream.
    pub fn virtual_resource(&mut self, data: &[u8], kind: AssetKind) -> u64 {
        self.align(16);
        self.buf.extend_from_slice(&[0u8; 16]);
        self.zlib(data, kind)
    }

    pub fn stored(&mut self, data: &[u8], kind: AssetKind) -> u64 {
        self.align(16);
        let at = self.pos();
        self.buf.extend_from_slice(data);
        self.align(16);
        self.expect(at, kind, Codec::Stored, 1, false, data.to_vec());
        at
    }

    /// A nested archive placed in raw bytes: the header is reported as a
    /// provisional stored entry covering the front region; its streams are
    /// reported at this level.
    pub fn raw_nested(&mut self, inner: Builder) -> u64 {
        self.align(16);
        let at = self.pos();
        let front = u32::from_le_bytes(inner.buf[4..8].try_into().unwrap()) as usize;
        self.expect(
            at,
            AssetKind::Archive,
            Codec::Stored,
            1,
            true,
            inner.buf[..front].to_vec(),
        );
        for mut e in inner.expect {
            e.chain[0] += at;
            let mut chain = self.prefix.clone();
            chain.extend(e.chain);
            e.chain = chain;
            self.expect.push(e);
        }
        self.buf.extend_from_slice(&inner.buf);
        self.align(16);
        at
    }

    /// A zlib stream whose decoded bytes are `inner` (which may embed an
    /// archive). Inner expectations are re-rooted under the stream.
    pub fn zlib_container(&mut self, inner: Builder, kind: AssetKind) -> u64 {
        let at = self.zlib(&inner.buf, kind);
        for mut e in inner.expect {
            let mut chain = self.prefix.clone();
            chain.push(at);
            chain.extend(e.chain);
            e.chain = chain;
            self.expect.push(e);
        }
        at
    }

    /// Trailer of noise.
    pub fn finish(&mut self) -> Vec<u8> {
        self.align(16);
        self.buf.extend_from_slice(&noise(0x90, 0x7777));
        self.buf.clone()
    }
}
