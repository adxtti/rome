//! RIFF WAVE output (16-bit PCM).

use crate::decode::Pcm;

/// Largest data chunk a RIFF size field can describe.
const MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;

/// Serialise PCM as a canonical 44-byte-header WAV file.
///
/// Audio longer than the 4 GiB RIFF limit is truncated to whole frames with a
/// warning. A channel count of 0 is written as 1.
pub fn write_wav(pcm: &Pcm) -> Vec<u8> {
    let channels = if pcm.channels == 0 {
        log::warn!("WAV: PCM has 0 channels, writing as mono");
        1
    } else {
        pcm.channels
    };
    let frame_bytes = u64::from(channels) * 2;
    let mut data_bytes = pcm.samples.len() as u64 * 2;
    data_bytes -= data_bytes % frame_bytes;
    if data_bytes > MAX_DATA_BYTES {
        log::warn!("WAV: {data_bytes} bytes of audio exceed the RIFF limit; truncating");
        data_bytes = MAX_DATA_BYTES - MAX_DATA_BYTES % frame_bytes;
    }
    let samples = (data_bytes / 2) as usize;

    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&pcm.sample_rate.to_le_bytes());
    let byte_rate = u64::from(pcm.sample_rate) * frame_bytes;
    out.extend_from_slice(&u32::try_from(byte_rate).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&(channels.saturating_mul(2)).to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for s in pcm.samples.iter().take(samples) {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields() {
        let pcm = Pcm {
            samples: vec![1, -2, 3, -4, 5, -6],
            channels: 2,
            sample_rate: 48_000,
        };
        let w = write_wav(&pcm);
        assert_eq!(w.len(), 44 + 12);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 12);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes([w[20], w[21]]), 1);
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 2);
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 192_000);
        assert_eq!(u16::from_le_bytes([w[32], w[33]]), 4);
        assert_eq!(u16::from_le_bytes([w[34], w[35]]), 16);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 12);
        assert_eq!(i16::from_le_bytes([w[46], w[47]]), -2);
    }

    #[test]
    fn partial_frame_dropped_and_zero_channels() {
        let pcm = Pcm {
            samples: vec![1, 2, 3],
            channels: 2,
            sample_rate: 22_050,
        };
        let w = write_wav(&pcm);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 4);
        assert_eq!(w.len(), 48);

        let mono = Pcm {
            samples: vec![7],
            channels: 0,
            sample_rate: 8_000,
        };
        let w = write_wav(&mono);
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1);
        assert_eq!(w.len(), 46);
    }
}
