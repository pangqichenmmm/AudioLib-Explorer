// crates/audiolib-audio/src/format_pipeline_tests.rs
//! Source-format, decoder, seek, Waveform, and bounded network-startup regressions.
use crate::{
    decoder::Decoder,
    probe,
    source::{
        ByteSource, SourceBuffer, SourceOpener, register_buffered_stream_source_with_metadata,
        register_stream_source, unregister_stream_source,
    },
    waveform::{WaveformData, WaveformRequest, waveform},
};
use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

const CBR: &[u8] = include_bytes!("fixtures/mp3/cbr-info.mp3");
const VBR: &[u8] = include_bytes!("fixtures/mp3/vbr-xing.mp3");
const NO_XING: &[u8] = include_bytes!("fixtures/mp3/cbr-no-xing.mp3");

fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    if !payload.len().is_multiple_of(2) {
        bytes.push(0);
    }
    bytes
}

struct EncodedWave {
    tag: u16,
    align: u16,
    bits: u16,
    frames: u32,
    extra: Vec<u8>,
    audio: Vec<u8>,
}

fn mulaw() -> EncodedWave {
    EncodedWave {
        tag: 7,
        align: 1,
        bits: 8,
        frames: 4000,
        extra: Vec::new(),
        // A deterministic changing signal makes a wrong seek position visible.
        audio: (0..4000)
            .map(|index| ((index * 37 + index / 101) % 256) as u8)
            .collect(),
    }
}

fn ima_adpcm() -> EncodedWave {
    let mut audio = Vec::new();
    for block in 0..80_i16 {
        audio.extend_from_slice(&(500 + block * 250).to_le_bytes());
        audio.extend_from_slice(&[0, 0]);
        audio.extend_from_slice(&[0; 12]);
    }
    EncodedWave {
        tag: 17,
        align: 16,
        bits: 4,
        frames: 80 * 25 - 7,
        extra: 25_u16.to_le_bytes().to_vec(),
        audio,
    }
}

fn pcm16() -> EncodedWave {
    EncodedWave {
        tag: 1,
        align: 2,
        bits: 16,
        frames: 4000,
        extra: Vec::new(),
        audio: (0..4000_i16).flat_map(i16::to_le_bytes).collect(),
    }
}

fn encoded_wave(id: &[u8; 4], encoded: &EncodedWave) -> (Vec<u8>, u64) {
    let large = id != b"RIFF";
    let mut body = Vec::new();
    if large {
        let mut ds64 = vec![0; 28];
        ds64[8..16].copy_from_slice(&(encoded.audio.len() as u64).to_le_bytes());
        ds64[16..24].copy_from_slice(&u64::from(encoded.frames).to_le_bytes());
        body.extend(chunk(b"ds64", &ds64));
    }
    let mut format = Vec::new();
    format.extend_from_slice(&encoded.tag.to_le_bytes());
    format.extend_from_slice(&1_u16.to_le_bytes());
    format.extend_from_slice(&8000_u32.to_le_bytes());
    let samples_per_block = if encoded.tag == 17 { 25 } else { 1 };
    format.extend_from_slice(&(8000 * u32::from(encoded.align) / samples_per_block).to_le_bytes());
    format.extend_from_slice(&encoded.align.to_le_bytes());
    format.extend_from_slice(&encoded.bits.to_le_bytes());
    format.extend_from_slice(&(encoded.extra.len() as u16).to_le_bytes());
    format.extend_from_slice(&encoded.extra);
    body.extend(chunk(b"fmt ", &format));
    body.extend(chunk(b"fact", &encoded.frames.to_le_bytes()));
    let data_offset = body.len() as u64 + 20;
    body.extend_from_slice(b"data");
    body.extend_from_slice(
        &(if large {
            u32::MAX
        } else {
            encoded.audio.len() as u32
        })
        .to_le_bytes(),
    );
    body.extend_from_slice(&encoded.audio);
    if !encoded.audio.len().is_multiple_of(2) {
        body.push(0);
    }
    // Both valid in-container Metadata and private external trailer must remain
    // irrelevant to compressed decoder packetization and network startup.
    body.extend(chunk(b"LIST", b"INFOINAM\x04\0\0\0test"));
    let riff_size = body.len() as u64 + 4;
    if large {
        body[8..16].copy_from_slice(&riff_size.to_le_bytes());
    }
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&(if large { u32::MAX } else { riff_size as u32 }).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend(body);
    bytes.extend_from_slice(b"ID3Helper outside the declared parent");
    (bytes, data_offset)
}

fn decode_all(path: &Path) -> Vec<f32> {
    let mut decoder = Decoder::open(path).unwrap();
    let mut samples = Vec::new();
    let channels = usize::from(decoder.info.metadata.channels);
    let expected = decoder.info.metadata.frames as usize * channels;
    loop {
        let next = decoder.read(257).unwrap();
        if next.is_empty() {
            break;
        }
        samples.extend(next);
        assert!(
            samples.len() <= expected,
            "decoder exceeded its presentation duration"
        );
    }
    samples
}

fn summary(path: &Path) -> WaveformData {
    waveform(
        path,
        WaveformRequest {
            samples_per_pixel: 127,
            include_colors: false,
            ..WaveformRequest::default()
        },
    )
    .unwrap()
}

fn assert_waveform_equal(actual: &WaveformData, expected: &WaveformData) {
    assert_eq!(
        (actual.frames, actual.channels, actual.sample_rate),
        (expected.frames, expected.channels, expected.sample_rate)
    );
    assert_eq!(actual.peaks.len(), expected.peaks.len());
    for (actual, expected) in actual.peaks.iter().zip(&expected.peaks) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(
                (actual.min, actual.max, actual.rms),
                (expected.min, expected.max, expected.rms)
            );
        }
    }
}

#[test]
fn compressed_rf64_and_bw64_match_riff_decoding_seek_and_waveform() {
    let root = tempfile::tempdir().unwrap();
    for (codec, encoded) in [("mulaw", mulaw()), ("ima-adpcm", ima_adpcm())] {
        let reference_path = root.path().join(format!("reference-{codec}.wav"));
        fs::write(&reference_path, encoded_wave(b"RIFF", &encoded).0).unwrap();
        let expected = decode_all(&reference_path);
        assert_eq!(expected.len(), encoded.frames as usize);
        let expected_wave = summary(&reference_path);
        for id in [b"RF64", b"BW64"] {
            let container = std::str::from_utf8(id).unwrap();
            let path = root.path().join(format!("{container}-{codec}.wav"));
            fs::write(&path, encoded_wave(id, &encoded).0).unwrap();
            let metadata = probe(&path).unwrap();
            assert_eq!(metadata.attributes["container"], container);
            assert_eq!(metadata.frames, u64::from(encoded.frames));
            assert_eq!(decode_all(&path), expected, "{container} {codec}");
            let mut decoder = Decoder::open(&path).unwrap();
            for frame in [
                0,
                1,
                24,
                25,
                encoded.frames / 2,
                encoded.frames - 1,
                encoded.frames,
            ] {
                decoder.seek(u64::from(frame)).unwrap();
                let actual = decoder.read(71).unwrap();
                let end = (frame as usize + 71).min(expected.len());
                assert_eq!(
                    actual,
                    expected[frame as usize..end],
                    "{container} {codec} frame={frame}"
                );
            }
            assert_waveform_equal(&summary(&path), &expected_wave);
        }
    }
}

struct Registration(PathBuf);
impl Drop for Registration {
    fn drop(&mut self) {
        unregister_stream_source(&self.0);
    }
}

#[derive(Default)]
struct Access {
    opens: AtomicUsize,
    highest_read: AtomicU64,
    blocked: AtomicUsize,
}

struct PrefixSource {
    bytes: Arc<Vec<u8>>,
    at: u64,
    limit: u64,
    access: Arc<Access>,
}
impl PrefixSource {
    fn forbidden(&self) -> io::Error {
        self.access.blocked.fetch_add(1, Ordering::Relaxed);
        io::Error::other("source attempted to fetch beyond its initial network prefix")
    }
}
impl Read for PrefixSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.at >= self.limit {
            return Err(self.forbidden());
        }
        let count = out
            .len()
            .min((self.limit - self.at) as usize)
            .min(self.bytes.len().saturating_sub(self.at as usize));
        out[..count].copy_from_slice(&self.bytes[self.at as usize..self.at as usize + count]);
        self.at += count as u64;
        self.access
            .highest_read
            .fetch_max(self.at, Ordering::Relaxed);
        Ok(count)
    }
}
impl Seek for PrefixSource {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let at = match position {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(offset) => self.at.checked_add_signed(offset),
            SeekFrom::End(_) => None,
        };
        match at {
            Some(at) if at <= self.limit => {
                self.at = at;
                Ok(at)
            }
            _ => Err(self.forbidden()),
        }
    }
}

fn prefix_opener(bytes: Vec<u8>, limit: u64, access: Arc<Access>) -> SourceOpener {
    let bytes = Arc::new(bytes);
    Arc::new(move || {
        access.opens.fetch_add(1, Ordering::Relaxed);
        let source: Box<dyn ByteSource> = Box::new(PrefixSource {
            bytes: bytes.clone(),
            at: 0,
            limit,
            access: access.clone(),
        });
        Ok((source, bytes.len() as u64))
    })
}

#[test]
fn wav_playback_layout_never_reads_audio_or_metadata_tails_from_registered_sources() {
    let root = tempfile::tempdir().unwrap();
    for id in [b"RIFF", b"RF64", b"BW64"] {
        for (codec, encoded) in [("mulaw", mulaw()), ("ima", ima_adpcm()), ("pcm", pcm16())] {
            let (bytes, data_offset) = encoded_wave(id, &encoded);
            let path = root.path().join(format!(
                "header-{}-{codec}.opaque",
                std::str::from_utf8(id).unwrap()
            ));
            let access = Arc::new(Access::default());
            register_stream_source(
                path.clone(),
                prefix_opener(bytes, data_offset, access.clone()),
            );
            let _registration = Registration(path.clone());
            let layout = crate::wav::read_info(&path).unwrap();
            assert_eq!(layout.metadata.frames, u64::from(encoded.frames));
            assert_eq!(layout.data_offset, data_offset);
            assert_eq!(access.highest_read.load(Ordering::Relaxed), data_offset);
            assert_eq!(access.blocked.load(Ordering::Relaxed), 0);
        }
    }
}

struct PrefixBuffer(u64);
impl SourceBuffer for PrefixBuffer {
    fn is_buffered(&self, offset: u64, len: u64) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.0)
    }
}

#[test]
fn cached_no_xing_mp3_starts_from_a_small_network_prefix_on_an_opaque_wav_path() {
    let root = tempfile::tempdir().unwrap();
    let bytes = NO_XING.repeat(100);
    assert!(bytes.len() > 64 * 1024);
    let local = root.path().join("large-no-xing.bin");
    fs::write(&local, &bytes).unwrap();
    let metadata = probe(&local).unwrap();
    assert_eq!(metadata.frames, 100 * 4032);
    // Without a gapless tag, the initial coded frames legitimately retain the
    // encoder's unknown leading delay. Compare the actual timeline and read far
    // enough to include the signal instead of assuming the first 1024 are audible.
    let expected = Decoder::open(&local).unwrap().read(4096).unwrap();
    for limit in [16 * 1024, 32 * 1024] {
        let path = root.path().join(format!("remote-{limit}.wav"));
        let access = Arc::new(Access::default());
        register_buffered_stream_source_with_metadata(
            path.clone(),
            prefix_opener(bytes.clone(), limit, access.clone()),
            Arc::new(PrefixBuffer(limit)),
            metadata.clone(),
        );
        let _registration = Registration(path.clone());
        let mut decoder = Decoder::open(&path).unwrap();
        assert_eq!(decoder.info.metadata.attributes["container"], "MP3");
        assert_eq!(decoder.info.metadata.frames, metadata.frames);
        let pcm = decoder.read(4096).unwrap();
        assert_eq!(pcm.len(), 4096);
        assert_eq!(pcm, expected);
        assert!(pcm.iter().any(|sample| sample.abs() > 0.01));
        assert!(access.opens.load(Ordering::Relaxed) >= 2);
        assert!(access.highest_read.load(Ordering::Relaxed) <= limit);
        assert_eq!(access.blocked.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn local_mp3_probe_decode_seek_and_waveform_share_the_presentation_timeline() {
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [("cbr", CBR), ("vbr", VBR), ("no-xing", NO_XING)] {
        let path = root.path().join(format!("{name}.opaque"));
        fs::write(&path, bytes).unwrap();
        let metadata = probe(&path).unwrap();
        let reference = decode_all(&path);
        let channels = usize::from(metadata.channels);
        let frames = metadata.frames as usize;
        assert_eq!(
            reference.len(),
            frames * channels,
            "{name}: decoded length disagrees with Metadata"
        );
        assert!(reference.iter().all(|sample| sample.is_finite()));
        assert!(reference.iter().any(|sample| sample.abs() > 0.01));
        let mut decoder = Decoder::open(&path).unwrap();
        for frame in [
            0,
            1,
            frames / 2,
            frames.saturating_sub(257),
            frames - 1,
            frames,
        ] {
            decoder.seek(frame as u64).unwrap();
            let actual = decoder.read(256).unwrap();
            let end = (frame + 256).min(frames);
            let expected = &reference[frame * channels..end * channels];
            assert_eq!(actual.len(), expected.len(), "{name} frame={frame}");
            let max_error = actual
                .iter()
                .zip(expected)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f32, f32::max);
            let rms_error = (actual
                .iter()
                .zip(expected)
                .map(|(a, b)| (a - b).powi(2))
                .sum::<f32>()
                / actual.len().max(1) as f32)
                .sqrt();
            // MP3 reset/seek can have small synthesis-filter differences. The
            // RMS bound is under 4% of these fixtures' ordinary signal RMS;
            // shifting by a MPEG frame produces a much larger phase error.
            assert!(
                max_error <= 0.015 && rms_error <= 0.003,
                "{name} frame={frame}: max={max_error} rms={rms_error}"
            );
        }
        let wave = summary(&path);
        assert_eq!(
            (wave.frames, wave.channels),
            (metadata.frames, metadata.channels)
        );
        assert_eq!(wave.peaks.len(), channels);
        assert!(
            wave.peaks
                .iter()
                .all(|channel| channel.len() == frames.div_ceil(127))
        );
        assert!(
            wave.peaks
                .iter()
                .flatten()
                .all(|peak| peak.min.is_finite() && peak.max.is_finite() && peak.rms.is_finite())
        );
    }
}

#[test]
fn inconsistent_xing_frame_counts_cannot_override_the_verified_decode_duration() {
    let root = tempfile::tempdir().unwrap();
    let reference_path = root.path().join("reference.mp3");
    fs::write(&reference_path, CBR).unwrap();
    let expected = decode_all(&reference_path);
    for declared in [0_u32, 1, 999999] {
        let mut bytes = CBR.to_vec();
        // This Info header's frame count conflicts with the seven actual audio
        // frames. The bounded extractor ignores it; playback must use that same
        // verified timeline instead of allowing an optional tag to trim audio.
        bytes[44..48].copy_from_slice(&declared.to_be_bytes());
        let path = root.path().join(format!("stale-count-{declared}.mp3"));
        fs::write(&path, bytes).unwrap();
        let metadata = probe(&path).unwrap();
        assert_eq!(metadata.frames, 6000);
        let actual = decode_all(&path);
        assert_eq!(
            actual.len(),
            expected.len(),
            "declared Xing frame count={declared}"
        );
        assert_eq!(actual, expected, "declared Xing frame count={declared}");
    }
}

#[test]
fn unsupported_wav_widths_cannot_wrap_into_supported_typed_layouts() {
    let root = tempfile::tempdir().unwrap();
    for (tag, bits, align) in [(1, 2064, 258), (3, 2080, 260), (1, 64, 8), (3, 16, 2)] {
        let encoded = EncodedWave {
            tag,
            bits,
            align,
            frames: 1,
            extra: Vec::new(),
            audio: vec![0; usize::from(align)],
        };
        let path = root.path().join(format!("unsupported-{tag}-{bits}.wav"));
        fs::write(&path, encoded_wave(b"RIFF", &encoded).0).unwrap();
        assert_eq!(probe(&path).unwrap().bit_depth, bits);
        assert!(
            Decoder::open(&path).is_err(),
            "tag={tag} bits={bits} align={align}"
        );
    }
}

#[test]
fn long_no_xing_mp3_seek_resets_from_nonzero_anchors_without_losing_sample_position() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("long-no-xing.mp3");
    fs::write(&path, NO_XING.repeat(100)).unwrap();
    let metadata = probe(&path).unwrap();
    let reference = decode_all(&path);
    assert_eq!(reference.len() as u64, metadata.frames);
    let mut decoder = Decoder::open(&path).unwrap();
    for frame in [metadata.frames / 2, metadata.frames - 769] {
        assert!(frame > 32 * 1152);
        decoder.seek(frame).unwrap();
        let actual = decoder.read(512).unwrap();
        let expected = &reference[frame as usize..frame as usize + 512];
        assert_eq!(actual.len(), expected.len());
        let max_error = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        let rms_error = (actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f32>()
            / actual.len() as f32)
            .sqrt();
        assert!(
            max_error <= 0.015 && rms_error <= 0.003,
            "frame={frame}: max={max_error} rms={rms_error}"
        );
    }
}

#[test]
fn cached_mp3_information_frame_claims_cannot_skip_audio_or_include_an_information_frame() {
    let root = tempfile::tempdir().unwrap();
    for (name, bytes, claimed_headers) in [("no-xing", NO_XING, 1_u64), ("info", CBR, 0_u64)] {
        let local = root.path().join(format!("actual-{name}.mp3"));
        fs::write(&local, bytes).unwrap();
        let mut metadata = probe(&local).unwrap();
        let layout = &metadata.attributes["source_audio_layout"];
        let frames = (layout["mpeg_frame_count"].as_u64().unwrap() - claimed_headers)
            * layout["samples_per_frame"].as_u64().unwrap();
        metadata.frames = frames;
        metadata.duration = frames as f64 / f64::from(metadata.sample_rate);
        let layout = &mut metadata.attributes["source_audio_layout"];
        layout["header_frames"] = serde_json::json!(claimed_headers);
        layout["encoder_delay"] = serde_json::json!(0);
        layout["end_padding"] = serde_json::json!(0);
        layout["frames"] = serde_json::json!(frames);
        let path = root.path().join(format!("forged-{name}.wav"));
        let access = Arc::new(Access::default());
        register_buffered_stream_source_with_metadata(
            path.clone(),
            prefix_opener(bytes.to_vec(), bytes.len() as u64, access.clone()),
            Arc::new(PrefixBuffer(bytes.len() as u64)),
            metadata,
        );
        let _registration = Registration(path.clone());
        assert!(
            Decoder::open(&path).is_err(),
            "{name}: false information-frame claim was accepted"
        );
        assert_eq!(access.blocked.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn malformed_xing_flags_keep_metadata_and_actual_decoder_duration_consistent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("malformed-flags.mp3");
    let mut bytes = CBR.to_vec();
    bytes[40..44].copy_from_slice(&0x8000_000f_u32.to_be_bytes());
    fs::write(&path, bytes).unwrap();
    let metadata = probe(&path).unwrap();
    assert_eq!(metadata.frames, 7 * 1152);
    let samples = decode_all(&path);
    assert_eq!(
        samples.len() as u64,
        metadata.frames * u64::from(metadata.channels)
    );
    assert!(samples.iter().all(|sample| sample.is_finite()));
    assert!(samples.iter().any(|sample| sample.abs() > 0.01));
}

#[test]
fn mp3_trusted_prefix_matches_native_symphonia_gapless_samples_including_both_edges() {
    use symphonia::core::{
        codecs::audio::AudioDecoder,
        formats::{FormatReader, TrackType},
        io::MediaSourceStream,
    };

    fn native_samples(bytes: &[u8]) -> Vec<f32> {
        // Feed the original bytes directly to Symphonia. No project probe,
        // cached layout, synthesized prefix, presentation cap, or seek is used.
        let adapter = crate::source::MediaAdapter::new(
            Box::new(std::io::Cursor::new(bytes.to_vec())),
            bytes.len() as u64,
        );
        let stream = MediaSourceStream::new(Box::new(adapter), Default::default());
        let mut reader =
            symphonia::default::formats::MpaReader::try_new(stream, Default::default()).unwrap();
        let track = reader.default_track(TrackType::Audio).unwrap();
        let track_id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .unwrap();
        let mut decoder: Box<dyn AudioDecoder> = symphonia::default::get_codecs()
            .make_audio_decoder(params, &Default::default())
            .unwrap();
        let mut output = Vec::new();
        while let Some(packet) = reader.next_packet().unwrap() {
            if packet.track_id != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).unwrap();
            let mut samples = vec![0.0; decoded.samples_interleaved()];
            decoded.copy_to_slice_interleaved(&mut samples);
            output.extend(samples);
        }
        output
    }

    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [("cbr", CBR), ("vbr", VBR), ("no-xing", NO_XING)] {
        let expected = native_samples(bytes);
        let path = root.path().join(format!("native-reference-{name}.mp3"));
        fs::write(&path, bytes).unwrap();
        let actual = decode_all(&path);
        assert!(!expected.is_empty());
        assert_eq!(
            actual.len(),
            expected.len(),
            "{name}: gapless presentation length"
        );
        for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(actual, expected, "{name}: presentation sample {index}");
        }
    }
}
