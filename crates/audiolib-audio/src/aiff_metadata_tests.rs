// crates/audiolib-audio/src/aiff_metadata_tests.rs
//! Owned AIFF/AIFC byte fixtures cover layout, Metadata bounds, decoding, and seeks.
use super::*;
use crate::decoder::Decoder;
use std::{fs, io::Cursor};

fn extended(rate: u32) -> [u8; 10] {
    let shift = 31 - rate.leading_zeros();
    let mut bytes = [0_u8; 10];
    bytes[..2].copy_from_slice(&(16_383 + shift as u16).to_be_bytes());
    bytes[2..].copy_from_slice(&(u64::from(rate) << (63 - shift)).to_be_bytes());
    bytes
}

fn comm(bits: u16, channels: u16, frames: u32, compression: Option<&[u8; 4]>) -> Vec<u8> {
    let mut bytes = channels.to_be_bytes().to_vec();
    bytes.extend(frames.to_be_bytes());
    bytes.extend(bits.to_be_bytes());
    bytes.extend(extended(48_000));
    if let Some(compression) = compression {
        bytes.extend(compression);
        bytes.extend([0, 0]);
    }
    bytes
}

fn ssnd(samples: &[u8], offset: u32) -> Vec<u8> {
    let mut bytes = offset.to_be_bytes().to_vec();
    bytes.extend(0_u32.to_be_bytes());
    bytes.resize(8 + offset as usize, 0x55);
    bytes.extend(samples);
    bytes
}

fn form(kind: &[u8; 4], chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut bytes = b"FORM\0\0\0\0".to_vec();
    bytes.extend(kind);
    for (id, body) in chunks {
        bytes.extend(*id);
        bytes.extend((body.len() as u32).to_be_bytes());
        bytes.extend(body);
        if !body.len().is_multiple_of(2) {
            bytes.push(0);
        }
    }
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_be_bytes());
    bytes
}

fn read(bytes: Vec<u8>) -> Result<WavInfo> {
    let len = bytes.len() as u64;
    read_info(Cursor::new(bytes), len, true, false)
}

fn syncsafe(size: usize) -> [u8; 4] {
    [
        ((size >> 21) & 127) as u8,
        ((size >> 14) & 127) as u8,
        ((size >> 7) & 127) as u8,
        (size & 127) as u8,
    ]
}

fn id3(frames: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in frames {
        body.extend(*id);
        body.extend(syncsafe(value.len()));
        body.extend([0, 0]);
        body.extend(value);
    }
    let mut tag = b"ID3\x04\0\0".to_vec();
    tag.extend(syncsafe(body.len()));
    tag.extend(body);
    tag
}

fn tagged_text(value: &str) -> Vec<u8> {
    let mut bytes = vec![3];
    bytes.extend(value.as_bytes());
    bytes
}

#[test]
fn comm_frames_are_authoritative_and_valid_bits_keep_the_storage_width() {
    for bits in [1_u16, 8, 9, 16, 20, 24, 31, 32] {
        let width = usize::from(bits.div_ceil(8));
        let bytes = form(
            b"AIFF",
            &[
                (b"COMM", comm(bits, 2, 3, None)),
                (b"SSND", ssnd(&vec![0; 2 * width * 5], 3)),
            ],
        );
        let info = read(bytes).unwrap();
        assert_eq!(info.metadata.frames, 3);
        assert_eq!(info.metadata.sample_rate, 48_000);
        assert_eq!(info.metadata.channels, 2);
        assert_eq!(info.metadata.bit_depth, bits);
        assert_eq!(info.block_align, (2 * width) as u16);
        assert_eq!(info.pcm_layout.unwrap().bytes_per_sample, width as u8);
        assert_eq!(info.metadata.attributes["container"], "AIFF");
        assert_eq!(info.metadata.attributes["wav_metadata"]["aiff_version"], 1);
    }
}

#[test]
fn signed_and_float_containers_decode_with_their_real_byte_order_and_seek() {
    type Case<'a> = (&'a [u8; 4], u16, Option<&'a [u8; 4]>, Vec<u8>);
    let cases: Vec<Case<'_>> = vec![
        (b"AIFF", 8, None, vec![0x80, 0, 0x40]),
        (b"AIFF", 9, None, vec![0x80, 0, 0, 0, 0x40, 0]),
        (b"AIFF", 16, None, vec![0x80, 0, 0, 0, 0x40, 0]),
        (b"AIFF", 24, None, vec![0x80, 0, 0, 0, 0, 0, 0x40, 0, 0]),
        (
            b"AIFF",
            32,
            None,
            [-2_147_483_648_i32, 0, 1_073_741_824]
                .into_iter()
                .flat_map(i32::to_be_bytes)
                .collect(),
        ),
        (b"AIFC", 16, Some(b"NONE"), vec![0x80, 0, 0, 0, 0x40, 0]),
        (b"AIFC", 16, Some(b"twos"), vec![0x80, 0, 0, 0, 0x40, 0]),
        (b"AIFC", 16, Some(b"sowt"), vec![0, 0x80, 0, 0, 0, 0x40]),
        (
            b"AIFC",
            32,
            Some(b"fl32"),
            [-1_f32, 0.0, 0.5]
                .into_iter()
                .flat_map(f32::to_be_bytes)
                .collect(),
        ),
        (
            b"AIFC",
            64,
            Some(b"fl64"),
            [-1_f64, 0.0, 0.5]
                .into_iter()
                .flat_map(f64::to_be_bytes)
                .collect(),
        ),
    ];
    let temp = tempfile::tempdir().unwrap();
    for (index, (kind, bits, compression, samples)) in cases.into_iter().enumerate() {
        let bytes = form(
            kind,
            &[
                (b"COMM", comm(bits, 1, 3, compression)),
                (b"SSND", ssnd(&samples, 2)),
            ],
        );
        let path = temp.path().join(format!("owned-{index}.aiff"));
        fs::write(&path, bytes).unwrap();
        let mut decoder = Decoder::open(&path).unwrap();
        let actual = decoder.read(10).unwrap();
        assert_eq!(actual, [-1.0, 0.0, 0.5], "case {index}");
        assert!(decoder.read(1).unwrap().is_empty());
        decoder.seek(1).unwrap();
        assert_eq!(decoder.read(2).unwrap(), [0.0, 0.5], "case {index}");
        decoder.seek(0).unwrap();
        assert_eq!(decoder.read(1).unwrap(), [-1.0], "case {index}");
    }
}

#[test]
fn opaque_wav_stream_id_uses_aiff_magic_for_probe_decode_and_seek() {
    struct Registered(std::path::PathBuf);
    impl Drop for Registered {
        fn drop(&mut self) {
            crate::unregister_stream_source(&self.0);
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("opaque-server-stream.wav");
    let bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 3, None)),
            (b"SSND", ssnd(&[0x80, 0, 0, 0, 0x40, 0], 0)),
        ],
    );
    let opener: crate::SourceOpener = std::sync::Arc::new(move || {
        let len = bytes.len() as u64;
        let source: Box<dyn crate::ByteSource> = Box::new(Cursor::new(bytes.clone()));
        Ok((source, len))
    });
    crate::register_stream_source(&path, opener);
    let _registered = Registered(path.clone());
    assert_eq!(crate::probe(&path).unwrap().attributes["container"], "AIFF");
    let mut decoder = Decoder::open(&path).unwrap();
    assert_eq!(decoder.read(3).unwrap(), [-1.0, 0.0, 0.5]);
    decoder.seek(2).unwrap();
    assert_eq!(decoder.read(3).unwrap(), [0.5]);
}

#[test]
fn id3_is_canonical_but_aiff_originals_and_comment_context_survive() {
    let mut comment = 1_u16.to_be_bytes().to_vec();
    comment.extend(1234_u32.to_be_bytes());
    comment.extend((-1_i16).to_be_bytes());
    comment.extend(5_u16.to_be_bytes());
    comment.extend(b"notes\0");
    let bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 1, None)),
            (b"NAME", b"Original title".to_vec()),
            (b"NAME", b"Second title".to_vec()),
            (b"AUTH", b"Original artist".to_vec()),
            (b"ANNO", b"Original description".to_vec()),
            (b"(c) ", b"Owned synthetic fixture".to_vec()),
            (b"COMT", comment),
            (b"SSND", ssnd(&[0, 0], 0)),
            (
                b"ID3 ",
                id3(&[
                    (b"TIT2", tagged_text("ID3 title")),
                    (b"COMM", {
                        let mut value = vec![3];
                        value.extend(b"eng\0");
                        value.extend("树林微风".as_bytes());
                        value
                    }),
                    (b"TXXX", tagged_text("UNKNOWN\0retained vendor text")),
                ]),
            ),
        ],
    );
    let info = read(bytes).unwrap();
    let metadata = &info.metadata.attributes["wav_metadata"];
    assert_eq!(
        metadata["aiff"]["NAME"],
        json!(["Original title", "Second title"])
    );
    assert_eq!(metadata["aiff"]["COMT"], "notes");
    assert_eq!(metadata["aiff"]["COMT_records"][0]["marker_id"], -1);
    assert_eq!(metadata["aiff"]["COMT_records"][0]["timestamp"], 1234);
    assert_eq!(metadata["fields"]["title"], "ID3 title");
    assert_eq!(metadata["fields"]["description"], "树林微风");
    assert_eq!(metadata["fields"]["artist"], "Original artist");
    let search = audiolib_core::source_metadata_search_text("", &info.metadata.attributes);
    for value in [
        "Original title",
        "Second title",
        "Original description",
        "树林微风",
        "retained vendor text",
    ] {
        assert!(search.contains(value), "{value}: {search}");
    }
}

#[test]
fn aiff_text_alone_populates_canonical_fields_and_trailing_bytes_are_only_warnings() {
    let mut bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 1, None)),
            (b"SSND", ssnd(&[0, 0], 0)),
            (b"NAME", b"Plain title".to_vec()),
            (b"ANNO", b"Plain description".to_vec()),
        ],
    );
    bytes.extend(b"private trailer with untrusted length");
    let info = read(bytes).unwrap();
    assert_eq!(
        info.metadata.attributes["wav_metadata"]["fields"]["title"],
        "Plain title"
    );
    assert_eq!(
        audiolib_core::source_description("", &info.metadata.attributes),
        "Plain description"
    );
    assert!(
        info.metadata.attributes["wav_metadata"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("trailing bytes"))
    );
}

#[test]
fn artwork_is_opt_in_and_binary_payloads_never_enter_metadata() {
    const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nowned-aiff-cover";
    let mut apic = vec![0];
    apic.extend(b"image/png\0\x03\0");
    apic.extend(IMAGE);
    let bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 1, None)),
            (b"SSND", ssnd(&[0, 0], 0)),
            (b"ID3 ", id3(&[(b"APIC", apic)])),
        ],
    );
    let ordinary = read(bytes.clone()).unwrap();
    assert!(ordinary.artwork.is_none());
    assert!(
        !ordinary
            .metadata
            .attributes
            .to_string()
            .contains("owned-aiff-cover")
    );
    let len = bytes.len() as u64;
    let selected = read_info(Cursor::new(bytes), len, true, true).unwrap();
    assert_eq!(selected.artwork.unwrap().bytes, IMAGE);
    assert!(
        !selected
            .metadata
            .attributes
            .to_string()
            .contains("owned-aiff-cover")
    );
}

struct HeaderOnly {
    source: Cursor<Vec<u8>>,
    end: u64,
}

impl Read for HeaderOnly {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.end.saturating_sub(self.source.position()) as usize;
        if !bytes.is_empty() && remaining == 0 {
            return Err(std::io::Error::other("sample/tail read is forbidden"));
        }
        let count = bytes.len().min(remaining);
        self.source.read(&mut bytes[..count])
    }
}

impl Seek for HeaderOnly {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            SeekFrom::Start(value) => value,
            SeekFrom::Current(value) => self
                .source
                .position()
                .checked_add_signed(value)
                .ok_or_else(|| std::io::Error::other("invalid seek"))?,
            SeekFrom::End(_) => return Err(std::io::Error::other("tail seek is forbidden")),
        };
        if next > self.end {
            return Err(std::io::Error::other("sample/tail seek is forbidden"));
        }
        self.source.seek(SeekFrom::Start(next))
    }
}

#[test]
fn lightweight_layout_stops_before_samples_offset_and_trailing_metadata() {
    let bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 100, None)),
            (b"SSND", ssnd(&[0; 200], 9)),
            (b"ANNO", b"must not be fetched to start playback".to_vec()),
        ],
    );
    let len = bytes.len() as u64;
    let source = HeaderOnly {
        source: Cursor::new(bytes),
        end: 12 + 8 + 18 + 8 + 8,
    };
    let info = read_info(source, len, false, false).unwrap();
    assert_eq!(info.metadata.frames, 100);
    assert_eq!(info.data_offset, 12 + 8 + 18 + 8 + 8 + 9);
    assert!(info.metadata.attributes.get("wav_metadata").is_none());
}

#[test]
fn malformed_audio_boundaries_and_unsupported_codecs_are_rejected() {
    let good_comm = comm(16, 1, 1, None);
    let good_sound = ssnd(&[0, 0], 0);
    let mut invalid = vec![
        form(b"AIFF", &[(b"COMM", good_comm.clone())]),
        form(b"AIFF", &[(b"SSND", good_sound.clone())]),
        form(
            b"AIFF",
            &[
                (b"COMM", good_comm.clone()),
                (b"COMM", good_comm.clone()),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFF",
            &[
                (b"COMM", good_comm.clone()),
                (b"SSND", good_sound.clone()),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFF",
            &[
                (b"COMM", comm(16, 1, 2, None)),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFF",
            &[
                (b"COMM", comm(33, 1, 1, None)),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFF",
            &[
                (b"COMM", comm(16, 0, 1, None)),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFC",
            &[
                (b"COMM", comm(16, 1, 1, Some(b"ima4"))),
                (b"SSND", good_sound.clone()),
            ],
        ),
        form(
            b"AIFC",
            &[
                (b"COMM", comm(16, 1, 1, Some(b"fl32"))),
                (b"SSND", good_sound.clone()),
            ],
        ),
    ];
    let mut bad_offset = good_sound.clone();
    bad_offset[..4].copy_from_slice(&3_u32.to_be_bytes());
    invalid.push(form(
        b"AIFF",
        &[(b"COMM", good_comm.clone()), (b"SSND", bad_offset)],
    ));
    let mut blocked = good_sound.clone();
    blocked[4..8].copy_from_slice(&2_u32.to_be_bytes());
    invalid.push(form(
        b"AIFF",
        &[(b"COMM", good_comm.clone()), (b"SSND", blocked)],
    ));
    let mut bad_name = comm(16, 1, 1, Some(b"NONE"));
    bad_name[22] = 10;
    invalid.push(form(
        b"AIFC",
        &[(b"COMM", bad_name), (b"SSND", good_sound.clone())],
    ));
    let mut parent = form(
        b"AIFF",
        &[(b"COMM", good_comm.clone()), (b"SSND", good_sound.clone())],
    );
    parent[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    invalid.push(parent);
    let mut padding = form(
        b"AIFF",
        &[
            (b"COMM", good_comm.clone()),
            (b"SSND", good_sound.clone()),
            (b"ANNO", vec![b'x']),
        ],
    );
    padding.pop();
    let size = (padding.len() - 8) as u32;
    padding[4..8].copy_from_slice(&size.to_be_bytes());
    invalid.push(padding);
    for (index, bytes) in invalid.into_iter().enumerate() {
        assert!(read(bytes).is_err(), "case {index}");
    }
}

#[test]
fn invalid_extended_sample_rates_never_turn_into_saturated_integer_rates() {
    let valid = extended(48_000);
    let mut negative = valid;
    negative[0] |= 0x80;
    let mut non_normal = valid;
    non_normal[2] &= 0x7f;
    let mut fractional = valid;
    fractional[9] = 1;
    for rate in [
        [0; 10],
        [0xff; 10],
        negative,
        non_normal,
        fractional,
        extended(u32::MAX),
    ] {
        if rate == extended(u32::MAX) {
            assert_eq!(sample_rate(&rate).unwrap(), u32::MAX);
        } else {
            assert!(sample_rate(&rate).is_err());
        }
    }
}

#[test]
fn oversized_or_malformed_metadata_does_not_discard_valid_audio() {
    let bytes = form(
        b"AIFF",
        &[
            (b"COMM", comm(16, 1, 1, None)),
            (b"SSND", ssnd(&[0, 0], 0)),
            (b"NAME", vec![b'x'; MAX_CHUNK_BYTES as usize + 1]),
            (b"COMT", vec![0, 1, 0]),
            (b"ANNO", b"retained annotation".to_vec()),
        ],
    );
    let info = read(bytes).unwrap();
    assert_eq!(info.metadata.frames, 1);
    assert_eq!(
        info.metadata.attributes["wav_metadata"]["fields"]["description"],
        "retained annotation"
    );
    assert!(
        info.metadata.attributes["wav_metadata"]["aiff"]
            .get("NAME")
            .is_none()
    );
    assert!(
        info.metadata.attributes["wav_metadata"]["warnings"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
}
