// crates/audiolib-audio/src/mp3_metadata_tests.rs
use super::*;
use std::io::Cursor;

const CBR: &[u8] = include_bytes!("fixtures/mp3/cbr-info.mp3");
const VBR: &[u8] = include_bytes!("fixtures/mp3/vbr-xing.mp3");
const NO_XING: &[u8] = include_bytes!("fixtures/mp3/cbr-no-xing.mp3");

fn probe(bytes: &[u8]) -> WavInfo {
    read_info(Cursor::new(bytes), bytes.len() as u64, true, false).unwrap()
}

fn frame(id: &[u8; 4], value: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(value);
    bytes
}

fn id3(frames: &[u8]) -> Vec<u8> {
    let mut bytes = b"ID3\x03\0\0".to_vec();
    for shift in [21, 14, 7, 0] {
        bytes.push(((frames.len() >> shift) & 127) as u8);
    }
    bytes.extend_from_slice(frames);
    bytes
}

#[test]
fn cbr_vbr_and_headerless_duration_comes_from_actual_frames_and_gapless_padding() {
    for (bytes, rate, channels, frames, kind) in [
        (CBR, 48000, 2, 6000, Some("Info")),
        (VBR, 44100, 1, 8820, Some("Xing")),
        (NO_XING, 24000, 1, 4032, None),
    ] {
        let info = probe(bytes);
        let metadata = &info.metadata;
        assert_eq!(
            (metadata.sample_rate, metadata.channels, metadata.frames),
            (rate, channels, frames)
        );
        assert_eq!(metadata.bit_depth, 0);
        assert_eq!(metadata.encoding, "MP3");
        assert_eq!(metadata.attributes["container"], "MP3");
        assert_eq!(
            metadata.attributes["wav_metadata"]["mp3_version"],
            MP3_METADATA_VERSION
        );
        assert_eq!(
            metadata.attributes["wav_metadata"]["mp3_header"].as_str(),
            kind
        );
        assert_eq!(metadata.duration, frames as f64 / f64::from(rate));
        assert_eq!(info.data_offset, 0);
        assert_eq!(info.data_bytes, bytes.len() as u64);
        assert_eq!((info.block_align, info.format_tag), (0, 0));
        assert!(info.pcm_layout.is_none());
    }
}

#[test]
fn utf16_id3_and_duplicate_unknown_text_survive_without_binary_payloads() {
    let mut title = vec![1, 0xff, 0xfe];
    title.extend("中文音效库".encode_utf16().flat_map(u16::to_le_bytes));
    let mut frames = frame(b"TIT2", &title);
    frames.extend(frame(b"TXXX", b"\0VendorCustom\0first value"));
    frames.extend(frame(b"TXXX", b"\0VendorCustom\0second value"));
    frames.extend(frame(b"TZZZ", b"\0future text field"));
    frames.extend(frame(b"PRIV", b"secret-binary-marker"));
    let mut tagged = id3(&frames);
    let offset = tagged.len();
    tagged.extend_from_slice(CBR);
    let info = probe(&tagged);
    let metadata = &info.metadata.attributes["wav_metadata"];
    assert_eq!(metadata["fields"]["title"], "中文音效库");
    assert_eq!(
        metadata["id3"]["TXXX/VendorCustom"],
        json!(["first value", "second value"])
    );
    assert_eq!(metadata["id3"]["TZZZ"], "future text field");
    assert!(!metadata.to_string().contains("secret-binary-marker"));
    assert_eq!(info.data_offset, offset as u64);
    assert_eq!(info.data_bytes, CBR.len() as u64);
    assert_eq!(info.metadata.frames, 6000);
}

#[test]
fn optional_id3_corruption_warns_without_rejecting_valid_audio() {
    let mut bad_frame = frame(b"TIT2", b"\0short");
    bad_frame[4..8].copy_from_slice(&99999_u32.to_be_bytes());
    for tag in [
        id3(&bad_frame),
        b"ID3\x03\0\0\xff\xff\xff\xff".to_vec(),
        b"ID3\x03\0\0\x7f\x7f\x7f\x7f".to_vec(),
        b"ID3\x04\0\x10\0\0\0\0".to_vec(),
    ] {
        let mut bytes = tag;
        bytes.extend_from_slice(CBR);
        let metadata = probe(&bytes).metadata;
        assert_eq!(metadata.frames, 6000);
        assert!(
            metadata.attributes["wav_metadata"]["warnings"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
        );
    }
}

#[test]
fn id3v1_is_a_bounded_fallback_and_does_not_replace_id3v2() {
    let mut tag = [0; 128];
    tag[..3].copy_from_slice(b"TAG");
    tag[3..17].copy_from_slice(b"fallback title");
    tag[33..46].copy_from_slice(b"legacy artist");
    tag[63..75].copy_from_slice(b"legacy album");
    tag[97..111].copy_from_slice(b"legacy comment");
    tag[126] = 7;
    tag[127] = 12;
    let mut bytes = NO_XING.to_vec();
    bytes.extend_from_slice(&tag);
    let metadata = probe(&bytes).metadata;
    assert_eq!(metadata.frames, 4032);
    assert_eq!(
        metadata.attributes["wav_metadata"]["fields"]["title"],
        "fallback title"
    );
    assert_eq!(metadata.attributes["wav_metadata"]["id3v1"]["track"], "7");
    assert_eq!(
        metadata.attributes["source_audio_layout"]["data_bytes"],
        NO_XING.len()
    );

    let mut tagged = id3(&frame(b"TIT2", b"\0new title"));
    tagged.extend(bytes);
    let metadata = probe(&tagged).metadata;
    assert_eq!(
        metadata.attributes["wav_metadata"]["fields"]["title"],
        "new title"
    );
    assert_eq!(
        metadata.attributes["wav_metadata"]["id3v1"]["title"],
        "fallback title"
    );
    assert_eq!(
        metadata.attributes["wav_metadata"]["fields"]["artist"],
        "legacy artist"
    );
}

#[test]
fn apic_is_opt_in_and_kept_out_of_metadata_json() {
    let image = b"\x89PNG\r\n\x1a\nsynthetic-cover-marker";
    let mut apic = b"\0image/png\0\x03\0".to_vec();
    apic.extend_from_slice(image);
    let mut bytes = id3(&frame(b"APIC", &apic));
    bytes.extend_from_slice(CBR);
    let ordinary = probe(&bytes);
    assert!(ordinary.artwork.is_none());
    assert!(
        !ordinary
            .metadata
            .attributes
            .to_string()
            .contains("synthetic-cover-marker")
    );
    let with_cover = read_info(Cursor::new(&bytes), bytes.len() as u64, true, true).unwrap();
    let cover = with_cover.artwork.unwrap();
    assert_eq!(cover.bytes, image);
    assert_eq!(cover.mime, "image/png");
    assert!(
        !with_cover
            .metadata
            .attributes
            .to_string()
            .contains("synthetic-cover-marker")
    );
}

#[test]
fn bad_audio_headers_and_truncated_frames_are_rejected() {
    for bytes in [CBR, VBR, NO_XING] {
        for removed in [1, 3, 25] {
            let short = &bytes[..bytes.len() - removed];
            assert!(read_info(Cursor::new(short), short.len() as u64, true, false).is_err());
        }
        let first = header(bytes).unwrap();
        let mut corrupt = bytes.to_vec();
        corrupt[first.size as usize] = 0;
        assert!(read_info(Cursor::new(&corrupt), corrupt.len() as u64, true, false).is_err());
    }
    assert!(read_info(Cursor::new(b"ID3 no audio"), 12, true, false).is_err());
    assert!(read_info(Cursor::new(b"RIFF\0\0\0\0WAVE"), 12, true, false).is_err());
    let mut tagged = id3(&frame(b"TIT2", b"\0valid title"));
    let offset = tagged.len();
    tagged.extend_from_slice(CBR);
    tagged[offset] = 0;
    assert!(read_info(Cursor::new(&tagged), tagged.len() as u64, true, false).is_err());
}

#[test]
fn stale_xing_frame_counts_do_not_override_actual_audio_duration() {
    let mut bytes = CBR.to_vec();
    bytes[44..48].copy_from_slice(&999999_u32.to_be_bytes());
    let metadata = probe(&bytes).metadata;
    assert_eq!(metadata.frames, 6000);
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
}

#[test]
fn corrupt_lame_checksum_does_not_supply_gapless_padding() {
    let mut bytes = CBR.to_vec();
    // This Info frame has all four optional Xing fields, placing its encoder
    // extension at byte 156. A LAME extension requires a matching tag CRC.
    bytes[156..160].copy_from_slice(b"LAME");
    bytes[190..192].copy_from_slice(&0_u16.to_be_bytes());
    assert_ne!(tag_crc(&bytes[..190]), 0);
    let metadata = probe(&bytes).metadata;
    assert_eq!(metadata.frames, 7 * 1152);
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
}

#[test]
fn vbri_header_uses_the_actual_frame_count_without_bitrate_estimation() {
    let mut bytes = CBR.to_vec();
    bytes[36..384].fill(0);
    bytes[36..40].copy_from_slice(b"VBRI");
    bytes[40..42].copy_from_slice(&1_u16.to_be_bytes());
    let len = bytes.len() as u32;
    bytes[46..50].copy_from_slice(&len.to_be_bytes());
    bytes[50..54].copy_from_slice(&7_u32.to_be_bytes());
    let metadata = probe(&bytes).metadata;
    assert_eq!(metadata.frames, 7 * 1152);
    assert_eq!(metadata.attributes["wav_metadata"]["mp3_header"], "VBRI");
}

#[test]
fn cached_layout_validates_actual_header_with_only_a_bounded_read() {
    struct HeaderOnly(Cursor<Vec<u8>>);
    impl Read for HeaderOnly {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.position() + out.len() as u64 > 4 {
                return Err(std::io::Error::other("attempted audio scan"));
            }
            self.0.read(out)
        }
    }
    impl Seek for HeaderOnly {
        fn seek(&mut self, at: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(at)
        }
    }
    let metadata = probe(CBR).metadata;
    let mut source = HeaderOnly(Cursor::new(CBR.to_vec()));
    let result = cached_info(&mut source, CBR.len() as u64, metadata.clone()).unwrap();
    assert_eq!(result.metadata.frames, 6000);
    assert!(result.pcm_layout.is_none());
    for key in [
        "version",
        "source_len",
        "sample_rate",
        "channels",
        "frames",
        "data_offset",
        "data_bytes",
        "mpeg_frame_count",
        "samples_per_frame",
        "header_frames",
        "encoder_delay",
        "end_padding",
    ] {
        let mut invalid = metadata.clone();
        invalid.attributes["source_audio_layout"][key] = json!(u64::MAX);
        assert!(
            cached_info(&mut Cursor::new(CBR), CBR.len() as u64, invalid).is_err(),
            "{key}"
        );
    }
    let mut wrong = CBR.to_vec();
    wrong[2] &= !12;
    assert!(cached_info(&mut Cursor::new(wrong), CBR.len() as u64, metadata).is_err());
}

#[test]
fn cached_layout_rejects_unrepresentable_padding_and_zero_audio_counts() {
    let metadata = probe(CBR).metadata;
    for (delay, padding, count, headers) in [
        (1105_u64, 3567_u64, 8_u64, 1_u64),
        (528, 0, 8, 1),
        (0, 0, 1, 1),
    ] {
        let mut invalid = metadata.clone();
        let frames = (count - headers) * 1152 - delay - padding;
        invalid.frames = frames;
        invalid.duration = frames as f64 / f64::from(invalid.sample_rate);
        let layout = &mut invalid.attributes["source_audio_layout"];
        layout["encoder_delay"] = json!(delay);
        layout["end_padding"] = json!(padding);
        layout["mpeg_frame_count"] = json!(count);
        layout["header_frames"] = json!(headers);
        layout["frames"] = json!(frames);
        assert!(cached_info(&mut Cursor::new(CBR), CBR.len() as u64, invalid).is_err());
    }
}

#[test]
fn trusted_decoder_prefix_preserves_verified_duration_and_replaces_optional_headers() {
    for bytes in [CBR, VBR, NO_XING] {
        let info = probe(bytes);
        let (prefix, skip) = decoder_prefix(&mut Cursor::new(bytes), &info).unwrap();
        let original_header = header(bytes).unwrap();
        let has_header = info.metadata.attributes["source_audio_layout"]["header_frames"] == 1;
        assert_eq!(skip, if has_header { original_header.size } else { 0 });
        let synthesized = header(&prefix).unwrap();
        assert_eq!(
            (synthesized.rate, synthesized.channels),
            (info.metadata.sample_rate, info.metadata.channels)
        );
        assert!(!synthesized.has_crc);
        let mut clean = prefix;
        clean.extend_from_slice(&bytes[skip as usize..]);
        assert_eq!(probe(&clean).metadata.frames, info.metadata.frames);
    }
    let mut corrupt = CBR.to_vec();
    corrupt[44..48].copy_from_slice(&1_u32.to_be_bytes());
    let info = probe(&corrupt);
    let (prefix, skip) = decoder_prefix(&mut Cursor::new(&corrupt), &info).unwrap();
    let mut clean = prefix;
    clean.extend_from_slice(&corrupt[skip as usize..]);
    assert_eq!(probe(&clean).metadata.frames, 6000);
}

#[test]
fn trusted_prefix_checks_actual_information_frame_presence_in_both_directions() {
    for (bytes, claimed_headers) in [(NO_XING, 1_u64), (CBR, 0_u64)] {
        let mut metadata = probe(bytes).metadata;
        let layout = &metadata.attributes["source_audio_layout"];
        let frames = (layout["mpeg_frame_count"].as_u64().unwrap() - claimed_headers)
            * layout["samples_per_frame"].as_u64().unwrap();
        metadata.frames = frames;
        metadata.duration = frames as f64 / f64::from(metadata.sample_rate);
        let layout = &mut metadata.attributes["source_audio_layout"];
        layout["header_frames"] = json!(claimed_headers);
        layout["encoder_delay"] = json!(0);
        layout["end_padding"] = json!(0);
        layout["frames"] = json!(frames);
        // The fast cache validator deliberately reads only four header bytes.
        // The decoder's bounded first-frame validation must reject this coherent
        // but false claim before any real audio can be skipped or duplicated.
        let info = cached_info(&mut Cursor::new(bytes), bytes.len() as u64, metadata).unwrap();
        assert!(decoder_prefix(&mut Cursor::new(bytes), &info).is_err());
    }
}

#[test]
fn malformed_xing_flags_discard_duration_fields_but_preserve_information_frame_presence() {
    let mut bytes = CBR.to_vec();
    bytes[40..44].copy_from_slice(&0x8000_000f_u32.to_be_bytes());
    let info = probe(&bytes);
    assert_eq!(info.metadata.frames, 7 * 1152);
    assert_eq!(
        info.metadata.attributes["source_audio_layout"]["header_frames"],
        1
    );
    assert_eq!(
        info.metadata.attributes["wav_metadata"]["mp3_header"],
        "Info"
    );
    assert!(info.metadata.attributes["wav_metadata"]["warnings"].is_array());
    let (mut clean, skip) = decoder_prefix(&mut Cursor::new(&bytes), &info).unwrap();
    clean.extend_from_slice(&bytes[skip as usize..]);
    assert_eq!(probe(&clean).metadata.frames, 7 * 1152);
}
