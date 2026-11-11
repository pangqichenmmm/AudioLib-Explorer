// crates/audiolib-db/tests/vorbis_scan.rs
//! Real FLAC/Ogg scanning, searchable source tags and lossless incremental re-extraction.
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{fs, path::Path, sync::Arc};
mod support;

fn copy_sources(root: &Path) {
    fs::create_dir_all(root).unwrap();
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../audiolib-audio/src/fixtures/vorbis");
    for name in ["comments.flac", "comments.ogg", "comments.oga"] {
        fs::copy(fixtures.join(name), root.join(name)).unwrap();
    }
}

#[test]
fn actual_vorbis_tags_scan_search_and_reextract_without_losing_annotations_or_vendor_data() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sources");
    copy_sources(&root);
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let scan = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(scan.added, 3);
    assert_eq!(scan.errors, 0, "{scan:?}");
    let original = provider
        .search(&SearchRequest {
            query: "树林 微风".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(original.total_count, 3);
    for file in &original.results {
        assert_eq!(
            file.attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
            "树林微风"
        );
        assert_eq!(
            file.attributes["wav_metadata"]["vorbis_version"],
            audiolib_audio::VORBIS_METADATA_VERSION
        );
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    notes: Some("user annotation".into()),
                    tags: Some("user-tag".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let conn = support::connection(&database).unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_set(metadata_json,'$.unknown_extension',json('{\"preserve\":true}'),'$.vendor_metadata',json('{\"fields\":{\"description\":\"Vendor controlled description\",\"keywords\":\"vendor-word\"}}'),'$.wav_metadata.vorbis_version',0)",[]).unwrap();
    drop(conn);
    let again = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(again.updated, 3);
    assert_eq!(again.errors, 0);
    for old in original.results {
        let file = provider.get_audio(old.id).unwrap();
        assert_eq!(file.notes, "user annotation");
        assert_eq!(file.tags, "user-tag");
        assert!(file.favorite);
        assert_eq!(file.attributes["unknown_extension"]["preserve"], true);
        assert_eq!(
            file.attributes["vendor_metadata"]["fields"]["description"],
            "Vendor controlled description"
        );
        assert_eq!(
            audiolib_core::source_description("", &file.attributes),
            "Vendor controlled description"
        );
        assert_eq!(
            file.attributes["wav_metadata"]["vorbis_version"],
            audiolib_audio::VORBIS_METADATA_VERSION
        );
    }
    for query in [
        "树林 微风",
        "Vendor controlled",
        "vendor-word",
        "user annotation",
    ] {
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: query.into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            3,
            "{query}"
        );
    }
    let unchanged = ScanService::new(provider)
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(unchanged.unchanged, 3);
    assert_eq!(unchanged.updated, 0);
}

#[test]
fn incremental_vorbis_reextract_preserves_hot_rankings_annotations_and_directory_membership() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sources");
    let recordings = root.join("recordings");
    let detached = root.join("detached");
    let unrelated = root.join("unrelated");
    copy_sources(&recordings);
    fs::create_dir_all(&detached).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    // Keep the three path members below the existing >50% wide-path policy.
    // These four controls are valid mono PCM WAVs with one silent sample, using
    // the same owned container layout as scan_boundaries; no forged audio rows.
    let mut control = Vec::new();
    control.extend_from_slice(b"RIFF");
    control.extend_from_slice(&38u32.to_le_bytes());
    control.extend_from_slice(b"WAVEfmt ");
    control.extend_from_slice(&16u32.to_le_bytes());
    control.extend_from_slice(&1u16.to_le_bytes());
    control.extend_from_slice(&1u16.to_le_bytes());
    control.extend_from_slice(&48_000u32.to_le_bytes());
    control.extend_from_slice(&96_000u32.to_le_bytes());
    control.extend_from_slice(&2u16.to_le_bytes());
    control.extend_from_slice(&16u16.to_le_bytes());
    control.extend_from_slice(b"data");
    control.extend_from_slice(&2u32.to_le_bytes());
    control.extend_from_slice(&0i16.to_le_bytes());
    for index in 0..4 {
        fs::write(unrelated.join(format!("control-{index}.wav")), &control).unwrap();
    }
    let source_bytes: Vec<_> = ["comments.flac", "comments.ogg", "comments.oga"]
        .into_iter()
        .map(|name| {
            (
                recordings.join(name),
                fs::read(recordings.join(name)).unwrap(),
            )
        })
        .collect();
    let database = temp.path().join("hot-vorbis.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let run_scan = || {
        ScanService::new(provider.clone())
            .scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    };
    assert_eq!(run_scan().added, 7);
    provider.ensure_scan_folder(&detached, &root).unwrap();
    let original = provider
        .search(&SearchRequest {
            query: "树林 微风".into(),
            ..Default::default()
        })
        .unwrap()
        .results;
    assert_eq!(original.len(), 3);
    for row in &original {
        provider
            .update_metadata(
                row.id,
                &MetadataPatch {
                    notes: Some("preserved user annotation".into()),
                    tags: Some("preserved-user-tag".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let conn = support::connection(&database).unwrap();
    conn.execute(
        "UPDATE audio_files SET metadata_json=json_set(metadata_json,
         '$.wav_metadata.vorbis_version',0,
         '$.wav_metadata.vorbis.DESCRIPTION','oldextractonly',
         '$.wav_metadata.fields.description','oldextractonly',
         '$.unknown_extension',json('{\"preserve\":true}'),
         '$.vendor_metadata',json('{\"fields\":{\"description\":\"Preserved vendor text\",\"keywords\":\"preserved-vendor-tag\"}}')),
         folder_id=(SELECT id FROM folders WHERE folder_name='detached')
         WHERE id IN (?1,?2,?3)", rusqlite::params![original[0].id, original[1].id, original[2].id],
    ).unwrap();
    // The source path stays authoritative. The refresh must repair the old folder
    // membership while preserving the annotations and independent vendor mirror.
    provider.rebuild_directory_search().unwrap();
    let requests = ["oldextractonly", "树林 微风"].map(|query| SearchRequest {
        query: query.into(),
        scope: audiolib_core::SearchScope::Main,
        ..Default::default()
    });
    for (request, expected) in requests.iter().zip([3, 0]) {
        assert_eq!(provider.search(request).unwrap().total_count, expected);
        assert!(
            provider.search(request).unwrap().cache_hit,
            "warm before scan: {request:?}"
        );
    }
    let mut observed_completed_batch = false;
    let refreshed = ScanService::new(provider.clone())
        .scan(
            &recordings,
            &ScanOptions {
                batch_size: 3,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |progress| {
                if progress.updated == 3 && !progress.finalizing {
                    observed_completed_batch = true;
                    for (request, expected) in requests.iter().zip([0, 3]) {
                        let page = provider.search(request).unwrap();
                        assert!(page.cache_hit, "published scan batch: {request:?}");
                        assert_eq!(page.total_count, expected);
                    }
                }
            },
        )
        .unwrap();
    assert!(observed_completed_batch);
    assert_eq!(refreshed.updated, 3);
    assert_eq!(refreshed.errors, 0, "{refreshed:?}");
    for (request, expected) in requests.iter().zip([0, 3]) {
        let page = provider.search(request).unwrap();
        assert!(
            page.cache_hit,
            "incremental finalization retained cache: {request:?}"
        );
        assert_eq!(page.total_count, expected);
    }
    for old in original {
        let current = provider.get_audio(old.id).unwrap();
        assert_eq!(current.notes, "preserved user annotation");
        assert_eq!(current.tags, "preserved-user-tag");
        assert!(current.favorite);
        assert_eq!(current.attributes["unknown_extension"]["preserve"], true);
        assert_eq!(
            current.attributes["vendor_metadata"]["fields"]["description"],
            "Preserved vendor text"
        );
        assert_eq!(
            current.attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
            "树林微风"
        );
        assert_eq!(
            current.attributes["wav_metadata"]["vorbis_version"],
            audiolib_audio::VORBIS_METADATA_VERSION
        );
    }
    for (query, expected) in [("recordings", 3), ("detached", 0)] {
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: query.into(),
                    scope: audiolib_core::SearchScope::Path,
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            expected,
            "{query}"
        );
    }
    for (path, original) in source_bytes {
        assert_eq!(
            fs::read(path).unwrap(),
            original,
            "source audio must remain byte-identical"
        );
    }
    let unchanged = run_scan();
    assert_eq!(unchanged.unchanged, 7);
    assert_eq!(unchanged.updated, 0);
    assert_eq!(unchanged.errors, 0);
    assert!(provider.search(&requests[1]).unwrap().cache_hit);
}
