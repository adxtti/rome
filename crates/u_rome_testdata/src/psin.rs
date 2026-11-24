//! Synthetic `PSIN` section chains.
//!
//! A PSO document is a contiguous chain of sections. Each section is a 4-byte
//! ASCII tag followed by a **big-endian** u32 size that includes the 8-byte
//! section header, then the payload. No padding between sections.
//!
//! [`PsinBuilder::standard`] emits the chain [`PSIN_CHAIN`]:
//! `PSIN, LSCH, LENM, LSNS, LGID, LRSC, CHKS`, with noise payloads and a
//! `CHKS` payload of three big-endian u32s: total document length, wrapping
//! byte sum of everything before the `CHKS` section, and 0. The checksum is a
//! stand-in; it is not the game's algorithm.

use crate::Prng;

/// Section order produced by [`PsinBuilder::standard`].
pub const PSIN_CHAIN: [[u8; 4]; 7] = [
    *b"PSIN", *b"LSCH", *b"LENM", *b"LSNS", *b"LGID", *b"LRSC", *b"CHKS",
];

/// One section as placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsinSection {
    pub tag: [u8; 4],
    /// Offset of the tag from the start of the document.
    pub offset: u64,
    /// Size including the 8-byte header (the value written).
    pub size: u32,
}

#[derive(Clone, Debug)]
pub struct BuiltPsin {
    pub bytes: Vec<u8>,
    pub sections: Vec<PsinSection>,
}

#[derive(Clone, Debug)]
enum Section {
    Raw([u8; 4], Vec<u8>),
    Noise([u8; 4], usize),
    Checksum,
}

/// Builds a section chain. See the module docs for the layout.
#[derive(Clone, Debug)]
pub struct PsinBuilder {
    seed: u64,
    sections: Vec<Section>,
}

impl PsinBuilder {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            sections: Vec::new(),
        }
    }

    /// The full [`PSIN_CHAIN`] with noise payloads and a computed `CHKS`.
    pub fn standard(seed: u64) -> Self {
        Self::new(seed)
            .noise_section(*b"PSIN", 0x60)
            .noise_section(*b"LSCH", 0x30)
            .noise_section(*b"LENM", 0x20)
            .noise_section(*b"LSNS", 0x18)
            .noise_section(*b"LGID", 0x10)
            .noise_section(*b"LRSC", 0x28)
            .checksum()
    }

    /// Append a section with an explicit payload.
    pub fn section(mut self, tag: [u8; 4], payload: impl Into<Vec<u8>>) -> Self {
        self.sections.push(Section::Raw(tag, payload.into()));
        self
    }

    /// Append a section with `len` bytes of noise payload.
    pub fn noise_section(mut self, tag: [u8; 4], len: usize) -> Self {
        self.sections.push(Section::Noise(tag, len));
        self
    }

    /// Append a `CHKS` section computed at build time.
    pub fn checksum(mut self) -> Self {
        self.sections.push(Section::Checksum);
        self
    }

    pub fn build(&self) -> BuiltPsin {
        let mut noise = Prng::new(self.seed);
        let mut out = Vec::new();
        let mut placed = Vec::with_capacity(self.sections.len());
        let total = self.total_len();
        for s in &self.sections {
            let offset = out.len();
            let tag = match s {
                Section::Raw(tag, _) | Section::Noise(tag, _) => *tag,
                Section::Checksum => *b"CHKS",
            };
            out.extend_from_slice(&tag);
            out.extend_from_slice(&[0; 4]);
            match s {
                Section::Raw(_, payload) => out.extend_from_slice(payload),
                Section::Noise(_, len) => noise.push_noise(&mut out, *len),
                Section::Checksum => {
                    let sum = out[..offset]
                        .iter()
                        .fold(0u32, |a, &b| a.wrapping_add(u32::from(b)));
                    out.extend_from_slice(&total.to_be_bytes());
                    out.extend_from_slice(&sum.to_be_bytes());
                    out.extend_from_slice(&0u32.to_be_bytes());
                }
            }
            let size = u32::try_from(out.len() - offset).unwrap_or(u32::MAX);
            out[offset + 4..offset + 8].copy_from_slice(&size.to_be_bytes());
            placed.push(PsinSection {
                tag,
                offset: offset as u64,
                size,
            });
        }
        BuiltPsin {
            bytes: out,
            sections: placed,
        }
    }

    fn total_len(&self) -> u32 {
        let n: usize = self
            .sections
            .iter()
            .map(|s| {
                8 + match s {
                    Section::Raw(_, p) => p.len(),
                    Section::Noise(_, len) => *len,
                    Section::Checksum => 12,
                }
            })
            .sum();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use u_rome_core::{AssetKind, Endian, Reader, detect};

    #[test]
    fn standard_chain_walks() {
        let built = PsinBuilder::standard(5).build();
        let b = &built.bytes;
        assert_eq!(detect(b), AssetKind::Pso);
        let mut r = Reader::with_endian(b, Endian::Big);
        let mut tags = Vec::new();
        while r.remaining() > 0 {
            let off = r.pos();
            let tag: [u8; 4] = r.bytes(4).unwrap().try_into().unwrap();
            let size = r.u32().unwrap();
            assert!(size >= 8);
            tags.push(tag);
            let s = built.sections[tags.len() - 1];
            assert_eq!((s.tag, s.offset, s.size), (tag, off as u64, size));
            r.seek(off + size as usize);
        }
        assert_eq!(r.pos(), b.len());
        assert_eq!(tags, PSIN_CHAIN);

        let chks = built.sections[6];
        let r = Reader::with_endian(b, Endian::Big);
        assert_eq!(chks.size, 20);
        assert_eq!(
            r.u32_at(chks.offset as usize + 8).unwrap() as usize,
            b.len()
        );
        let sum = b[..chks.offset as usize]
            .iter()
            .fold(0u32, |a, &x| a.wrapping_add(x.into()));
        assert_eq!(r.u32_at(chks.offset as usize + 12).unwrap(), sum);
        assert_eq!(PsinBuilder::standard(5).build().bytes, built.bytes);
    }

    #[test]
    fn custom_sections() {
        let built = PsinBuilder::new(0)
            .section(*b"PSIN", vec![1, 2, 3])
            .section(*b"LSCH", Vec::new())
            .build();
        assert_eq!(built.bytes, b"PSIN\0\0\0\x0b\x01\x02\x03LSCH\0\0\0\x08");
    }
}
