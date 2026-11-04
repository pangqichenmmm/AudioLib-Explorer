// crates/audiolib-audio/src/wav_artwork_tests.rs
//! Opt-in SMED image boundaries and source-independent ID3 cover priority.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use blowfish::cipher::{Block, BlockEncrypt, KeyInit};
use std::{
    fs,
    path::{Path, PathBuf},
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsmed-picture";
const JPEG: &[u8] = b"\xff\xd8\xffid3-picture";

fn wav(path: &Path) {
    crate::tests::write_wav(path, 48000, 1, 16, 1, &[0; 128], false, false);
}

fn add(path: &Path, id: &[u8; 4], payload: &[u8]) {
    let mut bytes = fs::read(path).unwrap();
    bytes.extend(id);
    bytes.extend((payload.len() as u32).to_le_bytes());
    bytes.extend(payload);
    if !payload.len().is_multiple_of(2) {
        bytes.push(0);
    }
    let length = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn smed(xml: &str, utf16: bool) -> Vec<u8> {
    let mut bytes = if utf16 {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    } else {
        xml.as_bytes().to_vec()
    };
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
    payload
}

fn xml(pictures: &str) -> String {
    format!(
        "<MAGIC><Library>BOOM test library</Library><Description>Preserved text</Description><CustomVendorField>Retain extension</CustomVendorField>{pictures}</MAGIC>"
    )
}

fn picture(bytes: &[u8]) -> String {
    format!(
        "<Picture type=\"NSData\">{}</Picture>",
        STANDARD.encode(bytes)
    )
}

fn id3(pictures: &[(&[u8], u8, &str)]) -> Vec<u8> {
    id3_version(3, pictures)
}

fn id3_version(version: u8, pictures: &[(&[u8], u8, &str)]) -> Vec<u8> {
    let mut frames = Vec::new();
    for (image, kind, mime) in pictures {
        let mut payload = vec![0];
        if version == 2 {
            payload.extend(b"PNG");
            payload.extend([*kind, 0]);
        } else {
            payload.extend(mime.as_bytes());
            payload.extend([0, *kind, 0]);
        }
        payload.extend(*image);
        if version == 2 {
            frames.extend(b"PIC");
            frames.extend(&(payload.len() as u32).to_be_bytes()[1..]);
        } else {
            frames.extend(b"APIC");
            if version == 4 {
                for shift in [21, 14, 7, 0] {
                    frames.push(((payload.len() >> shift) & 127) as u8);
                }
            } else {
                frames.extend((payload.len() as u32).to_be_bytes());
            }
            frames.extend([0, 0]);
        }
        frames.extend(payload);
    }
    let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
    for shift in [21, 14, 7, 0] {
        tag.push(((frames.len() >> shift) & 127) as u8);
    }
    tag.extend(frames);
    tag
}

fn assert_text_only(attributes: &Value) {
    assert_eq!(
        attributes["wav_metadata"]["fields"]["library"],
        "BOOM test library"
    );
    assert_eq!(
        attributes["wav_metadata"]["fields"]["description"],
        "Preserved text"
    );
    assert_eq!(
        attributes["wav_metadata"]["smed"]["MAGIC/CustomVendorField"],
        "Retain extension"
    );
    assert!(attributes["wav_metadata"]["smed"]["MAGIC/Picture"].is_null());
    assert!(!attributes.to_string().contains(&STANDARD.encode(PNG)));
    assert_eq!(attributes["wav_metadata"]["version"], 4);
}

#[test]
fn smed_artwork_is_opt_in_and_preserves_binary_free_metadata_and_source() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("soundminer.wav");
    wav(&path);
    add(&path, b"SMED", &smed(&xml(&picture(PNG)), true));
    let before = fs::read(&path).unwrap();
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    let ordinary = crate::probe(&path).unwrap();
    assert_text_only(&ordinary.attributes);
    let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
    assert_text_only(&metadata.attributes);
    assert_eq!(ordinary.attributes, metadata.attributes);
    let cover = cover.expect("SMED-only image");
    assert_eq!(cover.bytes, PNG);
    assert_eq!(cover.mime, "image/png");
    let mut handle = fs::File::open(&path).unwrap();
    let (from_handle, handle_cover) = crate::probe_file_with_artwork(&mut handle).unwrap();
    assert_eq!(from_handle.attributes, metadata.attributes);
    assert_eq!(handle_cover, Some(cover));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), stamp);
}

#[test]
fn smed_artwork_accepts_folded_base64_cdata_and_jpeg_png_magic() {
    for (index, image) in [PNG, JPEG].into_iter().enumerate() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("folded.wav");
        wav(&path);
        let encoded = STANDARD.encode(image);
        let middle = encoded.len() / 2;
        let fields = format!(
            "<Artwork type=\"NSData\">\r\n{} <![CDATA[{}]]>\t</Artwork>",
            &encoded[..middle],
            &encoded[middle..]
        );
        add(&path, b"SMED", &smed(&xml(&fields), index == 0));
        let (_, cover) = crate::probe_with_artwork(&path).unwrap();
        let cover = cover.unwrap();
        assert_eq!(cover.bytes, image);
        assert_eq!(
            cover.mime,
            if index == 0 {
                "image/png"
            } else {
                "image/jpeg"
            }
        );
    }
}

#[test]
fn id3_artwork_wins_over_smed_in_either_chunk_order_and_ignores_mime_label() {
    for smed_first in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("priority.wav");
        wav(&path);
        let smed_tag = smed(&xml(&picture(PNG)), true);
        let id3_tag = id3(&[(JPEG, 0, "image/png")]);
        for (kind, payload) in if smed_first {
            [(b"SMED", &smed_tag), (b"ID3 ", &id3_tag)]
        } else {
            [(b"ID3 ", &id3_tag), (b"SMED", &smed_tag)]
        } {
            add(&path, kind, payload);
        }
        let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_text_only(&metadata.attributes);
        let cover = cover.unwrap();
        assert_eq!(cover.bytes, JPEG);
        assert_eq!(cover.mime, "image/jpeg");
    }
}

#[test]
fn id3_artwork_prefers_front_cover_within_and_across_tags() {
    for (version, separate_chunks) in
        (2..=4).flat_map(|version| [false, true].map(|separate| (version, separate)))
    {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("front.wav");
        wav(&path);
        if separate_chunks {
            add(
                &path,
                b"ID3 ",
                &id3_version(version, &[(PNG, 1, "image/png")]),
            );
            add(
                &path,
                b"id3 ",
                &id3_version(version, &[(JPEG, 3, "image/jpeg")]),
            );
        } else {
            add(
                &path,
                b"ID3 ",
                &id3_version(version, &[(PNG, 1, "image/png"), (JPEG, 3, "image/jpeg")]),
            );
        }
        let (_, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(cover.unwrap().bytes, JPEG);
    }
}

#[test]
fn invalid_id3_image_magic_keeps_smed_fallback_in_either_chunk_order() {
    for smed_first in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("smed-fallback.wav");
        wav(&path);
        let smed_tag = smed(&xml(&picture(PNG)), true);
        let id3_tag = id3(&[(b"not an image", 3, "image/jpeg")]);
        for (kind, payload) in if smed_first {
            [(b"SMED", &smed_tag), (b"ID3 ", &id3_tag)]
        } else {
            [(b"ID3 ", &id3_tag), (b"SMED", &smed_tag)]
        } {
            add(&path, kind, payload);
        }
        let (_, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(cover.unwrap().bytes, PNG);
    }
}

#[test]
fn invalid_picture_payloads_keep_smed_text_and_allow_later_valid_images() {
    for bad in ["!!!=", "AAAA===", "éééé", "A", "UExBSU4="] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad-picture.wav");
        wav(&path);
        add(
            &path,
            b"SMED",
            &smed(
                &xml(&format!("<Picture type=\"NSData\">{bad}</Picture>")),
                true,
            ),
        );
        let ordinary = crate::probe(&path).unwrap();
        assert_text_only(&ordinary.attributes);
        assert!(
            ordinary.attributes["wav_metadata"]["warnings"].is_null(),
            "ordinary scan must not decode pictures"
        );
        let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_text_only(&metadata.attributes);
        assert!(cover.is_none());
        assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
        add(
            &path,
            b"ID3 ",
            &id3(&[(b"broken payload", 3, "image/png"), (JPEG, 3, "ignored")]),
        );
        let (_, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(cover.unwrap().bytes, JPEG);
    }
}

#[test]
fn smed_artwork_enforces_decoded_and_encoded_limits_without_losing_text() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bounded.wav");
    for length in [
        MAX_ARTWORK_BYTES,
        MAX_ARTWORK_BYTES + 1,
        MAX_ARTWORK_BYTES + 3,
    ] {
        let mut bytes = PNG.to_vec();
        bytes.resize(length, 42);
        wav(&path);
        // UTF-8 keeps this boundary fixture inside the existing 16 MiB SMED cap.
        add(&path, b"SMED", &smed(&xml(&picture(&bytes)), false));
        let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_text_only(&metadata.attributes);
        if length == MAX_ARTWORK_BYTES {
            assert_eq!(cover.unwrap().bytes, bytes);
        } else {
            assert!(cover.is_none());
            assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
        }
    }
}

#[test]
fn smed_artwork_obeys_xml_depth_and_rejects_dtd() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("xml-bounds.wav");
    let nested = format!(
        "<Picture>{}{}{}</Picture>",
        "<x>".repeat(MAX_XML_DEPTH),
        STANDARD.encode(PNG),
        "</x>".repeat(MAX_XML_DEPTH)
    );
    wav(&path);
    add(&path, b"SMED", &smed(&xml(&nested), true));
    let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
    assert_text_only(&metadata.attributes);
    assert!(cover.is_none());
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
    wav(&path);
    let document = format!(
        "<!DOCTYPE MAGIC [<!ENTITY picture '{}'>]><MAGIC><Picture>&picture;</Picture></MAGIC>",
        STANDARD.encode(PNG)
    );
    add(&path, b"SMED", &smed(&document, true));
    let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
    assert!(cover.is_none());
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_array());
}

#[test]
fn malformed_smed_picture_does_not_override_valid_id3_or_add_artwork_warning() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("id3-first.wav");
    wav(&path);
    add(&path, b"ID3 ", &id3(&[(JPEG, 0, "image/png")]));
    add(
        &path,
        b"SMED",
        &smed(&xml("<Picture type=\"NSData\">!!!=</Picture>"), true),
    );
    let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
    assert_text_only(&metadata.attributes);
    assert_eq!(cover.unwrap().bytes, JPEG);
    assert!(metadata.attributes["wav_metadata"]["warnings"].is_null());
}

fn real_wavs(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
        {
            files.push(path.to_owned());
        }
    } else {
        for entry in fs::read_dir(path).expect("real fixture directory must exist") {
            real_wavs(&entry.unwrap().path(), files);
        }
    }
}

#[test]
#[ignore = "requires read-only BOOM WAVs or an owned WAV retaining an authentic SMED tag"]
fn real_boom_smed_artwork_fixture() {
    let root = std::env::var_os("AUDIOLIB_BOOM_ARTWORK_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test_audiolib/P-51 Mustang")
        });
    let mut files = Vec::new();
    real_wavs(&root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "fixture must contain at least one WAV");
    for path in files {
        let before = fs::metadata(&path).unwrap();
        let ordinary = crate::probe(&path).unwrap();
        let (metadata, cover) = crate::probe_with_artwork(&path).unwrap();
        assert_eq!(
            ordinary.attributes,
            metadata.attributes,
            "{}",
            path.display()
        );
        let cover = cover.expect("authentic BOOM fixture should supply embedded artwork");
        assert!(matches!(cover.mime.as_str(), "image/jpeg" | "image/png"));
        assert!(cover.bytes.len() <= MAX_ARTWORK_BYTES);
        let mut handle = fs::File::open(&path).unwrap();
        let (_, from_handle) = crate::probe_file_with_artwork(&mut handle).unwrap();
        assert_eq!(from_handle.as_ref(), Some(&cover));
        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        eprintln!(
            "BOOM embedded fixture: {} {} {} bytes",
            path.display(),
            cover.mime,
            cover.bytes.len()
        );
    }
}
