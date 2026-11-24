//! Synthetic `RDS5` chunk stores.
//!
//! Layout (little-endian):
//!
//! ```text
//! 0x00  [u8;4] "RDS5"
//! 0x04  u32    version (1)
//! 0x08  u32    chunk count
//! 0x0C  u32    data offset (16-byte aligned)
//! 0x10  chunk table, 16 bytes per chunk: u64 key, u32 offset, u32 size
//! ....  zero padding to the data offset
//! data  chunks, each 16-byte aligned, offsets from the start of the store
//! ```
//!
//! This is a minimal stand-in for scanner and dispatcher tests, not a
//! complete texture store layout.

use crate::{ALIGN, Prng, align_up, pad_to_align};

/// One chunk as placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rds5Chunk {
    pub key: u64,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct BuiltRds5 {
    pub bytes: Vec<u8>,
    pub chunks: Vec<Rds5Chunk>,
}

#[derive(Clone, Debug)]
enum Chunk {
    Raw(u64, Vec<u8>),
    Noise(u64, usize),
}

/// Builds an `RDS5` store. See the module docs for the layout.
#[derive(Clone, Debug)]
pub struct Rds5Builder {
    seed: u64,
    chunks: Vec<Chunk>,
}

impl Rds5Builder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            chunks: Vec::new(),
        }
    }

    /// Append a chunk with explicit contents.
    pub fn chunk(mut self, key: u64, data: impl Into<Vec<u8>>) -> Self {
        self.chunks.push(Chunk::Raw(key, data.into()));
        self
    }

    /// Append a chunk of `len` noise bytes.
    pub fn noise_chunk(mut self, key: u64, len: usize) -> Self {
        self.chunks.push(Chunk::Noise(key, len));
        self
    }

    pub fn build(&self) -> BuiltRds5 {
        let mut noise = Prng::new(self.seed);
        let n = self.chunks.len();
        let data_offset = align_up(16 + 16 * n, ALIGN);
        let mut out = vec![0u8; data_offset];
        let mut placed = Vec::with_capacity(n);
        for c in &self.chunks {
            pad_to_align(&mut out);
            let offset = out.len();
            let key = match c {
                Chunk::Raw(key, data) => {
                    out.extend_from_slice(data);
                    *key
                }
                Chunk::Noise(key, len) => {
                    noise.push_noise(&mut out, *len);
                    *key
                }
            };
            placed.push(Rds5Chunk {
                key,
                offset: offset as u64,
                size: (out.len() - offset) as u64,
            });
        }

        let mut header = Vec::with_capacity(data_offset);
        header.extend_from_slice(b"RDS5");
        for v in [1u32, n as u32, data_offset as u32] {
            header.extend_from_slice(&v.to_le_bytes());
        }
        for c in &placed {
            header.extend_from_slice(&c.key.to_le_bytes());
            header.extend_from_slice(&(c.offset as u32).to_le_bytes());
            header.extend_from_slice(&(c.size as u32).to_le_bytes());
        }
        out[..header.len()].copy_from_slice(&header);
        BuiltRds5 {
            bytes: out,
            chunks: placed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::{AssetKind, Reader, detect};

    #[test]
    fn table_matches_chunks() {
        let built = Rds5Builder::new(2)
            .noise_chunk(0xAAAA, 100)
            .chunk(0xBBBB, vec![7; 33])
            .noise_chunk(0xCCCC, 0)
            .build();
        let b = &built.bytes;
        assert_eq!(detect(b), AssetKind::TextureStore);
        let r = Reader::new(b);
        assert_eq!(r.u32_at(4).unwrap(), 1);
        assert_eq!(r.u32_at(8).unwrap(), 3);
        let data_offset = r.u32_at(12).unwrap() as u64;
        assert_eq!(data_offset, 64);
        for (i, c) in built.chunks.iter().enumerate() {
            let e = 16 + 16 * i;
            assert_eq!(r.u64_at(e).unwrap(), c.key);
            assert_eq!(r.u32_at(e + 8).unwrap() as u64, c.offset);
            assert_eq!(r.u32_at(e + 12).unwrap() as u64, c.size);
            assert_eq!(c.offset % 16, 0);
            assert!(c.offset >= data_offset);
        }
        assert_eq!(built.chunks[1].size, 33);
        assert_eq!(&b[built.chunks[1].offset as usize..][..33], &[7; 33]);
        assert_eq!(built.chunks[2].size, 0);
        assert_eq!(
            Rds5Builder::new(2)
                .noise_chunk(0xAAAA, 100)
                .chunk(0xBBBB, vec![7; 33])
                .noise_chunk(0xCCCC, 0)
                .build()
                .bytes,
            built.bytes
        );
    }
}
