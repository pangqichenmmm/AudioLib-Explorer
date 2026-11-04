// crates/audiolib-audio/src/wav.rs
use crate::wav_metadata::{
    Artwork, EmbeddedMetadata, MAX_CHUNK_BYTES, MAX_ID3_BYTES, MAX_METADATA_BYTES, MAX_SMED_BYTES,
    text,
};
use audiolib_core::{AudioMetadata, Error, Result};
use serde_json::{Map, Value, json};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Clone, Debug)]
pub(crate) struct WavInfo {
    pub metadata: AudioMetadata,
    pub data_offset: u64,
    pub data_bytes: u64,
    pub block_align: u16,
    pub format_tag: u16,
    pub artwork: Option<Artwork>,
}

/// Read RIFF/RF64 headers and public embedded Metadata without decoding audio samples.
pub fn probe(path: &Path) -> Result<AudioMetadata> {
    Ok(read_info_impl(path, true, false)?.metadata)
}

/// Probe Metadata and also return the WAV's own cover artwork, when it carries one.
///
/// Only this path extracts ID3 `APIC`/`PIC` or Soundminer `SMED` pictures. Ordinary
/// scans keep binary artwork out of Metadata and never base64-decode SMED pictures.
pub fn probe_with_artwork(path: &Path) -> Result<(AudioMetadata, Option<Artwork>)> {
    let info = read_info_impl(path, true, true)?;
    Ok((info.metadata, info.artwork))
}

/// Read a cover from an already opened and authorized local file handle.
///
/// Seeks to the start and leaves the position after the probe. Servers use this
/// after validating root containment so replacing the path cannot redirect reads.
pub fn probe_file_with_artwork(
    file: &mut std::fs::File,
) -> Result<(AudioMetadata, Option<Artwork>)> {
    let file_len = file.metadata()?.len();
    file.seek(SeekFrom::Start(0))?;
    let info = read_info_reader(file, file_len, true, true)?;
    Ok((info.metadata, info.artwork))
}

fn u16le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}
fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn u64le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

/// Playback layout only: stops at the `data` chunk once `fmt ` is known, so a network
/// source never fetches the file's tail (trailing metadata chunks) to start playing.
pub(crate) fn read_info(path: &Path) -> Result<WavInfo> {
    read_info_impl(path, false, false)
}

fn read_info_impl(path: &Path, include_metadata: bool, include_artwork: bool) -> Result<WavInfo> {
    let (file, file_len) = crate::source::open_source(path)?;
    read_info_reader(file, file_len, include_metadata, include_artwork)
}

fn read_info_reader(
    mut file: impl Read + Seek,
    file_len: u64,
    include_metadata: bool,
    include_artwork: bool,
) -> Result<WavInfo> {
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)?;
    if !matches!(&header[..4], b"RIFF" | b"RF64" | b"BW64") || &header[8..] != b"WAVE" {
        return Err(Error::Audio(
            "Expected little-endian RIFF/RF64/BW64 WAV".into(),
        ));
    }
    let rf64 = &header[..4] != b"RIFF";
    let mut attrs = Map::new();
    let mut embedded = EmbeddedMetadata::default();
    let mut metadata_bytes = 0_u64;
    let mut sample_rate = 0;
    let mut channels = 0;
    let mut bits = 0;
    let mut block_align = 0;
    let mut format_tag = 0;
    let mut data_offset = 0;
    let mut data_bytes = 0;
    let mut rf64_data_size = None;
    let mut fact_frames = None;
    let mut position = 12_u64;
    while position.checked_add(8).is_some_and(|end| end <= file_len) {
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)?;
        let raw_size = u32le(&chunk[4..]);
        let payload = position + 8;
        let size = if &chunk[..4] == b"data" && raw_size == u32::MAX && rf64 {
            rf64_data_size
                .ok_or_else(|| Error::Audio("RF64 data requires a preceding ds64 chunk".into()))?
        } else {
            u64::from(raw_size)
        };
        let end = payload
            .checked_add(size)
            .ok_or_else(|| Error::Audio("WAV chunk size overflow".into()))?;
        if end > file_len {
            return Err(Error::Audio(format!(
                "Truncated WAV chunk {:?}",
                &chunk[..4]
            )));
        }
        if include_metadata {
            embedded.chunk(&chunk[..4], size);
        }
        let metadata_chunk = matches!(
            &chunk[..4],
            b"bext"
                | b"iXML"
                | b"axml"
                | b"aXML"
                | b"_PMX"
                | b"XMP "
                | b"LIST"
                | b"id3 "
                | b"ID3 "
                | b"SMED"
        );
        let chunk_limit = if &chunk[..4] == b"SMED" {
            MAX_SMED_BYTES
        } else if matches!(&chunk[..4], b"id3 " | b"ID3 ") {
            MAX_ID3_BYTES
        } else {
            MAX_CHUNK_BYTES
        };
        let within_budget = !metadata_chunk
            || (include_metadata
                && size <= chunk_limit
                && metadata_bytes + size <= MAX_METADATA_BYTES);
        if metadata_chunk && within_budget {
            metadata_bytes += size;
        }
        if include_metadata && !within_budget {
            embedded.warning(format!(
                "{}: Metadata size limit exceeded",
                String::from_utf8_lossy(&chunk[..4])
            ));
        }
        match &chunk[..4] {
            b"ds64" => {
                if size < 28 {
                    return Err(Error::Audio("Invalid ds64 chunk".into()));
                }
                let mut data = [0_u8; 28];
                file.read_exact(&mut data)?;
                rf64_data_size = Some(u64le(&data[8..16]));
            }
            b"fmt " => {
                if !(16..=65536).contains(&size) {
                    return Err(Error::Audio("Invalid WAV fmt chunk".into()));
                }
                let mut data = vec![0_u8; size as usize];
                file.read_exact(&mut data)?;
                format_tag = u16le(&data);
                channels = u16le(&data[2..]);
                sample_rate = u32le(&data[4..]);
                block_align = u16le(&data[12..]);
                bits = u16le(&data[14..]);
                if format_tag == 0xfffe {
                    if data.len() < 40 {
                        return Err(Error::Audio("Truncated WAV extensible fmt".into()));
                    }
                    attrs.insert("valid_bits_per_sample".into(), json!(u16le(&data[18..])));
                    attrs.insert("channel_mask".into(), json!(u32le(&data[20..])));
                    format_tag = u16le(&data[24..]);
                }
            }
            b"data" => {
                data_offset = payload;
                data_bytes = size;
            }
            b"fact" if size >= 4 => {
                let mut data = [0_u8; 4];
                file.read_exact(&mut data)?;
                fact_frames = Some(u64::from(u32le(&data)));
            }
            b"bext" if within_budget && size >= 602 => {
                let mut data = vec![0_u8; size as usize];
                file.read_exact(&mut data)?;
                for (key, range) in [
                    ("bwf_description", 0..256),
                    ("bwf_originator", 256..288),
                    ("bwf_originator_reference", 288..320),
                    ("bwf_origination_date", 320..330),
                    ("bwf_origination_time", 330..338),
                    ("bwf_coding_history", 602..data.len()),
                ] {
                    attrs.insert(key.into(), json!(text(&data[range])));
                }
                attrs.insert("bwf_time_reference".into(), json!(u64le(&data[338..346])));
                attrs.insert("bwf_version".into(), json!(u16le(&data[346..348])));
                attrs.insert(
                    "bwf_umid".into(),
                    json!(
                        data[348..412]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>()
                    ),
                );
            }
            b"id3 " | b"ID3 " if within_budget => {
                embedded.read_id3(&mut file, size as usize)?;
                if include_artwork {
                    // compact_tag() seeks past binary frames, so the cover needs its own
                    // bounded pass over the same chunk.
                    file.seek(SeekFrom::Start(payload))?;
                    embedded.read_id3_artwork(&mut file, size as usize)?;
                }
            }
            b"iXML" | b"axml" | b"aXML" | b"_PMX" | b"XMP " | b"LIST" | b"SMED"
                if within_budget =>
            {
                let mut data = vec![0_u8; size as usize];
                file.read_exact(&mut data)?;
                match &chunk[..4] {
                    b"LIST" => embedded.info(&data),
                    b"iXML" => embedded.xml("ixml", &data, &mut attrs),
                    b"axml" | b"aXML" => embedded.xml("axml", &data, &mut attrs),
                    b"_PMX" | b"XMP " => embedded.xml("xmp", &data, &mut attrs),
                    b"SMED" => embedded.smed(&data, &mut attrs, include_artwork),
                    _ => embedded.id3(&data),
                }
            }
            b"bext" if within_budget => embedded.warning("bext: chunk shorter than 602 bytes"),
            _ => {}
        }
        position = end
            .checked_add(size & 1)
            .ok_or_else(|| Error::Audio("Chunk offset overflow".into()))?;
        if !include_metadata && data_offset != 0 && block_align != 0 {
            // Playback needs nothing after the audio; Metadata (`probe`) walks on.
            break;
        }
    }
    if channels == 0 || channels > 256 || sample_rate == 0 || block_align == 0 || data_offset == 0 {
        return Err(Error::Audio("WAV is missing valid fmt/data chunks".into()));
    }
    if matches!(format_tag, 1 | 3)
        && (bits == 0
            || !bits.is_multiple_of(8)
            || u32::from(block_align) < u32::from(channels) * u32::from(bits / 8))
    {
        return Err(Error::Audio("Invalid PCM frame layout".into()));
    }
    let frames = if matches!(format_tag, 1 | 3) {
        data_bytes / u64::from(block_align)
    } else {
        fact_frames
            .ok_or_else(|| Error::Audio("Compressed WAV requires a fact sample count".into()))?
    };
    attrs.insert("format_tag".into(), json!(format_tag));
    attrs.insert(
        "container".into(),
        json!(if rf64 { "RF64" } else { "RIFF" }),
    );
    // finish() consumes the collector, so take the cover first.
    let artwork = embedded.take_artwork();
    if include_metadata {
        embedded.finish(&mut attrs);
    }
    Ok(WavInfo {
        metadata: AudioMetadata {
            sample_rate,
            channels,
            bit_depth: bits,
            frames,
            duration: frames as f64 / f64::from(sample_rate),
            encoding: match format_tag {
                1 => "PCM",
                3 => "IEEE_FLOAT",
                _ => "WAV_COMPRESSED",
            }
            .into(),
            bext_description: attrs
                .get("bwf_description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            attributes: Value::Object(attrs),
        },
        data_offset,
        data_bytes,
        block_align,
        format_tag,
        artwork,
    })
}
