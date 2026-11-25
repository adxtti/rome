//! AWC (`ADAT`) bank parsing and Opus packet extraction.
//!
//! Format facts come from two sources, cited inline:
//! - (FORMAT) Albotterin/AWC_from_RPF9_Extractor `docs/FORMAT.md` (MIT),
//!   derived from GTA VI samples and vgmstream.
//! - (VGM) vgmstream `src/meta/awc.c` and `awc_streamfile.h` (ISC-style
//!   license), derived from GTA V / RDR / Max Payne 3 banks. Facts only; no
//!   code was adapted.
//!
//! Layout summary (little-endian):
//!
//! ```text
//! 0x00 "ADAT" | u16 version | u16 flags | u32 stream count | u32 header size
//! [u16 x streams]  chunk start index per stream (when flagged)
//! u32 x streams    bits 0-28 stream id, bits 29-31 chunk count
//! u64 x chunks     bits 0-27 offset, 28-55 size, 56-63 tag
//! ```
//!
//! A stream that owns a 0x48 "music header" chunk describes a streamed bank:
//! its 0x55 data chunk is split into fixed-size blocks that interleave
//! several independent channel streams ((FORMAT) section 4).

use std::sync::Arc;

use u_rome_core::{Hash32, Reader, joaat};

use crate::codec::Codec;
use crate::error::{AwcError, ReadCtx, Result};
use crate::ogg::{DEFAULT_PRE_SKIP, OggOpusOptions, write_ogg_opus_with};
use crate::opus::{OpusToc, opus_packet_samples};

/// Chunk tags. (FORMAT) section 2, (VGM) `parse_awc_header`.
pub mod tag {
    /// Stream format / music header.
    pub const MUSIC_HEADER: u8 = 0x48;
    /// Audio data.
    pub const DATA: u8 = 0x55;
    /// Block-to-sample seek table, one u32 per block.
    pub const SEEK_TABLE: u8 = 0xA3;
    /// Event / marker records.
    pub const EVENTS: u8 = 0xBD;
    /// Single-stream (sfx) header.
    pub const SFX_HEADER: u8 = 0xFA;
    /// Single-stream Vorbis header.
    pub const VORBIS_SFX_HEADER: u8 = 0x76;
    /// Vorbis setup.
    pub const VORBIS_SETUP: u8 = 0x7F;
    /// MIDI data.
    pub const MIDI: u8 = 0x68;
}

/// Size of one audio unit inside a block ((FORMAT) section 4, (VGM) AWC_FRAME_SIZE).
pub const UNIT_SIZE: usize = 0x800;

/// Sanity limits for header-driven sizes.
const MAX_STREAMS: u32 = 65_536; // (FORMAT) scanner accepts 1..=65536
const MAX_MUSIC_CHANNELS: u32 = 32; // (VGM) AWC_MAX_MUSIC_CHANNELS
const MAX_UNITS_PER_BLOCK: i32 = 65_535; // (VGM) AWC_MAX_ENTRIES
const MAX_BLOCK_SIZE: u32 = 64 << 20; // samples use 0x80000
const MAX_EVENTS: usize = 1 << 20;
const BLOCK_CHANNEL_HEADER: usize = 0x18;

/// Header flag bits (low byte of the u16 at 0x06). (VGM)
const FLAG_CHUNK_INDICES: u16 = 0x0001;
const FLAG_ENCRYPTED: u16 = 0x0008;

/// One chunk table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub tag: u8,
    /// Offset from the start of the bank.
    pub offset: usize,
    pub size: usize,
}

/// One row of the stream table with its chunks, as stored in the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawStream {
    /// 29-bit stream id (JOAAT of the name, truncated).
    pub id: u32,
    pub chunks: Vec<Chunk>,
}

impl RawStream {
    /// First chunk with `tag`.
    pub fn chunk(&self, tag: u8) -> Option<Chunk> {
        self.chunks.iter().copied().find(|c| c.tag == tag)
    }
}

/// A 16-byte record from a 0xBD events chunk.
///
/// Field meanings follow (FORMAT) section 7 (GTA VI samples): type hash,
/// f32 value, time in samples at 48 kHz, flags. (VGM) instead describes the
/// record as type hash, params hash, timestamp in ms, flags. Both raw words
/// are kept (`value_bits`), and `provisional` is set because the sources
/// disagree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AwcEvent {
    /// JOAAT type hash.
    pub kind: Hash32,
    /// Known name for `kind`, if any (see [`event_name`]).
    pub name: Option<&'static str>,
    /// Second word read as f32.
    pub value: f32,
    /// Second word as raw bits (a params hash per (VGM)).
    pub value_bits: u32,
    /// Third word: samples at 48 kHz from stream start per (FORMAT).
    pub time: u32,
    pub flags: u32,
    pub provisional: bool,
}

impl AwcEvent {
    /// `time` in seconds, assuming 48 kHz samples.
    pub fn time_secs(&self) -> f64 {
        f64::from(self.time) / 48_000.0
    }
}

/// Event type names from (FORMAT) section 7 (guesses that hash to the type).
const EVENT_NAMES: [&str; 7] = ["beat", "trackid", "dj", "intro", "outro", "low", "high"];

/// Resolve a known event type hash to its name.
pub fn event_name(kind: Hash32) -> Option<&'static str> {
    EVENT_NAMES.iter().copied().find(|n| joaat(n) == kind)
}

/// Where a stream lives in the bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    /// Channel `channel` of the streamed (blocked) bank owned by raw stream
    /// `entry`.
    Music { entry: usize, channel: usize },
    /// A single-stream (sfx/speech) entry.
    Sfx { entry: usize },
}

/// Public description of one playable stream.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    /// Stream id: the 32-bit channel hash for music channels, the 29-bit
    /// stream table id for sfx streams.
    pub id: u32,
    pub codec: Codec,
    /// Raw codec byte.
    pub codec_id: u8,
    pub sample_rate: u32,
    /// Decoded channel count. For Opus music this comes from the stereo bit of
    /// the first packet ((FORMAT) section 5); sfx streams are mono ((VGM)).
    pub channels: u16,
    /// Length in samples per channel (best effort, see `raw_sample_count`).
    pub sample_count: u64,
    /// Sample count as stored in the header. (FORMAT) section 3 observes that
    /// for Opus music it counts interleaved samples (about 2x for stereo).
    pub raw_sample_count: u32,
    pub headroom: i16,
    pub kind: StreamKind,
    pub events: Vec<AwcEvent>,
    /// True when any part of this description relies on an unconfirmed
    /// layout or a fallback; see `notes`.
    pub provisional: bool,
    /// Human-readable reasons for `provisional` and other warnings.
    pub notes: Vec<String>,
}

impl StreamInfo {
    /// Duration in seconds from `sample_count` and `sample_rate`.
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.sample_count as f64 / f64::from(self.sample_rate)
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::debug!("AWC stream 0x{:08X}: {msg}", self.id);
        self.provisional = true;
        self.notes.push(msg);
    }
}

/// Per-channel part of a block header ((FORMAT) section 4, (VGM) `read_awc_block`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockChannel {
    /// Start entry (-1 in samples; inaccurate per (VGM)).
    pub start_entry: i32,
    /// Number of 0x800-byte units for this channel in this block.
    pub units: u32,
    /// Samples to discard at block start (0 in samples).
    pub skip_samples: i32,
    /// Samples in this block for this channel (interleaved count).
    pub samples: i32,
    /// Per-unit seek table (cumulative sample counts).
    pub seek: Vec<u32>,
    /// Absolute offset of this channel's first unit.
    pub data_offset: usize,
}

/// One parsed data block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub index: usize,
    /// Absolute offset of the block.
    pub offset: usize,
    /// Bytes available for the block (the last block may be short).
    pub len: usize,
    pub channels: Vec<BlockChannel>,
}

/// A streamed (music) group: one 0x48 header with its data chunk.
#[derive(Clone, Debug)]
struct MusicGroup {
    block_size: usize,
    block_count: usize,
    channels: usize,
    data: Chunk,
    seek_table: Vec<u32>,
}

#[derive(Clone, Debug)]
enum Source {
    Music { group: usize, channel: usize },
    Sfx { data: Option<Chunk> },
}

/// A parsed AWC bank. Holds the bank bytes so packets can be extracted later.
#[derive(Clone, Debug)]
pub struct AwcBank {
    data: Arc<[u8]>,
    version: u16,
    flags: u16,
    encrypted: bool,
    provisional: bool,
    notes: Vec<String>,
    entries: Vec<RawStream>,
    groups: Vec<MusicGroup>,
    streams: Vec<StreamInfo>,
    sources: Vec<Source>,
}

impl AwcBank {
    /// Parse a bank. Accepts `&[u8]`, `Vec<u8>` or `Arc<[u8]>` (zero copy).
    pub fn parse(bytes: impl Into<Arc<[u8]>>) -> Result<AwcBank> {
        let data: Arc<[u8]> = bytes.into();
        let mut bank = AwcBank {
            data,
            version: 0,
            flags: 0,
            encrypted: false,
            provisional: false,
            notes: Vec::new(),
            entries: Vec::new(),
            groups: Vec::new(),
            streams: Vec::new(),
            sources: Vec::new(),
        };
        bank.parse_header()?;
        bank.build_streams()?;
        Ok(bank)
    }

    /// The bank bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Header version (1 in all known banks).
    pub fn version(&self) -> u16 {
        self.version
    }

    /// Header flags (u16 at 0x06).
    pub fn flags(&self) -> u16 {
        self.flags
    }

    /// Whether the encrypted-data flag is set ((VGM): GTA V PC).
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// True when the header was parsed through a fallback path.
    pub fn provisional(&self) -> bool {
        self.provisional
    }

    /// Bank-level diagnostics.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The raw stream table.
    pub fn entries(&self) -> &[RawStream] {
        &self.entries
    }

    /// Playable streams.
    pub fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn note(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::warn!("AWC: {msg}");
        self.provisional = true;
        self.notes.push(msg);
    }

    fn stream(&self, index: usize) -> Result<(&StreamInfo, &Source)> {
        match (self.streams.get(index), self.sources.get(index)) {
            (Some(s), Some(src)) => Ok((s, src)),
            _ => Err(AwcError::StreamIndex {
                index,
                count: self.streams.len(),
            }),
        }
    }

    // ------------------------------------------------------------ header

    fn parse_header(&mut self) -> Result<()> {
        let data = Arc::clone(&self.data);
        let r = Reader::new(&data);
        let magic = r.bytes_at(0, 4).ctx(|| "reading AWC magic".into())?;
        if magic == b"TADA" {
            return Err(AwcError::BigEndian);
        }
        if magic != b"ADAT" {
            return Err(AwcError::BadMagic {
                found: magic.to_vec(),
            });
        }
        let version = r.u16_at(4).ctx(|| "reading AWC version at 0x4".into())?;
        let flags = r.u16_at(6).ctx(|| "reading AWC flags at 0x6".into())?;
        let count = r
            .u32_at(8)
            .ctx(|| "reading AWC stream count at 0x8".into())?;
        let header_size = r
            .u32_at(12)
            .ctx(|| "reading AWC header size at 0xC".into())?;
        log::debug!(
            "AWC header: version {version}, flags 0x{flags:04X}, {count} streams, header size 0x{header_size:X}, {} bytes",
            data.len()
        );
        self.version = version;
        self.flags = flags;

        match version {
            1 => {}
            // (VGM) comment: "00=rarely, 01=common".
            0 => self.note("header version 0 (rare per vgmstream)"),
            _ => return Err(AwcError::UnsupportedVersion { version, flags }),
        }
        if flags >> 8 != 0xFF {
            // (VGM) rejects banks whose flags high byte is not 0xFF.
            self.note(format!("flags 0x{flags:04X}: high byte is not 0xFF"));
        }
        if flags & FLAG_ENCRYPTED != 0 {
            self.encrypted = true;
            self.note("encrypted-data flag 0x0008 is set; audio cannot be extracted");
        }
        if count == 0 {
            return Err(AwcError::InvalidHeader {
                offset: 8,
                reason: "stream count is 0".into(),
            });
        }
        if count > MAX_STREAMS {
            return Err(AwcError::Limit {
                what: "stream count",
                value: u64::from(count),
                max: u64::from(MAX_STREAMS),
            });
        }
        let header_size = header_size as usize;
        if header_size < 0x10 || header_size > data.len() {
            return Err(AwcError::InvalidHeader {
                offset: 12,
                reason: format!(
                    "header size 0x{header_size:X} outside 0x10..=0x{:X}",
                    data.len()
                ),
            });
        }

        // Sources disagree on what signals the chunk index array:
        // (VGM) uses flag bit 0x0001 (high byte is always 0xFF), (FORMAT)
        // says "high byte 0xFF". Try the (VGM) reading first, validate the
        // result, and fall back to the other reading, then to a scan.
        let by_bit = flags & FLAG_CHUNK_INDICES != 0;
        let by_high_byte = flags >> 8 == 0xFF;
        let count = count as usize;
        match parse_table(&r, count, header_size, by_bit) {
            Ok((entries, _)) => {
                if by_bit != by_high_byte {
                    log::debug!(
                        "AWC: flags 0x{flags:04X}: chunk indices {} (vgmstream bit 0 rule; FORMAT.md high-byte rule disagrees)",
                        if by_bit { "present" } else { "absent" }
                    );
                }
                self.entries = entries;
            }
            // The contradicting reading is only trusted when its tables end
            // exactly at the declared header size ((VGM): "after stream
            // id+tags"), so random data cannot pass as a valid table.
            Err(first) => {
                match parse_table(&r, count, header_size, !by_bit).and_then(|(entries, end)| {
                    if end == header_size {
                        Ok(entries)
                    } else {
                        Err(AwcError::InvalidHeader {
                            offset: end,
                            reason: format!(
                                "tables end at 0x{end:X}, header size is 0x{header_size:X}"
                            ),
                        })
                    }
                }) {
                    Ok(entries) => {
                        self.note(format!(
                        "stream table only parses with chunk indices {} although flag bit 0 says otherwise ({first})",
                        if by_bit { "absent" } else { "present" }
                    ));
                        self.entries = entries;
                    }
                    Err(_) => {
                        self.note(format!(
                        "stream table invalid ({first}); located chunks by scanning the header (FORMAT.md heuristic)"
                    ));
                        self.entries = scan_chunks(&r, header_size);
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ streams

    fn build_streams(&mut self) -> Result<()> {
        let mut first_error = None;
        let mut used = vec![false; self.entries.len()];
        for ei in 0..self.entries.len() {
            let entry = self.entries[ei].clone();
            let result = if entry.chunk(tag::MUSIC_HEADER).is_some() {
                used[ei] = true;
                self.build_music(ei, &entry, &mut used)
            } else if entry.chunk(tag::SFX_HEADER).is_some()
                || entry.chunk(tag::VORBIS_SFX_HEADER).is_some()
            {
                used[ei] = true;
                self.build_sfx(ei, &entry)
            } else {
                Ok(())
            };
            if let Err(e) = result {
                self.note(format!(
                    "stream table entry {ei} (id 0x{:08X}): {e}",
                    entry.id
                ));
                first_error.get_or_insert(e);
            }
        }
        for (ei, entry) in self.entries.iter().enumerate() {
            if !used[ei] && !entry.chunks.is_empty() {
                let tags: Vec<String> = entry
                    .chunks
                    .iter()
                    .map(|c| format!("0x{:02X}", c.tag))
                    .collect();
                log::debug!(
                    "AWC: entry {ei} (id 0x{:08X}) with chunks [{}] has no stream header; ignored",
                    entry.id,
                    tags.join(", ")
                );
            }
        }
        if self.streams.is_empty() {
            return Err(first_error.unwrap_or_else(|| AwcError::InvalidHeader {
                offset: 0x10,
                reason: "no stream has a music (0x48) or sfx (0xFA/0x76) header chunk".into(),
            }));
        }
        Ok(())
    }

    fn build_music(&mut self, ei: usize, entry: &RawStream, used: &mut [bool]) -> Result<()> {
        let data = Arc::clone(&self.data);
        let fmt_chunk = entry.chunk(tag::MUSIC_HEADER).unwrap_or(Chunk {
            tag: tag::MUSIC_HEADER,
            offset: 0,
            size: 0,
        });
        let fmt = chunk_bytes(&data, fmt_chunk)?;
        let r = Reader::new(fmt);
        let at = fmt_chunk.offset;
        let block_count = r
            .u32_at(0)
            .ctx(|| format!("music header at 0x{at:X}: block count"))?;
        let block_size = r
            .u32_at(4)
            .ctx(|| format!("music header at 0x{at:X}: block size"))?;
        let nch = r
            .u32_at(8)
            .ctx(|| format!("music header at 0x{at:X}: channel count"))?;
        if nch == 0 || nch > MAX_MUSIC_CHANNELS {
            return Err(AwcError::Limit {
                what: "music channel count",
                value: u64::from(nch),
                max: u64::from(MAX_MUSIC_CHANNELS),
            });
        }
        if (block_size as usize) < UNIT_SIZE || block_size > MAX_BLOCK_SIZE {
            return Err(AwcError::InvalidHeader {
                offset: at + 4,
                reason: format!(
                    "block size 0x{block_size:X} outside 0x{UNIT_SIZE:X}..=0x{MAX_BLOCK_SIZE:X}"
                ),
            });
        }
        let data_chunk = entry.chunk(tag::DATA).ok_or(AwcError::InvalidHeader {
            offset: at,
            reason: "music header without a 0x55 data chunk in the same stream".into(),
        })?;
        let data_len = available(&data, data_chunk);
        if data_len < data_chunk.size {
            self.note(format!(
                "data chunk at 0x{:X} declares 0x{:X} bytes but only 0x{data_len:X} are present (truncated bank)",
                data_chunk.offset, data_chunk.size
            ));
        }
        let block_size = block_size as usize;
        let blocks_in_data = data_len.div_ceil(block_size);
        let block_total = if block_count == 0 {
            blocks_in_data
        } else {
            blocks_in_data.min(block_count as usize)
        };
        if block_count as usize != blocks_in_data {
            log::warn!(
                "AWC: music header says {block_count} blocks, data chunk holds {blocks_in_data}; using {block_total}"
            );
        }

        let seek_table = match entry.chunk(tag::SEEK_TABLE) {
            Some(c) => match chunk_bytes(&data, c) {
                Ok(b) => b
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|w| u32::from_le_bytes(*w))
                    .collect(),
                Err(e) => {
                    self.note(format!("seek table unreadable: {e}"));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        let own_events = self.events_of(entry);
        let group_index = self.groups.len();
        self.groups.push(MusicGroup {
            block_size,
            block_count: block_total,
            channels: nch as usize,
            data: data_chunk,
            seek_table,
        });
        let group = self.groups[group_index].clone();

        // Block 0 gives the Opus channel count and skip values.
        let first_block = if self.encrypted || block_total == 0 {
            None
        } else {
            match parse_block(&data, &group, 0) {
                Ok(b) => Some(b),
                Err(reason) => {
                    self.note(format!("block 0 unreadable: {reason}"));
                    None
                }
            }
        };

        for ch in 0..nch as usize {
            let base = 0x0C + 0x10 * ch;
            let ctx = |field: &str| format!("music header at 0x{at:X}: channel {ch} {field}");
            let id = r.u32_at(base).ctx(|| ctx("stream id"))?;
            let raw_samples = r.u32_at(base + 4).ctx(|| ctx("sample count"))?;
            let headroom = r.i16_at(base + 8).ctx(|| ctx("headroom"))?;
            let sample_rate = r.u16_at(base + 0x0A).ctx(|| ctx("sample rate"))?;
            let codec_id = r.u8_at(base + 0x0C).ctx(|| ctx("codec"))?;
            let codec = Codec::from_raw(codec_id);

            let mut info = StreamInfo {
                id,
                codec,
                codec_id,
                sample_rate: u32::from(sample_rate),
                channels: 1,
                sample_count: u64::from(raw_samples),
                raw_sample_count: raw_samples,
                headroom,
                kind: StreamKind::Music {
                    entry: ei,
                    channel: ch,
                },
                events: own_events.clone(),
                provisional: false,
                notes: Vec::new(),
            };

            // Events of the stream table entry with this channel's id. (VGM)
            // shows channel entries carrying their own event chunk.
            let short_id = id & 0x1FFF_FFFF;
            if let Some(ci) = self
                .entries
                .iter()
                .enumerate()
                .position(|(i, e)| i != ei && e.id == short_id)
            {
                if let Some(u) = used.get_mut(ci) {
                    *u = true;
                }
                let companion = self.entries[ci].clone();
                info.events.extend(self.events_of(&companion));
            }

            if entry.id != 0 {
                // (VGM): the music entry has id 0 in GTA V era banks.
                info.note(format!(
                    "music header owned by stream id 0x{:08X}, not 0 as in vgmstream",
                    entry.id
                ));
            }
            if self.provisional {
                info.note("bank header parsed through a fallback path");
            }
            if self.encrypted {
                info.note("bank data is encrypted");
            }

            if codec == Codec::Opus {
                match first_block.as_ref().and_then(|b| b.channels.get(ch)) {
                    Some(bc) => {
                        if bc.skip_samples != 0 {
                            info.note(format!(
                                "block 0 asks to skip {} samples; not applied",
                                bc.skip_samples
                            ));
                        }
                        match first_packet_toc(&data, bc) {
                            Some(toc) => info.channels = u16::from(toc.channels()),
                            None => {
                                info.channels = 2;
                                info.note("no packet in block 0; assuming stereo");
                            }
                        }
                    }
                    None => {
                        info.channels = 2;
                        info.note("could not probe block 0; assuming stereo");
                    }
                }
                // (FORMAT) section 3: interleaved sample count.
                info.sample_count = u64::from(raw_samples) / u64::from(info.channels.max(1));
                if sample_rate != 48_000 {
                    info.note(format!(
                        "sample rate {sample_rate} is not 48000 as in all known samples"
                    ));
                }
            } else {
                info.note(format!(
                    "codec {codec} in a streamed bank; only Opus (0x0B) is understood"
                ));
            }

            self.streams.push(info);
            self.sources.push(Source::Music {
                group: group_index,
                channel: ch,
            });
        }
        Ok(())
    }

    fn build_sfx(&mut self, ei: usize, entry: &RawStream) -> Result<()> {
        let data = Arc::clone(&self.data);
        // (VGM) (GTA V layout): 0xFA header: u32 samples @0, u16 rate @8,
        // s16 headroom @0xA, u8 codec @0x13. 0x76 (Vorbis): codec @0x1C.
        let (hdr, codec_at) = match entry.chunk(tag::SFX_HEADER) {
            Some(c) => (c, 0x13),
            None => match entry.chunk(tag::VORBIS_SFX_HEADER) {
                Some(c) => (c, 0x1C),
                None => return Ok(()),
            },
        };
        let bytes = chunk_bytes(&data, hdr)?;
        let r = Reader::new(bytes);
        let at = hdr.offset;
        let ctx = |field: &str| format!("sfx header 0x{:02X} at 0x{at:X}: {field}", hdr.tag);
        let samples = r.u32_at(0).ctx(|| ctx("sample count"))?;
        let rate = r.u16_at(8).ctx(|| ctx("sample rate"))?;
        let headroom = r.i16_at(0x0A).ctx(|| ctx("headroom"))?;
        let codec_id = r.u8_at(codec_at).ctx(|| ctx("codec"))?;
        let codec = Codec::from_raw(codec_id);
        let data_chunk = entry.chunk(tag::DATA);

        let mut info = StreamInfo {
            id: entry.id,
            codec,
            codec_id,
            sample_rate: u32::from(rate),
            channels: 1,
            sample_count: u64::from(samples),
            raw_sample_count: samples,
            headroom,
            kind: StreamKind::Sfx { entry: ei },
            events: self.events_of(entry),
            provisional: false,
            notes: Vec::new(),
        };
        info.note(
            "single-stream (sfx) layout taken from vgmstream's GTA V notes; unconfirmed for GTA VI",
        );
        if data_chunk.is_none() {
            info.note("no 0x55 data chunk");
        }
        if self.encrypted {
            info.note("bank data is encrypted");
        }
        self.streams.push(info);
        self.sources.push(Source::Sfx { data: data_chunk });
        Ok(())
    }

    fn events_of(&mut self, entry: &RawStream) -> Vec<AwcEvent> {
        let Some(c) = entry.chunk(tag::EVENTS) else {
            return Vec::new();
        };
        match parse_events(&self.data, c) {
            Ok(ev) => ev,
            Err(e) => {
                self.note(format!(
                    "events of stream 0x{:08X} unreadable: {e}",
                    entry.id
                ));
                Vec::new()
            }
        }
    }

    // ------------------------------------------------------------ blocks

    /// Parse every data block of a music stream's bank (all channels).
    pub fn blocks(&self, stream_index: usize) -> Result<Vec<BlockInfo>> {
        let (_, src) = self.stream(stream_index)?;
        let Source::Music { group, .. } = src else {
            return Err(AwcError::UnsupportedLayout {
                index: stream_index,
                reason: "not a streamed (music) stream; it has no blocks".into(),
            });
        };
        let g = self.group(*group, stream_index)?;
        (0..g.block_count)
            .map(|b| {
                parse_block(&self.data, g, b).map_err(|reason| AwcError::Block {
                    stream: stream_index,
                    block: b,
                    offset: block_offset(g, b).unwrap_or(0),
                    reason,
                })
            })
            .collect()
    }

    /// The 0xA3 block-to-sample table of a music stream's bank, if present.
    pub fn block_sample_table(&self, stream_index: usize) -> Option<&[u32]> {
        match self.sources.get(stream_index)? {
            Source::Music { group, .. } => self.groups.get(*group).map(|g| &g.seek_table[..]),
            Source::Sfx { .. } => None,
        }
    }

    fn group(&self, group: usize, stream_index: usize) -> Result<&MusicGroup> {
        self.groups.get(group).ok_or(AwcError::UnsupportedLayout {
            index: stream_index,
            reason: "internal: missing music group".into(),
        })
    }

    // ------------------------------------------------------------ packets

    /// Extract the Opus packets of a stream, in order.
    ///
    /// Music streams: every 0x800-byte unit of the channel in every block is
    /// read as `u16 length | packet` records until a zero length ((FORMAT)
    /// section 4). A malformed block 0 is an error; a malformed later block
    /// (typically a truncated bank) ends extraction with a warning.
    ///
    /// Sfx streams with codec Opus are read with the same unit layout as a
    /// best-effort guess; the stream is marked provisional.
    pub fn opus_packets(&self, stream_index: usize) -> Result<Vec<Vec<u8>>> {
        let (info, src) = self.stream(stream_index)?;
        if info.codec != Codec::Opus {
            return Err(AwcError::UnsupportedCodec {
                index: stream_index,
                id: info.id,
                codec: info.codec,
                operation: "Opus packet extraction",
            });
        }
        if self.encrypted {
            return Err(AwcError::Encrypted);
        }
        let mut out = Vec::new();
        let mut stats = UnitStats::default();
        match src {
            Source::Music { group, channel } => {
                let g = self.group(*group, stream_index)?;
                for b in 0..g.block_count {
                    let block = match parse_block(&self.data, g, b) {
                        Ok(block) => block,
                        Err(reason) => {
                            let offset = block_offset(g, b).unwrap_or(0);
                            if b == 0 {
                                return Err(AwcError::Block {
                                    stream: stream_index,
                                    block: b,
                                    offset,
                                    reason,
                                });
                            }
                            log::warn!(
                                "AWC stream {stream_index}: block {b} at 0x{offset:X}: {reason}; stopping after {} packets",
                                out.len()
                            );
                            break;
                        }
                    };
                    let Some(bc) = block.channels.get(*channel) else {
                        break;
                    };
                    for u in 0..bc.units as usize {
                        let off = bc.data_offset + u * UNIT_SIZE;
                        let unit = self.data.get(off..off + UNIT_SIZE).unwrap_or(&[]);
                        read_unit(unit, off, &mut out, &mut stats);
                    }
                }
            }
            Source::Sfx { data } => {
                let Some(c) = data else {
                    return Err(AwcError::NoAudio {
                        stream: stream_index,
                    });
                };
                let bytes = self
                    .data
                    .get(c.offset..c.offset + available(&self.data, *c))
                    .unwrap_or(&[]);
                for (i, unit) in bytes.chunks(UNIT_SIZE).enumerate() {
                    read_unit(unit, c.offset + i * UNIT_SIZE, &mut out, &mut stats);
                }
                if stats.bad > 0 && out.is_empty() {
                    return Err(AwcError::UnsupportedLayout {
                        index: stream_index,
                        reason: format!(
                            "sfx Opus data at 0x{:X} does not match the 0x800-unit packet layout ({} bad records)",
                            c.offset, stats.bad
                        ),
                    });
                }
            }
        }
        if stats.bad > 0 {
            log::warn!(
                "AWC stream {stream_index}: skipped {} malformed packet records (first at 0x{:X})",
                stats.bad,
                stats.first_bad.unwrap_or(0)
            );
        }
        if out.is_empty() {
            return Err(AwcError::NoAudio {
                stream: stream_index,
            });
        }
        log::debug!(
            "AWC stream {stream_index}: extracted {} Opus packets",
            out.len()
        );
        Ok(out)
    }

    /// Extract a stream as a complete Ogg Opus file.
    ///
    /// Uses [`DEFAULT_PRE_SKIP`] (provisional, see its docs), trims the end to
    /// the header sample count when that is shorter than the packets, and
    /// stores the stream id as an `AWC_STREAM_ID` comment.
    pub fn ogg_opus(&self, stream_index: usize) -> Result<Vec<u8>> {
        let packets = self.opus_packets(stream_index)?;
        let (info, _) = self.stream(stream_index)?;
        let channels = packets
            .first()
            .and_then(|p| p.first())
            .map(|&t| OpusToc::parse(t).channels())
            .unwrap_or(2);
        let total =
            (info.sample_count > 0 && info.sample_rate == 48_000).then_some(info.sample_count);
        let opts = OggOpusOptions {
            channels,
            input_sample_rate: info.sample_rate,
            pre_skip: DEFAULT_PRE_SKIP,
            serial: info.id,
            comments: vec![("AWC_STREAM_ID".into(), format!("0x{:08X}", info.id))],
            total_samples: total,
            ..OggOpusOptions::default()
        };
        Ok(write_ogg_opus_with(&packets, &opts))
    }

    /// Raw bytes of an sfx stream's data chunk (for PCM decoding).
    pub(crate) fn sfx_data(&self, stream_index: usize) -> Result<&[u8]> {
        let (_, src) = self.stream(stream_index)?;
        match src {
            Source::Sfx { data: Some(c) } => Ok(self
                .data
                .get(c.offset..c.offset + available(&self.data, *c))
                .unwrap_or(&[])),
            Source::Sfx { data: None } => Err(AwcError::NoAudio {
                stream: stream_index,
            }),
            Source::Music { .. } => Err(AwcError::UnsupportedLayout {
                index: stream_index,
                reason: "not a single-stream (sfx) stream".into(),
            }),
        }
    }
}

// ---------------------------------------------------------------- helpers

/// Parse the stream table and chunk table. `with_indices` selects whether a
/// u16 chunk start index per stream precedes the stream info array. Returns
/// the entries and the offset where the chunk table ends.
fn parse_table(
    r: &Reader<'_>,
    count: usize,
    header_size: usize,
    with_indices: bool,
) -> Result<(Vec<RawStream>, usize)> {
    let mut pos = 0x10usize;
    let mut indices = Vec::new();
    if with_indices {
        indices.reserve(count);
        for i in 0..count {
            indices.push(
                r.u16_at(pos + 2 * i)
                    .ctx(|| format!("chunk start index {i} at 0x{:X}", pos + 2 * i))?,
            );
        }
        pos += 2 * count;
    }
    let mut infos = Vec::with_capacity(count);
    let mut total = 0usize;
    for i in 0..count {
        let v = r
            .u32_at(pos + 4 * i)
            .ctx(|| format!("stream info {i} at 0x{:X}", pos + 4 * i))?;
        let n = (v >> 29) as usize;
        if let Some(&index) = indices.get(i)
            && usize::from(index) != total & 0xFFFF
        {
            return Err(AwcError::InvalidHeader {
                offset: 0x10 + 2 * i,
                reason: format!(
                    "chunk start index {index} of stream {i} does not match running chunk count {total}"
                ),
            });
        }
        infos.push((v & 0x1FFF_FFFF, n));
        total += n;
    }
    pos += 4 * count;
    let table_end = pos + 8 * total;
    if table_end > header_size {
        return Err(AwcError::InvalidHeader {
            offset: pos,
            reason: format!(
                "{total} chunk entries end at 0x{table_end:X}, past header size 0x{header_size:X}"
            ),
        });
    }
    let mut entries = Vec::with_capacity(count);
    for (id, n) in infos {
        let mut chunks = Vec::with_capacity(n);
        for _ in 0..n {
            let v = r.u64_at(pos).ctx(|| format!("chunk entry at 0x{pos:X}"))?;
            let c = decode_chunk(v);
            if c.size > 0 && c.offset < table_end {
                return Err(AwcError::InvalidHeader {
                    offset: pos,
                    reason: format!(
                        "chunk 0x{:02X} at 0x{:X} overlaps the header (ends 0x{table_end:X})",
                        c.tag, c.offset
                    ),
                });
            }
            chunks.push(c);
            pos += 8;
        }
        entries.push(RawStream { id, chunks });
    }
    Ok((entries, table_end))
}

fn decode_chunk(v: u64) -> Chunk {
    Chunk {
        tag: (v >> 56) as u8,
        offset: (v & 0x0FFF_FFFF) as usize,
        size: ((v >> 28) & 0x0FFF_FFFF) as usize,
    }
}

/// (FORMAT) section 2 heuristic: scan the header (4-byte aligned) for chunk
/// entries with a known music tag and an offset past the header.
fn scan_chunks(r: &Reader<'_>, header_size: usize) -> Vec<RawStream> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut pos = 0x10;
    while pos + 8 <= header_size {
        if let Ok(v) = r.u64_at(pos) {
            let c = decode_chunk(v);
            let known = matches!(
                c.tag,
                tag::MUSIC_HEADER | tag::DATA | tag::SEEK_TABLE | tag::EVENTS
            );
            if known && c.offset >= header_size && !chunks.iter().any(|k| k.tag == c.tag) {
                chunks.push(c);
            }
        }
        pos += 4;
    }
    vec![RawStream { id: 0, chunks }]
}

/// Bytes of a chunk that are actually present.
fn available(data: &[u8], c: Chunk) -> usize {
    data.len().saturating_sub(c.offset).min(c.size)
}

fn chunk_bytes(data: &[u8], c: Chunk) -> Result<&[u8]> {
    c.offset
        .checked_add(c.size)
        .and_then(|end| data.get(c.offset..end))
        .ok_or(AwcError::ChunkOutOfBounds {
            tag: c.tag,
            offset: c.offset,
            size: c.size,
            len: data.len(),
        })
}

fn parse_events(data: &[u8], c: Chunk) -> Result<Vec<AwcEvent>> {
    let bytes = chunk_bytes(data, c)?;
    let n = bytes.len() / 16;
    if n > MAX_EVENTS {
        return Err(AwcError::Limit {
            what: "event count",
            value: n as u64,
            max: MAX_EVENTS as u64,
        });
    }
    if bytes.len() % 16 != 0 {
        log::warn!(
            "AWC: events chunk at 0x{:X} size 0x{:X} is not a multiple of 16",
            c.offset,
            c.size
        );
    }
    Ok(bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|rec| {
            let (words, _) = rec.as_chunks::<4>();
            let w = |i: usize| words.get(i).map_or(0, |b| u32::from_le_bytes(*b));
            let kind = Hash32(w(0));
            AwcEvent {
                kind,
                name: event_name(kind),
                value: f32::from_bits(w(1)),
                value_bits: w(1),
                time: w(2),
                flags: w(3),
                provisional: true,
            }
        })
        .collect())
}

fn block_offset(g: &MusicGroup, b: usize) -> Option<usize> {
    b.checked_mul(g.block_size)?.checked_add(g.data.offset)
}

/// Parse block `b` of a music group ((FORMAT) section 4).
fn parse_block(data: &[u8], g: &MusicGroup, b: usize) -> std::result::Result<BlockInfo, String> {
    let offset = block_offset(g, b).ok_or("block offset overflows")?;
    let data_end = g.data.offset + available(data, g.data);
    if offset >= data_end {
        return Err(format!(
            "block starts past the data chunk end 0x{data_end:X}"
        ));
    }
    let len = (data_end - offset).min(g.block_size);
    let block = data.get(offset..offset + len).ok_or("block outside bank")?;
    let r = Reader::new(block);

    let mut channels = Vec::with_capacity(g.channels);
    for ch in 0..g.channels {
        let base = ch * BLOCK_CHANNEL_HEADER;
        let field = |o: usize| {
            r.i32_at(base + o)
                .map_err(|e| format!("channel {ch} header: {e}"))
        };
        let start_entry = field(0)?;
        let units = field(4)?;
        let skip_samples = field(8)?;
        let samples = field(12)?;
        if !(0..=MAX_UNITS_PER_BLOCK).contains(&units) {
            return Err(format!("channel {ch}: unit count {units} out of range"));
        }
        channels.push(BlockChannel {
            start_entry,
            units: units as u32,
            skip_samples,
            samples,
            seek: Vec::new(),
            data_offset: 0,
        });
    }
    let total_units: usize = channels.iter().map(|c| c.units as usize).sum();
    let header = g.channels * BLOCK_CHANNEL_HEADER + 4 * total_units;
    let data_start = header.div_ceil(UNIT_SIZE) * UNIT_SIZE;
    let needed = data_start + total_units * UNIT_SIZE;
    if needed > len {
        return Err(format!(
            "{total_units} units need 0x{needed:X} bytes but the block has 0x{len:X}"
        ));
    }
    let mut seek_pos = g.channels * BLOCK_CHANNEL_HEADER;
    let mut unit_pos = data_start;
    for (ch, c) in channels.iter_mut().enumerate() {
        let n = c.units as usize;
        let mut seek = Vec::with_capacity(n);
        for i in 0..n {
            seek.push(
                r.u32_at(seek_pos + 4 * i)
                    .map_err(|e| format!("channel {ch} seek table: {e}"))?,
            );
        }
        c.seek = seek;
        c.data_offset = offset + unit_pos;
        seek_pos += 4 * n;
        unit_pos += n * UNIT_SIZE;
    }
    Ok(BlockInfo {
        index: b,
        offset,
        len,
        channels,
    })
}

fn first_packet_toc(data: &[u8], bc: &BlockChannel) -> Option<OpusToc> {
    if bc.units == 0 {
        return None;
    }
    let len = u16::from_le_bytes([*data.get(bc.data_offset)?, *data.get(bc.data_offset + 1)?]);
    if len == 0 {
        return None;
    }
    data.get(bc.data_offset + 2).map(|&t| OpusToc::parse(t))
}

#[derive(Default)]
struct UnitStats {
    bad: usize,
    first_bad: Option<usize>,
}

/// Read `u16 length | packet` records from one unit until a zero length.
fn read_unit(unit: &[u8], abs: usize, out: &mut Vec<Vec<u8>>, stats: &mut UnitStats) {
    let mut p = 0usize;
    while p + 2 <= unit.len() {
        let len = usize::from(u16::from_le_bytes([unit[p], unit[p + 1]]));
        if len == 0 {
            break;
        }
        let Some(pkt) = unit.get(p + 2..p + 2 + len) else {
            log::warn!(
                "AWC: packet length {len} at 0x{:X} runs past its 0x800 unit",
                abs + p
            );
            stats.bad += 1;
            stats.first_bad.get_or_insert(abs + p);
            break;
        };
        if let Err(e) = opus_packet_samples(pkt) {
            log::warn!("AWC: packet at 0x{:X}: {e}; skipped", abs + p + 2);
            stats.bad += 1;
            stats.first_bad.get_or_insert(abs + p);
        } else {
            out.push(pkt.to_vec());
        }
        p += 2 + len;
    }
}
