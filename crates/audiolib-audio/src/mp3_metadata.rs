// crates/audiolib-audio/src/mp3_metadata.rs
//! Bounded ID3 extraction and exact MPEG Layer III frame accounting.
use crate::{
    wav::WavInfo,
    wav_metadata::{EmbeddedMetadata, MAX_ID3_BYTES, MAX_METADATA_BYTES, append},
};
use audiolib_core::{AudioMetadata, Error, Result};
use serde_json::{Map, Value, json};
use std::io::{Read, Seek, SeekFrom};

/// Revision of MP3 extraction and the owned, validated remote playback layout.
pub const MP3_METADATA_VERSION: i64 = 1;
const MAX_RESYNC_BYTES: u64 = 1024 * 1024;
const DECODER_DELAY: u64 = 529;

fn invalid(message: &str) -> Error {
    Error::Audio(message.into())
}

#[derive(Clone, Copy)]
struct Header {
    version: u8,
    rate: u32,
    channels: u16,
    size: u64,
    samples: u64,
    side_bytes: usize,
    has_crc: bool,
}

fn header(bytes: &[u8]) -> Option<Header> {
    let bytes = bytes.get(..4)?;
    if bytes[0] != 0xff || bytes[1] & 0xe0 != 0xe0 || (bytes[1] >> 1) & 3 != 1 {
        return None;
    }
    let version = (bytes[1] >> 3) & 3;
    let bitrate_index = usize::from(bytes[2] >> 4);
    let rate_index = usize::from((bytes[2] >> 2) & 3);
    if version == 1 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let rates = [44100_u32, 48000, 32000];
    let rate = rates[rate_index]
        / if version == 3 {
            1
        } else if version == 2 {
            2
        } else {
            4
        };
    let bitrates = if version == 3 {
        [
            0_u32, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ]
    } else {
        [
            0_u32, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160,
        ]
    };
    let channels = if bytes[3] >> 6 == 3 { 1 } else { 2 };
    let side_bytes = match (version == 3, channels) {
        (true, 1) | (false, 2) => 17,
        (true, _) => 32,
        _ => 9,
    };
    let factor = if version == 3 { 144000 } else { 72000 };
    let size = u64::from(factor * bitrates[bitrate_index] / rate + u32::from((bytes[2] >> 1) & 1));
    let crc_bytes = usize::from(bytes[1] & 1 == 0) * 2;
    if size < (4 + crc_bytes + side_bytes) as u64 {
        return None;
    }
    Some(Header {
        version,
        rate,
        channels,
        size,
        samples: if version == 3 { 1152 } else { 576 },
        // Xing's offset follows the fixed side information, as used by the
        // bundled MPEG demuxer; CRC is not added to this tag offset.
        side_bytes,
        has_crc: bytes[1] & 1 == 0,
    })
}

fn read_at(source: &mut (impl Read + Seek), at: u64, bytes: &mut [u8]) -> Result<()> {
    source.seek(SeekFrom::Start(at))?;
    source.read_exact(bytes)?;
    Ok(())
}

fn syncsafe(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        (*byte < 128).then_some((value << 7) | u64::from(*byte))
    })
}

fn read_tag(
    source: &mut (impl Read + Seek),
    start: u64,
    size: u64,
    include: bool,
    artwork: bool,
    embedded: &mut EmbeddedMetadata,
) -> Result<()> {
    if !include && !artwork {
        return Ok(());
    }
    if size > MAX_ID3_BYTES {
        embedded.warning("ID3: tag exceeds the extraction limit");
        return Ok(());
    }
    source.seek(SeekFrom::Start(start))?;
    if let Err(error) = embedded.read_id3(source, size as usize) {
        embedded.warning(format!("ID3: optional tag could not be read: {error}"));
    }
    if artwork {
        source.seek(SeekFrom::Start(start))?;
        if let Err(error) = embedded.read_id3_artwork(source, size as usize) {
            embedded.warning(format!("ID3: optional artwork could not be read: {error}"));
        }
    }
    Ok(())
}

fn leading_tags(
    source: &mut (impl Read + Seek),
    len: u64,
    include: bool,
    artwork: bool,
    embedded: &mut EmbeddedMetadata,
) -> Result<(u64, bool)> {
    let mut at = 0_u64;
    for _ in 0..16 {
        if len.saturating_sub(at) < 10 {
            break;
        }
        let mut bytes = [0; 10];
        read_at(source, at, &mut bytes)?;
        if &bytes[..3] != b"ID3" {
            return Ok((at, false));
        }
        let Some(size) = syncsafe(&bytes[6..]) else {
            embedded.warning("ID3: invalid tag length; locating actual MPEG audio");
            return Ok((at + 10, true));
        };
        let footer = u64::from(bytes[3] == 4 && bytes[5] & 0x10 != 0) * 10;
        let Some(mut end) = at.checked_add(10 + size).filter(|end| *end <= len) else {
            embedded.warning("ID3: truncated optional tag; locating actual MPEG audio");
            return Ok((at + 10, true));
        };
        if footer != 0 {
            let mut bytes_at_end = [0; 10];
            if end + footer <= len {
                read_at(source, end, &mut bytes_at_end)?;
            }
            if &bytes_at_end[..3] == b"3DI" && bytes_at_end[3..] == bytes[3..] {
                end += footer;
            } else {
                embedded.warning("ID3: invalid optional footer was ignored");
            }
        }
        if !(2..=4).contains(&bytes[3]) {
            embedded.warning("ID3: unsupported tag version");
        } else if end <= MAX_METADATA_BYTES {
            read_tag(source, at, 10 + size, include, artwork, embedded)?;
        } else {
            embedded.warning("ID3: leading tags exceed the extraction budget");
        }
        at = end;
    }
    Ok((at, false))
}

fn latin_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| char::from(*byte))
        .collect::<String>()
        .trim_matches('\0')
        .trim()
        .into()
}

fn tail_tags(
    source: &mut (impl Read + Seek),
    len: u64,
    include: bool,
    artwork: bool,
    embedded: &mut EmbeddedMetadata,
) -> Result<(u64, Map<String, Value>)> {
    let mut end = len;
    let mut id3v1 = Map::new();
    for _ in 0..4 {
        if end >= 128 {
            let mut bytes = [0; 128];
            read_at(source, end - 128, &mut bytes)?;
            if &bytes[..3] == b"TAG" {
                if include {
                    for (key, range) in [
                        ("title", 3..33),
                        ("artist", 33..63),
                        ("album", 63..93),
                        ("year", 93..97),
                    ] {
                        append(&mut id3v1, key, &latin_text(&bytes[range]));
                    }
                    let comment_end = if bytes[125] == 0 && bytes[126] != 0 {
                        125
                    } else {
                        127
                    };
                    append(&mut id3v1, "comment", &latin_text(&bytes[97..comment_end]));
                    if comment_end == 125 {
                        append(&mut id3v1, "track", &bytes[126].to_string());
                    }
                    id3v1.insert("genre_index".into(), json!(bytes[127]));
                }
                end -= 128;
                continue;
            }
        }
        if end >= 32 {
            let mut footer = [0; 32];
            read_at(source, end - 32, &mut footer)?;
            let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
            if &footer[..8] == b"APETAGEX" && matches!(version, 1000 | 2000) {
                let size = u64::from(u32::from_le_bytes(footer[12..16].try_into().unwrap()));
                if (32..=MAX_METADATA_BYTES).contains(&size) && size <= end {
                    end -= size;
                    // APEv2's size excludes its optional header.
                    if end >= 32 {
                        let mut marker = [0; 8];
                        read_at(source, end - 32, &mut marker)?;
                        if &marker == b"APETAGEX" {
                            end -= 32;
                        }
                    }
                    embedded.warning("APE: trailing tag was skipped without extraction");
                    continue;
                }
                embedded.warning("APE: invalid trailing tag size");
            }
        }
        if end >= 20 {
            let mut footer = [0; 10];
            read_at(source, end - 10, &mut footer)?;
            if &footer[..3] == b"3DI"
                && footer[3] == 4
                && let Some(size) = syncsafe(&footer[6..])
                && size + 20 <= end
            {
                let start = end - size - 20;
                let mut head = [0; 10];
                read_at(source, start, &mut head)?;
                if &head[..3] == b"ID3" && head[3..] == footer[3..] {
                    read_tag(source, start, size + 10, include, artwork, embedded)?;
                    end = start;
                    continue;
                }
            }
        }
        break;
    }
    Ok((end, id3v1))
}

fn find_first(
    source: &mut (impl Read + Seek),
    start: u64,
    end: u64,
    recover: bool,
) -> Result<(u64, Header)> {
    let count = end
        .saturating_sub(start)
        .min(if recover { MAX_RESYNC_BYTES } else { 4 });
    if count < 4 {
        return Err(invalid("Missing MPEG Layer III audio frame"));
    }
    let mut bytes = vec![0; count as usize];
    read_at(source, start, &mut bytes)?;
    for offset in 0..=bytes.len() - 4 {
        let Some(first) = header(&bytes[offset..]) else {
            continue;
        };
        let at = start + offset as u64;
        let next = at + first.size;
        if next > end {
            continue;
        }
        if !recover {
            return Ok((at, first));
        }
        if next == end {
            return Ok((at, first));
        }
        if end - next < 4 {
            continue;
        }
        let mut following = [0; 4];
        read_at(source, next, &mut following)?;
        if let Some(second) = header(&following)
            && (first.version, first.rate, first.channels)
                == (second.version, second.rate, second.channels)
        {
            return Ok((at, first));
        }
    }
    Err(invalid("Invalid or truncated MPEG Layer III frame stream"))
}

#[derive(Default)]
struct Vbr {
    kind: Option<&'static str>,
    frames: Option<u64>,
    bytes: Option<u64>,
    delay: u64,
    padding: u64,
}

fn big_u32(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(
        bytes.get(at..at + 4)?.try_into().ok()?,
    )))
}

fn tag_crc(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xa001
            };
        }
    }
    crc
}

// Presence is independent of the optional duration fields: malformed Xing
// flags/counts must not turn its reserved information frame into actual audio.
fn information_frame_kind(frame: &[u8], first: Header) -> Option<&'static str> {
    let at = 4 + first.side_bytes;
    if frame.len() >= at + 8 && frame[4..at].iter().all(|byte| *byte == 0) {
        match frame.get(at..at + 4) {
            Some(b"Xing") => return Some("Xing"),
            Some(b"Info") => return Some("Info"),
            _ => {}
        }
    }
    if frame.len() >= 62
        && frame.get(36..40) == Some(b"VBRI")
        && frame.get(40..42) == Some(&[0, 1])
        && frame[4..36].iter().all(|byte| *byte == 0)
    {
        return Some("VBRI");
    }
    None
}

fn vbr(frame: &[u8], first: Header, embedded: &mut EmbeddedMetadata) -> Vbr {
    let kind = information_frame_kind(frame, first);
    let at = 4 + first.side_bytes;
    if matches!(kind, Some("Xing" | "Info")) {
        let parsed = (|| {
            let flags = big_u32(frame, at + 4)?;
            if flags & !15 != 0 {
                return None;
            }
            let mut cursor = at + 8;
            let mut take = |flag: u64| -> Option<Option<u64>> {
                if flags & flag == 0 {
                    return Some(None);
                }
                let value = big_u32(frame, cursor)?;
                cursor += 4;
                Some(Some(value))
            };
            let frames = take(1)?;
            let bytes = take(2)?;
            if flags & 4 != 0 {
                cursor += 100;
            }
            if flags & 8 != 0 {
                cursor += 4;
            }
            frame.get(..cursor)?;
            let mut result = Vbr {
                kind,
                frames,
                bytes,
                ..Vbr::default()
            };
            if let Some(lame) = frame.get(cursor..cursor + 24)
                && matches!(&lame[..4], b"LAME" | b"Lavc" | b"Lavf")
            {
                let crc_valid =
                    if frame.len() >= cursor + 36 && (first.has_crc || &lame[..4] == b"LAME") {
                        let expected =
                            u16::from_be_bytes(frame[cursor + 34..cursor + 36].try_into().ok()?);
                        expected == tag_crc(&frame[..cursor + 34])
                    } else {
                        true
                    };
                let raw_delay = (u64::from(lame[21]) << 4) | u64::from(lame[22] >> 4);
                let raw_padding = (u64::from(lame[22] & 15) << 8) | u64::from(lame[23]);
                // Match Symphonia's Layer III synthesis-delay correction. These
                // values describe its already gapless presentation, not a second trim.
                if crc_valid {
                    result.delay = raw_delay + DECODER_DELAY;
                    result.padding = raw_padding.saturating_sub(DECODER_DELAY);
                } else {
                    embedded
                        .warning("MP3: invalid optional LAME tag checksum; padding was ignored");
                }
            }
            Some(result)
        })();
        if let Some(result) = parsed {
            return result;
        }
        embedded.warning("MP3: malformed optional Xing/Info header");
        return Vbr {
            kind,
            ..Vbr::default()
        };
    }
    if kind == Some("VBRI") {
        return Vbr {
            kind,
            bytes: big_u32(frame, 46),
            frames: big_u32(frame, 50),
            ..Vbr::default()
        };
    }
    if frame.get(36..40) == Some(b"VBRI") && frame[4..36].iter().all(|byte| *byte == 0) {
        embedded.warning("MP3: malformed optional VBRI header");
    }
    Vbr::default()
}

fn info(metadata: AudioMetadata, data_offset: u64, data_bytes: u64) -> WavInfo {
    WavInfo {
        metadata,
        data_offset,
        data_bytes,
        block_align: 0,
        format_tag: 0,
        format_data: Vec::new(),
        pcm_layout: None,
        artwork: None,
    }
}

/// Read actual Layer III frame headers and bounded optional tags without decoding audio.
pub(crate) fn read_info(
    mut source: impl Read + Seek,
    len: u64,
    include_metadata: bool,
    include_artwork: bool,
) -> Result<WavInfo> {
    let mut embedded = EmbeddedMetadata::default();
    let (start, recover) = leading_tags(
        &mut source,
        len,
        include_metadata,
        include_artwork,
        &mut embedded,
    )?;
    let (end, id3v1) = tail_tags(
        &mut source,
        len,
        include_metadata,
        include_artwork,
        &mut embedded,
    )?;
    // Only an invalid optional ID3 length permits bounded resynchronization.
    // After a valid tag, invalid MPEG bytes are an audio failure, not more tags.
    let (data_offset, first) = find_first(&mut source, start, end, recover)?;
    if data_offset != start {
        embedded.warning("ID3: skipped malformed tag bytes before actual MPEG audio");
    }
    let mut first_bytes = vec![0; first.size as usize];
    read_at(&mut source, data_offset, &mut first_bytes)?;
    let mut vbr = vbr(&first_bytes, first, &mut embedded);
    let mut at = data_offset;
    let mut count = 0_u64;
    while at < end {
        if end - at < 4 {
            return Err(invalid("Truncated MPEG frame header"));
        }
        let mut bytes = [0; 4];
        read_at(&mut source, at, &mut bytes)?;
        let current =
            header(&bytes).ok_or_else(|| invalid("Invalid MPEG Layer III frame header"))?;
        if (current.version, current.rate, current.channels)
            != (first.version, first.rate, first.channels)
        {
            return Err(invalid("MPEG audio format changes within the file"));
        }
        at = at
            .checked_add(current.size)
            .filter(|next| *next <= end)
            .ok_or_else(|| invalid("Truncated MPEG audio frame"))?;
        count += 1;
    }
    let data_bytes = end - data_offset;
    let header_frames = u64::from(vbr.kind.is_some());
    let audio_frames = count
        .checked_sub(header_frames)
        .filter(|count| *count > 0)
        .ok_or_else(|| invalid("MP3 contains no audio after its information frame"))?;
    if vbr.frames.is_some_and(|frames| frames != audio_frames)
        || vbr.bytes.is_some_and(|bytes| bytes != data_bytes)
    {
        embedded.warning(
            "MP3: optional duration header differs from actual frames; used exact frame count",
        );
    }
    let raw_frames = audio_frames
        .checked_mul(first.samples)
        .ok_or_else(|| invalid("MP3 sample count overflow"))?;
    if vbr.delay + vbr.padding > raw_frames {
        embedded.warning("MP3: invalid encoder delay/padding was ignored");
        vbr.delay = 0;
        vbr.padding = 0;
    }
    let frames = raw_frames - vbr.delay - vbr.padding;
    let mut attrs = Map::new();
    attrs.insert("container".into(), json!("MP3"));
    attrs.insert("format_tag".into(), json!(0));
    attrs.insert(
        "source_audio_layout".into(),
        json!({
            "version": MP3_METADATA_VERSION, "container": "MP3", "source_len": len,
            "sample_rate": first.rate, "channels": first.channels, "frames": frames,
            "data_offset": data_offset, "data_bytes": data_bytes,
            "mpeg_frame_count": count, "samples_per_frame": first.samples,
            "header_frames": header_frames, "encoder_delay": vbr.delay, "end_padding": vbr.padding
        }),
    );
    let artwork = embedded.take_artwork();
    if include_metadata {
        embedded.finish(&mut attrs);
        let metadata = attrs
            .get_mut("wav_metadata")
            .and_then(Value::as_object_mut)
            .unwrap();
        metadata.insert("mp3_version".into(), json!(MP3_METADATA_VERSION));
        if let Some(kind) = vbr.kind {
            metadata.insert("mp3_header".into(), json!(kind));
        }
        if !id3v1.is_empty() {
            let fields = metadata
                .get_mut("fields")
                .and_then(Value::as_object_mut)
                .unwrap();
            for (raw, canonical) in [
                ("title", "title"),
                ("artist", "artist"),
                ("album", "library"),
                ("comment", "description"),
            ] {
                if !fields.contains_key(canonical)
                    && let Some(value) = id3v1.get(raw)
                {
                    fields.insert(canonical.into(), value.clone());
                }
            }
            metadata.insert("id3v1".into(), Value::Object(id3v1));
        }
    }
    let metadata = AudioMetadata {
        sample_rate: first.rate,
        channels: first.channels,
        bit_depth: 0,
        frames,
        duration: frames as f64 / f64::from(first.rate),
        encoding: "MP3".into(),
        bext_description: String::new(),
        attributes: Value::Object(attrs),
    };
    let mut result = info(metadata, data_offset, data_bytes);
    result.artwork = artwork;
    Ok(result)
}

/// Validate an owned scan layout and the actual first frame without scanning network audio.
pub(crate) fn cached_info(
    source: &mut (impl Read + Seek),
    len: u64,
    metadata: AudioMetadata,
) -> Result<WavInfo> {
    let layout = &metadata.attributes["source_audio_layout"];
    let number = |key: &str| {
        layout[key]
            .as_u64()
            .ok_or_else(|| invalid("Invalid cached MP3 layout"))
    };
    let offset = number("data_offset")?;
    let bytes = number("data_bytes")?;
    let count = number("mpeg_frame_count")?;
    let samples = number("samples_per_frame")?;
    let headers = number("header_frames")?;
    let delay = number("encoder_delay")?;
    let padding = number("end_padding")?;
    let encoded = count
        .checked_sub(headers)
        .and_then(|count| count.checked_mul(samples));
    let expected = encoded
        .and_then(|frames| frames.checked_sub(delay))
        .and_then(|frames| frames.checked_sub(padding));
    if number("version")? != MP3_METADATA_VERSION as u64
        || number("source_len")? != len
        || layout["container"] != "MP3"
        || metadata.attributes["container"] != "MP3"
        || metadata.encoding != "MP3"
        || metadata.bit_depth != 0
        || number("sample_rate")? != u64::from(metadata.sample_rate)
        || number("channels")? != u64::from(metadata.channels)
        || number("frames")? != metadata.frames
        || expected != Some(metadata.frames)
        || count <= headers
        || headers > 1
        || !matches!(samples, 576 | 1152)
        || (headers == 0 && (delay != 0 || padding != 0))
        || delay > 4095 + DECODER_DELAY
        || padding > 4095 - DECODER_DELAY
        || ((delay != 0 || padding != 0) && delay < DECODER_DELAY)
        || bytes < 4
        || count > bytes / 24
        || offset.checked_add(bytes).is_none_or(|end| end > len)
        || !metadata.duration.is_finite()
        || metadata.sample_rate == 0
        || (metadata.duration - metadata.frames as f64 / f64::from(metadata.sample_rate)).abs()
            > 1.0 / f64::from(metadata.sample_rate)
    {
        return Err(invalid("Invalid cached MP3 layout"));
    }
    let mut raw = [0; 4];
    read_at(source, offset, &mut raw)?;
    let first =
        header(&raw).ok_or_else(|| invalid("Cached MP3 layout does not point to MPEG audio"))?;
    if first.rate != metadata.sample_rate
        || first.channels != metadata.channels
        || first.samples != samples
        || first.size > bytes
    {
        return Err(invalid("Cached MP3 layout differs from actual MPEG audio"));
    }
    Ok(info(metadata, offset, bytes))
}

/// Build a trusted Xing frame from the verified scan layout, replacing optional
/// duration/padding claims before the MPEG demuxer sees them. The returned skip
/// covers only an original information frame, never an actual audio frame.
pub(crate) fn decoder_prefix(
    source: &mut (impl Read + Seek),
    info: &WavInfo,
) -> Result<(Vec<u8>, u64)> {
    let layout = &info.metadata.attributes["source_audio_layout"];
    let number = |key: &str| {
        layout[key]
            .as_u64()
            .ok_or_else(|| invalid("Invalid MP3 decoder layout"))
    };
    let count = number("mpeg_frame_count")?;
    let headers = number("header_frames")?;
    let delay = number("encoder_delay")?;
    let padding = number("end_padding")?;
    let samples = number("samples_per_frame")?;
    let source_len = number("source_len")?;
    let audio_frames = count
        .checked_sub(headers)
        .filter(|frames| *frames > 0)
        .ok_or_else(|| invalid("MP3 decoder layout contains no audio frames"))?;
    let frame_count =
        u32::try_from(audio_frames).map_err(|_| invalid("MP3 frame count exceeds Xing's range"))?;
    let presentation = audio_frames
        .checked_mul(samples)
        .and_then(|frames| frames.checked_sub(delay))
        .and_then(|frames| frames.checked_sub(padding));
    if number("version")? != MP3_METADATA_VERSION as u64
        || layout["container"] != "MP3"
        || info.metadata.attributes["container"] != "MP3"
        || info.metadata.encoding != "MP3"
        || info.metadata.bit_depth != 0
        || number("data_offset")? != info.data_offset
        || number("data_bytes")? != info.data_bytes
        || number("sample_rate")? != u64::from(info.metadata.sample_rate)
        || number("channels")? != u64::from(info.metadata.channels)
        || number("frames")? != info.metadata.frames
        || presentation != Some(info.metadata.frames)
        || headers > 1
        || !matches!(samples, 576 | 1152)
        || (headers == 0 && (delay != 0 || padding != 0))
        || delay > 4095 + DECODER_DELAY
        || padding > 4095 - DECODER_DELAY
        || ((delay != 0 || padding != 0) && delay < DECODER_DELAY)
        || info
            .data_offset
            .checked_add(info.data_bytes)
            .is_none_or(|end| end > source_len)
    {
        return Err(invalid("Invalid MP3 decoder layout"));
    }
    let mut original = [0; 4];
    read_at(source, info.data_offset, &mut original)?;
    let first = header(&original)
        .ok_or_else(|| invalid("MP3 decoder layout does not point to MPEG audio"))?;
    if first.rate != info.metadata.sample_rate
        || first.channels != info.metadata.channels
        || first.samples != samples
        || first.size > info.data_bytes
    {
        return Err(invalid("MP3 decoder layout differs from actual MPEG audio"));
    }
    // Header parsing bounds this allocation to the largest supported Layer III
    // frame (1,441 bytes). No subsequent frame or source tail is read here.
    let mut original_frame = vec![0; first.size as usize];
    read_at(source, info.data_offset, &mut original_frame)?;
    let actual_headers = u64::from(information_frame_kind(&original_frame, first).is_some());
    if actual_headers != headers {
        return Err(invalid(
            "MP3 decoder layout information-frame claim differs from actual audio",
        ));
    }
    let skip = if headers == 1 { first.size } else { 0 };
    if skip >= info.data_bytes {
        return Err(invalid(
            "MP3 decoder information frame consumes its audio payload",
        ));
    }
    // The maximum legal bitrate gives the synthetic frame room for all bounded
    // header fields. Preserve MPEG version/rate/channels; clear CRC and padding.
    let mut synthetic = original;
    synthetic[1] |= 1;
    synthetic[2] = (synthetic[2] & 0x0c) | 0xe0;
    let synthetic_header =
        header(&synthetic).ok_or_else(|| invalid("Invalid synthesized MPEG header"))?;
    let mut prefix = vec![0; synthetic_header.size as usize];
    prefix[..4].copy_from_slice(&synthetic);
    let at = 4 + synthetic_header.side_bytes;
    prefix[at..at + 4].copy_from_slice(b"Xing");
    prefix[at + 4..at + 8].copy_from_slice(&1_u32.to_be_bytes());
    prefix[at + 8..at + 12].copy_from_slice(&frame_count.to_be_bytes());
    if delay != 0 || padding != 0 {
        let raw_delay = delay
            .checked_sub(DECODER_DELAY)
            .filter(|value| *value <= 4095)
            .ok_or_else(|| invalid("MP3 decoder delay cannot be represented in its LAME tag"))?;
        let raw_padding = padding
            .checked_add(DECODER_DELAY)
            .filter(|value| *value <= 4095)
            .ok_or_else(|| invalid("MP3 decoder padding cannot be represented in its LAME tag"))?;
        let lame = at + 12;
        prefix[lame..lame + 9].copy_from_slice(b"LAME3.100");
        let packed = ((raw_delay << 12) | raw_padding) as u32;
        prefix[lame + 21..lame + 24].copy_from_slice(&packed.to_be_bytes()[1..]);
        let crc = tag_crc(&prefix[..lame + 34]);
        prefix[lame + 34..lame + 36].copy_from_slice(&crc.to_be_bytes());
    }
    Ok((prefix, skip))
}

#[cfg(test)]
#[path = "mp3_metadata_tests.rs"]
mod tests;
