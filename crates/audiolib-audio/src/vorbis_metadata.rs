// crates/audiolib-audio/src/vorbis_metadata.rs
//! Bounded FLAC/Vorbis/Opus comment extraction, independent of playback codecs.
use crate::{
    wav::WavInfo,
    wav_metadata::{WAV_METADATA_VERSION, append, canonical_field},
};
use audiolib_core::{AudioMetadata, Error, Result};
use serde_json::{Map, Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// Revision of the bounded non-WAV Metadata extractor used by incremental scanning.
pub const VORBIS_METADATA_VERSION: i64 = 1;
const MAX_COMMENT_BYTES: usize = 1024 * 1024;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const MAX_COMMENTS: usize = 4096;
const MAX_HEADER_PAGES: usize = 1024;
const MAX_PAGES: usize = 1_000_000;

/// Default library source extensions with a bundled playback decoder.
/// Opus comments can be probed separately, but Opus is not scanned by default.
pub fn is_supported_audio_extension(path: &Path) -> bool {
    audiolib_core::is_supported_audio_extension(path)
}

fn invalid(message: &str) -> Error {
    Error::Audio(message.into())
}
fn little(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[derive(Default)]
struct Comments {
    tags: Map<String, Value>,
    warnings: Vec<String>,
    retained: usize,
}
impl Comments {
    fn warn(&mut self, message: &str) {
        if self.warnings.len() < 32 {
            self.warnings.push(message.into());
        }
    }
    fn read(&mut self, bytes: &[u8]) {
        let mut at = 0;
        let number = |at: &mut usize| -> Option<usize> {
            let end = at.checked_add(4)?;
            let value = usize::try_from(little(bytes.get(*at..end)?)).ok()?;
            *at = end;
            Some(value)
        };
        let Some(vendor) = number(&mut at) else {
            self.warn("Truncated Vorbis vendor length");
            return;
        };
        let Some(end) = at.checked_add(vendor).filter(|end| *end <= bytes.len()) else {
            self.warn("Truncated Vorbis vendor");
            return;
        };
        at = end;
        let Some(count) = number(&mut at) else {
            self.warn("Truncated Vorbis comment count");
            return;
        };
        if count > MAX_COMMENTS {
            self.warn("Vorbis comment count exceeded the extraction limit");
        }
        for _ in 0..count.min(MAX_COMMENTS) {
            let Some(size) = number(&mut at) else {
                self.warn("Truncated Vorbis comment length");
                break;
            };
            let Some(end) = at.checked_add(size).filter(|end| *end <= bytes.len()) else {
                self.warn("Truncated Vorbis comment value");
                break;
            };
            let value = &bytes[at..end];
            at = end;
            let Some(eq) = value.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let key = &value[..eq];
            if key.is_empty()
                || key.len() > 128
                || !key.iter().all(|b| (0x20..=0x7d).contains(b) && *b != b'=')
            {
                continue;
            }
            let key = String::from_utf8_lossy(key).to_ascii_uppercase();
            if matches!(
                key.as_str(),
                "METADATA_BLOCK_PICTURE" | "COVERART" | "COVERARTMIME"
            ) {
                continue;
            }
            let raw = &value[eq + 1..];
            if raw.is_empty() {
                continue;
            }
            if raw.len() > MAX_VALUE_BYTES {
                self.warn("Vorbis value exceeded the extraction limit");
                continue;
            }
            if self.retained.saturating_add(raw.len()) > MAX_COMMENT_BYTES {
                self.warn("Vorbis text exceeded the extraction budget");
                break;
            }
            let text = String::from_utf8_lossy(raw);
            if text.contains('\u{fffd}') {
                self.warn("Invalid UTF-8 in Vorbis text was replaced");
            }
            let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
            if text.is_empty() {
                continue;
            }
            self.retained += raw.len();
            append(&mut self.tags, &key, text);
        }
    }
    fn attributes(self, container: &str, include: bool) -> Value {
        let mut attrs = Map::new();
        attrs.insert("container".into(), json!(container));
        attrs.insert("format_tag".into(), json!(0));
        if include {
            let mut fields = Map::new();
            // DESCRIPTION precedes comment aliases; every original value remains.
            let mut tags: Vec<_> = self.tags.iter().collect();
            tags.sort_by_key(|(key, _)| {
                usize::from(matches!(key.as_str(), "COMMENT" | "COMMENTS"))
            });
            for (key, value) in tags {
                if let Some(field) = canonical_field("vorbis", key) {
                    match value {
                        Value::String(text) => append(&mut fields, field, text),
                        Value::Array(items) => {
                            for text in items.iter().filter_map(Value::as_str) {
                                append(&mut fields, field, text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            attrs.insert("wav_metadata".into(), json!({"version":WAV_METADATA_VERSION,
                "vorbis_version":VORBIS_METADATA_VERSION,"vorbis":self.tags,"fields":fields,"warnings":self.warnings}));
        }
        Value::Object(attrs)
    }
}

#[derive(Default)]
struct Layout {
    rate: u32,
    channels: u16,
    bits: u16,
    frames: u64,
    pre_skip: u64,
    codec: &'static str,
    ogg: bool,
}
fn streaminfo(bytes: &[u8], layout: &mut Layout) -> Result<()> {
    if bytes.len() < 34 {
        return Err(invalid("Truncated FLAC STREAMINFO"));
    }
    let packed = u64::from_be_bytes(bytes[10..18].try_into().unwrap());
    layout.rate = (packed >> 44) as u32;
    layout.channels = ((packed >> 41) & 7) as u16 + 1;
    layout.bits = ((packed >> 36) & 31) as u16 + 1;
    layout.frames = packed & 0x0f_ffff_ffff;
    layout.codec = "FLAC";
    Ok(())
}

fn finish(layout: Layout, comments: Comments, len: u64, include: bool) -> Result<WavInfo> {
    if layout.rate == 0 || layout.channels == 0 || layout.channels > 32 {
        return Err(invalid("Missing or invalid compressed audio format"));
    }
    let attrs = comments.attributes(if layout.ogg { "Ogg" } else { layout.codec }, include);
    Ok(WavInfo {
        metadata: AudioMetadata {
            sample_rate: layout.rate,
            channels: layout.channels,
            bit_depth: layout.bits,
            frames: layout.frames,
            duration: layout.frames as f64 / f64::from(layout.rate),
            encoding: layout.codec.into(),
            bext_description: String::new(),
            attributes: attrs,
        },
        data_offset: 0,
        data_bytes: len,
        block_align: 0,
        format_tag: 0,
        pcm_layout: None,
        format_data: Vec::new(),
        artwork: None,
    })
}

pub(crate) fn read_info(mut source: impl Read + Seek, len: u64, include: bool) -> Result<WavInfo> {
    source.seek(SeekFrom::Start(0))?;
    let mut magic = [0; 4];
    source.read_exact(&mut magic)?;
    match &magic {
        b"fLaC" => flac(&mut source, len, include),
        b"OggS" => ogg(&mut source, len, include),
        _ => Err(invalid(
            "Unsupported audio container; expected WAV, FLAC or Ogg",
        )),
    }
}

fn flac(source: &mut (impl Read + Seek), len: u64, include: bool) -> Result<WavInfo> {
    let mut layout = Layout::default();
    let mut comments = Comments::default();
    let mut at = 4_u64;
    let mut budget = MAX_COMMENT_BYTES;
    for _ in 0..MAX_COMMENTS {
        if at.saturating_add(4) > len {
            comments.warn("Truncated FLAC metadata header");
            break;
        }
        source.seek(SeekFrom::Start(at))?;
        let mut header = [0; 4];
        source.read_exact(&mut header)?;
        let size =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        let end = at + 4 + size as u64;
        if end > len {
            comments.warn("Truncated FLAC metadata block");
            break;
        }
        match header[0] & 0x7f {
            0 => {
                let mut data = [0; 34];
                if size != 34 {
                    return Err(invalid("Invalid FLAC STREAMINFO length"));
                }
                source.read_exact(&mut data)?;
                streaminfo(&data, &mut layout)?;
            }
            4 if include => {
                let retained = size.min(budget);
                let mut data = vec![0; retained];
                source.read_exact(&mut data)?;
                budget -= retained;
                if size > retained {
                    comments.warn("FLAC comment block exceeded the extraction limit");
                }
                if !data.is_empty() {
                    comments.read(&data);
                }
            }
            // Pictures, padding and applications never become searchable text.
            _ => {}
        }
        at = end;
        if header[0] & 0x80 != 0 {
            break;
        }
    }
    finish(layout, comments, len, include)
}

fn identify(packet: &[u8], layout: &mut Layout) -> Result<()> {
    if packet.starts_with(b"\x01vorbis") && packet.len() >= 30 {
        layout.channels = u16::from(packet[11]);
        layout.rate = little(&packet[12..]);
        layout.bits = 32;
        layout.codec = "VORBIS";
    } else if packet.starts_with(b"OpusHead") && packet.len() >= 19 {
        layout.channels = u16::from(packet[9]);
        layout.rate = 48000;
        layout.pre_skip = u64::from(u16::from_le_bytes(packet[10..12].try_into().unwrap()));
        layout.bits = 32;
        layout.codec = "OPUS";
    } else if packet.starts_with(b"\x7fFLAC") {
        let at = packet
            .windows(4)
            .position(|p| p == b"fLaC")
            .ok_or_else(|| invalid("Invalid Ogg FLAC mapping"))?
            + 8;
        streaminfo(
            packet
                .get(at..)
                .ok_or_else(|| invalid("Truncated Ogg FLAC header"))?,
            layout,
        )?;
    } else {
        return Err(invalid("Unsupported Ogg audio identification packet"));
    }
    Ok(())
}

fn ogg(source: &mut (impl Read + Seek), len: u64, include: bool) -> Result<WavInfo> {
    let mut layout = Layout::default();
    let mut comments = Comments::default();
    layout.ogg = true;
    let mut at = 0_u64;
    let mut serial = None;
    let mut packet = Vec::new();
    let mut packets = 0;
    let mut overflow = false;
    let mut granule = 0;
    let mut tail_checked = false;
    for page in 0..MAX_PAGES {
        if at.saturating_add(27) > len {
            comments.warn("Truncated Ogg page header");
            break;
        }
        source.seek(SeekFrom::Start(at))?;
        let mut header = [0; 27];
        source.read_exact(&mut header)?;
        if &header[..4] != b"OggS" || header[4] != 0 {
            return Err(invalid("Invalid Ogg page header"));
        }
        let this_serial = little(&header[14..]);
        let first = *serial.get_or_insert(this_serial);
        let mut laces = vec![0; usize::from(header[26])];
        source.read_exact(&mut laces)?;
        let body: u64 = laces.iter().map(|v| u64::from(*v)).sum();
        let end = at + 27 + laces.len() as u64 + body;
        if end > len {
            comments.warn("Truncated Ogg page body");
            break;
        }
        if this_serial == first {
            let position = u64::from_le_bytes(header[6..14].try_into().unwrap());
            if position != u64::MAX {
                granule = position;
            }
            if packets < 3 && page < MAX_HEADER_PAGES {
                for lace in laces {
                    let size = usize::from(lace);
                    if packets >= 3 {
                        break;
                    }
                    if !overflow && packet.len().saturating_add(size) <= MAX_COMMENT_BYTES {
                        let old = packet.len();
                        packet.resize(old + size, 0);
                        source.read_exact(&mut packet[old..])?;
                    } else {
                        source.seek(SeekFrom::Current(size as i64))?;
                        overflow = true;
                    }
                    if lace < 255 {
                        if packets == 0 {
                            identify(&packet, &mut layout)?;
                        } else if include {
                            if packet.starts_with(b"\x03vorbis") {
                                comments.read(&packet[7..]);
                            } else if packet.starts_with(b"OpusTags") {
                                comments.read(&packet[8..]);
                            } else if layout.codec == "FLAC"
                                && packet.len() >= 4
                                && packet[0] & 0x7f == 4
                            {
                                comments.read(&packet[4..]);
                            }
                        }
                        if overflow {
                            comments.warn("Ogg header packet exceeded the extraction limit");
                        }
                        packets += 1;
                        packet.clear();
                        overflow = false;
                    }
                }
            }
            if header[5] & 4 != 0 {
                break;
            }
            if packets >= 2 && !tail_checked {
                tail_checked = true;
                if let Some(last) = final_granule(source, len, first)? {
                    granule = last;
                    break;
                }
            }
        }
        at = end;
    }
    if layout.codec != "FLAC" || layout.frames == 0 {
        layout.frames = granule.saturating_sub(layout.pre_skip);
    }
    finish(layout, comments, len, include)
}

// Ordinary single-stream files need only bounded header/tail IO. A chain whose
// first EOS is outside the tail falls back to page-header walking and still stops
// at that first stream; audio payloads are skipped without allocation.
fn final_granule(source: &mut (impl Read + Seek), len: u64, serial: u32) -> Result<Option<u64>> {
    let saved = source.stream_position()?;
    let count = len.min(128 * 1024) as usize;
    source.seek(SeekFrom::Start(len - count as u64))?;
    let mut tail = vec![0; count];
    source.read_exact(&mut tail)?;
    source.seek(SeekFrom::Start(saved))?;
    for at in 0..tail.len().saturating_sub(26) {
        let header = &tail[at..at + 27];
        if &header[..4] != b"OggS"
            || header[4] != 0
            || header[5] & 4 == 0
            || little(&header[14..]) != serial
        {
            continue;
        }
        let header_end = at + 27 + usize::from(header[26]);
        let Some(laces) = tail.get(at + 27..header_end) else {
            continue;
        };
        let body: usize = laces.iter().map(|value| usize::from(*value)).sum();
        if header_end + body <= tail.len() {
            let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
            if granule != u64::MAX {
                return Ok(Some(granule));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "vorbis_metadata_tests.rs"]
mod tests;
