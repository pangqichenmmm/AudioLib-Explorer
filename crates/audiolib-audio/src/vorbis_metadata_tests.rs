// crates/audiolib-audio/src/vorbis_metadata_tests.rs
//! Real compressed sources and bounded malformed-comment fixtures.
use super::*;
use crate::{decoder::Decoder, probe};
use std::{fs, io::Cursor};

struct Registered(std::path::PathBuf);
impl Drop for Registered {
    fn drop(&mut self) {
        crate::unregister_stream_source(&self.0);
    }
}

fn register_bytes(path: std::path::PathBuf, bytes: Vec<u8>) -> Registered {
    let opener: crate::SourceOpener = std::sync::Arc::new(move || {
        let len = bytes.len() as u64;
        let source: Box<dyn crate::ByteSource> = Box::new(Cursor::new(bytes.clone()));
        Ok((source, len))
    });
    crate::register_stream_source(&path, opener);
    Registered(path)
}

fn comments(values: &[&[u8]]) -> Vec<u8> {
    let mut bytes = 0_u32.to_le_bytes().to_vec();
    bytes.extend((values.len() as u32).to_le_bytes());
    for value in values {
        bytes.extend((value.len() as u32).to_le_bytes());
        bytes.extend(*value);
    }
    bytes
}
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/fixtures/vorbis")
        .join(name)
}

#[test]
fn real_flac_ogg_oga_and_opus_tags_are_public_and_searchable() {
    for name in [
        "comments.flac",
        "comments.ogg",
        "comments.oga",
        "comments.opus",
    ] {
        let metadata = probe(&fixture(name)).unwrap();
        assert!(metadata.frames > 0, "{name}");
        assert_eq!(metadata.channels, 1, "{name}");
        assert!(
            (metadata.duration - 0.08).abs() < 0.001,
            "{name}: {}",
            metadata.duration
        );
        let tags = &metadata.attributes["wav_metadata"]["vorbis"];
        assert_eq!(tags["DESCRIPTION"], "树林微风", "{name}");
        assert_eq!(tags["ARTIST"], "AudioLib synthetic", "{name}");
        assert_eq!(
            audiolib_core::source_description("", &metadata.attributes),
            "树林微风"
        );
        let search = audiolib_core::source_metadata_search_text("", &metadata.attributes);
        assert!(
            search.contains("树林微风") && search.contains("Forest signal"),
            "{name}: {search}"
        );
        assert!(
            !tags
                .as_object()
                .unwrap()
                .contains_key("METADATA_BLOCK_PICTURE")
        );
        let mut file = fs::File::open(fixture(name)).unwrap();
        let (from_handle, cover) = crate::probe_file_with_artwork(&mut file).unwrap();
        assert_eq!(from_handle.frames, metadata.frames);
        assert!(cover.is_none());
    }
}

#[test]
fn real_compressed_sources_decode_and_seek_without_using_wav_block_layout() {
    for name in ["comments.flac", "comments.ogg", "comments.oga"] {
        let mut decoder = Decoder::open(&fixture(name)).unwrap();
        let audio = decoder.read(128).unwrap();
        assert!(
            !audio.is_empty() && audio.iter().any(|sample| sample.abs() > 0.001),
            "{name}"
        );
        decoder.seek(64).unwrap();
        assert!(!decoder.read(64).unwrap().is_empty(), "{name}");
    }
    let error = Decoder::open(&fixture("comments.opus"))
        .err()
        .expect("Opus playback must be honest");
    assert!(error.to_string().contains("Opus"));
}

#[test]
fn compressed_preroll_seek_matches_sequential_pcm_and_eof() {
    for name in ["comments.ogg", "comments.flac", "comments.oga"] {
        let path = fixture(name);
        let mut sequential = Decoder::open(&path).unwrap();
        let expected = sequential.read(4096).unwrap();
        assert_eq!(expected.len(), 640, "{name}");
        assert!(expected.iter().any(|sample| sample.abs() > 0.001));
        assert!(sequential.read(1).unwrap().is_empty());
        let mut decoder = Decoder::open(&path).unwrap();
        for (index, start) in [0_usize, 1, 64, 255, 256, 320, 639, 640, 0]
            .into_iter()
            .enumerate()
        {
            // Ogg must support repeated seeks/EOF on this reader. Native FLAC
            // and OGA follow the production fresh-decoder-per-seek pattern.
            if index > 0 && name != "comments.ogg" {
                decoder = Decoder::open(&path).unwrap();
            }
            decoder
                .seek(start as u64)
                .unwrap_or_else(|error| panic!("{name} seek {start}: {error}"));
            assert_eq!(
                decoder
                    .read(4096)
                    .unwrap_or_else(|error| panic!("{name} read after seek {start}: {error}")),
                expected[start..],
                "{name} seek {start}"
            );
            assert!(
                decoder.read(1).unwrap().is_empty(),
                "{name} seek {start} EOF"
            );
        }
        decoder.seek(u64::MAX).unwrap();
        assert!(decoder.read(1).unwrap().is_empty(), "{name} clamped EOF");
    }
}

#[test]
fn compressed_preroll_resampling_and_audition_keep_complete_pcm() {
    use crate::decoder::ResampledDecoder;
    for name in ["comments.flac", "comments.oga", "comments.ogg"] {
        let path = fixture(name);
        let expected = Decoder::open(&path).unwrap().read(4096).unwrap();
        for start in [0_usize, 64, 320, 640] {
            let mut decoder =
                ResampledDecoder::new(Decoder::open(&path).unwrap(), 8000, start as u64).unwrap();
            let mut samples = Vec::new();
            while !decoder.is_finished() {
                samples.extend(decoder.next().unwrap());
            }
            assert_eq!(samples, expected[start..], "{name} converted seek {start}");
            assert!(decoder.next().unwrap().is_empty());
        }
        let mut converted = ResampledDecoder::new(Decoder::open(&path).unwrap(), 16000, 0).unwrap();
        let mut samples = Vec::new();
        while !converted.is_finished() {
            samples.extend(converted.next().unwrap());
        }
        assert_eq!(samples.len(), 1280, "{name} resampled complete tail");
        assert!(samples.iter().all(|sample| sample.is_finite()));
        assert!(samples.iter().any(|sample| sample.abs() > 0.001));
        #[cfg(feature = "playback")]
        {
            let mut audition = crate::audition::AuditionDecoder::new(
                Decoder::open(&path).unwrap(),
                8000,
                0,
                0,
                crate::AuditionSettings::default(),
            )
            .unwrap();
            let mut stereo = Vec::new();
            while !audition.is_finished() {
                stereo.extend(audition.next().unwrap());
            }
            let expected_stereo: Vec<_> = expected
                .iter()
                .flat_map(|sample| [*sample, *sample])
                .collect();
            assert_eq!(
                stereo, expected_stereo,
                "{name} audition starts at valid frame zero"
            );
            assert!(audition.next().unwrap().is_empty());
        }
    }
}

#[test]
fn compressed_virtual_sources_probe_decode_and_seek_with_opaque_or_wav_cache_names() {
    let owned = tempfile::tempdir().unwrap();
    for name in ["comments.flac", "comments.ogg", "comments.oga"] {
        let file = fixture(name);
        let expected_metadata = probe(&file).unwrap();
        for virtual_path in [
            owned
                .path()
                .join("absent-stream-cache")
                .join(format!("{name}.wav")),
            std::path::PathBuf::from(format!(
                "audiolib-stream://{}/{name}",
                owned.path().file_name().unwrap().to_string_lossy()
            )),
        ] {
            assert!(!virtual_path.exists());
            let registered = register_bytes(virtual_path.clone(), fs::read(&file).unwrap());
            let metadata = probe(&virtual_path).unwrap();
            assert_eq!(metadata.frames, expected_metadata.frames, "{name}");
            assert_eq!(metadata.attributes, expected_metadata.attributes, "{name}");
            let mut source = Decoder::open(&virtual_path).unwrap();
            let mut local = Decoder::open(&file).unwrap();
            assert_eq!(
                source.read(128).unwrap(),
                local.read(128).unwrap(),
                "{name}"
            );
            source.seek(320).unwrap();
            local.seek(320).unwrap();
            assert_eq!(source.read(64).unwrap(), local.read(64).unwrap(), "{name}");
            drop(source);
            drop(registered);
            assert!(!crate::is_stream_source(&virtual_path));
            assert!(probe(&virtual_path).is_err());
        }
    }
}

#[test]
fn tiny_flac_comment_and_picture_declarations_fail_before_claimed_gigabyte_allocations() {
    let original = fs::read(fixture("comments.flac")).unwrap();
    let owned = tempfile::tempdir().unwrap();
    let comment = [
        0_u32.to_le_bytes(),
        1_u32.to_le_bytes(),
        u32::MAX.to_le_bytes(),
    ]
    .concat();
    let picture = [3_u32.to_be_bytes(), u32::MAX.to_be_bytes()].concat();
    for (kind, payload) in [(4_u8, comment), (6_u8, picture)] {
        let mut bytes = original[..42].to_vec();
        bytes[4] = 0; // STREAMINFO is followed by one bounded physical Metadata block.
        bytes.push(0x80 | kind);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        bytes.extend(payload);
        assert!(bytes.len() < 100);
        let path = owned.path().join(format!("malformed-{kind}.wav"));
        let _registered = register_bytes(path.clone(), bytes);
        assert_eq!(
            probe(&path).unwrap().frames,
            640,
            "bounded scanning retains STREAMINFO"
        );
        // Symphonia 0.6.1's FLAC ScopedStream rejects an internal length larger
        // than the physical block before allocating it. MetadataOptions alone
        // do not establish that embedded-reader invariant.
        assert!(Decoder::open(&path).is_err());
    }
}

#[test]
fn repeated_unknown_and_invalid_utf8_values_survive_but_pictures_are_excluded() {
    let bytes = comments(&[
        b"artist=First",
        "ARTIST=第二位".as_bytes(),
        b"UNKNOWN_FIELD=keep",
        b"DESCRIPTION=bad\xfftext",
        b"KEYWORDS=wind,forest",
        b"METADATA_BLOCK_PICTURE=secret-base64",
        b"COVERART=another-secret",
        b"EMPTY=",
        b"NO_EQUALS",
    ]);
    let mut parsed = Comments::default();
    parsed.read(&bytes);
    assert_eq!(parsed.tags["ARTIST"], json!(["First", "第二位"]));
    assert_eq!(parsed.tags["UNKNOWN_FIELD"], "keep");
    assert!(
        parsed.tags["DESCRIPTION"]
            .as_str()
            .unwrap()
            .contains('\u{fffd}')
    );
    let attrs = parsed.attributes("FLAC", true);
    let text = audiolib_core::source_metadata_search_text("", &attrs);
    assert!(text.contains("First") && text.contains("第二位") && text.contains("keep"));
    assert!(!text.contains("secret") && !text.contains("another-secret"));
    assert!(
        !attrs["wav_metadata"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn truncated_lengths_and_oversized_flac_comments_do_not_allocate_the_declared_block() {
    for bytes in [
        vec![],
        u32::MAX.to_le_bytes().to_vec(),
        comments(&[b"DESCRIPTION=retained"])[..10].to_vec(),
    ] {
        let mut parsed = Comments::default();
        parsed.read(&bytes);
        assert!(!parsed.warnings.is_empty());
    }
    let original = fs::read(fixture("comments.flac")).unwrap();
    let mut oversized = comments(&[b"DESCRIPTION=retained"]);
    oversized[4..8].copy_from_slice(&2_u32.to_le_bytes());
    oversized.extend((16_u32 * 1024 * 1024 - 64).to_le_bytes());
    oversized.resize(16 * 1024 * 1024 - 1, b'x');
    let mut bytes = b"fLaC".to_vec();
    bytes.extend(&[0, 0, 0, 34]);
    bytes.extend_from_slice(&original[8..42]);
    bytes.extend(&[0x84, 0xff, 0xff, 0xff]);
    bytes.extend(oversized);
    let info = read_info(Cursor::new(&bytes), bytes.len() as u64, true).unwrap();
    assert_eq!(
        info.metadata.attributes["wav_metadata"]["fields"]["description"],
        "retained"
    );
    assert!(
        !info.metadata.attributes["wav_metadata"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_chained_ogg_uses_the_first_logical_stream_only() {
    let first = fs::read(fixture("comments.ogg")).unwrap();
    let mut chained = first.clone();
    chained.extend(&first);
    let info = read_info(Cursor::new(&chained), chained.len() as u64, true).unwrap();
    assert_eq!(info.metadata.frames, 640);
    assert_eq!(
        info.metadata.attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
        "树林微风"
    );
}
