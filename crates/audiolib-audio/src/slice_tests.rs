// crates/audiolib-audio/src/slice_tests.rs
//! Sample/byte precision and owned-output failure regressions for slice exports.
use super::*;
use crate::source::{register_stream_source, unregister_stream_source};
use std::{io::Cursor, sync::Arc};

fn range(start_frame: u64, end_frame: u64, sample_rate: u32) -> AudioFrameRange {
    AudioFrameRange {
        start_frame,
        end_frame,
        sample_rate,
    }
}

fn wave(format: OutputFormat, frames: u64, audio: &[u8]) -> Vec<u8> {
    let mut bytes = format.header(frames).unwrap();
    bytes.extend_from_slice(audio);
    if !audio.len().is_multiple_of(2) {
        bytes.push(0);
    }
    bytes
}

fn format(bytes: u16, channels: u16, float: bool) -> OutputFormat {
    OutputFormat {
        channels,
        rate: 48000,
        bytes,
        valid_bits: bytes * 8,
        mask: 0,
        float,
    }
}

fn export(source: &Path, output: &Path, selection: AudioFrameRange) {
    export_audio_slice(
        source,
        output,
        selection,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .unwrap();
}

fn data(path: &Path) -> (WavInfo, Vec<u8>) {
    let mut file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len();
    let info = read_info_reader(&mut file, len, false, false).unwrap();
    file.seek(SeekFrom::Start(info.data_offset)).unwrap();
    let mut bytes = vec![0; info.data_bytes as usize];
    file.read_exact(&mut bytes).unwrap();
    (info, bytes)
}

#[test]
fn pcm_and_float_export_retain_every_stored_sample_bit_and_channel() {
    let temp = tempfile::tempdir().unwrap();
    for (width, float) in [
        (1, false),
        (2, false),
        (3, false),
        (4, false),
        (4, true),
        (8, true),
    ] {
        let layout = format(width, 4, float);
        // Raw float values deliberately include unusual bit patterns: direct
        // copying must not sanitize, round or run through the audition mixer.
        let samples: Vec<u8> = (0..17 * usize::from(layout.align()))
            .map(|index| (index * 67 + 131) as u8)
            .collect();
        let source = temp.path().join(format!("source-{width}-{float}.bwf"));
        let output = temp.path().join(format!("slice-{width}-{float}.wav"));
        let original = wave(layout, 17, &samples);
        std::fs::write(&source, &original).unwrap();
        export(&source, &output, range(3, 13, 48000));
        let (info, exported) = data(&output);
        assert_eq!(
            (
                info.metadata.frames,
                info.metadata.channels,
                info.metadata.bit_depth
            ),
            (10, 4, width * 8)
        );
        assert_eq!(
            exported,
            samples[3 * usize::from(layout.align())..13 * usize::from(layout.align())]
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}

#[test]
fn extensible_valid_bits_and_channel_mask_survive_cropping() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("valid-bits.wav");
    let output = temp.path().join("out.wav");
    let mut layout = format(4, 6, false);
    layout.valid_bits = 24;
    layout.mask = 0x3f;
    let samples: Vec<u8> = (0..8 * usize::from(layout.align()))
        .map(|index| (index * 31) as u8)
        .collect();
    std::fs::write(&source, wave(layout, 8, &samples)).unwrap();
    export(&source, &output, range(1, 7, 48000));
    let (info, bytes) = data(&output);
    assert_eq!(info.metadata.attributes["valid_bits_per_sample"], 24);
    assert_eq!(info.metadata.attributes["channel_mask"], 0x3f);
    assert_eq!(bytes, samples[24..7 * 24]);
}

fn aiff(
    width: u16,
    valid_bits: u16,
    channels: u16,
    frames: u32,
    samples: &[u8],
    compression: Option<&[u8; 4]>,
) -> Vec<u8> {
    let mut common = channels.to_be_bytes().to_vec();
    common.extend_from_slice(&frames.to_be_bytes());
    common.extend_from_slice(&valid_bits.to_be_bytes());
    // 48000 as an IEEE 80-bit extended float.
    common.extend_from_slice(&[0x40, 0x0e, 0xbb, 0x80, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        samples.len(),
        width as usize * channels as usize * frames as usize
    );
    let mut body = if compression.is_some() {
        b"AIFC"
    } else {
        b"AIFF"
    }
    .to_vec();
    if let Some(codec) = compression {
        body.extend_from_slice(b"FVER\0\0\0\x04");
        body.extend_from_slice(&0xa280_5140_u32.to_be_bytes());
        common.extend_from_slice(codec);
        common.extend_from_slice(&[0, 0]); // Empty padded Pascal compression name.
    }
    body.extend_from_slice(b"COMM");
    body.extend_from_slice(&(common.len() as u32).to_be_bytes());
    body.extend_from_slice(&common);
    body.extend_from_slice(b"SSND");
    body.extend_from_slice(&(samples.len() as u32 + 8).to_be_bytes());
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(samples);
    if !samples.len().is_multiple_of(2) {
        body.push(0);
    }
    let mut bytes = b"FORM".to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);
    bytes
}

#[test]
fn aiff_integer_endianness_and_signed_eight_bit_are_lossless() {
    let temp = tempfile::tempdir().unwrap();
    for (width, bits) in [(1, 8), (2, 12), (2, 16), (3, 24), (4, 32)] {
        let source = temp.path().join(format!("{bits}.aiff"));
        let output = temp.path().join(format!("{bits}.wav"));
        let samples: Vec<u8> = (0..19 * 2 * width)
            .map(|index| (index * 79 + 151) as u8)
            .collect();
        std::fs::write(&source, aiff(width, bits, 2, 19, &samples, None)).unwrap();
        export(&source, &output, range(4, 17, 48000));
        let (info, bytes) = data(&output);
        let mut expected = samples[4 * 2 * width as usize..17 * 2 * width as usize].to_vec();
        for sample in expected.chunks_exact_mut(width as usize) {
            sample.reverse();
            if width == 1 {
                sample[0] ^= 0x80;
            }
        }
        assert_eq!(bytes, expected);
        assert_eq!(info.metadata.bit_depth, width * 8);
        if bits != width * 8 {
            assert_eq!(info.metadata.attributes["valid_bits_per_sample"], bits);
        }
    }
}

#[test]
fn aifc_float_preserves_source_precision_and_bit_patterns_in_little_endian_wav() {
    let temp = tempfile::tempdir().unwrap();
    for (width, codec) in [(4_u16, b"fl32"), (8, b"fl64")] {
        let patterns: [u64; 6] = if width == 4 {
            [
                0x0000_0001,
                0x3f00_0001,
                0x8000_0001,
                0x7f7f_ffff,
                0x7fc0_0005,
                0x7f80_0000,
            ]
        } else {
            [
                0x0000_0000_0000_0001,
                0x3fe0_0000_0000_0001,
                0x8000_0000_0000_0001,
                0x7fef_ffff_ffff_ffff,
                0x7ff8_0000_0000_0005,
                0x7ff0_0000_0000_0000,
            ]
        };
        let mut samples = Vec::new();
        for index in 0..34 {
            let bytes = patterns[index % patterns.len()].to_be_bytes();
            samples.extend_from_slice(&bytes[8 - usize::from(width)..]);
        }
        let source = temp.path().join(format!("float-{}.aiff", width * 8));
        let output = temp.path().join(format!("float-{}.wav", width * 8));
        let original = aiff(width, width * 8, 2, 17, &samples, Some(codec));
        std::fs::write(&source, &original).unwrap();
        export(&source, &output, range(4, 13, 48_000));
        let (info, bytes) = data(&output);
        assert_eq!(info.metadata.encoding, "IEEE_FLOAT");
        assert_eq!(
            (
                info.metadata.frames,
                info.metadata.sample_rate,
                info.metadata.channels,
                info.metadata.bit_depth
            ),
            (9, 48_000, 2, width * 8)
        );
        let mut expected =
            samples[4 * 2 * usize::from(width)..13 * 2 * usize::from(width)].to_vec();
        for sample in expected.chunks_exact_mut(usize::from(width)) {
            sample.reverse();
        }
        assert_eq!(bytes, expected);
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}

fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0_u8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn flac_integer(bits: u16, samples: &[i32]) -> Vec<u8> {
    // One standards-shaped verbatim mono FLAC frame, with valid header/frame CRCs.
    let frames = samples.len() as u16;
    let mut streaminfo = frames.to_be_bytes().to_vec();
    streaminfo.extend_from_slice(&frames.to_be_bytes());
    streaminfo.extend_from_slice(&[0; 6]);
    let packed = (48000_u64 << 44) | (u64::from(bits - 1) << 36) | u64::from(frames);
    streaminfo.extend_from_slice(&packed.to_be_bytes());
    streaminfo.extend_from_slice(&[0; 16]);
    let mut bytes = b"fLaC\x80\0\0\x22".to_vec();
    bytes.extend_from_slice(&streaminfo);
    let mut frame = vec![0xff, 0xf8, 0x60, 0, 0, (frames - 1) as u8];
    frame.push(crc8(&frame));
    frame.push(2); // Verbatim subframe, no wasted bits.
    for sample in samples {
        frame.extend_from_slice(&sample.to_be_bytes()[4 - usize::from(bits / 8)..]);
    }
    frame.extend_from_slice(&crc16(&frame).to_be_bytes());
    bytes.extend_from_slice(&frame);
    bytes
}

#[test]
fn flac_thirty_two_bit_low_bits_are_never_rounded_through_float() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("precise.flac");
    let output = temp.path().join("out.wav");
    let samples: Vec<i32> = (0..64)
        .map(|index| match index % 4 {
            0 => 0x0100_0001 + index,
            1 => -0x0100_0003 - index,
            2 => i32::MAX - index,
            _ => i32::MIN + index,
        })
        .collect();
    std::fs::write(&source, flac_integer(32, &samples)).unwrap();
    export(&source, &output, range(7, 53, 48000));
    let (info, bytes) = data(&output);
    assert_eq!(info.metadata.bit_depth, 32);
    assert_eq!(
        bytes,
        samples[7..53]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>()
    );
}

#[test]
fn flac_sixteen_and_twenty_four_bit_slices_keep_valid_low_bits_and_signed_extremes() {
    let temp = tempfile::tempdir().unwrap();
    for bits in [16_u16, 24] {
        let maximum = (1_i32 << (bits - 1)) - 1;
        let minimum = -(1_i32 << (bits - 1));
        let samples: Vec<i32> = (0..64)
            .map(|index| match index % 4 {
                0 => 1 + index,
                1 => -3 - index,
                2 => maximum - index,
                _ => minimum + index,
            })
            .collect();
        let source = temp.path().join(format!("precise-{bits}.flac"));
        let output = temp.path().join(format!("precise-{bits}.wav"));
        let original = flac_integer(bits, &samples);
        std::fs::write(&source, &original).unwrap();
        export(&source, &output, range(7, 53, 48_000));
        let (info, bytes) = data(&output);
        assert_eq!(info.metadata.encoding, "PCM");
        assert_eq!(
            (
                info.metadata.frames,
                info.metadata.sample_rate,
                info.metadata.channels,
                info.metadata.bit_depth
            ),
            (46, 48_000, 1, bits)
        );
        let expected: Vec<u8> = samples[7..53]
            .iter()
            .flat_map(|sample| sample.to_le_bytes()[..usize::from(bits / 8)].to_vec())
            .collect();
        assert_eq!(bytes, expected);
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}

fn ima_adpcm_wave(container: &[u8; 4]) -> Vec<u8> {
    // Same legal small-block layout as format_pipeline_tests: each 16-byte IMA
    // block is a predictor plus 24 zero nibbles, producing 25 constant samples.
    let mut audio = Vec::new();
    for block in 0..40_i16 {
        audio.extend_from_slice(&(block * 250 - 4000).to_le_bytes());
        audio.extend_from_slice(&[0, 0]);
        audio.extend_from_slice(&[0; 12]);
    }
    let frames = 40_u32 * 25 - 7; // fact trims the seven padding samples at EOF.
    let large = container != b"RIFF";
    let mut body = Vec::new();
    if large {
        body.extend_from_slice(b"ds64\x1c\0\0\0");
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.extend_from_slice(&(audio.len() as u64).to_le_bytes());
        body.extend_from_slice(&u64::from(frames).to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
    }
    body.extend_from_slice(b"fmt \x14\0\0\0\x11\0\x01\0");
    body.extend_from_slice(&8000_u32.to_le_bytes());
    body.extend_from_slice(&(8000_u32 * 16 / 25).to_le_bytes());
    body.extend_from_slice(b"\x10\0\x04\0\x02\0\x19\0fact\x04\0\0\0");
    body.extend_from_slice(&frames.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(if large { u32::MAX } else { audio.len() as u32 }).to_le_bytes());
    body.extend_from_slice(&audio);
    let riff_size = body.len() as u64 + 4;
    if large {
        body[8..16].copy_from_slice(&riff_size.to_le_bytes());
    }
    let mut bytes = container.to_vec();
    bytes.extend_from_slice(&(if large { u32::MAX } else { riff_size as u32 }).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend(body);
    bytes.extend_from_slice(b"private trailer outside the RIFF parent");
    bytes
}

#[test]
fn ima_adpcm_riff_rf64_and_bw64_export_exact_block_crossing_and_fact_tail_frames() {
    let temp = tempfile::tempdir().unwrap();
    for container in [b"RIFF", b"RF64", b"BW64"] {
        let name = std::str::from_utf8(container).unwrap();
        let source = temp.path().join(format!("adpcm-{name}.wav"));
        let original = ima_adpcm_wave(container);
        std::fs::write(&source, &original).unwrap();
        for (start, end) in [(27, 166), (975, 993)] {
            let output = temp.path().join(format!("{name}-{start}-{end}.wav"));
            export(&source, &output, range(start, end, 8000));
            let (info, bytes) = data(&output);
            assert_eq!(info.metadata.encoding, "IEEE_FLOAT");
            assert_eq!(
                (
                    info.metadata.frames,
                    info.metadata.sample_rate,
                    info.metadata.channels,
                    info.metadata.bit_depth
                ),
                (end - start, 8000, 1, 32)
            );
            let expected: Vec<u8> = (start..end)
                .flat_map(|frame| {
                    let predictor = (frame / 25) as i32 * 250 - 4000;
                    (predictor as f32 / 32768.0).to_le_bytes()
                })
                .collect();
            assert_eq!(bytes, expected, "{name} frames {start}..{end}");
        }
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
}

#[test]
fn compressed_sources_crop_their_presentation_frames_to_float_wav() {
    let temp = tempfile::tempdir().unwrap();
    for (name, source_bytes) in [
        (
            "test.mp3",
            include_bytes!("fixtures/mp3/vbr-xing.mp3").as_slice(),
        ),
        (
            "test.ogg",
            include_bytes!("fixtures/vorbis/comments.ogg").as_slice(),
        ),
    ] {
        let source = temp.path().join(name);
        let output = temp.path().join(format!("{name}.wav"));
        std::fs::write(&source, source_bytes).unwrap();
        let mut reference = Decoder::open(&source).unwrap();
        let info = reference.info.clone();
        let start = info.metadata.frames / 3;
        let end = (start + 71).min(info.metadata.frames);
        reference.seek(start).unwrap();
        let expected = reference.read((end - start) as usize).unwrap();
        export(
            &source,
            &output,
            range(start, end, info.metadata.sample_rate),
        );
        let (exported, bytes) = data(&output);
        assert_eq!(exported.metadata.encoding, "IEEE_FLOAT");
        assert_eq!(exported.metadata.frames, end - start);
        assert_eq!(
            bytes,
            expected
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn registered_audition_bytes_are_never_used_by_export() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original.wav");
    let output = temp.path().join("out.wav");
    let original = wave(format(2, 1, false), 16, &[1; 32]);
    let preview = wave(format(2, 1, false), 16, &[9; 32]);
    std::fs::write(&source, &original).unwrap();
    register_stream_source(
        source.clone(),
        Arc::new(move || Ok((Box::new(Cursor::new(preview.clone())), preview.len() as u64))),
    );
    export(&source, &output, range(2, 8, 48000));
    unregister_stream_source(&source);
    assert_eq!(data(&output).1, vec![1; 12]);
}

#[test]
fn invalid_ranges_existing_destinations_and_unsupported_files_are_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original.wav");
    let output = temp.path().join("out.wav");
    let original = wave(format(2, 1, false), 16, &[1; 32]);
    std::fs::write(&source, &original).unwrap();
    for selection in [
        range(2, 2, 48000),
        range(9, 8, 48000),
        range(0, 17, 48000),
        range(0, 8, 44100),
    ] {
        assert!(
            export_audio_slice(
                &source,
                &output,
                selection,
                &AtomicBool::new(false),
                &mut |_, _| {}
            )
            .is_err()
        );
        assert!(!output.exists());
    }
    std::fs::write(&output, b"keep existing destination").unwrap();
    assert!(
        export_audio_slice(
            &source,
            &output,
            range(0, 8, 48000),
            &AtomicBool::new(false),
            &mut |_, _| {}
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"keep existing destination"
    );
    assert_eq!(std::fs::read(&source).unwrap(), original);
    let unsupported = temp.path().join("unsupported.wav");
    std::fs::write(&unsupported, b"this is not audio").unwrap();
    assert!(
        export_audio_slice(
            &unsupported,
            &temp.path().join("missing.wav"),
            range(0, 1, 48000),
            &AtomicBool::new(false),
            &mut |_, _| {}
        )
        .is_err()
    );
}

#[test]
fn cancelling_during_copy_removes_only_the_owned_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original.wav");
    let output = temp.path().join("out.wav");
    let original = wave(format(2, 2, false), 200000, &vec![1; 800000]);
    std::fs::write(&source, &original).unwrap();
    let cancel = AtomicBool::new(false);
    let result = export_audio_slice(
        &source,
        &output,
        range(0, 200000, 48000),
        &cancel,
        &mut |done, _| {
            if done > 0 {
                cancel.store(true, Ordering::Release);
            }
        },
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!output.exists());
    assert_eq!(std::fs::read(&source).unwrap(), original);
}

#[test]
fn source_changes_are_blocked_or_rejected_before_output_publication() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original.wav");
    let output = temp.path().join("out.wav");
    std::fs::write(&source, wave(format(2, 1, false), 64, &[1; 128])).unwrap();
    let mut attempted = false;
    let mut changed = false;
    let result = export_audio_slice(
        &source,
        &output,
        range(0, 64, 48000),
        &AtomicBool::new(false),
        &mut |done, _| {
            if done > 0 && !attempted {
                attempted = true;
                // Windows refuses the competing writer while the original read
                // handle is held; Unix permits it and the final identity check fails.
                if let Ok(mut writer) = OpenOptions::new().append(true).open(&source) {
                    writer.write_all(&[7]).unwrap();
                    changed = true;
                }
            }
        },
    );
    assert!(attempted);
    if changed {
        assert!(result.is_err());
        assert!(!output.exists());
    } else {
        result.unwrap();
        assert_eq!(data(&output).1, vec![1; 128]);
    }
}

#[test]
fn rf64_header_describes_large_payload_without_narrowing_sizes() {
    let layout = format(4, 8, false);
    let frames = u64::from(u32::MAX);
    let header = layout.header(frames).unwrap();
    assert_eq!(&header[..4], b"RF64");
    assert_eq!(&header[12..16], b"ds64");
    assert_eq!(
        u64::from_le_bytes(header[28..36].try_into().unwrap()),
        frames * 32
    );
    assert_eq!(
        u64::from_le_bytes(header[36..44].try_into().unwrap()),
        frames
    );
    assert_eq!(
        u64::from_le_bytes(header[20..28].try_into().unwrap()),
        header.len() as u64 - 8 + frames * 32
    );
}
