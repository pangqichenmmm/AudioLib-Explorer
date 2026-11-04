// crates/audiolib-db/src/metadata_storage.rs
//! Plain Metadata writes and strict, bounded decoding of experimental legacy BLOBs.
use audiolib_core::{Error, Result};
use flate2::{Decompress, FlushDecompress, Status};
use rusqlite::types::{Value, ValueRef};

const MAGIC: &[u8; 8] = b"ALEMD\x01Z\0";
const HEADER_BYTES: usize = MAGIC.len() + 8;
/// Compressed records have a bounded decode budget. Larger legacy text stays text.
pub(crate) const MAX_DECODE_BYTES: usize = 64 * 1024 * 1024;

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Database(format!("Invalid Metadata storage: {message}"))
}

/// Store the exact UTF-8 bytes as TEXT. The retired experimental policy argument
/// remains accepted for owned legacy-copy adapters, but never enables compression.
pub(crate) fn encode(raw: &str, _enabled: bool) -> Result<Value> {
    Ok(Value::Text(raw.to_owned()))
}

/// Construct an explicit historical fixture; production code cannot encode BLOBs.
#[cfg(test)]
pub(crate) fn encode_legacy_fixture(raw: &str) -> Result<Value> {
    Ok(Value::Blob(compressed_fixture_bytes(raw.as_bytes())?))
}

#[cfg(test)]
fn compressed_fixture_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(raw).map_err(invalid)?;
    let compressed = encoder.finish().map_err(invalid)?;
    let mut bytes = Vec::with_capacity(HEADER_BYTES + compressed.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&compressed);
    Ok(bytes)
}

/// Read raw text or the explicitly versioned compressed representation. Unknown
/// SQLite types, invalid UTF-8 and malformed streams are errors, never empty Metadata.
pub(crate) fn decode_value(value: ValueRef<'_>) -> Result<String> {
    match value {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(invalid),
        ValueRef::Blob(bytes) => decode_blob(bytes),
        _ => Err(invalid("expected JSON text or a versioned compressed blob")),
    }
}

/// Decode one complete record with a bounded streaming output and exact length /
/// checksum validation. The untrusted header never determines an allocation size.
pub(crate) fn decode_blob(bytes: &[u8]) -> Result<String> {
    let expected = decoded_blob_length(bytes)?;
    let payload = &bytes[HEADER_BYTES..];
    let mut decoder = Decompress::new(true);
    let mut decoded = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let offset = usize::try_from(before_in).map_err(invalid)?;
        let status = decoder
            .decompress(&payload[offset..], &mut chunk, FlushDecompress::None)
            .map_err(invalid)?;
        if decoder.total_out() > expected {
            return Err(invalid("decompressed Metadata exceeds its declared length"));
        }
        let produced = usize::try_from(decoder.total_out() - before_out).map_err(invalid)?;
        decoded.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            if decoder.total_in() != payload.len() as u64 {
                return Err(invalid("trailing data after compressed Metadata"));
            }
            if decoder.total_out() != expected {
                return Err(invalid(
                    "decompressed Metadata length differs from its header",
                ));
            }
            break;
        }
        if decoder.total_in() == before_in && decoder.total_out() == before_out {
            return Err(invalid("truncated compressed Metadata stream"));
        }
    }
    String::from_utf8(decoded).map_err(invalid)
}

/// Read the bounded original length from an experimental header (at least 16
/// bytes). Space estimates may use this without reading the payload; only full
/// decoding validates stream completion, checksum and UTF-8.
pub(crate) fn decoded_blob_length(header: &[u8]) -> Result<u64> {
    if header.len() < HEADER_BYTES || !header.starts_with(MAGIC) {
        return Err(invalid("unknown or truncated codec header"));
    }
    let expected = u64::from_le_bytes(
        header[MAGIC.len()..HEADER_BYTES]
            .try_into()
            .map_err(invalid)?,
    );
    if expected > MAX_DECODE_BYTES as u64 {
        return Err(invalid("decompressed Metadata exceeds the record budget"));
    }
    Ok(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> String {
        format!(
            " {{\n  \"unknown\": \"{}\", \"number\": 1.00 }}\n",
            "音效🦉 <XML>  ".repeat(400)
        )
    }

    fn compressed_bytes(raw: &[u8]) -> Vec<u8> {
        compressed_fixture_bytes(raw).unwrap()
    }

    #[test]
    fn legacy_decoder_retains_unknown_fields_unicode_and_exact_json_bytes() {
        let text = raw();
        let Value::Blob(encoded) = encode_legacy_fixture(&text).unwrap() else {
            panic!("legacy fixture must contain a BLOB");
        };
        assert!(encoded.len() < text.len() / 2);
        assert_eq!(
            decode_value(ValueRef::Blob(&encoded)).unwrap().as_bytes(),
            text.as_bytes()
        );
    }

    #[test]
    fn policy_and_small_values_remain_plain_text_without_json_normalization() {
        let text = raw();
        assert_eq!(encode(&text, false).unwrap(), Value::Text(text.clone()));
        assert_eq!(encode(&text, true).unwrap(), Value::Text(text));
        for text in ["{}", " \n{\"extra\": 1.00}\n", "not-json"] {
            assert_eq!(encode(text, true).unwrap(), Value::Text(text.into()));
            assert_eq!(decode_value(ValueRef::Text(text.as_bytes())).unwrap(), text);
        }
    }

    #[test]
    fn ineffective_compression_preserves_original_text() {
        let mut state = 0x6e62_7365_u32;
        let text: String = (0..256)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                char::from(b'!' + ((state >> 16) % 94) as u8)
            })
            .collect();
        assert_eq!(encode(&text, true).unwrap(), Value::Text(text));
    }

    #[test]
    fn corrupted_truncated_unknown_and_trailing_streams_fail_explicitly() {
        let valid = compressed_bytes(raw().as_bytes());
        for length in [0, MAGIC.len(), HEADER_BYTES, valid.len() - 1] {
            assert!(decode_blob(&valid[..length]).is_err());
        }
        let mut unknown = valid.clone();
        unknown[5] = 2;
        assert!(decode_blob(&unknown).is_err());
        let mut corrupt = valid.clone();
        *corrupt.last_mut().unwrap() ^= 0x80;
        assert!(decode_blob(&corrupt).is_err());
        let mut trailing = valid;
        trailing.extend_from_slice(b"another stream or garbage");
        assert!(decode_blob(&trailing).is_err());
    }

    #[test]
    fn malicious_lengths_and_non_utf8_cannot_escape_the_record_contract() {
        let mut bytes = compressed_bytes(b"small");
        bytes[MAGIC.len()..HEADER_BYTES].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_blob(&bytes).is_err());
        bytes[MAGIC.len()..HEADER_BYTES].copy_from_slice(&0_u64.to_le_bytes());
        assert!(decode_blob(&bytes).is_err());
        assert!(decode_blob(&compressed_bytes(&[0xff; 512])).is_err());
        assert!(decode_value(ValueRef::Text(&[0xff])).is_err());
        assert!(decode_value(ValueRef::Null).is_err());
        assert!(decode_value(ValueRef::Integer(1)).is_err());
    }

    #[test]
    fn header_length_estimates_are_bounded_and_do_not_replace_stream_validation() {
        let text = raw();
        let bytes = compressed_bytes(text.as_bytes());
        assert_eq!(
            decoded_blob_length(&bytes[..HEADER_BYTES]).unwrap(),
            text.len() as u64
        );
        assert!(decode_blob(&bytes[..HEADER_BYTES]).is_err());
        for length in [0, MAGIC.len(), HEADER_BYTES - 1] {
            assert!(decoded_blob_length(&bytes[..length]).is_err());
        }
        let mut future = bytes[..HEADER_BYTES].to_vec();
        future[5] += 1;
        assert!(decoded_blob_length(&future).is_err());
        future = bytes[..HEADER_BYTES].to_vec();
        future[MAGIC.len()..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decoded_blob_length(&future).is_err());
    }
}
