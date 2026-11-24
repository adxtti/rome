//! AWC (ADAT) audio banks: stream tables, Opus packet extraction, Ogg Opus muxing, decoding.

pub mod codec;
pub mod error;
pub mod ogg;
pub mod opus;

pub use codec::Codec;
pub use error::{AwcError, Result};
pub use ogg::{
    DEFAULT_PACKETS_PER_PAGE, DEFAULT_PRE_SKIP, OggOpusOptions, ogg_crc32, write_ogg_opus,
    write_ogg_opus_with,
};
pub use opus::{
    MAX_PACKET_SAMPLES, OpusBandwidth, OpusMode, OpusPacketError, OpusToc, opus_packet_frames,
    opus_packet_samples,
};
