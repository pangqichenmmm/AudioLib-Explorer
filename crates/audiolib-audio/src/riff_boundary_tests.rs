// crates/audiolib-audio/src/riff_boundary_tests.rs
use crate::{decoder::Decoder, probe};
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

fn fmt() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1_u16.to_le_bytes());
    payload.extend_from_slice(&1_u16.to_le_bytes());
    payload.extend_from_slice(&48000_u32.to_le_bytes());
    payload.extend_from_slice(&96000_u32.to_le_bytes());
    payload.extend_from_slice(&2_u16.to_le_bytes());
    payload.extend_from_slice(&16_u16.to_le_bytes());
    chunk(b"fmt ", &payload)
}

fn body() -> Vec<u8> {
    let mut bytes = fmt();
    bytes.extend(chunk(b"data", &[0, 0, 0, 64]));
    bytes
}

fn riff(body: &[u8]) -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(body);
    bytes
}

fn set_riff_end(bytes: &mut [u8], end: usize) {
    bytes[4..8].copy_from_slice(&((end - 8) as u32).to_le_bytes());
}

fn sentinel_chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = chunk(id, payload);
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes
}

fn large_body() -> Vec<u8> {
    let mut bytes = fmt();
    bytes.extend(sentinel_chunk(b"data", &[0, 0, 0, 64]));
    bytes
}

fn large_riff(id: &[u8; 4], body: &[u8], table: &[([u8; 4], u64)]) -> Vec<u8> {
    let mut ds64 = vec![0; 28];
    ds64[8..16].copy_from_slice(&4_u64.to_le_bytes());
    ds64[16..24].copy_from_slice(&2_u64.to_le_bytes());
    ds64[24..28].copy_from_slice(&(table.len() as u32).to_le_bytes());
    for (chunk_id, size) in table {
        ds64.extend_from_slice(chunk_id);
        ds64.extend_from_slice(&size.to_le_bytes());
    }
    let riff_size = 4 + 8 + ds64.len() as u64 + body.len() as u64;
    ds64[..8].copy_from_slice(&riff_size.to_le_bytes());
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend(chunk(b"ds64", &ds64));
    bytes.extend_from_slice(body);
    bytes
}

fn assert_audio(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    let metadata = probe(path).unwrap();
    assert_eq!(metadata.frames, 2);
    assert_eq!(metadata.channels, 1);
    assert_eq!(Decoder::open(path).unwrap().read(8).unwrap(), [0.0, 0.5]);
}

#[test]
fn riff_external_trailers_do_not_become_chunks_or_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("tail.wav");
    let clean = riff(&body());
    let mut info = b"INFO".to_vec();
    info.extend(chunk(b"INAM", b"outside title\0"));
    for tail in [
        b"ID3Helper private trailer".to_vec(),
        vec![0xff; 3],
        [b"LIST".as_slice(), &u32::MAX.to_le_bytes()].concat(),
        chunk(b"LIST", &info),
    ] {
        let mut bytes = clean.clone();
        bytes.extend(tail);
        assert_audio(&path, &bytes);
        let metadata = probe(&path).unwrap();
        assert!(metadata.attributes["wav_metadata"]["riff_info"]["INAM"].is_null());
        assert!(!metadata.attributes.to_string().contains("outside title"));
    }
}

#[test]
fn metadata_after_data_inside_riff_is_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.wav");
    let mut data = body();
    let mut info = b"INFO".to_vec();
    info.extend(chunk(b"INAM", b"inside title\0"));
    data.extend(chunk(b"LIST", &info));
    let mut bytes = riff(&data);
    bytes.extend_from_slice(b"ID3Helper");
    assert_audio(&path, &bytes);
    assert_eq!(
        probe(&path).unwrap().attributes["wav_metadata"]["riff_info"]["INAM"],
        "inside title"
    );
}

#[test]
fn data_or_metadata_crossing_parent_bounds_is_rejected_even_with_physical_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("parent.wav");
    let mut data = riff(&body());
    let data_end = data.len();
    set_riff_end(&mut data, data_end - 2);
    fs::write(&path, data).unwrap();
    assert!(probe(&path).is_err());
    assert!(Decoder::open(&path).is_err());

    let mut contents = body();
    contents.extend(chunk(b"LIST", b"INFO"));
    let mut bytes = riff(&contents);
    let end = bytes.len();
    set_riff_end(&mut bytes, end - 2);
    fs::write(&path, bytes).unwrap();
    assert!(probe(&path).is_err());
}

#[test]
fn physically_truncated_audio_or_metadata_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("truncated.wav");
    for mut contents in [body(), {
        let mut data = body();
        data.extend(chunk(b"LIST", b"INFO"));
        data
    }] {
        let mut bytes = riff(&contents);
        bytes.pop();
        fs::write(&path, bytes).unwrap();
        assert!(probe(&path).is_err());
        // Also reject when the parent accurately describes the shorter file but
        // the child still declares a payload extending beyond it.
        contents.pop();
        fs::write(&path, riff(&contents)).unwrap();
        assert!(probe(&path).is_err());
    }
}

#[test]
fn odd_chunks_require_padding_inside_the_parent() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("odd.wav");
    let mut contents = fmt();
    contents.extend(chunk(b"JUNK", &[1, 2, 3]));
    contents.extend(chunk(b"data", &[0, 0, 0, 64]));
    assert_audio(&path, &riff(&contents));

    let mut contents = body();
    contents.extend(chunk(b"JUNK", &[1, 2, 3]));
    assert_audio(&path, &riff(&contents));
    let mut bytes = riff(&contents);
    let end_without_pad = bytes.len() - 1;
    set_riff_end(&mut bytes, end_without_pad);
    fs::write(&path, &bytes).unwrap();
    assert!(probe(&path).is_err());
    bytes.pop();
    fs::write(&path, &bytes).unwrap();
    assert!(probe(&path).is_err());
}

#[test]
fn invalid_parent_lengths_and_partial_internal_headers_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid-parent.wav");
    for declared_size in [0, 3, u32::MAX] {
        let mut bytes = riff(&body());
        bytes[4..8].copy_from_slice(&declared_size.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(probe(&path).is_err(), "size={declared_size}");
    }
    for remainder in 1..8 {
        let mut contents = body();
        contents.extend(vec![0; remainder]);
        fs::write(&path, riff(&contents)).unwrap();
        assert!(probe(&path).is_err(), "remainder={remainder}");
    }
}

#[test]
fn rf64_and_bw64_use_ds64_parent_bounds_and_ignore_external_tails() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large.wav");
    for id in [b"RF64", b"BW64"] {
        let mut bytes = large_riff(id, &large_body(), &[]);
        assert_audio(&path, &bytes);
        let parent_end = bytes.len() as u64;
        bytes.extend_from_slice(b"ID3Helper private trailer");
        assert_audio(&path, &bytes);
        bytes[20..28].copy_from_slice(&(parent_end - 10).to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(probe(&path).is_err());
    }
}

#[test]
fn rf64_and_bw64_require_a_valid_first_ds64_chunk() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid-ds64.wav");
    for id in [b"RF64", b"BW64"] {
        let mut missing = riff(&body());
        missing[..4].copy_from_slice(id);
        missing[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, missing).unwrap();
        assert!(probe(&path).is_err());

        let mut short = id.to_vec();
        short.extend_from_slice(&u32::MAX.to_le_bytes());
        short.extend_from_slice(b"WAVE");
        short.extend(chunk(b"ds64", &[0; 24]));
        short.extend(large_body());
        fs::write(&path, short).unwrap();
        assert!(probe(&path).is_err());

        let mut late = large_riff(id, &large_body(), &[]);
        late.splice(12..12, chunk(b"JUNK", &[]));
        let size = (late.len() - 8) as u64;
        late[28..36].copy_from_slice(&size.to_le_bytes());
        fs::write(&path, late).unwrap();
        assert!(probe(&path).is_err());

        let mut contents = large_body();
        contents.extend(chunk(b"ds64", &[0; 28]));
        fs::write(&path, large_riff(id, &contents, &[])).unwrap();
        assert!(probe(&path).is_err());
    }
}

#[test]
fn ds64_invalid_parent_lengths_data_overflow_and_short_tables_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ds64-bounds.wav");
    let clean = large_riff(b"RF64", &large_body(), &[]);
    for riff_size in [0, 3, 20, clean.len() as u64, u64::MAX] {
        let mut bytes = clean.clone();
        bytes[20..28].copy_from_slice(&riff_size.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(probe(&path).is_err(), "riff_size={riff_size}");
    }
    let mut bytes = clean.clone();
    bytes[28..36].copy_from_slice(&u64::MAX.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(probe(&path).is_err());
    for table_length in [1_u32, u32::MAX] {
        let mut bytes = clean.clone();
        bytes[44..48].copy_from_slice(&table_length.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(probe(&path).is_err(), "table_length={table_length}");
    }
}

#[test]
fn rf64_sentinel_non_data_chunks_are_rejected_even_with_a_size_table() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("table.wav");
    let mut contents = fmt();
    contents.extend(sentinel_chunk(b"JUNK", &[1, 2]));
    contents.extend(sentinel_chunk(b"data", &[0, 0, 0, 64]));
    for id in [b"RF64", b"BW64"] {
        for table in [
            vec![],
            vec![(*b"JUNK", 2)],
            vec![(*b"LIST", 2)],
            vec![(*b"JUNK", u64::MAX)],
        ] {
            fs::write(&path, large_riff(id, &contents, &table)).unwrap();
            assert!(probe(&path).is_err());
        }
    }
}
