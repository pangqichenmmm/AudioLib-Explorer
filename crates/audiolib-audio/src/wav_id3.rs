// crates/audiolib-audio/src/wav_id3.rs
//! Bounded ID3v2 text/COMM/TXXX reader; binary and compressed frames are skipped.
use super::{ArtworkPriority, EmbeddedMetadata, MAX_FIELDS, append};
use encoding_rs::{UTF_16BE, UTF_16LE};
use serde_json::Map;
use std::io::{Read, Seek, SeekFrom};

fn syncsafe(bytes: &[u8]) -> Option<usize> {
    bytes.iter().try_fold(0, |n, b| {
        if *b < 128 {
            Some((n << 7) | usize::from(*b))
        } else {
            None
        }
    })
}
fn big_endian(bytes: &[u8]) -> usize {
    bytes.iter().fold(0, |n, b| (n << 8) | usize::from(*b))
}
fn unsync(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            if *b == 0 && i > 0 && bytes[i - 1] == 255 {
                None
            } else {
                Some(*b)
            }
        })
        .collect()
}
fn decode(encoding: u8, bytes: &[u8]) -> Option<String> {
    Some(match encoding {
        0 => bytes.iter().map(|b| char::from(*b)).collect(),
        1 => {
            if bytes.starts_with(&[0xfe, 0xff]) {
                UTF_16BE.decode(bytes).0.into_owned()
            } else {
                UTF_16LE.decode(bytes).0.into_owned()
            }
        }
        2 => UTF_16BE.decode_without_bom_handling(bytes).0.into_owned(),
        3 => String::from_utf8_lossy(bytes).into_owned(),
        _ => return None,
    })
}

impl EmbeddedMetadata {
    /// Skip binary frame bodies on disk for ordinary tags. Complex or malformed
    /// layouts use the original bounded decoder, retaining its compatibility/warnings.
    pub(crate) fn read_id3(
        &mut self,
        reader: &mut (impl Read + Seek),
        size: usize,
    ) -> std::io::Result<()> {
        let start = reader.stream_position()?;
        let compact = compact_tag(reader, size)?;
        let bytes = if let Some(bytes) = compact {
            bytes
        } else {
            if size > super::MAX_CHUNK_BYTES as usize {
                self.warning("ID3: complex tag exceeds Metadata size limit");
                return Ok(());
            }
            reader.seek(SeekFrom::Start(start))?;
            let mut bytes = vec![0; size];
            reader.read_exact(&mut bytes)?;
            bytes
        };
        self.id3(&bytes);
        Ok(())
    }

    pub(crate) fn id3(&mut self, bytes: &[u8]) {
        if let Err(message) = self.id3_inner(bytes) {
            self.warning(format!("ID3: {message}"));
        }
    }

    fn id3_inner(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.len() < 10 || &bytes[..3] != b"ID3" || !(2..=4).contains(&bytes[3]) {
            return Err("unsupported or invalid header");
        }
        let version = bytes[3];
        let flags = bytes[5];
        if version == 2 && flags & 0x40 != 0 {
            return Err("compressed tag is not supported");
        }
        let size = syncsafe(&bytes[6..10]).ok_or("invalid tag size")?;
        let payload = bytes.get(10..10 + size).ok_or("truncated tag")?;
        let decoded;
        let mut rest = if version < 4 && flags & 0x80 != 0 {
            decoded = unsync(payload);
            &decoded[..]
        } else {
            payload
        };
        if version >= 3 && flags & 0x40 != 0 {
            let header = rest.get(..4).ok_or("truncated extended header")?;
            let size = if version == 4 {
                syncsafe(header).ok_or("invalid extended header")?
            } else {
                big_endian(header) + 4
            };
            if size < 6 {
                return Err("invalid extended header size");
            }
            rest = rest.get(size..).ok_or("truncated extended header")?;
        }
        let mut fields = Map::new();
        let header_size = if version == 2 { 6 } else { 10 };
        let id_size = if version == 2 { 3 } else { 4 };
        for _ in 0..MAX_FIELDS {
            if rest.is_empty() || rest[0] == 0 {
                break;
            }
            let Some(header) = rest.get(..header_size) else {
                self.warning("ID3: truncated frame header");
                break;
            };
            if !header[..id_size]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                self.warning("ID3: invalid frame identifier");
                break;
            }
            let id = std::str::from_utf8(&header[..id_size]).unwrap();
            let frame_size = if version == 4 {
                let Some(size) = syncsafe(&header[4..8]) else {
                    self.warning("ID3: invalid frame size");
                    break;
                };
                size
            } else {
                big_endian(&header[id_size..id_size + if version == 2 { 3 } else { 4 }])
            };
            let Some(content) = rest.get(header_size..header_size + frame_size) else {
                self.warning("ID3: truncated frame");
                break;
            };
            rest = &rest[header_size + frame_size..];
            if !id.starts_with('T') && !matches!(id, "COMM" | "COM") {
                continue;
            }
            let frame_flags = if version == 2 { 0 } else { header[9] };
            if (version == 3 && frame_flags & 0xc0 != 0)
                || (version == 4 && frame_flags & 0x0c != 0)
            {
                self.warning(format!(
                    "ID3 {id}: compressed/encrypted text is not supported"
                ));
                continue;
            }
            let decoded;
            let content = if version == 4 && (flags & 0x80 != 0 || frame_flags & 2 != 0) {
                decoded = unsync(content);
                &decoded[..]
            } else {
                content
            };
            let skip = usize::from(
                (version == 3 && frame_flags & 0x20 != 0)
                    || (version == 4 && frame_flags & 0x40 != 0),
            ) + if version == 4 && frame_flags & 1 != 0 {
                4
            } else {
                0
            };
            let Some(content) = content.get(skip..) else {
                self.warning("ID3: truncated frame flags");
                continue;
            };
            let Some((&encoding, text_bytes)) = content.split_first() else {
                continue;
            };
            let text_bytes = if matches!(id, "COMM" | "COM") {
                let Some(text) = text_bytes.get(3..) else {
                    self.warning("ID3: truncated comment language");
                    continue;
                };
                text
            } else {
                text_bytes
            };
            let Some(text) = decode(encoding, text_bytes) else {
                self.warning(format!("ID3 {id}: unsupported text encoding"));
                continue;
            };
            if matches!(id, "TXXX" | "TXX" | "COMM" | "COM") {
                let (description, value) = text.split_once('\0').unwrap_or(("", &text));
                let key = if description.is_empty() {
                    id.to_string()
                } else {
                    format!("{id}/{description}")
                };
                append(
                    &mut fields,
                    &key,
                    &value.trim_matches(['\0', '\u{feff}']).replace('\0', "; "),
                );
            } else {
                append(
                    &mut fields,
                    id,
                    &text.trim_matches('\0').replace('\0', "; "),
                );
            }
        }
        self.merge("id3", fields);
        Ok(())
    }
}

// Only called after WAV chunk/budget validation. All offsets stay within this tag.
fn compact_tag(reader: &mut (impl Read + Seek), size: usize) -> std::io::Result<Option<Vec<u8>>> {
    if size < 10 {
        return Ok(None);
    }
    let mut header = [0; 10];
    reader.read_exact(&mut header)?;
    if &header[..3] != b"ID3" || !(2..=4).contains(&header[3]) || header[5] != 0 {
        return Ok(None);
    }
    let Some(tag_size) = syncsafe(&header[6..10]).filter(|n| *n <= size - 10) else {
        return Ok(None);
    };
    let version = header[3];
    let id_size = if version == 2 { 3 } else { 4 };
    let header_size = if version == 2 { 6 } else { 10 };
    let mut remaining = tag_size;
    let mut bytes = header.to_vec();
    for _ in 0..MAX_FIELDS {
        if remaining == 0 {
            break;
        }
        if remaining < header_size {
            return Ok(None);
        }
        reader.read_exact(&mut header[..header_size])?;
        if header[0] == 0 {
            break;
        }
        if !header[..id_size]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Ok(None);
        }
        let frame_size = if version == 4 {
            let Some(size) = syncsafe(&header[4..8]) else {
                return Ok(None);
            };
            size
        } else {
            big_endian(&header[id_size..header_size - if version == 2 { 0 } else { 2 }])
        };
        remaining -= header_size;
        if frame_size > remaining {
            return Ok(None);
        }
        if header[0] == b'T' || matches!(&header[..id_size], b"COMM" | b"COM") {
            if bytes.len() + header_size + frame_size > super::MAX_CHUNK_BYTES as usize {
                return Ok(None);
            }
            bytes.extend_from_slice(&header[..header_size]);
            let offset = bytes.len();
            bytes.resize(offset + frame_size, 0);
            reader.read_exact(&mut bytes[offset..])?;
        } else {
            reader.seek(SeekFrom::Current(frame_size as i64))?;
        }
        remaining -= frame_size;
    }
    let size = bytes.len() - 10;
    for (index, byte) in bytes[6..10].iter_mut().enumerate() {
        *byte = ((size >> (7 * (3 - index))) & 0x7f) as u8;
    }
    Ok(Some(bytes))
}

impl EmbeddedMetadata {
    /// Read only the ID3 `APIC` cover. Ordinary tag reads skip binary frames, so artwork
    /// needs this targeted pass; callers request it for one file per album, not every file.
    pub(crate) fn read_id3_artwork(
        &mut self,
        reader: &mut (impl Read + Seek),
        size: usize,
    ) -> std::io::Result<()> {
        if !self.accepts_artwork(ArtworkPriority::Id3Front)
            || size < 10
            || size > super::MAX_ID3_BYTES as usize
        {
            return Ok(());
        }
        let mut bytes = vec![0u8; size];
        reader.read_exact(&mut bytes)?;
        self.id3_artwork(&bytes);
        Ok(())
    }

    fn id3_artwork(&mut self, bytes: &[u8]) {
        if bytes.len() < 10 || &bytes[..3] != b"ID3" || !(2..=4).contains(&bytes[3]) {
            return;
        }
        let version = bytes[3];
        let flags = bytes[5];
        if version == 2 && flags & 0x40 != 0 {
            return;
        }
        let Some(size) = syncsafe(&bytes[6..10]) else {
            return;
        };
        let Some(payload) = bytes.get(10..10 + size) else {
            return;
        };
        let decoded;
        let mut rest = if version < 4 && flags & 0x80 != 0 {
            decoded = unsync(payload);
            &decoded[..]
        } else {
            payload
        };
        if version >= 3 && flags & 0x40 != 0 {
            let Some(header) = rest.get(..4) else {
                return;
            };
            let size = if version == 4 {
                let Some(size) = syncsafe(header) else {
                    return;
                };
                size
            } else {
                big_endian(header) + 4
            };
            let Some(payload) = rest.get(size..).filter(|_| size >= 6) else {
                return;
            };
            rest = payload;
        }
        let header_size = if version == 2 { 6 } else { 10 };
        let id_size = if version == 2 { 3 } else { 4 };
        for _ in 0..MAX_FIELDS {
            if rest.is_empty() || rest[0] == 0 {
                break;
            }
            let Some(header) = rest.get(..header_size) else {
                break;
            };
            if !header[..id_size]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                break;
            }
            let frame_size = if version == 4 {
                let Some(size) = syncsafe(&header[4..8]) else {
                    break;
                };
                size
            } else {
                big_endian(&header[id_size..id_size + if version == 2 { 3 } else { 4 }])
            };
            let Some(content) = rest.get(header_size..header_size + frame_size) else {
                break;
            };
            rest = &rest[header_size + frame_size..];
            if &header[..id_size] == b"APIC" || &header[..id_size] == b"PIC" {
                let frame_flags = if version == 2 { 0 } else { header[9] };
                if (version == 3 && frame_flags & 0xc0 != 0)
                    || (version == 4 && frame_flags & 0x0c != 0)
                {
                    continue;
                }
                let decoded;
                let content = if version == 4 && (flags & 0x80 != 0 || frame_flags & 2 != 0) {
                    decoded = unsync(content);
                    &decoded[..]
                } else {
                    content
                };
                let skip = usize::from(
                    (version == 3 && frame_flags & 0x20 != 0)
                        || (version == 4 && frame_flags & 0x40 != 0),
                ) + if version == 4 && frame_flags & 1 != 0 {
                    4
                } else {
                    0
                };
                let Some(content) = content.get(skip..) else {
                    continue;
                };
                if let Some((picture_type, image)) = apic_parts(version, content)
                    && image.len() <= super::MAX_ARTWORK_BYTES
                {
                    let priority = if picture_type == 3 {
                        ArtworkPriority::Id3Front
                    } else {
                        ArtworkPriority::Id3
                    };
                    if self.accepts_artwork(priority) {
                        self.set_artwork(image.to_vec(), priority);
                    }
                }
                if !self.accepts_artwork(ArtworkPriority::Id3Front) {
                    return;
                }
            }
        }
    }
}

/// Split APIC/PIC into its picture type and bytes; image magic supplies the MIME.
fn apic_parts(version: u8, content: &[u8]) -> Option<(u8, &[u8])> {
    let (&encoding, body) = content.split_first()?;
    if encoding > 3 {
        return None;
    }
    let mut rest = body;
    if version == 2 {
        rest = rest.get(3..)?;
    } else {
        let end = rest.iter().position(|b| *b == 0)?;
        rest = rest.get(end + 1..)?;
    }
    let (&picture_type, body) = rest.split_first()?;
    rest = body;
    let utf16 = matches!(encoding, 1 | 2);
    let description_end = if utf16 {
        let mut index = 0;
        loop {
            let pair = rest.get(index..index + 2)?;
            if pair.iter().all(|byte| *byte == 0) {
                break index;
            }
            index += 2;
        }
    } else {
        rest.iter().position(|b| *b == 0)?
    };
    let image = rest.get(description_end + if utf16 { 2 } else { 1 }..)?;
    Some((picture_type, image))
}
