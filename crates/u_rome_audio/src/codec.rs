//! AWC codec identifiers.
//!
//! Sources:
//! - vgmstream `src/meta/awc.c` (ISC-style license; facts only): 0x00/0x01 PCM,
//!   0x04 IMA ADPCM, 0x05 XMA2, 0x07 MPEG, 0x08 Vorbis, 0x0C/0x10 DSP,
//!   0x0D Opus (RDR2 PC, different block layout), 0x0F ATRAC9, 0x11 MS ADPCM.
//! - Albotterin/AWC_from_RPF9_Extractor `docs/FORMAT.md` (MIT): 0x0B Opus in
//!   GTA VI music banks (standard Opus packets, 0x800-byte units).

use std::fmt;

/// Codec of an AWC stream, from the stream/sfx header codec byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    /// 16-bit PCM (ids 0x00 and 0x01).
    Pcm,
    /// IMA ADPCM (0x04).
    ImaAdpcm,
    /// XMA2 (0x05).
    Xma2,
    /// MPEG audio (0x07).
    Mpeg,
    /// RAGE custom Vorbis (0x08).
    Vorbis,
    /// Standard Opus packets in 0x800-byte units (0x0B, GTA VI).
    Opus,
    /// RDR2-style Opus with a different block layout (0x0D). Not supported.
    OpusRdr,
    /// Nintendo DSP ADPCM (0x0C, 0x10).
    Dsp,
    /// ATRAC9 (0x0F).
    Atrac9,
    /// RIFF MS ADPCM (0x11).
    MsAdpcm,
    /// MIDI data, no audio (0xFF / chunk 0x68).
    Midi,
    /// Unrecognised codec id.
    Unknown(u8),
}

impl Codec {
    /// Map a raw codec byte.
    pub fn from_raw(raw: u8) -> Codec {
        match raw {
            0x00 | 0x01 => Codec::Pcm,
            0x04 => Codec::ImaAdpcm,
            0x05 => Codec::Xma2,
            0x07 => Codec::Mpeg,
            0x08 => Codec::Vorbis,
            0x0B => Codec::Opus,
            0x0C | 0x10 => Codec::Dsp,
            0x0D => Codec::OpusRdr,
            0x0F => Codec::Atrac9,
            0x11 => Codec::MsAdpcm,
            0xFF => Codec::Midi,
            other => Codec::Unknown(other),
        }
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Pcm => "PCM",
            Codec::ImaAdpcm => "IMA ADPCM",
            Codec::Xma2 => "XMA2",
            Codec::Mpeg => "MPEG",
            Codec::Vorbis => "Vorbis",
            Codec::Opus => "Opus",
            Codec::OpusRdr => "Opus (RDR2 layout)",
            Codec::Dsp => "DSP ADPCM",
            Codec::Atrac9 => "ATRAC9",
            Codec::MsAdpcm => "MS ADPCM",
            Codec::Midi => "MIDI",
            Codec::Unknown(_) => "unknown",
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Codec::Unknown(raw) => write!(f, "unknown (0x{raw:02X})"),
            other => f.write_str(other.name()),
        }
    }
}
