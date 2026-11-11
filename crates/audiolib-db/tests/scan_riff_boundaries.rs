// crates/audiolib-db/tests/scan_riff_boundaries.rs
//! RIFF parent bounds survive scanning, incremental reuse and annotation refreshes.
#![cfg(feature = "scanner")]

use audiolib_core::{DataProvider, MetadataPatch, ScanProgress, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{collections::BTreeSet, fs, path::Path, sync::Arc};

fn append_chunk(bytes: &mut Vec<u8>, name: &[u8; 4], payload: &[u8]) {
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    if !payload.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn set_riff_end(bytes: &mut [u8], end: usize) {
    bytes[4..8].copy_from_slice(&((end - 8) as u32).to_le_bytes());
}

fn wav() -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&48_000_u32.to_le_bytes());
    fmt.extend_from_slice(&96_000_u32.to_le_bytes());
    fmt.extend_from_slice(&2_u16.to_le_bytes());
    fmt.extend_from_slice(&16_u16.to_le_bytes());
    append_chunk(&mut bytes, b"fmt ", &fmt);
    append_chunk(&mut bytes, b"data", &[0; 8]);
    let end = bytes.len();
    set_riff_end(&mut bytes, end);
    bytes
}

fn append_info(bytes: &mut Vec<u8>, description: &str) {
    let mut info = b"INFO".to_vec();
    append_chunk(&mut info, b"ICMT", description.as_bytes());
    append_chunk(bytes, b"LIST", &info);
}

fn scan(service: &ScanService, root: &Path, incremental: bool) -> ScanProgress {
    service
        .scan(
            root,
            &ScanOptions {
                incremental,
                batch_size: 2,
                workers: 2,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
}

#[test]
fn external_id3helper_tail_scans_once_and_preserves_annotations_on_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let source = root.join("external-tail.wav");
    let mut bytes = wav();
    // The data chunk ends exactly at the RIFF parent end. This vendor trailer
    // belongs outside RIFF and must not become an enormous "ID3H" child chunk.
    bytes.extend_from_slice(b"ID3Helper external vendor trailer");
    fs::write(&source, &bytes).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let service = ScanService::new(provider.clone());

    let initial = scan(&service, &root, true);
    assert_eq!(initial.state, ScanState::Completed);
    assert_eq!((initial.added, initial.errors), (1, 0));
    let page = provider.search(&SearchRequest::default()).unwrap();
    assert_eq!(page.total_count, 1);
    let id = page.results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("retained-riff-tag".into()),
                notes: Some("retained RIFF notes".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

    let repeated = scan(&service, &root, true);
    assert_eq!(
        (
            repeated.added,
            repeated.updated,
            repeated.unchanged,
            repeated.errors
        ),
        (0, 0, 1, 0)
    );
    // Full refresh exercises the existing-record merge rather than only its
    // unchanged fast path, without altering or replacing the source audio.
    let refreshed = scan(&service, &root, false);
    assert_eq!(
        (refreshed.added, refreshed.updated, refreshed.errors),
        (0, 1, 0)
    );
    let page = provider.search(&SearchRequest::default()).unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results[0].id, id);
    let retained = provider.get_audio(id).unwrap();
    assert_eq!(retained.tags, "retained-riff-tag");
    assert_eq!(retained.notes, "retained RIFF notes");
    assert!(retained.favorite);
    assert!(!retained.missing);
    assert_eq!(fs::read(source).unwrap(), bytes);
}

#[test]
fn metadata_after_data_is_indexed_only_inside_the_riff_parent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let source = root.join("metadata-after-data.wav");
    let mut bytes = wav();
    append_info(&mut bytes, "boundaryacceptedmetadataterm");
    let end = bytes.len();
    set_riff_end(&mut bytes, end);
    // Even a structurally valid LIST outside the parent must not enter Metadata.
    append_info(&mut bytes, "boundaryexternalmetadataterm");
    fs::write(&source, &bytes).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let progress = scan(&ScanService::new(provider.clone()), &root, true);
    assert_eq!((progress.added, progress.errors), (1, 0));
    let accepted = provider
        .search(&SearchRequest {
            query: "boundaryacceptedmetadataterm".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(accepted.total_count, 1);
    assert_eq!(
        accepted.results[0].attributes["wav_metadata"]["fields"]["description"],
        "boundaryacceptedmetadataterm"
    );
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "boundaryexternalmetadataterm".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
    assert_eq!(fs::read(source).unwrap(), bytes);
}

#[test]
fn in_parent_chunk_overruns_still_fail_without_marking_existing_audio_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let source = root.join("retained.wav");
    fs::write(&source, wav()).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let service = ScanService::new(provider.clone());
    assert_eq!(scan(&service, &root, false).added, 1);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    fs::remove_file(source).unwrap();

    let mut data_overrun = wav();
    let end = data_overrun.len() - 4;
    set_riff_end(&mut data_overrun, end);
    // Physical bytes exist, but data may not claim bytes beyond its parent.
    fs::write(root.join("data-parent-overrun.wav"), data_overrun).unwrap();
    let mut metadata_overrun = wav();
    append_info(&mut metadata_overrun, "invalidparentmetadata");
    let end = metadata_overrun.len() - 2;
    set_riff_end(&mut metadata_overrun, end);
    fs::write(root.join("metadata-parent-overrun.wav"), metadata_overrun).unwrap();
    let mut truncated = wav();
    truncated.truncate(truncated.len() - 2);
    fs::write(root.join("physically-truncated.wav"), truncated).unwrap();

    let progress = scan(&service, &root, false);
    assert_eq!(progress.state, ScanState::Completed);
    assert_eq!(
        (progress.discovered, progress.processed, progress.errors),
        (3, 3, 3)
    );
    assert_eq!(
        (progress.added, progress.updated, progress.removed),
        (0, 0, 0)
    );
    let failures: BTreeSet<_> = progress
        .error_details
        .iter()
        .map(|issue| {
            Path::new(&issue.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        failures,
        BTreeSet::from([
            "data-parent-overrun.wav".into(),
            "metadata-parent-overrun.wav".into(),
            "physically-truncated.wav".into()
        ])
    );
    assert!(!provider.get_audio(id).unwrap().missing);
    let page = provider.search(&SearchRequest::default()).unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results[0].id, id);
}
