// crates/audiolib-audio/src/tests.rs
use super::*;
use crate::decoder::{Decoder, ResampledDecoder};
use std::{fs, path::Path, sync::atomic::AtomicBool};

fn chunk(id: &[u8; 4], data: &[u8], output: &mut Vec<u8>) {
    output.extend_from_slice(id);
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        output.push(0);
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_wav(
    path: &Path,
    rate: u32,
    channels: u16,
    bits: u16,
    tag: u16,
    samples: &[u8],
    extra: bool,
    rf64: bool,
) {
    let mut body = Vec::new();
    if rf64 {
        let mut ds64 = vec![0; 28];
        ds64[8..16].copy_from_slice(&(samples.len() as u64).to_le_bytes());
        chunk(b"ds64", &ds64, &mut body);
    }
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&tag.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * u32::from(channels * bits / 8)).to_le_bytes());
    fmt.extend_from_slice(&(channels * bits / 8).to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    chunk(b"fmt ", &fmt, &mut body);
    if extra {
        let mut bext = vec![0; 602];
        bext[..4].copy_from_slice(b"wind");
        bext[256..260].copy_from_slice(b"BOOM");
        bext[338..346].copy_from_slice(&123456789_u64.to_le_bytes());
        bext[346..348].copy_from_slice(&2_u16.to_le_bytes());
        chunk(b"bext", &bext, &mut body);
        chunk(
            b"iXML",
            b"<BWFXML><SCENE>airfield</SCENE><TAKE>2</TAKE><CIRCLED>TRUE</CIRCLED></BWFXML>",
            &mut body,
        );
    }
    if rf64 {
        body.extend_from_slice(b"data");
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(samples);
        if !samples.len().is_multiple_of(2) {
            body.push(0);
        }
    } else {
        chunk(b"data", samples, &mut body);
    }
    let mut output = Vec::new();
    output.extend_from_slice(if rf64 { b"RF64" } else { b"RIFF" });
    output.extend_from_slice(
        &(if rf64 {
            u32::MAX
        } else {
            body.len() as u32 + 4
        })
        .to_le_bytes(),
    );
    output.extend_from_slice(b"WAVE");
    output.extend_from_slice(&body);
    fs::write(path, output).unwrap();
}
fn write_encoded_wav(
    path: &Path,
    tag: u16,
    block_align: u16,
    frames: u32,
    extra: &[u8],
    samples: &[u8],
) {
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&tag.to_le_bytes());
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&8000_u32.to_le_bytes());
    let frames_per_block = extra
        .get(..2)
        .map(|bytes| u16::from_le_bytes(bytes.try_into().unwrap()))
        .unwrap_or(1);
    fmt.extend_from_slice(
        &(8000 * u32::from(block_align) / u32::from(frames_per_block)).to_le_bytes(),
    );
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&(if matches!(tag, 2 | 17) { 4_u16 } else { 8 }).to_le_bytes());
    fmt.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    fmt.extend_from_slice(extra);
    let mut body = Vec::new();
    chunk(b"fmt ", &fmt, &mut body);
    chunk(b"fact", &frames.to_le_bytes(), &mut body);
    chunk(b"data", samples, &mut body);
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(&body);
    fs::write(path, wav).unwrap();
}
fn assert_compressed_seek_range_and_tail(path: &Path, frames: usize, range_start: usize) {
    let expected = Decoder::open(path).unwrap().read(frames + 100).unwrap();
    assert_eq!(expected.len(), frames);
    let mut decoder = Decoder::open(path).unwrap();
    for start in [0, 1, 24, 25, 450, 1149, 1152, 1230, frames - 1, frames] {
        decoder.seek(start as u64).unwrap();
        let first = decoder.read(17).unwrap();
        assert_eq!(first, expected[start..(start + 17).min(frames)]);
        let tail = decoder.read(frames).unwrap();
        assert_eq!(tail, expected[(start + first.len())..]);
        assert!(decoder.read(1).unwrap().is_empty());
    }
    let wave = waveform(
        path,
        WaveformRequest {
            start_frame: range_start as u64,
            end_frame: Some((range_start + 41) as u64),
            samples_per_pixel: 8,
            include_colors: false,
        },
    )
    .unwrap();
    assert_eq!(wave.frames, 41);
    for (peak, samples) in wave.peaks[0]
        .iter()
        .zip(expected[range_start..range_start + 41].chunks(8))
    {
        assert_eq!(
            peak.min,
            samples.iter().copied().fold(f32::INFINITY, f32::min)
        );
        assert_eq!(
            peak.max,
            samples.iter().copied().fold(f32::NEG_INFINITY, f32::max)
        );
    }
    let mut converted =
        ResampledDecoder::new(Decoder::open(path).unwrap(), 16000, range_start as u64).unwrap();
    let mut converted_frames = 0;
    while !converted.is_finished() {
        converted_frames += converted.next().unwrap().len();
    }
    assert_eq!(converted_frames, (frames - range_start) * 2);
}
#[test]
fn mulaw_and_alaw_seek_discard_packet_prefix_and_preserve_range_tail() {
    let temp = tempfile::tempdir().unwrap();
    for (tag, silence) in [(7, 255), (6, 0xd5)] {
        let path = temp.path().join(format!("companded-{tag}.wav"));
        let mut samples = vec![silence; 2000];
        samples[400..500].fill(0);
        samples[1150..1250].fill(0);
        samples[1990..].fill(0);
        write_encoded_wav(&path, tag, 1, 2000, &[], &samples);
        assert_compressed_seek_range_and_tail(&path, 2000, 450);
    }
}
#[test]
fn ima_and_ms_adpcm_seek_blocks_and_trim_padded_fact_tail() {
    let temp = tempfile::tempdir().unwrap();
    for (tag, frames_per_block) in [(17, 25_u16), (2, 20)] {
        let mut samples = Vec::new();
        for block in 0..80 {
            let predictor = (500 + block * 250) as i16;
            if tag == 17 {
                samples.extend_from_slice(&predictor.to_le_bytes());
                samples.extend_from_slice(&[0, 0]);
                samples.extend_from_slice(&[0; 12]);
            } else {
                samples.push(0);
                samples.extend_from_slice(&16_i16.to_le_bytes());
                samples.extend_from_slice(&predictor.to_le_bytes());
                samples.extend_from_slice(&predictor.to_le_bytes());
                samples.extend_from_slice(&[0; 9]);
            }
        }
        let mut extra = frames_per_block.to_le_bytes().to_vec();
        if tag == 2 {
            extra.extend_from_slice(&7_u16.to_le_bytes());
            for (a, b) in [
                (256_i16, 0_i16),
                (512, -256),
                (0, 0),
                (192, 64),
                (240, 0),
                (460, -208),
                (392, -232),
            ] {
                extra.extend_from_slice(&a.to_le_bytes());
                extra.extend_from_slice(&b.to_le_bytes());
            }
        }
        let frames = 80 * u32::from(frames_per_block) - 7;
        let path = temp.path().join(format!("adpcm-{tag}.wav"));
        write_encoded_wav(&path, tag, 16, frames, &extra, &samples);
        assert_compressed_seek_range_and_tail(&path, frames as usize, 1230);
    }
}
#[test]
fn pcm24_bwf_ixml_rf64_and_last_partial_peak() {
    let temp = tempfile::tempdir().unwrap();
    for rf64 in [false, true] {
        let path = temp.path().join(format!("test-{rf64}.wav"));
        let samples: Vec<u8> = [-8_388_608_i32, 0, 8_388_607, 4_194_304, -4_194_304]
            .iter()
            .flat_map(|sample| sample.to_le_bytes()[..3].to_vec())
            .collect();
        write_wav(&path, 192000, 1, 24, 1, &samples, true, rf64);
        let info = probe(&path).unwrap();
        assert_eq!(
            (info.sample_rate, info.channels, info.bit_depth, info.frames),
            (192000, 1, 24, 5)
        );
        assert_eq!(info.bext_description, "wind");
        assert_eq!(info.attributes["ixml_scene"], "airfield");
        assert_eq!(info.attributes["bwf_time_reference"], 123456789_u64);
        let mut decoder = Decoder::open(&path).unwrap();
        let samples = decoder.read(10).unwrap();
        assert_eq!(samples[0], -1.0);
        assert_eq!(samples[3], 0.5);
        assert!(samples[2] > 0.9999);
        decoder.seek(3).unwrap();
        assert_eq!(decoder.read(10).unwrap(), vec![0.5, -0.5]);
        decoder.seek(5).unwrap();
        assert!(decoder.read(1).unwrap().is_empty());
        let wave = waveform(
            &path,
            WaveformRequest {
                samples_per_pixel: 2,
                include_colors: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(wave.peaks[0].len(), 3);
        assert_eq!(wave.peaks[0][2].min, -0.5);
        assert_eq!(wave.peaks[0][2].rms, 0.5);
    }
}
#[test]
fn float_short_empty_and_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("float.wav");
    let samples: Vec<u8> = [0.25_f32, f32::NAN, -0.5]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    write_wav(&path, 48000, 1, 32, 3, &samples, false, false);
    assert_eq!(
        Decoder::open(&path).unwrap().read(100).unwrap(),
        vec![0.25, 0.0, -0.5]
    );
    let wave = waveform(&path, WaveformRequest::default()).unwrap();
    assert_eq!(wave.peaks[0].len(), 1);
    assert!(wave.peaks[0][0].color.is_some());
    assert!(matches!(
        waveform_with_cancel(&path, WaveformRequest::default(), &AtomicBool::new(true)),
        Err(audiolib_core::Error::Cancelled)
    ));
    write_wav(&path, 48000, 1, 32, 3, &[], false, false);
    assert!(waveform(&path, WaveformRequest::default()).unwrap().peaks[0].is_empty());
}
#[test]
fn resampling_preserves_duration_frequency_and_seek_length() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("sine.wav");
    let samples: Vec<u8> = (0..4800)
        .flat_map(|index| {
            (0.5 * (std::f32::consts::TAU * 1000.0 * index as f32 / 48000.0).sin()).to_le_bytes()
        })
        .collect();
    write_wav(&path, 48000, 1, 32, 3, &samples, false, false);
    for (rate, start) in [(96000, 0), (44100, 0), (96000, 2400)] {
        let mut decoder =
            ResampledDecoder::new(Decoder::open(&path).unwrap(), rate, start).unwrap();
        let mut output = Vec::new();
        loop {
            let samples = decoder.next().unwrap();
            if samples.is_empty() {
                break;
            }
            output.extend(samples);
        }
        assert_eq!(
            output.len(),
            ((4800 - start) as f64 * f64::from(rate) / 48000.0).round() as usize
        );
        let crossings = output
            .windows(2)
            .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
            .count();
        let expected = if start == 0 { 100 } else { 50 };
        assert!(
            crossings.abs_diff(expected) <= 2,
            "rate={rate} crossings={crossings}"
        );
        assert!(output.iter().copied().fold(0.0_f32, f32::max) > 0.45);
    }
}
#[test]
fn cache_fingerprint_changes_when_source_changes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source.wav");
    let cache = temp.path().join("cache");
    write_wav(
        &path,
        48000,
        1,
        32,
        3,
        &0.25_f32.to_le_bytes(),
        false,
        false,
    );
    let first = cached_waveform(&path, &cache, WaveformRequest::default()).unwrap();
    assert_eq!(first.peaks[0][0].min, 0.25);
    let samples: Vec<u8> = [-0.5_f32, 0.5]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    write_wav(&path, 48000, 1, 32, 3, &samples, false, false);
    let second = cached_waveform(&path, &cache, WaveformRequest::default()).unwrap();
    assert_eq!(second.peaks[0][0].min, -0.5);
    assert_eq!(second.frames, 2);
    assert_eq!(fs::read_dir(cache).unwrap().count(), 2);
}
#[test]
fn metadata_rejects_truncated_chunk() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("corrupt.wav");
    write_wav(&path, 48000, 1, 16, 1, &[0, 0], false, false);
    let mut bytes = fs::read(&path).unwrap();
    bytes.pop();
    fs::write(&path, bytes).unwrap();
    assert!(probe(&path).is_err());
}
#[test]
#[cfg(feature = "playback")]
fn headless_transport_reports_confirmed_ready_seek_stop_gain_and_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("audio.wav");
    write_wav(&path, 1000, 1, 16, 1, &vec![0; 2000], false, false);
    let engine = AudioEngine::new(PlaybackConfig::default()).unwrap();
    let handle = engine.handle();
    handle.send(AudioCommand::Load(path)).unwrap();
    for _ in 0..100 {
        if handle.status().state == PlaybackState::Ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(handle.status().state, PlaybackState::Ready);
    handle.send(AudioCommand::Seek(0.5)).unwrap();
    handle.send(AudioCommand::SetVolume(0.2)).unwrap();
    for _ in 0..100 {
        if handle.status().volume == 0.2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(handle.status().position, 0.5);
    assert_eq!(handle.status().volume, 0.2);
    handle.send(AudioCommand::Stop).unwrap();
    for _ in 0..100 {
        if handle.status().state == PlaybackState::Stopped {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(handle.status().position, 0.0);
    let rejected = handle.send_and_wait(
        AudioCommand::Seek(f64::NAN),
        std::time::Duration::from_secs(1),
    );
    assert!(rejected.is_err());
    assert_eq!(handle.status().state, PlaybackState::Stopped);
    assert!(handle.status().error.is_some());
}

#[test]
fn spectral_noise_tone_noise_survives_zoom_and_cache_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("noise-tone-noise.wav");
    let mut random = 731_u32;
    let samples: Vec<u8> = (0..288000)
        .flat_map(|frame| {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = random as f32 / u32::MAX as f32 * 2.0 - 1.0;
            let tone = (std::f32::consts::TAU * 619.0 * frame as f32 / 48000.0).sin();
            let sample = match frame / 96000 {
                0 => noise * 0.5,
                1 => tone * 0.5,
                _ => (noise + tone * 0.02) * 0.5,
            };
            sample.to_le_bytes()
        })
        .collect();
    write_wav(&path, 48000, 1, 32, 3, &samples, false, false);
    for samples_per_pixel in [240, 2400] {
        let request = WaveformRequest {
            samples_per_pixel,
            ..Default::default()
        };
        let cache = root.path().join(format!("cache-{samples_per_pixel}"));
        let wave = cached_waveform(&path, &cache, request.clone()).unwrap();
        for (index, peak) in wave.peaks[0].iter().enumerate() {
            let time = index as f32 * samples_per_pixel as f32 / 48000.0;
            if (0.3..1.7).contains(&time) || (4.3..5.7).contains(&time) {
                assert_eq!(
                    peak.color,
                    Some(SPECTRAL_PEAK_BASE),
                    "time={time}, {peak:?}"
                );
            } else if (2.3..3.7).contains(&time) {
                assert!((peak.dominant_hz - 619.0).abs() < 3.0);
                let color = peak.color.unwrap();
                assert!(color[1] > color[0] + 70 && color[2] > color[0] + 70);
            }
        }
        let cache_path = fs::read_dir(&cache)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut stale: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        // Existing v4 analysis is reusable even when it contains the old vivid
        // palette. Refresh RGB on read without rewriting/reanalyzing the cache.
        let tone_index = (3 * 48000 / samples_per_pixel) as usize;
        stale["waveform"]["peaks"][0][tone_index]["color"] = serde_json::json!([243, 243, 24]);
        let old_palette = serde_json::to_vec(&stale).unwrap();
        fs::write(&cache_path, &old_palette).unwrap();
        let recolored = cached_waveform(&path, &cache, request.clone()).unwrap();
        assert_eq!(
            recolored.peaks[0][tone_index].color,
            wave.peaks[0][tone_index].color
        );
        assert_eq!(
            recolored.peaks[0][tone_index].min,
            wave.peaks[0][tone_index].min
        );
        assert_eq!(fs::read(&cache_path).unwrap(), old_palette);
        stale["version"] = 3.into();
        stale["waveform"]["peaks"][0][12]["color"] = serde_json::json!([142, 142, 142]);
        fs::write(&cache_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let rebuilt = cached_waveform(&path, &cache, request).unwrap();
        assert_eq!(rebuilt.peaks[0][12].color, Some(SPECTRAL_PEAK_BASE));
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(&cache_path).unwrap()).unwrap();
        assert_eq!(saved["version"], 4);
    }
}

#[test]
fn short_resonance_stays_colored_between_broadband_noise_segments() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("brief-resonance.wav");
    let mut random = 9173_u32;
    let samples: Vec<u8> = (0..96000)
        .flat_map(|frame| {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (random as f32 / u32::MAX as f32 * 2.0 - 1.0) * 0.25;
            let time = frame as f32 / 96000.0;
            let tone = if (0.48..0.52).contains(&time) {
                (std::f32::consts::TAU * 2470.0 * time).sin() * 0.7
            } else {
                0.0
            };
            (noise + tone).to_le_bytes()
        })
        .collect();
    write_wav(&path, 96000, 1, 32, 3, &samples, false, false);
    let wave = waveform(
        &path,
        WaveformRequest {
            samples_per_pixel: 480,
            ..Default::default()
        },
    )
    .unwrap();
    let resonance = &wave.peaks[0][100];
    assert!((resonance.dominant_hz - 2470.0).abs() < 5.0);
    assert!(resonance.tonality > 0.4, "{resonance:?}");
    assert!(resonance.color.unwrap()[2] > 150);
    for index in [20, 60, 140, 180] {
        assert_eq!(wave.peaks[0][index].color, Some(SPECTRAL_PEAK_BASE));
    }
}

#[test]
fn registered_stream_sources_decode_like_files_and_unregister_cleanly() {
    use crate::source::{ByteSource, SourceOpener};
    use std::{
        io::Cursor,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    let temp = tempfile::tempdir().unwrap();
    let pcm = temp.path().join("pcm.wav");
    let samples: Vec<u8> = (0..4000_i16)
        .flat_map(|i| i.wrapping_mul(7).to_le_bytes())
        .collect();
    write_wav(&pcm, 8000, 2, 16, 1, &samples, false, false);
    let mulaw = temp.path().join("mulaw.wav");
    let mut companded = vec![255_u8; 2000];
    companded[400..500].fill(0);
    write_encoded_wav(&mulaw, 7, 1, 2000, &[], &companded);
    for file in [&pcm, &mulaw] {
        let bytes = Arc::new(fs::read(file).unwrap());
        // The registered path does not exist on disk: every read goes through the opener.
        let virtual_path = temp.path().join("remote").join(file.file_name().unwrap());
        let opens = Arc::new(AtomicUsize::new(0));
        let opener: SourceOpener = {
            let bytes = bytes.clone();
            let opens = opens.clone();
            Arc::new(move || {
                opens.fetch_add(1, Ordering::SeqCst);
                let source: Box<dyn ByteSource> = Box::new(Cursor::new(bytes.to_vec()));
                Ok((source, bytes.len() as u64))
            })
        };
        assert!(Decoder::open(&virtual_path).is_err());
        register_stream_source(&virtual_path, opener);
        assert!(is_stream_source(&virtual_path));
        assert_eq!(
            format!("{:?}", probe(&virtual_path).unwrap()),
            format!("{:?}", probe(file).unwrap())
        );
        let expected = Decoder::open(file).unwrap().read(10_000).unwrap();
        assert!(!expected.is_empty());
        let mut streamed = Decoder::open(&virtual_path).unwrap();
        assert_eq!(streamed.read(10_000).unwrap(), expected);
        let mut reference = Decoder::open(file).unwrap();
        reference.seek(450).unwrap();
        streamed.seek(450).unwrap();
        assert_eq!(streamed.read(100).unwrap(), reference.read(100).unwrap());
        assert!(opens.load(Ordering::SeqCst) >= 2);
        assert!(unregister_stream_source(&virtual_path));
        assert!(!is_stream_source(&virtual_path));
        assert!(!unregister_stream_source(&virtual_path));
        assert!(Decoder::open(&virtual_path).is_err());
    }
}
#[test]
fn read_buffered_waveform_matches_unbuffered_for_every_source_kind() {
    let temp = tempfile::tempdir().unwrap();
    let pattern = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 2_654_435_761) >> 13) as u8)
            .collect()
    };
    let mut files = Vec::new();
    for (name, channels, bits, tag) in [
        ("pcm16.wav", 2_u16, 16_u16, 1_u16),
        ("pcm24.wav", 1, 24, 1),
        ("pcm8.wav", 2, 8, 1),
    ] {
        let path = temp.path().join(name);
        write_wav(
            &path,
            48000,
            channels,
            bits,
            tag,
            &pattern(300_001 * usize::from(channels) * usize::from(bits / 8)),
            true,
            false,
        );
        files.push(path);
    }
    let float: Vec<u8> = (0..200_000)
        .flat_map(|i| ((i as f32 * 0.001).sin()).to_le_bytes())
        .collect();
    let path = temp.path().join("float.wav");
    write_wav(&path, 44100, 1, 32, 3, &float, false, true);
    files.push(path);
    // mu-law (Symphonia format reader) and IMA ADPCM (block reader).
    let path = temp.path().join("mulaw.wav");
    write_encoded_wav(&path, 7, 1, 150_000, &[], &pattern(150_000));
    files.push(path);
    let mut adpcm = Vec::new();
    for block in 0..4000 {
        adpcm.extend_from_slice(&((block * 37 % 3000) as i16).to_le_bytes());
        adpcm.extend_from_slice(&[0, 0]);
        adpcm.extend_from_slice(&pattern(12));
    }
    let path = temp.path().join("adpcm.wav");
    write_encoded_wav(&path, 17, 16, 4000 * 25 - 7, &25_u16.to_le_bytes(), &adpcm);
    files.push(path);

    let cancel = AtomicBool::new(false);
    for path in &files {
        for samples_per_pixel in [97_u32, 4096] {
            let request = WaveformRequest {
                samples_per_pixel,
                ..WaveformRequest::default()
            };
            let reference = waveform_with_cancel(path, request.clone(), &cancel).unwrap();
            assert!(!reference.peaks[0].is_empty(), "{path:?}");
            for read_buffer in [7, 4096, 2 * 1024 * 1024] {
                let buffered =
                    waveform_with_read_buffer(path, request.clone(), &cancel, read_buffer).unwrap();
                assert_eq!(
                    format!("{:?}", buffered.peaks),
                    format!("{:?}", reference.peaks),
                    "{path:?} spp={samples_per_pixel} buffer={read_buffer}"
                );
                assert_eq!(buffered.frames, reference.frames);
            }
        }
    }
}
#[test]
fn single_open_summary_matches_probe_then_waveform_with_trailing_chunks() {
    fn append_trailing(path: &Path) {
        let mut wav = fs::read(path).unwrap();
        let mut tail = Vec::new();
        chunk(b"ID3 ", &[b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0], &mut tail);
        chunk(b"SMED", &[7; 333], &mut tail);
        chunk(b"iXML", b"<BWFXML><TAKE>3</TAKE></BWFXML>", &mut tail);
        chunk(b"_PMX", b"<x:xmpmeta xmlns:x='adobe:ns:meta/'/>", &mut tail);
        chunk(b"LIST", b"INFOINAM\x04\x00\x00\x00tail", &mut tail);
        wav.extend_from_slice(&tail);
        if &wav[..4] == b"RIFF" {
            let size = (wav.len() - 8) as u32;
            wav[4..8].copy_from_slice(&size.to_le_bytes());
        }
        fs::write(path, wav).unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    let pattern = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 2_654_435_761) >> 13) as u8)
            .collect()
    };
    let mut files = Vec::new();
    for (name, rate, channels, bits, tag, frames, rf64) in [
        (
            "pcm16.wav",
            48000_u32,
            2_u16,
            16_u16,
            1_u16,
            70_001_usize,
            false,
        ),
        ("pcm24-rf64.wav", 96000, 1, 24, 1, 50_000, true),
        ("float-rf64.wav", 44100, 2, 32, 3, 33_333, true),
        ("short.wav", 48000, 1, 16, 1, 100, false),
        ("empty.wav", 48000, 1, 16, 1, 0, false),
    ] {
        let path = temp.path().join(name);
        let bytes = frames * usize::from(channels) * usize::from(bits / 8);
        let samples = if tag == 3 {
            (0..bytes / 4)
                .flat_map(|i| ((i as f32 * 0.003).sin()).to_le_bytes())
                .collect()
        } else {
            pattern(bytes)
        };
        write_wav(&path, rate, channels, bits, tag, &samples, true, rf64);
        files.push(path);
    }
    let path = temp.path().join("mulaw.wav");
    write_encoded_wav(&path, 7, 1, 90_001, &[], &pattern(90_001));
    files.push(path);
    let mut adpcm = Vec::new();
    for block in 0..2000 {
        adpcm.extend_from_slice(&((block * 37 % 3000) as i16).to_le_bytes());
        adpcm.extend_from_slice(&[0, 0]);
        adpcm.extend_from_slice(&pattern(12));
    }
    let path = temp.path().join("adpcm.wav");
    write_encoded_wav(&path, 17, 16, 2000 * 25 - 7, &25_u16.to_le_bytes(), &adpcm);
    files.push(path);

    let cancel = AtomicBool::new(false);
    for path in &files {
        for trailing in [false, true] {
            if trailing {
                append_trailing(path);
            }
            let metadata = probe(path).unwrap();
            let samples_per_pixel = metadata.frames.div_ceil(128).max(1) as u32;
            let probed = waveform_with_read_buffer(
                path,
                WaveformRequest {
                    samples_per_pixel,
                    include_colors: false,
                    ..WaveformRequest::default()
                },
                &cancel,
                2 << 20,
            )
            .unwrap();
            for read_buffer in [0, 2 << 20] {
                let single =
                    summary_waveform_with_read_buffer(path, 128, &cancel, read_buffer).unwrap();
                let context = format!("{path:?} trailing={trailing} buffer={read_buffer}");
                assert_eq!(single.frames, metadata.frames, "{context}");
                assert_eq!(single.samples_per_pixel, samples_per_pixel, "{context}");
                assert_eq!(
                    format!("{:?}", single.peaks),
                    format!("{:?}", probed.peaks),
                    "{context}"
                );
                assert_eq!(
                    single.peaks[0].is_empty(),
                    metadata.frames == 0,
                    "{context}"
                );
            }
        }
    }
}
