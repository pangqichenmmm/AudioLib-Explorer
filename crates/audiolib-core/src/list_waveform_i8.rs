// crates/audiolib-core/src/list_waveform_i8.rs
//! Compact list Waveform encoding `i8-minmax-base64` (128 columns of interleaved
//! signed i8 `min,max`, 256 bytes for a full row, standard base64 on the wire).
//!
//! Quantization from the int16 list Waveform: `i8 = round(i16 * 127 / 32767)`, rounding
//! half away from zero, so `-32768` maps to `-127` and `-128` is never emitted. A decoder
//! clamps a received `-128` to `-127` and rejects any column with `min > max`.
//! For display the values are expanded back to the int16 scale (`round(i8 * 32767 / 127)`)
//! at their true amplitude, without normalization, so the existing int16 renderer is reused.
use crate::list_waveform::{LIST_WAVEFORM_COLUMNS, ListWaveform, decode_base64};
use crate::{Error, Result};

/// The original int16 wire encoding (§9.1).
pub const LIST_WAVEFORM_ENCODING_I16: &str = "i16le-minmax-base64";
/// The compact int8 wire encoding.
pub const LIST_WAVEFORM_ENCODING_I8: &str = "i8-minmax-base64";
/// Entity tag suffix of an i8 list Waveform (compared as part of the full tag).
pub const LIST_WAVEFORM_I8_ETAG_SUFFIX: &str = "-l1i8";
/// Local zoom key of i8 list Waveforms derived from `list128-v4-l1` rows.
pub const LIST_WAVEFORM_I8_ZOOM_LEVEL: &str = "list128-v4-l1i8";
/// Largest i8 peaks blob (`128 × 2` bytes).
pub const LIST_WAVEFORM_I8_MAX_BYTES: usize = LIST_WAVEFORM_COLUMNS as usize * 2;

/// `round(value * 127 / 32767)`, half away from zero. Never returns `-128`.
pub fn quantize_i16_to_i8(value: i16) -> i8 {
    let scaled = i32::from(value) * 127;
    // 32767 is odd and `scaled` is a multiple of 127, so an exact .5 never occurs and
    // adding floor(32767 / 2) before the integer division rounds half away from zero.
    let magnitude = (scaled.abs() + 16_383) / 32_767;
    let rounded = if scaled < 0 { -magnitude } else { magnitude };
    rounded.clamp(-127, 127) as i8
}

/// `round(value * 32767 / 127)` (true amplitude on the int16 scale); `-128` counts as `-127`.
pub fn expand_i8_to_i16(value: i8) -> i16 {
    let value = i32::from(value.max(-127));
    let scaled = value * 32_767;
    let magnitude = (scaled.abs() + 63) / 127;
    (if scaled < 0 { -magnitude } else { magnitude }) as i16
}

/// Base64 length of `n` columns: `4 * ceil(2n / 3)`.
pub fn i8_base64_len(columns: usize) -> usize {
    4 * (columns * 2).div_ceil(3)
}

/// One compact list Waveform: interleaved `[min₀, max₀, …]`, at most 128 columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListWaveformI8 {
    pub values: Vec<i8>,
}

impl ListWaveformI8 {
    /// Validate pairing, column count, the `-128` clamp and `min <= max`.
    pub fn new(mut values: Vec<i8>) -> Result<Self> {
        if !values.len().is_multiple_of(2) {
            return Err(Error::Validation(
                "i8 list Waveform values must be min/max pairs".into(),
            ));
        }
        if values.len() > LIST_WAVEFORM_I8_MAX_BYTES {
            return Err(Error::Validation(format!(
                "i8 list Waveform has more than {LIST_WAVEFORM_COLUMNS} columns"
            )));
        }
        for value in &mut values {
            *value = (*value).max(-127);
        }
        if let Some(index) = values
            .as_chunks::<2>()
            .0
            .iter()
            .position(|pair| pair[0] > pair[1])
        {
            return Err(Error::Validation(format!(
                "i8 list Waveform column {index} has min > max"
            )));
        }
        Ok(Self { values })
    }

    pub fn columns(&self) -> usize {
        self.values.len() / 2
    }

    /// Raw bytes (two's complement), `2n` bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.values.iter().map(|value| *value as u8).collect()
    }

    /// Parse raw bytes (after base64 decoding or from storage).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::new(bytes.iter().map(|byte| *byte as i8).collect())
    }

    /// Parse a wire item: `peaks` is standard base64 of exactly `2n` bytes.
    pub fn from_base64_peaks(peaks: &str, columns: usize) -> Result<Self> {
        if columns > LIST_WAVEFORM_COLUMNS as usize {
            return Err(Error::Validation(format!(
                "i8 list Waveform n = {columns} exceeds {LIST_WAVEFORM_COLUMNS}"
            )));
        }
        if peaks.len() != i8_base64_len(columns) {
            return Err(Error::Validation(format!(
                "i8 list Waveform peaks have {} base64 characters, expected {} for n = {columns}",
                peaks.len(),
                i8_base64_len(columns)
            )));
        }
        let bytes = decode_base64(peaks)?;
        if bytes.len() != columns * 2 {
            return Err(Error::Validation(format!(
                "i8 list Waveform peaks hold {} bytes, expected {} for n = {columns}",
                bytes.len(),
                columns * 2
            )));
        }
        Self::from_bytes(&bytes)
    }

    /// Quantize an int16 list Waveform (local derivation; no audio decode).
    pub fn from_list_waveform(waveform: &ListWaveform) -> Self {
        Self {
            values: waveform
                .values
                .iter()
                .map(|value| quantize_i16_to_i8(*value))
                .collect(),
        }
    }

    /// Expand to the int16 scale for display (true amplitude, `samples_per_pixel` 1).
    pub fn to_list_waveform(&self) -> ListWaveform {
        ListWaveform {
            samples_per_pixel: 1,
            values: self.values.iter().map(|v| expand_i8_to_i16(*v)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let w = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    T[(w >> (18 - 6 * i)) as usize & 63] as char
                } else {
                    '='
                });
            }
        }
        out
    }

    #[test]
    fn quantization_table_including_rounding_edges() {
        let table: &[(i16, i8)] = &[
            (0, 0),
            (1, 0),
            (-1, 0),
            (129, 0), // 0.49998 rounds down
            (130, 1), // 0.50388 rounds up
            (-129, 0),
            (-130, -1), // half away from zero, negative side
            (258, 1),
            (387, 1), // 1.49997
            (388, 2), // 1.50385
            (16_383, 63),
            (16_384, 64),
            (32_637, 126),
            (32_638, 127), // 126.5004
            (32_766, 127),
            (32_767, 127),
            (-32_767, -127),
            (-32_768, -127), // -128 is never emitted
        ];
        for (input, expected) in table {
            assert_eq!(quantize_i16_to_i8(*input), *expected, "input {input}");
        }
        for value in i16::MIN..=i16::MAX {
            let exact = f64::from(value) * 127.0 / 32767.0;
            let reference = exact.abs().round().copysign(exact) as i32;
            assert_eq!(i32::from(quantize_i16_to_i8(value)), reference.max(-127));
            assert_ne!(quantize_i16_to_i8(value), -128);
        }
    }

    #[test]
    fn expansion_keeps_true_amplitude() {
        assert_eq!(expand_i8_to_i16(127), 32_767);
        assert_eq!(expand_i8_to_i16(-127), -32_767);
        assert_eq!(expand_i8_to_i16(-128), -32_767);
        assert_eq!(expand_i8_to_i16(0), 0);
        assert_eq!(expand_i8_to_i16(64), 16_513);
        let waveform = ListWaveformI8::new(vec![-10, 10])
            .unwrap()
            .to_list_waveform();
        assert_eq!(waveform.values, vec![-2580, 2580]);
    }

    #[test]
    fn minus_128_is_clamped_and_min_above_max_rejected() {
        let parsed = ListWaveformI8::from_bytes(&[0x80, 0x05]).unwrap();
        assert_eq!(parsed.values, vec![-127, 5]);
        assert!(ListWaveformI8::from_bytes(&[0x80, 0x80]).is_ok());
        assert!(ListWaveformI8::from_bytes(&[10, 9]).is_err());
        assert!(ListWaveformI8::from_bytes(&[1, 2, 3]).is_err());
        assert!(ListWaveformI8::new(vec![0; 258]).is_err());
    }

    #[test]
    fn base64_length_checks_for_full_short_and_empty_rows() {
        assert_eq!(i8_base64_len(128), 344);
        assert_eq!(i8_base64_len(0), 0);
        assert_eq!(i8_base64_len(1), 4);
        assert_eq!(i8_base64_len(2), 8);
        assert_eq!(i8_base64_len(3), 8);
        let full: Vec<u8> = (0..128)
            .flat_map(|i| [(-(i as i8) / 2) as u8, (i / 2) as u8])
            .collect();
        let text = b64(&full);
        assert_eq!(text.len(), 344);
        let parsed = ListWaveformI8::from_base64_peaks(&text, 128).unwrap();
        assert_eq!(parsed.columns(), 128);
        assert_eq!(parsed.to_bytes(), full);
        assert!(ListWaveformI8::from_base64_peaks(&text, 127).is_err());
        assert!(ListWaveformI8::from_base64_peaks(&text, 129).is_err());
        assert_eq!(
            ListWaveformI8::from_base64_peaks("", 0).unwrap().columns(),
            0
        );
        assert!(ListWaveformI8::from_base64_peaks("AAAA", 0).is_err());
        let short = ListWaveformI8::from_base64_peaks(&b64(&[0xfe, 3]), 1).unwrap();
        assert_eq!(short.values, vec![-2, 3]);
        // Right length, but decodes to 3 bytes instead of 2 for n = 1.
        assert!(ListWaveformI8::from_base64_peaks("AAAA", 1).is_err());
        assert!(ListWaveformI8::from_base64_peaks("AA==", 1).is_err());
    }

    #[test]
    fn derived_from_int16_rows() {
        let source = ListWaveform::new(4, vec![-32_768, 32_767, -130, 129, 0, 0]).unwrap();
        let compact = ListWaveformI8::from_list_waveform(&source);
        assert_eq!(compact.values, vec![-127, 127, -1, 0, 0, 0]);
        assert!(ListWaveformI8::new(compact.values.clone()).is_ok());
    }
}
