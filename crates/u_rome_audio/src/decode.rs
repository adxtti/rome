//! PCM decoding of AWC streams.
//!
//! Opus is decoded with the pure-Rust `opus-decoder` crate (MIT OR
//! Apache-2.0, `#![forbid(unsafe_code)]`, passes the RFC 8251 test vectors),
//! so no C toolchain is needed for Windows ARM64 builds.

use std::panic::{AssertUnwindSafe, catch_unwind};

use opus_decoder::OpusDecoder;

use crate::awc::AwcBank;
use crate::codec::Codec;
use crate::error::{AwcError, Result};
use crate::ogg::DEFAULT_PRE_SKIP;
use crate::opus::{MAX_PACKET_SAMPLES, opus_packet_samples};

/// Upper bound for pre-allocation driven by header sample counts.
const MAX_RESERVE_SAMPLES: u64 = 48_000 * 2 * 60 * 10;

/// Interleaved 16-bit PCM.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pcm {
    /// Interleaved samples.
    pub samples: Vec<i16>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl Pcm {
    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / f64::from(self.sample_rate)
        }
    }
}

/// Decode a stream to PCM.
///
/// - Opus streams: packets are decoded at 48 kHz, the first
///   [`DEFAULT_PRE_SKIP`] samples are dropped and the result is cut to the
///   header sample count when that is shorter. Packets that fail to decode
///   become silence of their nominal duration (logged); the call fails only
///   when no packet decodes.
/// - PCM sfx streams: the data chunk is read as 16-bit little-endian mono.
/// - Anything else returns [`AwcError::UnsupportedCodec`].
pub fn decode_pcm(bank: &AwcBank, stream_index: usize) -> Result<Pcm> {
    let info = bank
        .streams()
        .get(stream_index)
        .ok_or(AwcError::StreamIndex {
            index: stream_index,
            count: bank.streams().len(),
        })?;
    match info.codec {
        Codec::Opus => {
            let packets = bank.opus_packets(stream_index)?;
            let total =
                (info.sample_count > 0 && info.sample_rate == 48_000).then_some(info.sample_count);
            decode_opus_inner(
                &packets,
                info.channels,
                DEFAULT_PRE_SKIP,
                total,
                &format!("stream {stream_index} (id 0x{:08X})", info.id),
            )
        }
        Codec::Pcm => {
            let data = bank.sfx_data(stream_index)?;
            if bank.is_encrypted() {
                return Err(AwcError::Encrypted);
            }
            let mut samples: Vec<i16> = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b))
                .collect();
            if info.sample_count > 0 && (info.sample_count as usize) < samples.len() {
                samples.truncate(info.sample_count as usize);
            }
            if samples.is_empty() {
                return Err(AwcError::NoAudio {
                    stream: stream_index,
                });
            }
            Ok(Pcm {
                samples,
                channels: 1,
                sample_rate: info.sample_rate,
            })
        }
        codec => Err(AwcError::UnsupportedCodec {
            index: stream_index,
            id: info.id,
            codec,
            operation: "PCM decoding",
        }),
    }
}

/// Decode raw Opus packets (one logical stream) to 48 kHz PCM.
///
/// `channels` must be 1 or 2; `pre_skip` samples are dropped from the start.
pub fn decode_opus<P: AsRef<[u8]>>(packets: &[P], channels: u16, pre_skip: u16) -> Result<Pcm> {
    decode_opus_inner(packets, channels, pre_skip, None, "Opus packets")
}

fn decode_opus_inner<P: AsRef<[u8]>>(
    packets: &[P],
    channels: u16,
    pre_skip: u16,
    total_frames: Option<u64>,
    context: &str,
) -> Result<Pcm> {
    let ch = usize::from(channels);
    let new_decoder = || {
        OpusDecoder::new(48_000, ch).map_err(|e| AwcError::Decode {
            context: context.to_owned(),
            packets: packets.len(),
            message: format!("cannot create a {channels}-channel decoder: {e}"),
        })
    };
    let mut decoder = new_decoder()?;
    let mut buf = vec![0i16; MAX_PACKET_SAMPLES as usize * ch];
    let reserve = total_frames
        .unwrap_or(0)
        .saturating_add(u64::from(pre_skip))
        .saturating_mul(ch as u64)
        .min(MAX_RESERVE_SAMPLES);
    let mut out: Vec<i16> = Vec::with_capacity(reserve as usize);
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;

    for (i, p) in packets.iter().enumerate() {
        let p = p.as_ref();
        let result = catch_unwind(AssertUnwindSafe(|| decoder.decode(p, &mut buf, false)));
        let message = match result {
            Ok(Ok(n)) => {
                let n = n.saturating_mul(ch).min(buf.len());
                out.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                ok += 1;
                continue;
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => {
                // The decoder state is unknown after a panic: start over.
                decoder = new_decoder()?;
                "decoder panicked".to_owned()
            }
        };
        failed += 1;
        log::warn!("{context}: packet {i} ({} bytes): {message}", p.len());
        first_error.get_or_insert_with(|| format!("packet {i}: {message}"));
        let silence = opus_packet_samples(p).unwrap_or(0) as usize * ch;
        out.resize(out.len() + silence, 0);
    }

    if ok == 0 {
        return Err(AwcError::Decode {
            context: context.to_owned(),
            packets: packets.len(),
            message: first_error.unwrap_or_else(|| "no packets".into()),
        });
    }
    if failed > 0 {
        log::warn!(
            "{context}: {failed} of {} packets failed to decode",
            packets.len()
        );
    }

    let skip = (usize::from(pre_skip) * ch).min(out.len());
    out.drain(..skip);
    if let Some(total) = total_frames {
        let want = usize::try_from(total.saturating_mul(ch as u64)).unwrap_or(usize::MAX);
        if want < out.len() {
            out.truncate(want);
        }
    }
    Ok(Pcm {
        samples: out,
        channels,
        sample_rate: 48_000,
    })
}
