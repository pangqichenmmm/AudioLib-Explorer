// crates/audiolib-audio/src/wav_metadata_tests.rs
use super::*;
use crate::probe;
use crate::tests::write_wav;
use std::{fs, path::Path};

fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    if !payload.len().is_multiple_of(2) {
        bytes.push(0);
    }
    bytes
}

fn add(path: &Path, id: &[u8; 4], payload: &[u8]) {
    let mut bytes = fs::read(path).unwrap();
    bytes.extend(chunk(id, payload));
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn wav(path: &Path) {
    write_wav(path, 48000, 1, 16, 1, &[0; 128], false, false);
}

#[test]
fn public_metadata_after_audio_preserves_vendor_fields_entities_arrays_and_encodings() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("professional.wav");
    wav(&path);
    let mut info = b"INFO".to_vec();
    info.extend(chunk(b"ICMT", b"Legacy description\0"));
    info.extend(chunk(b"INAM", b"Caf\xe9 Foley\0"));
    info.extend(chunk(b"IKEY", "门碰撞; 关门\0".as_bytes()));
    info.extend(chunk(b"IART", b"Sound designer\0"));
    add(&path, b"LIST", &info);
    let xml = r#"<?xml version="1.0"?><BWFXML xmlns:p="urn:vendor"><SCENE title="scene">EXT &#x95E8; &amp; &amp;lt;</SCENE><TAKE><![CDATA[03 & safety]]></TAKE><USER><CATEGORY>DOORS</CATEGORY><CATID>DOORWood</CATID><DESCRIPTION><![CDATA[木门 & latch]]></DESCRIPTION><p:CUSTOM>Field preserved</p:CUSTOM><KEYWORDS>开门</KEYWORDS><KEYWORDS>门闩</KEYWORDS></USER></BWFXML>"#;
    add(&path, b"iXML", xml.as_bytes());
    add(&path, b"_PMX", br#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:r="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:d="http://purl.org/dc/elements/1.1/" xmlns:m="urn:soundminer"><r:RDF><r:Description m:Designer="Test designer"><d:subject><r:Bag><r:li>latch</r:li><r:li>wood</r:li></r:Bag></d:subject><d:description><r:Alt><r:li xml:lang="x-default">Full XMP description</r:li></r:Alt></d:description></r:Description></r:RDF></x:xmpmeta>"#);
    let axml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><metadata><location>录音棚</location></metadata>";
    let mut utf16 = vec![0xff, 0xfe];
    utf16.extend(axml.encode_utf16().flat_map(u16::to_le_bytes));
    utf16.extend([0, 0]);
    add(&path, b"axml", &utf16);
    let result = probe(&path).unwrap();
    assert_eq!(result.frames, 64);
    assert!(
        result.bext_description.is_empty(),
        "INFO description must not masquerade as BWF"
    );
    assert_eq!(result.attributes["ixml_scene"], "EXT 门 & &lt;");
    assert_eq!(result.attributes["ixml_take"], "03 & safety");
    let m = &result.attributes["wav_metadata"];
    assert_eq!(m["riff_info"]["INAM"], "Café Foley");
    assert_eq!(
        m["ixml"]["BWFXML/USER/{urn:vendor}CUSTOM"],
        "Field preserved"
    );
    assert_eq!(m["ixml"]["BWFXML/USER/KEYWORDS"], json!(["开门", "门闩"]));
    assert_eq!(m["fields"]["category"], "DOORS");
    assert_eq!(m["fields"]["designer"], "Test designer");
    assert_eq!(m["fields"]["location"], "录音棚");
    assert_eq!(
        audiolib_core::source_description("", &result.attributes),
        "木门 & latch"
    );
    let search = audiolib_core::source_metadata_search_text("", &result.attributes);
    for word in ["开门", "latch", "wood", "Field preserved", "录音棚", "Café"] {
        assert!(search.contains(word), "{word}");
    }
    assert!(!search.contains("http://"));
    assert!(m["warnings"].is_null());
}

#[test]
fn declared_xml_encodings_decode_non_utf8_values_near_the_declaration() {
    let temp = tempfile::tempdir().unwrap();
    for (id, encoding, text, expected) in [
        (b"iXML", "Windows-1252", &b"Caf\xe9 Foley"[..], "Café Foley"),
        (b"axml", "ISO-8859-1", &b"Cr\xe8me"[..], "Crème"),
        (
            b"_PMX",
            "Windows-1251",
            &b"\xcf\xf0\xe8\xe2\xe5\xf2"[..],
            "Привет",
        ),
    ] {
        let path = temp.path().join(format!("{encoding}.wav"));
        wav(&path);
        let mut xml =
            format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?><metadata><Description>")
                .into_bytes();
        xml.extend(text);
        xml.extend(b"</Description></metadata>");
        assert!(xml.len() < 256);
        add(&path, id, &xml);
        let metadata = probe(&path).unwrap();
        assert_eq!(
            audiolib_core::source_description("", &metadata.attributes),
            expected
        );
        assert!(metadata.attributes["wav_metadata"]["warnings"].is_null());
    }
}

#[test]
fn xml_depth_and_expanded_element_and_attribute_paths_are_bounded() {
    let namespace = format!("urn:{}", "x".repeat(MAX_FIELD_PATH_BYTES));
    let mut deep = "<MAGIC><Description>Source description survives</Description>".to_string();
    deep.push_str(&"<Nested>".repeat(MAX_XML_DEPTH + 8));
    deep.push_str("overdeepvalue");
    deep.push_str(&"</Nested>".repeat(MAX_XML_DEPTH + 8));
    deep.push_str("</MAGIC>");
    for (group, xml, expected_warning) in [
        ("smed", deep, "nesting exceeds 32 levels"),
        (
            "axml",
            format!(
                "<metadata xmlns:v=\"{namespace}\"><Description>Source description survives</Description><v:Custom>oversizedpathvalue</v:Custom></metadata>"
            ),
            "field path exceeds 4096 bytes",
        ),
        (
            "xmp",
            format!(
                "<metadata xmlns:v=\"{namespace}\" v:Custom=\"oversizedpathvalue\"><Description>Source description survives</Description></metadata>"
            ),
            "field path exceeds 4096 bytes",
        ),
    ] {
        let mut embedded = EmbeddedMetadata::default();
        let mut attrs = Map::new();
        embedded.xml(group, xml.as_bytes(), &mut attrs);
        embedded.finish(&mut attrs);
        let attrs = Value::Object(attrs);
        assert_eq!(
            audiolib_core::source_description("", &attrs),
            "Source description survives"
        );
        assert!(
            attrs["wav_metadata"]["warnings"]
                .to_string()
                .contains(expected_warning)
        );
        let source = attrs["wav_metadata"][group].as_object().unwrap();
        assert!(source.keys().all(|path| path.len() <= MAX_FIELD_PATH_BYTES));
        let search = audiolib_core::source_metadata_search_text("", &attrs);
        assert!(!search.contains("overdeepvalue"));
        assert!(!search.contains("oversizedpathvalue"));
    }
    let mut path = "prefix".to_string();
    assert!(!append_xml_name(&mut path, Some(&namespace), "Custom"));
    assert_eq!(path, "prefix", "Rejected names must never be copied");
}

#[test]
fn malformed_optional_metadata_and_private_chunks_do_not_hide_valid_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("damaged-tags.wav");
    wav(&path);
    add(&path, b"iXML", b"<!DOCTYPE BWFXML [<!ENTITY x SYSTEM 'file:///private'>]><BWFXML><NOTE>&x;</NOTE></BWFXML>");
    let mut info = b"INFO".to_vec();
    info.extend(chunk(b"ICMT", b"Good comment"));
    info.extend(b"IKEY\xff\xff\xff\xff");
    add(&path, b"LIST", &info);
    add(&path, b"SMED", &[1, 2, 3]);
    add(&path, b"_PMX", &vec![b' '; MAX_CHUNK_BYTES as usize + 1]);
    let metadata = probe(&path).unwrap();
    assert_eq!(
        metadata.attributes["wav_metadata"]["fields"]["description"],
        "Good comment"
    );
    let warnings = metadata.attributes["wav_metadata"]["warnings"].to_string();
    for word in ["DTD", "LIST/INFO", "SMED", "size limit"] {
        assert!(warnings.contains(word), "{warnings}");
    }
    assert_eq!(metadata.duration, 64.0 / 48000.0);
}

fn synchsafe(size: usize) -> [u8; 4] {
    [
        ((size >> 21) & 127) as u8,
        ((size >> 14) & 127) as u8,
        ((size >> 7) & 127) as u8,
        (size & 127) as u8,
    ]
}

fn id3_frame(version: u8, id: &str, data: &[u8], flags: u8) -> Vec<u8> {
    let mut result = id.as_bytes().to_vec();
    if version == 2 {
        result.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
    } else {
        result.extend_from_slice(&if version == 4 {
            synchsafe(data.len())
        } else {
            (data.len() as u32).to_be_bytes()
        });
        result.extend([0, flags]);
    }
    result.extend(data);
    result
}

#[test]
fn id3_versions_text_comments_custom_fields_and_utf16() {
    let temp = tempfile::tempdir().unwrap();
    for version in [2, 3, 4] {
        let path = temp.path().join(format!("v{version}.wav"));
        wav(&path);
        let mut frames = id3_frame(
            version,
            if version == 2 { "TT2" } else { "TIT2" },
            b"\0Foley title",
            0,
        );
        let mut text = vec![1, 0xff, 0xfe];
        text.extend("Category\0机械\0".encode_utf16().flat_map(u16::to_le_bytes));
        frames.extend(id3_frame(
            version,
            if version == 2 { "TXX" } else { "TXXX" },
            &text,
            0,
        ));
        frames.extend(id3_frame(
            version,
            if version == 2 { "COM" } else { "COMM" },
            b"\0eng\0Professional long description",
            0,
        ));
        let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
        tag.extend(synchsafe(frames.len()));
        tag.extend(frames);
        add(&path, b"id3 ", &tag);
        let metadata = probe(&path).unwrap();
        let fields = &metadata.attributes["wav_metadata"]["fields"];
        assert_eq!(fields["title"], "Foley title");
        assert_eq!(fields["category"], "机械");
        assert_eq!(fields["description"], "Professional long description");
    }
}

#[test]
fn id3_utf16_custom_and_comment_values_accept_their_own_bom() {
    let temp = tempfile::tempdir().unwrap();
    for (bom, big_endian) in [([0xff, 0xfe], false), ([0xfe, 0xff], true)] {
        let encode = |value: &str| {
            let mut bytes = bom.to_vec();
            for unit in value.encode_utf16() {
                bytes.extend(if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                });
            }
            bytes
        };
        let path = temp.path().join(format!("utf16-{big_endian}.wav"));
        wav(&path);
        let mut custom = vec![1];
        custom.extend(encode("Category\0"));
        custom.extend(encode("机械\0"));
        let mut comment = vec![1, b'e', b'n', b'g'];
        comment.extend(encode("Description\0"));
        comment.extend(encode("门闩声\0"));
        let mut frames = id3_frame(3, "TXXX", &custom, 0);
        frames.extend(id3_frame(3, "COMM", &comment, 0));
        let mut tag = vec![b'I', b'D', b'3', 3, 0, 0];
        tag.extend(synchsafe(frames.len()));
        tag.extend(frames);
        add(&path, b"id3 ", &tag);
        let metadata = probe(&path).unwrap();
        let fields = &metadata.attributes["wav_metadata"]["fields"];
        assert_eq!(fields["category"], "机械");
        assert_eq!(fields["description"], "门闩声");
        assert!(metadata.attributes["wav_metadata"]["warnings"].is_null());
    }
}

#[test]
fn id3_unsynchronisation_extended_headers_and_untrusted_frame_sizes_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    for version in [3, 4] {
        let path = temp.path().join(format!("flags{version}.wav"));
        wav(&path);
        let mut payload = if version == 3 {
            vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0]
        } else {
            vec![0, 0, 0, 6, 1, 0]
        };
        if version == 3 {
            let frame = id3_frame(3, "TIT2", &[0, b'A', 255, 224], 0);
            for byte in frame {
                payload.push(byte);
                if byte == 255 {
                    payload.push(0);
                }
            }
        } else {
            payload.extend(id3_frame(4, "TIT2", &[0, b'A', 255, 0, 224], 2));
        }
        let mut tag = vec![b'I', b'D', b'3', version, 0, 0xc0];
        payload.extend(id3_frame(
            version,
            "TXXX",
            b"compressed",
            if version == 3 { 0x80 } else { 8 },
        ));
        payload.extend(if version == 3 {
            b"TPE1\xff\xff\xff\xff\0\0"
        } else {
            b"TPE1\x7f\x7f\x7f\x7f\0\0"
        });
        tag.extend(synchsafe(payload.len()));
        tag.extend(payload);
        add(&path, b"ID3 ", &tag);
        let metadata = probe(&path).unwrap();
        assert_eq!(
            metadata.attributes["wav_metadata"]["fields"]["title"],
            "Aÿà"
        );
        assert!(
            metadata.attributes["wav_metadata"]["warnings"]
                .to_string()
                .contains("truncated frame")
        );
    }
}

#[test]
fn id3_streaming_skips_artwork_and_matches_bounded_decoder_including_fallbacks() {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    struct Meter {
        input: Cursor<Vec<u8>>,
        read: usize,
    }
    impl Read for Meter {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let size = self.input.read(bytes)?;
            self.read += size;
            Ok(size)
        }
    }
    impl Seek for Meter {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.input.seek(from)
        }
    }
    for version in [2, 3, 4] {
        for malformed in [false, true] {
            let mut frames = id3_frame(
                version,
                if version == 2 { "PIC" } else { "APIC" },
                &vec![42; 1_000_000],
                0,
            );
            frames.extend(id3_frame(
                version,
                if version == 2 { "TT2" } else { "TIT2" },
                b"\x03Artwork followed by text",
                0,
            ));
            if malformed {
                frames.extend_from_slice(b"INVALID");
            }
            let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
            tag.extend(synchsafe(frames.len()));
            tag.extend(frames);
            let size = tag.len();
            let mut old = EmbeddedMetadata::default();
            old.id3(&tag);
            let mut expected = Map::new();
            old.finish(&mut expected);
            let mut meter = Meter {
                input: Cursor::new(tag),
                read: 0,
            };
            let mut streamed = EmbeddedMetadata::default();
            streamed.read_id3(&mut meter, size).unwrap();
            let mut actual = Map::new();
            streamed.finish(&mut actual);
            assert_eq!(actual, expected);
            if !malformed {
                assert!(
                    meter.read < 100,
                    "Binary artwork must be sought past, not read"
                );
            }
        }
    }
}

#[test]
fn short_null_terminated_bwf_text_and_namespaced_ixml_do_not_capture_nested_scene() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bwf.wav");
    wav(&path);
    let mut bext = vec![b' '; 602];
    bext[..12].copy_from_slice(b"Caf\xe9\0garbage");
    add(&path, b"bext", &bext);
    add(&path, b"iXML", b"<b:BWFXML xmlns:b='urn:ixml'><b:USER><b:SCENE>wrong</b:SCENE></b:USER><b:SCENE>right</b:SCENE><b:CIRCLED>Yes</b:CIRCLED></b:BWFXML>");
    let metadata = probe(&path).unwrap();
    assert_eq!(metadata.bext_description, "Café");
    assert_eq!(metadata.attributes["ixml_scene"], "right");
    assert_eq!(metadata.attributes["ixml_circled"], true);
}

#[test]
fn smed_decodes_utf16_library_fields_without_indexing_artwork_or_changing_user_ratings() {
    use blowfish::cipher::{Block, BlockEncrypt, KeyInit};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("smed.wav");
    wav(&path);
    let xml = "<MAGIC><Description>完整描述</Description><Category>Vehicles</Category><SubCategory>Aircraft</SubCategory><Microphone>MKH8050</Microphone><Library>Test library</Library><Manufacturer>Publisher</Manufacturer><Rating>5</Rating><Picture type=\"NSData\">NOTSEARCHABLEPICTURE</Picture><Waveform>NOTSEARCHABLECACHE</Waveform><CustomVendorField>Keep custom text</CustomVendorField></MAGIC>";
    // Modern Soundminer files exceed 2 MiB because they include UTF-16 artwork.
    let xml = xml.replace("NOTSEARCHABLEPICTURE", &"A".repeat(1_100_000));
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    let length = bytes.len();
    bytes.resize(length.next_multiple_of(8), 0);
    let cipher: blowfish::Blowfish = blowfish::Blowfish::new_from_slice(b"u7w58he4746").unwrap();
    for bytes in bytes.chunks_exact_mut(8) {
        let mut block = Block::<blowfish::Blowfish>::default();
        block.copy_from_slice(bytes);
        cipher.encrypt_block(&mut block);
        bytes.copy_from_slice(&block);
    }
    let mut payload = (length as u32).to_be_bytes().to_vec();
    payload.extend(bytes);
    add(&path, b"SMED", &payload);
    let metadata = probe(&path).unwrap();
    let fields = &metadata.attributes["wav_metadata"]["fields"];
    assert_eq!(fields["description"], "完整描述");
    assert_eq!(fields["category"], "Vehicles");
    assert_eq!(fields["subcategory"], "Aircraft");
    assert_eq!(fields["microphone"], "MKH8050");
    assert_eq!(fields["library"], "Test library");
    assert_eq!(fields["manufacturer"], "Publisher");
    assert_eq!(
        metadata.attributes["wav_metadata"]["smed"]["MAGIC/Rating"],
        "5"
    );
    let search = audiolib_core::source_metadata_search_text("", &metadata.attributes);
    assert!(search.contains("Keep custom text"));
    assert!(!search.contains("NOTSEARCHABLE"));
    assert!(!metadata.attributes.to_string().contains("NOTSEARCHABLE"));
    assert!(metadata.attributes.to_string().len() < 16_384);
    let playback = crate::wav::read_info(&path).unwrap();
    assert_eq!(playback.metadata.frames, metadata.frames);
    assert!(playback.metadata.attributes["wav_metadata"].is_null());
    assert!(
        crate::decoder::Decoder::open(&path)
            .unwrap()
            .info
            .metadata
            .attributes["wav_metadata"]
            .is_null()
    );
    // Corrupt length and ciphertext must not break valid audio or allocate from the prefix.
    for payload in [
        &[0xff; 12][..],
        &[0; 12][..],
        &[0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0][..],
    ] {
        wav(&path);
        add(&path, b"SMED", payload);
        let metadata = probe(&path).unwrap();
        assert!(
            metadata.attributes["wav_metadata"]["warnings"]
                .as_array()
                .is_some()
        );
    }
}

#[test]
#[ignore = "requires the read-only local BOOM P-51 Mustang WAV fixtures"]
fn real_boom_smed_metadata_fixtures() {
    fn visit(path: &Path, count: &mut usize) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, count);
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
            {
                let before = fs::metadata(&path).unwrap();
                let metadata = probe(&path).unwrap();
                let after = fs::metadata(&path).unwrap();
                assert_eq!(before.len(), after.len());
                assert_eq!(before.modified().unwrap(), after.modified().unwrap());
                let m = &metadata.attributes["wav_metadata"];
                assert!(
                    m["warnings"].is_null(),
                    "{}: {}",
                    path.display(),
                    m["warnings"]
                );
                assert_eq!(m["fields"]["category"], "Vehicles", "{}", path.display());
                assert_eq!(m["fields"]["subcategory"], "Aircraft");
                assert_eq!(m["fields"]["manufacturer"], "BOOM Library");
                assert!(
                    m["fields"]["microphone"]
                        .as_str()
                        .is_some_and(|s| s.contains("MKH"))
                );
                assert!(
                    m["fields"]["library"]
                        .as_str()
                        .is_some_and(|s| s.contains("P-51 Mustang"))
                );
                assert!(m["smed"]["MAGIC/Picture"].is_null());
                let search = audiolib_core::source_metadata_search_text(
                    &metadata.bext_description,
                    &metadata.attributes,
                );
                assert!(search.contains("Aircraft"));
                *count += 1;
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_audiolib/P-51 Mustang");
    let mut count = 0;
    visit(&root, &mut count);
    assert_eq!(count, 34);
}

#[test]
fn playback_layout_stops_at_the_data_chunk() {
    // A network source must not fetch the file's tail to start playing: the playback
    // layout ignores everything after the audio (here a trailing chunk claiming more bytes
    // than the file has, which the full Metadata probe rejects).
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tail.wav");
    wav(&path);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"LIST");
    bytes.extend_from_slice(&1_000_000_u32.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    assert!(probe(&path).is_err());
    let playback = crate::wav::read_info(&path).unwrap();
    assert_eq!(playback.metadata.frames, 64);
    assert_eq!(
        crate::decoder::Decoder::open(&path)
            .unwrap()
            .info
            .metadata
            .frames,
        64
    );
}

#[test]
fn probe_with_artwork_returns_the_id3_cover_and_ordinary_probe_skips_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("album.wav");
    wav(&path);
    // Boom-style ID3v2.3: the album name is TALB, the cover is a raw APIC payload.
    let image = b"\x89PNG\r\n\x1a\nfixture cover bytes".to_vec();
    let mut apic = vec![0_u8]; // ISO-8859-1
    apic.extend_from_slice(b"image/png\0");
    apic.push(3); // front cover
    apic.push(0); // empty description
    apic.extend_from_slice(&image);
    let mut frames = id3_frame(3, "APIC", &apic, 0);
    frames.extend(id3_frame(3, "TALB", b"\x00Album Name", 0));
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend(synchsafe(frames.len()));
    tag.extend(frames);
    add(&path, b"ID3 ", &tag);

    let metadata = probe(&path).unwrap();
    assert_eq!(
        metadata.attributes["wav_metadata"]["fields"]["library"],
        "Album Name"
    );
    let (_, artwork) = crate::probe_with_artwork(&path).unwrap();
    let artwork = artwork.expect("ID3 APIC cover");
    assert_eq!(artwork.mime, "image/png");
    assert_eq!(artwork.bytes, image);

    // A file without a cover stays empty instead of failing or inventing one.
    let plain = temp.path().join("plain.wav");
    wav(&plain);
    let (_, none) = crate::probe_with_artwork(&plain).unwrap();
    assert!(none.is_none());
}

fn apic(image: &[u8]) -> Vec<u8> {
    let mut content = b"\0image/png\0\x03\0".to_vec();
    content.extend_from_slice(image);
    content
}

fn synchronise(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (index, byte) in bytes.iter().copied().enumerate() {
        encoded.push(byte);
        if byte == 255
            && bytes
                .get(index + 1)
                .is_none_or(|next| *next == 0 || *next >= 224)
        {
            encoded.push(0);
        }
    }
    encoded
}

#[test]
fn id3_large_cover_preserves_album_text_without_copying_artwork_into_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large-cover.wav");
    wav(&path);
    let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
    image.resize(3 * 1024 * 1024, 42);
    let mut frames = id3_frame(3, "APIC", &apic(&image), 0);
    frames.extend(id3_frame(3, "TALB", b"\0Large Cover Album", 0));
    let mut tag = b"ID3\x03\0\0".to_vec();
    tag.extend(synchsafe(frames.len()));
    tag.extend(frames);
    add(&path, b"ID3 ", &tag);

    let metadata = probe(&path).unwrap();
    assert_eq!(
        audiolib_core::album(&metadata.attributes),
        "Large Cover Album"
    );
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_null());
    assert!(metadata.attributes.to_string().len() < 1024);
    let (_, cover) = crate::probe_with_artwork(&path).unwrap();
    assert_eq!(cover.unwrap().bytes, image);
}

#[test]
fn id3_artwork_restores_unsynchronisation_and_skips_frame_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let image = b"\x89PNG\r\n\x1a\n\xff\xe0\xff\0image\xff";
    for (version, tag_flags, frame_flags) in [(3, 0xc0, 0x20), (4, 0xc0, 0x43), (4, 0, 2)] {
        let path = temp
            .path()
            .join(format!("artwork-flags-{version}-{tag_flags}.wav"));
        wav(&path);
        let content = apic(image);
        let mut framed = Vec::new();
        if frame_flags & if version == 3 { 0x20 } else { 0x40 } != 0 {
            framed.push(7); // grouping identity
        }
        if version == 4 && frame_flags & 1 != 0 {
            framed.extend(synchsafe(content.len()));
        }
        framed.extend(content);
        if version == 4 {
            framed = synchronise(&framed);
        }
        let mut payload = if tag_flags & 0x40 != 0 {
            if version == 3 {
                vec![0, 0, 0, 6, 0, 0, 0, 0, 0, 0]
            } else {
                vec![0, 0, 0, 6, 1, 0]
            }
        } else {
            Vec::new()
        };
        payload.extend(id3_frame(version, "APIC", &framed, frame_flags));
        payload.extend(id3_frame(version, "TALB", b"\0Flagged Album", 0));
        if version == 3 {
            payload = synchronise(&payload);
        }
        let mut tag = vec![b'I', b'D', b'3', version, 0, tag_flags];
        tag.extend(synchsafe(payload.len()));
        tag.extend(payload);
        add(&path, b"id3 ", &tag);

        let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(audiolib_core::album(&metadata.attributes), "Flagged Album");
        assert_eq!(cover.unwrap().bytes, image);
    }
}

#[test]
fn id3_artwork_skips_unsupported_invalid_and_oversized_pictures() {
    let image = b"\x89PNG\r\n\x1a\nvalid cover";
    for (version, flags) in [(3, 0xc0), (4, 0x0c)] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("picture-fallback.wav");
        wav(&path);
        let mut oversized = b"\x89PNG\r\n\x1a\n".to_vec();
        oversized.resize(MAX_ARTWORK_BYTES + 1, 42);
        let mut frames = id3_frame(version, "APIC", &apic(image), flags);
        frames.extend(id3_frame(version, "APIC", b"invalid picture", 0));
        frames.extend(id3_frame(version, "APIC", &apic(&oversized), 0));
        frames.extend(id3_frame(version, "APIC", &apic(image), 0));
        let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
        tag.extend(synchsafe(frames.len()));
        tag.extend(frames);
        add(&path, b"ID3 ", &tag);

        let (_, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(cover.unwrap().bytes, image);
    }
}
