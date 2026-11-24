//! Synthetic data builders for tests. No game data is used anywhere.

use crate::vertex::{VERTEX_MAGIC, VERTEX_MAGIC_FLOAT};

/// Builds vertex buffer blobs stream by stream.
pub struct BlobBuilder {
    pub float_positions: bool,
    pub base: [f32; 3],
    pub step: [f32; 3],
    pub axis_mode: u32,
    /// Raw position stream bytes.
    pub positions: Vec<u8>,
    /// (slot, format, bytes)
    pub streams: Vec<(u8, u8, Vec<u8>)>,
    /// Zero bytes inserted after each stream (alignment padding).
    pub padding: usize,
}

impl BlobBuilder {
    pub fn quantized(base: [f32; 3], step: [f32; 3], axis_mode: u32, words: &[u64]) -> Self {
        Self {
            float_positions: false,
            base,
            step,
            axis_mode,
            positions: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            streams: Vec::new(),
            padding: 0,
        }
    }

    pub fn float(positions: &[[f32; 3]]) -> Self {
        Self {
            float_positions: true,
            base: [0.0; 3],
            step: [0.0; 3],
            axis_mode: 2,
            positions: positions
                .iter()
                .flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes()))
                .collect(),
            streams: Vec::new(),
            padding: 0,
        }
    }

    pub fn stream(mut self, slot: u8, fmt: u8, bytes: Vec<u8>) -> Self {
        self.streams.push((slot, fmt, bytes));
        self
    }

    pub fn build(&self) -> Vec<u8> {
        let start = if self.float_positions { 0x60 } else { 0x80 };
        let mut out = vec![0u8; start];
        let slot0 = if self.float_positions {
            VERTEX_MAGIC_FLOAT
        } else {
            VERTEX_MAGIC
        };
        out[0..4].copy_from_slice(&slot0.to_le_bytes());
        if !self.float_positions {
            for i in 0..3 {
                out[0x60 + i * 4..0x64 + i * 4].copy_from_slice(&self.base[i].to_le_bytes());
                out[0x70 + i * 4..0x74 + i * 4].copy_from_slice(&self.step[i].to_le_bytes());
            }
            out[0x6C..0x70].copy_from_slice(&self.axis_mode.to_le_bytes());
        }
        out.extend_from_slice(&self.positions);
        out.resize(out.len() + self.padding, 0);
        for (slot, fmt, bytes) in &self.streams {
            if out.len() % 2 == 1 {
                out.push(0);
            }
            let off = out.len();
            let v = ((off as u32 / 2) << 8) | u32::from(*fmt);
            let s = usize::from(*slot) * 4;
            out[s..s + 4].copy_from_slice(&v.to_le_bytes());
            out.extend_from_slice(bytes);
            out.resize(out.len() + self.padding, 0);
        }
        out
    }
}

pub fn lcg(seed: &mut u64) -> u32 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*seed >> 33) as u32
}
