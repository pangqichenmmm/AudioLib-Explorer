// crates/audiolib-core/src/list_waveform.rs
//! Owned contracts for list thumbnail Waveforms (doc/api/README.md §9.1–§9.3).
//!
//! A list Waveform is a mono summary of a whole file in at most
//! [`LIST_WAVEFORM_COLUMNS`] columns: column `i` holds the union of every channel's
//! `min`/`max` envelope, quantized to int16 (`round(clamp(v, -1, 1) * 32767)`). The
//! same values are stored locally in `waveform_cache` (separate little-endian `min` and
//! `max` arrays) and sent by a server as interleaved little-endian pairs.
//! The computation itself lives in `audiolib-audio` (`list_waveform_with_cancel`).
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Columns of a list Waveform (the only `buckets` value of `POST /waveforms`).
pub const LIST_WAVEFORM_COLUMNS: u32 = 128;
/// List encoding version (`-l1` of the server entity tag and of the local zoom level).
pub const LIST_WAVEFORM_ENCODING_VERSION: u32 = 1;
/// Most ids a provider is asked about in one [`ListWaveformBatch`] request
/// (the server's `POST /waveforms` limit and the UI page size).
pub const MAX_LIST_WAVEFORM_IDS: usize = 200;

/// One list Waveform: `values` interleaves `[min₀, max₀, min₁, max₁, …]` for
/// `columns()` ≤ [`LIST_WAVEFORM_COLUMNS`] columns. Zero columns is a 0-frame file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWaveform {
    /// Source frames per column (`ceil(frames / 128)`, at least 1).
    pub samples_per_pixel: u32,
    pub values: Vec<i16>,
}

impl ListWaveform {
    /// Validate the column count and pairing.
    pub fn new(samples_per_pixel: u32, values: Vec<i16>) -> Result<Self> {
        if !values.len().is_multiple_of(2) {
            return Err(Error::Validation(
                "List Waveform values must be min/max pairs".into(),
            ));
        }
        if values.len() / 2 > LIST_WAVEFORM_COLUMNS as usize {
            return Err(Error::Validation(format!(
                "List Waveform has more than {LIST_WAVEFORM_COLUMNS} columns"
            )));
        }
        Ok(Self {
            samples_per_pixel: samples_per_pixel.max(1),
            values,
        })
    }

    /// Number of columns `n` (0 for an empty file).
    pub fn columns(&self) -> usize {
        self.values.len() / 2
    }

    /// `(min, max)` of column `index`.
    pub fn column(&self, index: usize) -> Option<(i16, i16)> {
        Some((
            *self.values.get(index * 2)?,
            *self.values.get(index * 2 + 1)?,
        ))
    }

    /// Server wire bytes (§9.1 `peaks` before base64): interleaved int16 little endian,
    /// `n × 4` bytes.
    pub fn to_interleaved_le(&self) -> Vec<u8> {
        self.values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// Parse §9.1 `peaks` bytes (after base64 decoding).
    pub fn from_interleaved_le(samples_per_pixel: u32, bytes: &[u8]) -> Result<Self> {
        if !bytes.len().is_multiple_of(4) {
            return Err(Error::Validation(
                "List Waveform peaks must be a multiple of 4 bytes".into(),
            ));
        }
        Self::new(samples_per_pixel, int16_le(bytes))
    }

    /// Local `waveform_cache.min_values`: the `n` column minima, int16 little endian.
    pub fn min_values_le(&self) -> Vec<u8> {
        self.values
            .iter()
            .step_by(2)
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// Local `waveform_cache.max_values`: the `n` column maxima, int16 little endian.
    pub fn max_values_le(&self) -> Vec<u8> {
        self.values
            .iter()
            .skip(1)
            .step_by(2)
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// Rebuild from local `min_values` / `max_values` blobs (each `n` int16 LE).
    pub fn from_min_max_le(samples_per_pixel: u32, min: &[u8], max: &[u8]) -> Result<Self> {
        if min.len() != max.len() || !min.len().is_multiple_of(2) {
            return Err(Error::Validation(
                "List Waveform min/max blobs differ in length or are not int16".into(),
            ));
        }
        let values = int16_le(min)
            .into_iter()
            .zip(int16_le(max))
            .flat_map(|(low, high)| [low, high])
            .collect();
        Self::new(samples_per_pixel, values)
    }

    /// Parse a `POST /waveforms` item (§9.1): `peaks` is standard base64 (RFC 4648, with
    /// padding) of `n` interleaved int16 LE min/max pairs. The server does not send
    /// `samples_per_pixel`, so it is reported as 1.
    pub fn from_base64_peaks(peaks: &str, columns: usize) -> Result<Self> {
        let bytes = decode_base64(peaks)?;
        if bytes.len() != columns.saturating_mul(4) {
            return Err(Error::Validation(format!(
                "List Waveform peaks hold {} bytes, expected {} for n = {columns}",
                bytes.len(),
                columns.saturating_mul(4)
            )));
        }
        Self::from_interleaved_le(1, &bytes)
    }
}

/// Strict standard base64 (RFC 4648 alphabet, `=` padding required, no whitespace).
pub fn decode_base64(text: &str) -> Result<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let invalid = || Error::Validation("Invalid base64 in list Waveform peaks".into());
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(invalid());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return Err(invalid());
        }
        let mut word = 0_u32;
        for byte in &chunk[..4 - padding] {
            word = (word << 6) | value(*byte).ok_or_else(invalid)?;
        }
        word <<= 6 * padding as u32;
        let decoded = [(word >> 16) as u8, (word >> 8) as u8, word as u8];
        // Non-canonical encodings (bits set in the padding) are rejected.
        if decoded[3 - padding..].iter().any(|byte| *byte != 0) {
            return Err(invalid());
        }
        out.extend_from_slice(&decoded[..3 - padding]);
    }
    Ok(out)
}

fn int16_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Where list Waveforms of a provider come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListWaveformSource {
    /// Not offered (an old server, or a read-only snapshot whose `waveform_cache` rows
    /// must not be used, §9.3). The list hides its Waveform column.
    #[default]
    Unavailable,
    /// The local library: lookups read `waveform_cache`; `Pending` items are computed
    /// by the client itself with [`crate::DataProvider::compute_list_waveform`].
    Local,
    /// A server (`POST /waveforms`): it computes `Pending` items in the background;
    /// the client only retries lookups of still-visible ids after `retry_after`.
    Server,
}

/// State of one record's list Waveform (the `status` values of §9.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ListWaveformState {
    /// Current for the file version on disk. A server provider resolves its
    /// `not_modified` replies into this from its own client cache.
    Ready { waveform: ListWaveform },
    /// Not available yet (never computed, or the file changed since). `previous` is an
    /// older copy a provider may still offer for display (§9.3: server mode keeps
    /// showing it); the local library never returns one.
    Pending { previous: Option<ListWaveform> },
    /// No such record, a missing record or no file at its path.
    Missing,
    /// The record's root is offline (server mode); `previous` as for `Pending`.
    RootOffline {
        root: String,
        previous: Option<ListWaveform>,
    },
    /// The file cannot be decoded; not retried while the file version is unchanged.
    Failed { message: String },
}

/// One answered id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWaveformItem {
    pub id: i64,
    pub state: ListWaveformState,
}

/// Lookup reply for up to [`MAX_LIST_WAVEFORM_IDS`] ids, in request order without
/// duplicates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWaveformBatch {
    pub items: Vec<ListWaveformItem>,
    /// Suggested wait before asking again about still-visible `Pending` ids (server mode).
    pub retry_after: Option<Duration>,
    /// Nothing was answered and every id should be asked again after this wait (a busy
    /// server, `503 BUSY` with `Retry-After`). `items` is then empty.
    #[serde(default)]
    pub deferred: Option<Duration>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_blobs_and_wire_bytes_round_trip() {
        let waveform = ListWaveform::new(8, vec![-16384, 24575, -32767, 32767, 0, 1]).unwrap();
        assert_eq!(waveform.columns(), 3);
        assert_eq!(waveform.column(1), Some((-32767, 32767)));
        assert_eq!(waveform.column(3), None);
        assert_eq!(waveform.min_values_le(), [0x00, 0xc0, 0x01, 0x80, 0, 0]);
        assert_eq!(waveform.max_values_le(), [0xff, 0x5f, 0xff, 0x7f, 1, 0]);
        let local =
            ListWaveform::from_min_max_le(8, &waveform.min_values_le(), &waveform.max_values_le())
                .unwrap();
        assert_eq!(local, waveform);
        // Same layout as the server's `encode_peaks` before base64.
        assert_eq!(
            ListWaveform::new(1, vec![1, -1])
                .unwrap()
                .to_interleaved_le(),
            [1, 0, 0xff, 0xff]
        );
        let wire = ListWaveform::from_interleaved_le(8, &waveform.to_interleaved_le()).unwrap();
        assert_eq!(wire, waveform);
    }

    #[test]
    fn malformed_values_are_rejected_and_empty_files_have_no_columns() {
        assert!(ListWaveform::new(1, vec![1]).is_err());
        assert!(ListWaveform::new(1, vec![0; 258]).is_err());
        assert!(ListWaveform::new(1, vec![0; 256]).is_ok());
        assert!(ListWaveform::from_min_max_le(1, &[0, 0], &[0]).is_err());
        assert!(ListWaveform::from_min_max_le(1, &[0], &[0]).is_err());
        assert!(ListWaveform::from_interleaved_le(1, &[0, 0]).is_err());
        let empty = ListWaveform::from_min_max_le(0, &[], &[]).unwrap();
        assert_eq!(empty.columns(), 0);
        assert_eq!(empty.samples_per_pixel, 1);
        assert!(empty.to_interleaved_le().is_empty());
    }

    #[test]
    fn base64_peaks_decode_rfc4648_vectors_strictly() {
        for (output, input) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(decode_base64(input).unwrap(), output.as_bytes(), "{input}");
        }
        assert_eq!(decode_base64("+/8=").unwrap(), [0xfb, 0xff]);
        for bad in [
            "Zg", "Zg=", "Z===", "Zm9v\n", "Zg==Zg==", "Zh==", "Zm9-", "Zm=v",
        ] {
            assert!(decode_base64(bad).is_err(), "{bad:?}");
        }
        // `n = 1`: [min 1, max -1] as int16 LE.
        let item = ListWaveform::from_base64_peaks("AQD//w==", 1).unwrap();
        assert_eq!(item.values, vec![1, -1]);
        assert!(ListWaveform::from_base64_peaks("AQD//w==", 2).is_err());
        let empty = ListWaveform::from_base64_peaks("", 0).unwrap();
        assert_eq!(empty.columns(), 0);
    }
}
