//! Opus packet TOC parsing (RFC 6716 section 3.1).
//!
//! Only the table-of-contents byte and, for frame code 3, the frame count byte
//! are interpreted. This is enough to compute packet durations for Ogg granule
//! positions without decoding audio.

use thiserror::Error;

/// Maximum duration of one Opus packet at 48 kHz (120 ms, RFC 6716 3.2.5).
pub const MAX_PACKET_SAMPLES: u32 = 5760;

/// Opus coding mode selected by the TOC config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusMode {
    Silk,
    Hybrid,
    Celt,
}

/// Audio bandwidth selected by the TOC config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpusBandwidth {
    /// 4 kHz.
    Narrow,
    /// 6 kHz.
    Medium,
    /// 8 kHz.
    Wide,
    /// 12 kHz.
    SuperWide,
    /// 20 kHz.
    Full,
}

/// Decoded TOC byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusToc {
    /// Config number 0..=31.
    pub config: u8,
    pub mode: OpusMode,
    pub bandwidth: OpusBandwidth,
    /// Samples per frame at 48 kHz (120, 240, 480, 960, 1920 or 2880).
    pub frame_samples: u32,
    /// Stereo flag (bit 2).
    pub stereo: bool,
    /// Frame count code 0..=3 (bits 0-1).
    pub frame_code: u8,
}

impl OpusToc {
    /// Parse a TOC byte. Every byte value is a valid TOC.
    pub fn parse(toc: u8) -> OpusToc {
        let config = toc >> 3;
        let (mode, bandwidth, frame_samples) = match config {
            // SILK-only: NB, MB, WB at 10/20/40/60 ms.
            0..=11 => {
                let bw = match config / 4 {
                    0 => OpusBandwidth::Narrow,
                    1 => OpusBandwidth::Medium,
                    _ => OpusBandwidth::Wide,
                };
                (
                    OpusMode::Silk,
                    bw,
                    [480, 960, 1920, 2880][(config & 3) as usize],
                )
            }
            // Hybrid: SWB, FB at 10/20 ms.
            12..=15 => {
                let bw = if config < 14 {
                    OpusBandwidth::SuperWide
                } else {
                    OpusBandwidth::Full
                };
                (OpusMode::Hybrid, bw, [480, 960][(config & 1) as usize])
            }
            // CELT-only: NB, WB, SWB, FB at 2.5/5/10/20 ms.
            _ => {
                let bw = match (config - 16) / 4 {
                    0 => OpusBandwidth::Narrow,
                    1 => OpusBandwidth::Wide,
                    2 => OpusBandwidth::SuperWide,
                    _ => OpusBandwidth::Full,
                };
                (
                    OpusMode::Celt,
                    bw,
                    [120, 240, 480, 960][(config & 3) as usize],
                )
            }
        };
        OpusToc {
            config,
            mode,
            bandwidth,
            frame_samples,
            stereo: toc & 0x04 != 0,
            frame_code: toc & 0x03,
        }
    }

    /// Channel count signalled by the stereo flag.
    pub fn channels(&self) -> u8 {
        if self.stereo { 2 } else { 1 }
    }
}

/// Malformed packet framing.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum OpusPacketError {
    #[error("empty Opus packet")]
    Empty,
    #[error("code 3 Opus packet is missing its frame count byte")]
    MissingFrameCount,
    #[error("code 3 Opus packet declares zero frames")]
    ZeroFrames,
    #[error("Opus packet duration {samples} samples exceeds 120 ms")]
    TooLong { samples: u32 },
}

/// Number of frames in a packet (RFC 6716 3.2).
pub fn opus_packet_frames(packet: &[u8]) -> Result<u32, OpusPacketError> {
    let toc = *packet.first().ok_or(OpusPacketError::Empty)?;
    match toc & 0x03 {
        0 => Ok(1),
        1 | 2 => Ok(2),
        _ => {
            let count = packet.get(1).ok_or(OpusPacketError::MissingFrameCount)? & 0x3F;
            if count == 0 {
                Err(OpusPacketError::ZeroFrames)
            } else {
                Ok(u32::from(count))
            }
        }
    }
}

/// Duration of a packet in samples per channel at 48 kHz.
pub fn opus_packet_samples(packet: &[u8]) -> Result<u32, OpusPacketError> {
    let toc = OpusToc::parse(*packet.first().ok_or(OpusPacketError::Empty)?);
    let samples = toc.frame_samples * opus_packet_frames(packet)?;
    if samples > MAX_PACKET_SAMPLES {
        return Err(OpusPacketError::TooLong { samples });
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6716 Table 2, frame sizes in ms (x10 to stay integral).
    fn expected(config: u8) -> (OpusMode, OpusBandwidth, u32) {
        use OpusBandwidth::*;
        use OpusMode::*;
        let ms_x10 = match config {
            0..=11 => [100, 200, 400, 600][(config % 4) as usize],
            12..=15 => [100, 200][(config % 2) as usize],
            _ => [25, 50, 100, 200][(config % 4) as usize],
        };
        let (mode, bw) = match config {
            0..=3 => (Silk, Narrow),
            4..=7 => (Silk, Medium),
            8..=11 => (Silk, Wide),
            12..=13 => (Hybrid, SuperWide),
            14..=15 => (Hybrid, Full),
            16..=19 => (Celt, Narrow),
            20..=23 => (Celt, Wide),
            24..=27 => (Celt, SuperWide),
            _ => (Celt, Full),
        };
        (mode, bw, ms_x10 * 48 / 10)
    }

    #[test]
    fn all_configs_and_codes() {
        for config in 0u8..32 {
            let (mode, bw, frame) = expected(config);
            for stereo in [false, true] {
                for code in 0u8..4 {
                    let toc = (config << 3) | (u8::from(stereo) << 2) | code;
                    let t = OpusToc::parse(toc);
                    assert_eq!(t.config, config);
                    assert_eq!(t.mode, mode, "config {config}");
                    assert_eq!(t.bandwidth, bw, "config {config}");
                    assert_eq!(t.frame_samples, frame, "config {config}");
                    assert_eq!(t.stereo, stereo);
                    assert_eq!(t.channels(), if stereo { 2 } else { 1 });
                    assert_eq!(t.frame_code, code);

                    let samples = match code {
                        0 => opus_packet_samples(&[toc, 0]).unwrap(),
                        1 | 2 => opus_packet_samples(&[toc, 0, 0]).unwrap(),
                        _ => {
                            // Largest frame count that stays within 120 ms.
                            let max = (MAX_PACKET_SAMPLES / frame) as u8;
                            for m in 1..=max {
                                assert_eq!(
                                    opus_packet_samples(&[toc, m]).unwrap(),
                                    frame * u32::from(m)
                                );
                                // VBR and padding flags must not affect the count.
                                assert_eq!(
                                    opus_packet_frames(&[toc, m | 0xC0]).unwrap(),
                                    u32::from(m)
                                );
                            }
                            assert_eq!(
                                opus_packet_samples(&[toc, max + 1]),
                                Err(OpusPacketError::TooLong {
                                    samples: frame * u32::from(max + 1)
                                })
                            );
                            frame
                        }
                    };
                    let frames = if code == 1 || code == 2 { 2 } else { 1 };
                    assert_eq!(samples, frame * frames);
                }
            }
        }
    }

    #[test]
    fn gta_vi_music_packet() {
        // FORMAT.md: TOC 0xFC = CELT fullband 20 ms stereo, one frame.
        let t = OpusToc::parse(0xFC);
        assert_eq!(t.mode, OpusMode::Celt);
        assert_eq!(t.bandwidth, OpusBandwidth::Full);
        assert!(t.stereo);
        assert_eq!(opus_packet_samples(&[0xFC; 320]).unwrap(), 960);
    }

    #[test]
    fn malformed() {
        assert_eq!(opus_packet_samples(&[]), Err(OpusPacketError::Empty));
        assert_eq!(
            opus_packet_samples(&[0xFF]),
            Err(OpusPacketError::MissingFrameCount)
        );
        assert_eq!(
            opus_packet_samples(&[0xFF, 0x00]),
            Err(OpusPacketError::ZeroFrames)
        );
        // SILK 60 ms x 3 = 180 ms.
        assert_eq!(
            opus_packet_samples(&[0x1B, 3]),
            Err(OpusPacketError::TooLong { samples: 8640 })
        );
    }
}
