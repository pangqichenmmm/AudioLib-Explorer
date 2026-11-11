// crates/audiolib-audio/src/list_waveform.rs
//! List thumbnail Waveforms (doc/api/README.md §9.1 and §9.3): one mono 128-column
//! min/max summary of a whole file, shared by the local library (`waveform_cache`) and
//! the server (`POST /waveforms`) so both produce byte-identical values.
use crate::waveform::{CACHE_VERSION, WaveformPeak, summary_waveform_with_read_buffer};
use audiolib_core::{LIST_WAVEFORM_COLUMNS, ListWaveform, Result};
use std::{path::Path, sync::atomic::AtomicBool};

/// `waveform_cache.zoom_level` of local list Waveforms: 128 columns, Waveform algorithm
/// version 4 and list encoding version 1. Rows with another value are stale.
pub const LIST_WAVEFORM_ZOOM_LEVEL: &str = "list128-v4-l1";
const _: () = assert!(CACHE_VERSION == 4 && LIST_WAVEFORM_COLUMNS == 128);
/// Bounded sequential-read buffer, matching the server's whole-file summary pass.
/// Each local decoder owns at most this much read-ahead, regardless of file length.
const LIST_WAVEFORM_READ_BUFFER: usize = 2 * 1024 * 1024;

/// `samples_per_pixel = ceil(frames / 128)`, at least 1.
pub fn list_samples_per_pixel(frames: u64) -> u32 {
    u32::try_from(frames.div_ceil(u64::from(LIST_WAVEFORM_COLUMNS)))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Quantize one peak value: clamp to -1..1, then `round(v * 32767)` (NaN is 0).
pub fn quantize_list_peak(value: f32) -> i16 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Union of every channel's envelope: column `i` is `[min_c(min), max_c(max)]`,
/// quantized and interleaved; at most 128 columns.
pub fn merge_list_channels(peaks: &[Vec<WaveformPeak>]) -> Vec<i16> {
    let columns = peaks
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min(LIST_WAVEFORM_COLUMNS as usize);
    let mut values = Vec::with_capacity(columns * 2);
    for column in 0..columns {
        let mut low = f32::INFINITY;
        let mut high = f32::NEG_INFINITY;
        for channel in peaks {
            if let Some(peak) = channel.get(column) {
                low = low.min(peak.min);
                high = high.max(peak.max);
            }
        }
        if !low.is_finite() {
            low = 0.0;
        }
        if !high.is_finite() {
            high = 0.0;
        }
        values.push(quantize_list_peak(low));
        values.push(quantize_list_peak(high));
    }
    values
}

/// Blocking: decode the whole file into the same 128-column summary as the server.
/// The decoder's layout supplies the frame count without a separate Metadata probe;
/// bounded 2 MiB reads keep plain network files streaming. Registered stream sources
/// retain their own buffering. A 0-frame file has no columns. `cancel` stops the
/// decode at its next 4096-frame block with `Error::Cancelled`.
pub fn list_waveform_with_cancel(path: &Path, cancel: &AtomicBool) -> Result<ListWaveform> {
    let waveform = summary_waveform_with_read_buffer(
        path,
        LIST_WAVEFORM_COLUMNS,
        cancel,
        LIST_WAVEFORM_READ_BUFFER,
    )?;
    ListWaveform::new(
        waveform.samples_per_pixel,
        merge_list_channels(&waveform.peaks),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::write_wav;
    use std::{fs, path::PathBuf, sync::atomic::Ordering};

    /// Expected interleaved int16 LE peaks (standard base64, as `POST /waveforms` sends
    /// them) of the generated fixture files below. Cross-checked byte for byte against the
    /// server's `list_waveform::compute` + `encode_peaks` (feat/batch-list-waveforms).
    const FIXTURE: &str = include_str!("list_waveform_fixture.txt");

    fn peak(min: f32, max: f32) -> WaveformPeak {
        WaveformPeak {
            min,
            max,
            ..WaveformPeak::default()
        }
    }

    /// Deterministic integer noise (no floating point in sample generation).
    fn noise(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *state >> 8
    }

    /// A fixture file: (name, rate, channels, bits, format tag, sample bytes).
    type Fixture = (&'static str, u32, u16, u16, u16, Vec<u8>);

    fn fixtures() -> Vec<Fixture> {
        let mut state = 0x5eed_u32;
        let mut stereo = Vec::new();
        for frame in 0..6000_u32 {
            // A decaying burst on the left, sparse clicks on the right.
            let envelope = 32767 - (frame * 5).min(32000) as i32;
            let left = (noise(&mut state) as i32 % (2 * envelope + 1)) - envelope;
            let right = if frame % 997 < 3 {
                if frame % 2 == 0 { 32767 } else { -32768 }
            } else {
                (noise(&mut state) as i32 % 201) - 100
            };
            stereo.extend_from_slice(&(left as i16).to_le_bytes());
            stereo.extend_from_slice(&(right as i16).to_le_bytes());
        }
        let mut mono = Vec::new();
        for frame in 0..1000_i32 {
            let value = ((frame * 131) % 65536 - 32768) as i16;
            mono.extend_from_slice(&value.to_le_bytes());
        }
        let mut float = Vec::new();
        for frame in 0..300_u32 {
            // Over-range float values must clamp to ±32767.
            let value = match frame % 7 {
                0 => 1.5_f32,
                1 => -2.0,
                n => n as f32 * 0.125 - 0.4,
            };
            float.extend_from_slice(&value.to_le_bytes());
        }
        let mut quad = Vec::new();
        for frame in 0..129_i32 {
            for channel in 0..4_i32 {
                let value = ((frame * 977 + channel * 1_234_567) % 16_777_216) - 8_388_608;
                quad.extend_from_slice(&value.to_le_bytes()[..3]);
            }
        }
        vec![
            ("stereo16_6000", 48_000, 2, 16, 1, stereo),
            ("mono16_1000", 44_100, 1, 16, 1, mono),
            ("float32_300", 48_000, 1, 32, 3, float),
            ("quad24_129", 96_000, 4, 24, 1, quad),
            ("empty16_0", 48_000, 2, 16, 1, Vec::new()),
        ]
    }

    fn write_fixtures(directory: &Path) -> Vec<(String, PathBuf)> {
        fixtures()
            .into_iter()
            .map(|(name, rate, channels, bits, tag, samples)| {
                let path = directory.join(format!("{name}.wav"));
                write_wav(&path, rate, channels, bits, tag, &samples, false, false);
                (name.to_string(), path)
            })
            .collect()
    }

    /// RFC 4648 base64 with padding (test-only; the client does not need an encoder).
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
                out.push(if index <= chunk.len() {
                    ALPHABET[((triple >> shift) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
    }

    // Same vectors as the server's `base64_matches_rfc4648_vectors`: the fixture strings
    // below are only comparable with the server's `peaks` if both encoders agree.
    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), output);
        }
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn zoom_level_names_columns_algorithm_and_encoding_versions() {
        assert_eq!(
            LIST_WAVEFORM_ZOOM_LEVEL,
            format!(
                "list{LIST_WAVEFORM_COLUMNS}-v{CACHE_VERSION}-l{}",
                audiolib_core::LIST_WAVEFORM_ENCODING_VERSION
            )
        );
        assert_eq!(list_samples_per_pixel(0), 1);
        assert_eq!(list_samples_per_pixel(1000), 8);
        assert_eq!(list_samples_per_pixel(128), 1);
        assert_eq!(list_samples_per_pixel(129), 2);
        assert_eq!(list_samples_per_pixel(u64::MAX), u32::MAX);
    }

    // Same vectors as the server's `channels_merge_into_a_quantized_envelope_union`.
    #[test]
    fn channels_merge_into_a_quantized_envelope_union() {
        let left = vec![peak(-0.5, 0.25), peak(-2.0, 0.0)];
        let right = vec![peak(-0.25, 0.75), peak(0.0, 1.5)];
        assert_eq!(
            merge_list_channels(&[left, right]),
            vec![-16384, 24575, -32767, 32767]
        );
        assert_eq!(quantize_list_peak(f32::NAN), 0);
        assert_eq!(quantize_list_peak(-1.0), -32767);
        assert_eq!(quantize_list_peak(0.5 / 32767.0), 1);
        assert!(merge_list_channels(&[]).is_empty());
        let long = vec![peak(0.0, 0.0); 200];
        assert_eq!(merge_list_channels(&[long]).len(), 256);
        // Unequal channel lengths keep the longest; missing columns do not contribute.
        let uneven = merge_list_channels(&[vec![peak(-0.5, 0.5)], vec![peak(0.0, 0.0); 2]]);
        assert_eq!(uneven, vec![-16384, 16384, 0, 0]);
        assert_eq!(
            base64(
                &ListWaveform::new(1, vec![1, -1])
                    .unwrap()
                    .to_interleaved_le()
            ),
            base64(&[1, 0, 0xff, 0xff])
        );
    }

    #[test]
    fn fixture_files_match_the_shared_byte_exact_vectors() {
        let directory = tempfile::tempdir().unwrap();
        let expected: Vec<(&str, usize, &str)> = FIXTURE
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split_whitespace();
                let name = fields.next().unwrap();
                let columns = fields.next().unwrap().parse().unwrap();
                (name, columns, fields.next().unwrap_or(""))
            })
            .collect();
        let files = write_fixtures(directory.path());
        assert_eq!(expected.len(), files.len());
        for ((name, path), (expected_name, columns, peaks)) in files.iter().zip(&expected) {
            assert_eq!(name, expected_name);
            let waveform = list_waveform_with_cancel(path, &AtomicBool::new(false)).unwrap();
            assert_eq!(waveform.columns(), *columns, "{name}");
            assert_eq!(base64(&waveform.to_interleaved_le()), *peaks, "{name}");
        }
    }

    #[test]
    fn short_files_have_fewer_columns_and_values_stay_in_range() {
        let directory = tempfile::tempdir().unwrap();
        let files = write_fixtures(directory.path());
        let get = |name: &str| {
            let path = &files.iter().find(|(file, _)| file == name).unwrap().1;
            list_waveform_with_cancel(path, &AtomicBool::new(false)).unwrap()
        };
        // 1000 frames -> 8 frames per column -> 125 columns (§9.1 example).
        let mono = get("mono16_1000");
        assert_eq!((mono.samples_per_pixel, mono.columns()), (8, 125));
        let float = get("float32_300");
        assert_eq!((float.samples_per_pixel, float.columns()), (3, 100));
        assert!(float.values.contains(&32767));
        assert!(float.values.contains(&-32767));
        let stereo = get("stereo16_6000");
        assert_eq!((stereo.samples_per_pixel, stereo.columns()), (47, 128));
        for column in 0..stereo.columns() {
            let (low, high) = stereo.column(column).unwrap();
            assert!(low <= high);
        }
        let empty = get("empty16_0");
        assert_eq!((empty.samples_per_pixel, empty.columns()), (1, 0));
    }

    #[test]
    fn cancellation_stops_the_decode() {
        let directory = tempfile::tempdir().unwrap();
        let files = write_fixtures(directory.path());
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            list_waveform_with_cancel(&files[0].1, &cancel),
            Err(audiolib_core::Error::Cancelled)
        ));
    }

    #[test]
    fn buffered_summary_matches_the_previous_probe_and_decode_with_trailing_metadata() {
        use crate::waveform::{WaveformRequest, waveform_with_cancel};

        fn previous(path: &Path, cancel: &AtomicBool) -> ListWaveform {
            let metadata = crate::wav::probe(path).unwrap();
            if metadata.frames == 0 {
                return ListWaveform::new(1, Vec::new()).unwrap();
            }
            let samples_per_pixel = list_samples_per_pixel(metadata.frames);
            let waveform = waveform_with_cancel(
                path,
                WaveformRequest {
                    samples_per_pixel,
                    start_frame: 0,
                    end_frame: None,
                    include_colors: false,
                },
                cancel,
            )
            .unwrap();
            ListWaveform::new(samples_per_pixel, merge_list_channels(&waveform.peaks)).unwrap()
        }

        fn append_chunk(wav: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
            wav.extend_from_slice(id);
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(data);
            if !data.len().is_multiple_of(2) {
                wav.push(0);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let mut files = write_fixtures(directory.path());
        // Exercise PCM32 and float64 without changing the shared server wire vectors.
        for (name, bits, tag, samples) in [
            (
                "stereo32_70001",
                32,
                1,
                (0..140_002_i32)
                    .flat_map(|i| i.wrapping_mul(1_234_567).to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
            (
                "float64_33333",
                64,
                3,
                (0..33_333)
                    .flat_map(|i| ((f64::from(i) * 0.003).sin()).to_le_bytes())
                    .collect::<Vec<_>>(),
            ),
        ] {
            let path = directory.path().join(format!("{name}.wav"));
            let channels = if tag == 1 { 2 } else { 1 };
            write_wav(&path, 48_000, channels, bits, tag, &samples, true, false);
            files.push((name.into(), path));
        }
        let cancel = AtomicBool::new(false);
        for (name, path) in files {
            for trailing in [false, true] {
                if trailing {
                    let mut wav = fs::read(&path).unwrap();
                    append_chunk(&mut wav, b"ID3 ", b"ID3\x04\0\0\0\0\0\0");
                    append_chunk(&mut wav, b"iXML", b"<BWFXML><TAKE>3</TAKE></BWFXML>");
                    append_chunk(&mut wav, b"SMED", &[7; 333]);
                    append_chunk(&mut wav, b"LIST", b"INFOINAM\x04\0\0\0tail");
                    let size = (wav.len() - 8) as u32;
                    wav[4..8].copy_from_slice(&size.to_le_bytes());
                    fs::write(&path, wav).unwrap();
                }
                assert_eq!(
                    list_waveform_with_cancel(&path, &cancel).unwrap(),
                    previous(&path, &cancel),
                    "{name} trailing={trailing}"
                );
            }
        }
    }

    /// Writes the fixture WAVs to `AUDIOLIB_LIST_WAVEFORM_FIXTURE_DIR` so another
    /// implementation (the server) can be cross-checked against `list_waveform_fixture.txt`.
    #[test]
    #[ignore = "writes the shared fixture WAVs for cross-checking another implementation"]
    fn export_fixture_files() {
        let directory = PathBuf::from(
            std::env::var_os("AUDIOLIB_LIST_WAVEFORM_FIXTURE_DIR")
                .expect("AUDIOLIB_LIST_WAVEFORM_FIXTURE_DIR"),
        );
        fs::create_dir_all(&directory).unwrap();
        for (name, path) in write_fixtures(&directory) {
            let waveform = list_waveform_with_cancel(&path, &AtomicBool::new(false)).unwrap();
            println!(
                "{name} {} {}",
                waveform.columns(),
                base64(&waveform.to_interleaved_le())
            );
        }
    }
}
