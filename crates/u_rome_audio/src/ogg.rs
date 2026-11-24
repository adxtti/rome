//! Ogg Opus muxing (RFC 3533 pages, RFC 7845 Opus mapping).
//!
//! Packets are written unchanged, so no re-encoding happens. Granule positions
//! are the running count of 48 kHz samples derived from each packet's TOC
//! byte (see [`crate::opus_packet_samples`]).

use crate::opus::opus_packet_samples;

/// Encoder delay written as OpusHead pre-skip when nothing better is known.
///
/// Source: Albotterin/AWC_from_RPF9_Extractor `docs/FORMAT.md` writes 312, the
/// standard libopus lookahead at 48 kHz for the audio/VoIP applications. The
/// game's encoder settings are not documented, so this is provisional.
pub const DEFAULT_PRE_SKIP: u16 = 312;

/// Packets per audio page, as in the reference tool (FORMAT.md section 5).
pub const DEFAULT_PACKETS_PER_PAGE: usize = 40;

/// Granule value meaning "no packet finishes on this page".
const NO_GRANULE: u64 = u64::MAX;

/// Header type flags.
const FLAG_CONTINUED: u8 = 0x01;
const FLAG_BOS: u8 = 0x02;
const FLAG_EOS: u8 = 0x04;

/// Options for [`write_ogg_opus_with`].
#[derive(Clone, Debug)]
pub struct OggOpusOptions {
    /// Output channel count, 1 or 2 (mapping family 0). Other values are clamped.
    pub channels: u8,
    /// Original input sample rate (informational OpusHead field).
    pub input_sample_rate: u32,
    /// Samples at 48 kHz to discard at the start of decoding.
    pub pre_skip: u16,
    /// Ogg bitstream serial number.
    pub serial: u32,
    /// OpusTags vendor string.
    pub vendor: String,
    /// OpusTags comments as `(key, value)` pairs, written `KEY=value`.
    pub comments: Vec<(String, String)>,
    /// Real stream length in samples per channel at 48 kHz, excluding
    /// pre-skip. When set and shorter than the packets' total duration the
    /// final granule position trims the end (RFC 7845 section 4.5).
    pub total_samples: Option<u64>,
    /// Maximum packets per audio page (at least 1).
    pub packets_per_page: usize,
}

impl Default for OggOpusOptions {
    fn default() -> Self {
        OggOpusOptions {
            channels: 2,
            input_sample_rate: 48_000,
            pre_skip: DEFAULT_PRE_SKIP,
            serial: 0x4D4F_5255, // "UROM"
            vendor: concat!("u_rome_audio ", env!("CARGO_PKG_VERSION")).to_owned(),
            comments: Vec::new(),
            total_samples: None,
            packets_per_page: DEFAULT_PACKETS_PER_PAGE,
        }
    }
}

/// Wrap Opus packets in an Ogg Opus file.
///
/// `channels` must be 1 or 2 (mapping family 0); `sample_rate` is the
/// informational input rate stored in OpusHead; `pre_skip` is in 48 kHz
/// samples (use [`DEFAULT_PRE_SKIP`] when unknown).
pub fn write_ogg_opus<P: AsRef<[u8]>>(
    packets: &[P],
    channels: u8,
    sample_rate: u32,
    pre_skip: u16,
) -> Vec<u8> {
    write_ogg_opus_with(
        packets,
        &OggOpusOptions {
            channels,
            input_sample_rate: sample_rate,
            pre_skip,
            ..OggOpusOptions::default()
        },
    )
}

/// Wrap Opus packets in an Ogg Opus file with full control over headers.
pub fn write_ogg_opus_with<P: AsRef<[u8]>>(packets: &[P], opts: &OggOpusOptions) -> Vec<u8> {
    let channels = if (1..=2).contains(&opts.channels) {
        opts.channels
    } else {
        log::warn!(
            "Ogg Opus: channel count {} is invalid for mapping family 0, writing 2",
            opts.channels
        );
        2
    };

    let mut mux = PageBuilder::default();

    // Page 0: OpusHead alone, beginning of stream.
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&opts.pre_skip.to_le_bytes());
    head.extend_from_slice(&opts.input_sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // mapping family
    mux.push_packet(&head, 0);
    mux.flush();

    // Page 1..: OpusTags, must end its own page.
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    push_string(&mut tags, opts.vendor.as_bytes());
    let comments: Vec<String> = opts
        .comments
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    tags.extend_from_slice(&u32::try_from(comments.len()).unwrap_or(0).to_le_bytes());
    for c in &comments {
        push_string(&mut tags, c.as_bytes());
    }
    mux.push_packet(&tags, 0);
    mux.flush();
    let header_pages = mux.pages.len();

    // Audio pages.
    let per_page = opts.packets_per_page.max(1);
    let mut granule: u64 = 0;
    for (i, p) in packets.iter().enumerate() {
        let p = p.as_ref();
        let samples = match opus_packet_samples(p) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Ogg Opus: packet {i} ({} bytes): {e}; counted as 0 samples",
                    p.len()
                );
                0
            }
        };
        granule = granule.saturating_add(u64::from(samples));
        mux.push_packet(p, granule);
        if mux.current_packets >= per_page {
            mux.flush();
        }
    }
    mux.flush();

    // End trimming on the final audio page.
    if let Some(total) = opts.total_samples {
        let wanted = total.saturating_add(u64::from(opts.pre_skip));
        if wanted < granule && mux.pages.len() > header_pages {
            // RFC 7845 4.5: never trim below the previous page's granule.
            let floor = mux
                .pages
                .iter()
                .rev()
                .skip(1)
                .map(|p| p.granule)
                .find(|&g| g != NO_GRANULE)
                .unwrap_or(0);
            if let Some(last) = mux.pages.last_mut() {
                last.granule = wanted.max(floor);
            }
        } else if wanted > granule {
            log::debug!(
                "Ogg Opus: declared length {total} + pre-skip exceeds packet total {granule}; not trimming"
            );
        }
    }

    if let Some(last) = mux.pages.last_mut() {
        last.flags |= FLAG_EOS;
    }
    if let Some(first) = mux.pages.first_mut() {
        first.flags |= FLAG_BOS;
    }
    mux.serialize(opts.serial)
}

fn push_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(s);
}

#[derive(Default)]
struct Page {
    flags: u8,
    granule: u64,
    lacing: Vec<u8>,
    body: Vec<u8>,
}

#[derive(Default)]
struct PageBuilder {
    pages: Vec<Page>,
    lacing: Vec<u8>,
    body: Vec<u8>,
    /// Granule of the last packet completed on the current page.
    granule: Option<u64>,
    current_packets: usize,
    /// The current page starts with the continuation of a packet.
    continued: bool,
}

impl PageBuilder {
    /// Append a packet; `granule` is the stream position after it.
    fn push_packet(&mut self, packet: &[u8], granule: u64) {
        // Lacing: n/255 segments of 255 then one of n%255 (possibly 0).
        let full = packet.len() / 255;
        let total_segments = full + 1;
        let mut seg = 0;
        let mut offset = 0;
        while seg < total_segments {
            if self.lacing.len() == 255 {
                self.flush_page(true);
            }
            let room = 255 - self.lacing.len();
            let take = room.min(total_segments - seg);
            for _ in 0..take {
                let len = (packet.len() - offset).min(255);
                self.lacing.push(len as u8);
                self.body
                    .extend_from_slice(packet.get(offset..offset + len).unwrap_or(&[]));
                offset += len;
                seg += 1;
            }
        }
        self.granule = Some(granule);
        self.current_packets += 1;
    }

    /// Close the current page if it holds anything.
    fn flush(&mut self) {
        if !self.lacing.is_empty() {
            self.flush_page(false);
        }
    }

    /// Close the current page; `mid_packet` means the next page continues a packet.
    fn flush_page(&mut self, mid_packet: bool) {
        let page = Page {
            flags: if self.continued { FLAG_CONTINUED } else { 0 },
            granule: self.granule.take().unwrap_or(NO_GRANULE),
            lacing: std::mem::take(&mut self.lacing),
            body: std::mem::take(&mut self.body),
        };
        self.pages.push(page);
        self.current_packets = 0;
        self.continued = mid_packet;
    }

    fn serialize(self, serial: u32) -> Vec<u8> {
        let size: usize = self
            .pages
            .iter()
            .map(|p| 27 + p.lacing.len() + p.body.len())
            .sum();
        let mut out = Vec::with_capacity(size);
        for (seq, page) in self.pages.iter().enumerate() {
            let start = out.len();
            out.extend_from_slice(b"OggS");
            out.push(0); // stream structure version
            out.push(page.flags);
            out.extend_from_slice(&page.granule.to_le_bytes());
            out.extend_from_slice(&serial.to_le_bytes());
            out.extend_from_slice(&u32::try_from(seq).unwrap_or(u32::MAX).to_le_bytes());
            out.extend_from_slice(&[0; 4]); // CRC placeholder
            out.push(page.lacing.len() as u8);
            out.extend_from_slice(&page.lacing);
            out.extend_from_slice(&page.body);
            let crc = ogg_crc32(&out[start..]);
            out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        }
        out
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut b = 0;
        while b < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04C1_1DB7
            } else {
                r << 1
            };
            b += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Ogg page CRC: polynomial 0x04C11DB7, not reflected, initial value 0, no
/// final xor, computed with the CRC field zeroed.
pub fn ogg_crc32(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[(((crc >> 24) as u8) ^ b) as usize]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_libogg_page() {
        // OpusHead page written by ffmpeg/libogg for a self-generated sine
        // clip (not game data). Bytes 22..26 hold libogg's CRC.
        let mut page: Vec<u8> = vec![
            0x4F, 0x67, 0x67, 0x53, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0x84, 0xBA, 0x44, 0x29, 0,
            0, 0, 0, 0x5D, 0x25, 0x0F, 0xF1, 0x01, 0x13,
        ];
        page.extend_from_slice(b"OpusHead");
        page.extend_from_slice(&[0x01, 0x02, 0x78, 0x00, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        let expected = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
        page[22..26].fill(0);
        assert_eq!(ogg_crc32(&page), expected);
        assert_eq!(ogg_crc32(b""), 0);
    }
}
