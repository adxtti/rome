//! Deterministic pseudo-random bytes (SplitMix64).
//!
//! Test data must be byte-for-byte reproducible across platforms and runs, so
//! nothing in this crate uses OS randomness.

/// SplitMix64 generator. Same seed, same bytes, everywhere.
#[derive(Clone, Debug)]
pub struct Prng {
    state: u64,
}

impl Prng {
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform-ish value in `0..n`; returns 0 when `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Fill `buf` with raw pseudo-random bytes (no sanitising).
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// `len` raw pseudo-random bytes (no sanitising).
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        self.fill(&mut v);
        v
    }

    /// Append `len` random-looking bytes to `out`, sanitised so that no byte
    /// at a 16-byte-aligned index of `out` can begin a zlib header or a known
    /// asset magic (see [`crate::can_start_signature`]).
    ///
    /// The guarantee holds in the final file as long as `out[0]` is placed at
    /// a 16-byte-aligned file offset, which every builder here ensures.
    pub fn push_noise(&mut self, out: &mut Vec<u8>, len: usize) {
        let start = out.len();
        out.resize(start + len, 0);
        self.fill(&mut out[start..]);
        let first_aligned = crate::align_up(start, crate::ALIGN);
        for i in (first_aligned..out.len()).step_by(crate::ALIGN) {
            if crate::can_start_signature(out[i]) {
                out[i] = 0xFF;
            }
        }
    }

    /// Compressible, text-like bytes: words drawn from a small vocabulary.
    /// Useful when a test wants zlib to actually shrink something.
    pub fn text(&mut self, len: usize) -> Vec<u8> {
        const WORDS: [&str; 12] = [
            "vice",
            "city",
            "leonida",
            "port",
            "gellhorn",
            "keys",
            "grassrivers",
            "ambrosia",
            "mount",
            "kalaga",
            "drawable",
            "entity",
        ];
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            let w = WORDS[self.below(WORDS.len() as u64) as usize];
            out.extend_from_slice(w.as_bytes());
            out.push(if self.below(8) == 0 { b'\n' } else { b' ' });
        }
        out.truncate(len);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = Prng::new(7).bytes(100);
        let b = Prng::new(7).bytes(100);
        assert_eq!(a, b);
        assert_ne!(a, Prng::new(8).bytes(100));
        // Pin the first output so an accidental algorithm change is caught.
        assert_eq!(Prng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn noise_is_sanitised_at_aligned_offsets() {
        let mut out = vec![0u8; 5];
        let mut p = Prng::new(1);
        p.push_noise(&mut out, 1 << 16);
        for i in (0..out.len()).step_by(16) {
            assert!(!crate::can_start_signature(out[i]), "offset {i}");
        }
    }
}
