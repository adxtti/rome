//! AWC (ADAT) audio banks: stream tables, Opus packet extraction, Ogg Opus muxing, decoding.
//!
//! # Overview
//!
//! ```no_run
//! use u_rome_audio::{AwcBank, decode_pcm, write_ogg_opus, write_wav, DEFAULT_PRE_SKIP};
//!
//! # fn main() -> u_rome_audio::Result<()> {
//! let bytes: Vec<u8> = std::fs::read("bank.awc").unwrap();
//! let bank = AwcBank::parse(bytes)?;
//! for (i, s) in bank.streams().iter().enumerate() {
//!     println!(
//!         "{i}: id 0x{:08X} {} {} Hz x{} {:.1}s events={} provisional={}",
//!         s.id, s.codec, s.sample_rate, s.channels, s.duration_secs(),
//!         s.events.len(), s.provisional
//!     );
//! }
//! // Lossless export: Opus packets in an Ogg container.
//! let packets = bank.opus_packets(0)?;
//! let ogg = write_ogg_opus(&packets, 2, 48_000, DEFAULT_PRE_SKIP);
//! // Or with end trimming and the stream id comment:
//! let ogg = bank.ogg_opus(0)?;
//! // Decoded playback / WAV export.
//! let pcm = decode_pcm(&bank, 0)?;
//! let wav = write_wav(&pcm);
//! # let _ = (ogg, wav);
//! # Ok(())
//! # }
//! ```
//!
//! # API
//!
//! - [`AwcBank::parse`]`(bytes) -> Result<AwcBank>`: header, stream table,
//!   music (0x48) and sfx (0xFA/0x76) headers, events (0xBD), seek table (0xA3).
//! - [`AwcBank::streams`]`() -> &[StreamInfo]`: id, codec, sample rate,
//!   channels, sample count, events, `provisional` flag and `notes`.
//! - [`AwcBank::opus_packets`]`(stream_index) -> Result<Vec<Vec<u8>>>`.
//! - [`AwcBank::ogg_opus`]`(stream_index) -> Result<Vec<u8>>`.
//! - [`AwcBank::blocks`] / [`AwcBank::block_sample_table`]: block-level
//!   diagnostics for streamed banks.
//! - [`write_ogg_opus`]`(packets, channels, sample_rate, pre_skip) -> Vec<u8>`
//!   and [`write_ogg_opus_with`] for full control.
//! - [`decode_pcm`]`(&bank, stream_index) -> Result<Pcm>`, [`decode_opus`].
//! - [`write_wav`]`(&Pcm) -> Vec<u8>`.
//! - [`opus_packet_samples`], [`OpusToc`]: TOC parsing (RFC 6716).
//!
//! # Supported
//!
//! - Streamed (music) banks with codec 0x0B Opus as documented in
//!   Albotterin/AWC_from_RPF9_Extractor `docs/FORMAT.md`. Each music
//!   "channel" is an independent (usually stereo) Opus stream.
//! - Single-stream PCM (codec 0x00/0x01) decoding, using vgmstream's GTA V
//!   sfx header layout (provisional for GTA VI).
//! - Single-stream Opus is attempted with the music unit layout and marked
//!   provisional.
//!
//! Other codecs (XMA2, MPEG, Vorbis, ADPCM, ATRAC9, DSP, RDR2 Opus) are
//! listed with their codec but extraction returns
//! [`AwcError::UnsupportedCodec`]. Encrypted (flag 0x0008) and big-endian
//! (`TADA`) banks are reported as errors.
//!
//! # Provisional facts
//!
//! Anything not confirmed by the GTA VI samples behind FORMAT.md sets
//! `StreamInfo::provisional` and adds a note: fallback header layouts, sfx
//! layout, non-Opus codecs, non-48 kHz rates, non-zero block skip values.
//! The Opus pre-skip of 312 ([`DEFAULT_PRE_SKIP`]) and the event field
//! meanings ([`AwcEvent::provisional`]) are also unconfirmed.

pub mod awc;
pub mod codec;
pub mod decode;
pub mod error;
pub mod ogg;
pub mod opus;
pub mod wav;

pub use awc::{
    AwcBank, AwcEvent, BlockChannel, BlockInfo, Chunk, RawStream, StreamInfo, StreamKind,
    UNIT_SIZE, event_name,
};
pub use codec::Codec;
pub use decode::{Pcm, decode_opus, decode_pcm};
pub use error::{AwcError, Result};
pub use ogg::{
    DEFAULT_PACKETS_PER_PAGE, DEFAULT_PRE_SKIP, OggOpusOptions, ogg_crc32, write_ogg_opus,
    write_ogg_opus_with,
};
pub use opus::{
    MAX_PACKET_SAMPLES, OpusBandwidth, OpusMode, OpusPacketError, OpusToc, opus_packet_frames,
    opus_packet_samples,
};
pub use wav::write_wav;
