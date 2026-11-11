// crates/audiolib-audio/src/aiff_metadata.rs
//! Bounded AIFF/AIFC PCM layout and public embedded Metadata extraction.
use crate::{
    pcm_layout::{ByteOrder, PcmLayout, SampleKind},
    wav::WavInfo,
    wav_metadata::{
        EmbeddedMetadata, MAX_CHUNK_BYTES, MAX_ID3_BYTES, MAX_METADATA_BYTES, append, text,
    },
};
use audiolib_core::{AudioMetadata, Error, Result};
use serde_json::{Map, Value, json};
use std::io::{Read, Seek, SeekFrom};

/// Increment when AIFF extraction changes so incremental scans refresh owned records.
pub const AIFF_METADATA_VERSION: i64 = 1;
const MAX_CHUNKS: usize = 65_536;
const MAX_COMMENTS: usize = 4096;

fn invalid(message: &str) -> Error {
    Error::Audio(message.into())
}

fn u16be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes[..2].try_into().unwrap())
}

fn u32be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

// The domain contract stores an integer sample rate. Decode the explicit integer
// bit and exponent without lossy float casts, rejecting unsupported fractional,
// negative, non-normal, non-finite, zero, or overflowing rates.
fn sample_rate(bytes: &[u8]) -> Result<u32> {
    let sign_exponent = u16be(bytes);
    let exponent = i32::from(sign_exponent & 0x7fff) - 16_383;
    let mantissa = u64::from_be_bytes(bytes[2..10].try_into().unwrap());
    if sign_exponent & 0x8000 != 0 || !(0..=31).contains(&exponent) || mantissa >> 63 != 1 {
        return Err(invalid("Invalid or unsupported AIFF sample rate"));
    }
    let fraction_bits = 63 - exponent as u32;
    if mantissa & ((1_u64 << fraction_bits) - 1) != 0 {
        return Err(invalid("AIFF sample rate must be an integer"));
    }
    u32::try_from(mantissa >> fraction_bits)
        .map_err(|_| invalid("AIFF sample rate exceeds the supported range"))
}

struct Common {
    channels: u16,
    frames: u64,
    bits: u16,
    rate: u32,
    align: u16,
    layout: PcmLayout,
    encoding: &'static str,
    compression: [u8; 4],
}

fn common(file: &mut (impl Read + Seek), size: u64, compressed: bool) -> Result<Common> {
    if size < if compressed { 24 } else { 18 } {
        return Err(invalid("Truncated AIFF COMM chunk"));
    }
    let mut bytes = [0_u8; 18];
    file.read_exact(&mut bytes)?;
    let channels = u16be(&bytes);
    let frames = u64::from(u32be(&bytes[2..]));
    let bits = u16be(&bytes[6..]);
    let rate = sample_rate(&bytes[8..])?;
    if !(1..=256).contains(&channels) {
        return Err(invalid("Invalid AIFF channel count"));
    }
    let mut compression = *b"NONE";
    if compressed {
        file.read_exact(&mut compression)?;
        let mut length = [0_u8; 1];
        file.read_exact(&mut length)?;
        let pascal_bytes = 1 + u64::from(length[0]);
        if 22 + pascal_bytes + (pascal_bytes & 1) > size {
            return Err(invalid("Truncated AIFC compression name"));
        }
    }
    let (endian, kind, encoding) = match &compression {
        b"NONE" | b"none" | b"twos" | b"TWOS" => (ByteOrder::Big, SampleKind::Signed, "PCM"),
        b"sowt" | b"SOWT" => (ByteOrder::Little, SampleKind::Signed, "PCM"),
        b"fl32" | b"FL32" if bits == 32 => (ByteOrder::Big, SampleKind::Float, "IEEE_FLOAT"),
        b"fl64" | b"FL64" if bits == 64 => (ByteOrder::Big, SampleKind::Float, "IEEE_FLOAT"),
        b"fl32" | b"FL32" | b"fl64" | b"FL64" => {
            return Err(invalid(
                "AIFC float sample size does not match its compression type",
            ));
        }
        _ => return Err(invalid("Unsupported AIFC compression type")),
    };
    if encoding == "PCM" && !(1..=32).contains(&bits) {
        return Err(invalid(
            "AIFF PCM sample size must be between 1 and 32 bits",
        ));
    }
    let bytes_per_sample = u8::try_from(bits.div_ceil(8)).unwrap();
    let align = channels
        .checked_mul(u16::from(bytes_per_sample))
        .ok_or_else(|| invalid("AIFF frame size overflow"))?;
    Ok(Common {
        channels,
        frames,
        bits,
        rate,
        align,
        layout: PcmLayout {
            endian,
            kind,
            bytes_per_sample,
        },
        encoding,
        compression,
    })
}

fn comments(
    bytes: &[u8],
    raw: &mut Map<String, Value>,
    fields: &mut Map<String, Value>,
    embedded: &mut EmbeddedMetadata,
) {
    if bytes.len() < 2 {
        embedded.warning("AIFF COMT: truncated comment count");
        return;
    }
    let count = usize::from(u16be(bytes));
    if count > MAX_COMMENTS {
        embedded.warning("AIFF COMT: comment count exceeds the extraction limit");
    }
    let mut position = 2_usize;
    let mut records = Vec::new();
    for _ in 0..count.min(MAX_COMMENTS) {
        let Some(header) = bytes.get(position..position + 8) else {
            embedded.warning("AIFF COMT: truncated comment header");
            break;
        };
        let timestamp = u32be(header);
        let marker = i16::from_be_bytes(header[4..6].try_into().unwrap());
        let size = usize::from(u16be(&header[6..]));
        position += 8;
        let end = position + size;
        let next = end + (size & 1);
        let Some(value) = bytes.get(position..end).filter(|_| next <= bytes.len()) else {
            embedded.warning("AIFF COMT: truncated comment text or padding");
            break;
        };
        let value = text(value);
        append(raw, "COMT", &value);
        append(fields, "description", &value);
        records.push(json!({"timestamp": timestamp, "marker_id": marker, "text": value}));
        position = next;
    }
    if !records.is_empty() {
        let values = raw.entry("COMT_records").or_insert_with(|| json!([]));
        if let Some(values) = values.as_array_mut() {
            values.extend(
                records
                    .into_iter()
                    .take(MAX_COMMENTS.saturating_sub(values.len())),
            );
        }
    }
}

/// Read AIFF or the supported AIFC PCM/float subset without trusting a path suffix.
/// Playback-layout probes stop as soon as COMM and SSND are known, before reading
/// sample bytes or trailing Metadata; full probes validate the complete FORM body.
pub(crate) fn read_info(
    mut source: impl Read + Seek,
    len: u64,
    include_metadata: bool,
    include_artwork: bool,
) -> Result<WavInfo> {
    source.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; 12];
    source.read_exact(&mut header)?;
    if &header[..4] != b"FORM" || !matches!(&header[8..], b"AIFF" | b"AIFC") {
        return Err(invalid("Expected FORM AIFF or AIFC audio"));
    }
    let compressed = &header[8..] == b"AIFC";
    let container_end = 8 + u64::from(u32be(&header[4..]));
    if container_end < 12 {
        return Err(invalid("Invalid AIFF FORM size"));
    }
    if container_end > len {
        return Err(invalid("Truncated AIFF FORM container"));
    }
    let mut embedded = EmbeddedMetadata::default();
    if include_metadata && container_end < len {
        embedded.warning(format!(
            "Ignored {} trailing bytes outside the AIFF container",
            len - container_end
        ));
    }
    let mut raw = Map::new();
    let mut fields = Map::new();
    let mut metadata_bytes = 0_u64;
    let mut layout = None;
    let mut sound = None;
    let mut position = 12_u64;
    let mut chunks = 0_usize;
    while position < container_end {
        chunks += 1;
        if chunks > MAX_CHUNKS {
            return Err(invalid("AIFF chunk count exceeds the supported limit"));
        }
        let payload = position
            .checked_add(8)
            .filter(|end| *end <= container_end)
            .ok_or_else(|| invalid("Truncated AIFF chunk header"))?;
        source.seek(SeekFrom::Start(position))?;
        let mut header = [0_u8; 8];
        source.read_exact(&mut header)?;
        let id: &[u8; 4] = header[..4].try_into().unwrap();
        let size = u64::from(u32be(&header[4..]));
        let end = payload
            .checked_add(size)
            .filter(|end| *end <= container_end)
            .ok_or_else(|| invalid("Truncated AIFF chunk"))?;
        let next = end
            .checked_add(size & 1)
            .filter(|end| *end <= container_end)
            .ok_or_else(|| invalid("Truncated AIFF chunk padding"))?;
        if include_metadata {
            embedded.chunk(id, size);
        }
        match id {
            b"COMM" => {
                if layout.is_some() {
                    return Err(invalid("Duplicate AIFF COMM chunk"));
                }
                layout = Some(common(&mut source, size, compressed)?);
            }
            b"SSND" => {
                if sound.is_some() {
                    return Err(invalid("Duplicate AIFF SSND chunk"));
                }
                if size < 8 {
                    return Err(invalid("Truncated AIFF SSND chunk"));
                }
                let mut sound_header = [0_u8; 8];
                source.read_exact(&mut sound_header)?;
                let offset = u64::from(u32be(&sound_header));
                if u32be(&sound_header[4..]) != 0 {
                    return Err(invalid("Unsupported block-aligned AIFF sound data"));
                }
                if offset > size - 8 {
                    return Err(invalid("AIFF SSND offset exceeds its chunk"));
                }
                sound = Some((payload + 8 + offset, size - 8 - offset));
            }
            b"NAME" | b"AUTH" | b"ANNO" | b"(c) " | b"COMT" | b"ID3 " | b"id3 "
                if include_metadata || include_artwork =>
            {
                let id3 = matches!(id, b"ID3 " | b"id3 ");
                let limit = if id3 { MAX_ID3_BYTES } else { MAX_CHUNK_BYTES };
                if size > limit || metadata_bytes + size > MAX_METADATA_BYTES {
                    embedded.warning(format!(
                        "AIFF {}: Metadata size limit exceeded",
                        String::from_utf8_lossy(id)
                    ));
                } else {
                    metadata_bytes += size;
                    if id3 {
                        if include_metadata {
                            embedded.read_id3(&mut source, size as usize)?;
                        }
                        if include_artwork {
                            source.seek(SeekFrom::Start(payload))?;
                            embedded.read_id3_artwork(&mut source, size as usize)?;
                        }
                    } else if include_metadata {
                        let mut bytes = vec![0_u8; size as usize];
                        source.read_exact(&mut bytes)?;
                        if id == b"COMT" {
                            comments(&bytes, &mut raw, &mut fields, &mut embedded);
                        } else {
                            let value = text(&bytes);
                            append(&mut raw, std::str::from_utf8(id).unwrap(), &value);
                            let field = match id {
                                b"NAME" => "title",
                                b"AUTH" => "artist",
                                b"ANNO" => "description",
                                _ => "copyright",
                            };
                            append(&mut fields, field, &value);
                        }
                    }
                }
            }
            _ => {}
        }
        position = next;
        if !include_metadata && !include_artwork && layout.is_some() && sound.is_some() {
            break;
        }
    }
    let common = layout.ok_or_else(|| invalid("AIFF is missing a COMM chunk"))?;
    let (data_offset, data_bytes) =
        sound.ok_or_else(|| invalid("AIFF is missing an SSND chunk"))?;
    if common.frames > data_bytes / u64::from(common.align) {
        return Err(invalid("AIFF COMM frame count exceeds SSND audio capacity"));
    }
    let mut attrs = Map::new();
    attrs.insert(
        "container".into(),
        json!(if compressed { "AIFC" } else { "AIFF" }),
    );
    attrs.insert("format_tag".into(), json!(0));
    attrs.insert(
        "aiff_compression".into(),
        json!(String::from_utf8_lossy(&common.compression)),
    );
    if common.bits != u16::from(common.layout.bytes_per_sample) * 8 {
        attrs.insert("valid_bits_per_sample".into(), json!(common.bits));
    }
    let artwork = embedded.take_artwork();
    if include_metadata {
        embedded.finish(&mut attrs);
        let metadata = attrs
            .get_mut("wav_metadata")
            .unwrap()
            .as_object_mut()
            .unwrap();
        metadata.insert("aiff_version".into(), json!(AIFF_METADATA_VERSION));
        metadata.insert("aiff".into(), Value::Object(raw));
        let canonical = metadata.get_mut("fields").unwrap().as_object_mut().unwrap();
        for (key, value) in fields {
            // ID3 is the preferred canonical source; retain every AIFF original.
            canonical.entry(key).or_insert(value);
        }
    }
    Ok(WavInfo {
        metadata: AudioMetadata {
            sample_rate: common.rate,
            channels: common.channels,
            bit_depth: common.bits,
            frames: common.frames,
            duration: common.frames as f64 / f64::from(common.rate),
            encoding: common.encoding.into(),
            bext_description: String::new(),
            attributes: Value::Object(attrs),
        },
        data_offset,
        data_bytes,
        block_align: common.align,
        format_tag: 0,
        artwork,
        pcm_layout: Some(common.layout),
        format_data: Vec::new(),
    })
}

#[cfg(test)]
#[path = "aiff_metadata_tests.rs"]
mod tests;
