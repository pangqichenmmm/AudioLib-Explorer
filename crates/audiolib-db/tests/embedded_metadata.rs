// crates/audiolib-db/tests/embedded_metadata.rs
use audiolib_core::{DataProvider, MaintenanceAction, MetadataPatch, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::Connection;
use std::{fs, path::Path, sync::Arc};

fn append(bytes: &mut Vec<u8>, name: &[u8; 4], value: &[u8]) {
    bytes.extend(name);
    bytes.extend((value.len() as u32).to_le_bytes());
    bytes.extend(value);
    if !value.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn wav(path: &Path, embedded: bool) {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut fmt = Vec::new();
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(48000_u32.to_le_bytes());
    fmt.extend(96000_u32.to_le_bytes());
    fmt.extend(2_u16.to_le_bytes());
    fmt.extend(16_u16.to_le_bytes());
    append(&mut bytes, b"fmt ", &fmt);
    append(&mut bytes, b"data", &[0; 128]);
    if embedded {
        let mut info = b"INFO".to_vec();
        append(&mut info, b"ICMT", b"rarefoleykeyword recorded outdoors");
        append(&mut info, b"IKEY", "木门碰撞\0".as_bytes());
        append(&mut bytes, b"LIST", &info);
        append(&mut bytes, b"iXML", b"<BWFXML><USER><CATID>DOORWood</CATID><DESIGNER>foleydesigner</DESIGNER><VENDOR_CUSTOM>vendorsearchable</VENDOR_CUSTOM></USER></BWFXML>");
    }
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn search(provider: &SqliteProvider, query: &str) -> u64 {
    provider
        .search(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .total_count
}

#[test]
fn source_metadata_search_rebuild_upgrade_and_rescan_preserve_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let path = root.join("unnamed.wav");
    wav(&path, true);
    let original = fs::read(&path).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let service = ScanService::new(provider.clone());
    let scan = || {
        service
            .scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    };
    let first = scan();
    assert_eq!(first.state, ScanState::Completed);
    assert_eq!(first.errors, 0);
    assert_eq!(first.added, 1);
    let file = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .remove(0);
    assert_eq!(
        file.attributes["wav_metadata"]["fields"]["cat_id"],
        "DOORWood"
    );
    assert!(
        file.tags.is_empty(),
        "Embedded keywords must not become user annotations"
    );
    assert_eq!(
        file.attributes["wav_metadata"]["fields"]["description"],
        "rarefoleykeyword recorded outdoors"
    );
    assert!(file.bext_description.is_empty());
    let keywords = [
        "rarefoleykeyword",
        "DOORWood",
        "foleydesigner",
        "vendorsearchable",
        "木门",
        "碰撞",
    ];
    for keyword in keywords {
        assert_eq!(search(&provider, keyword), 1, "{keyword}");
    }
    provider
        .update_metadata(
            file.id,
            &MetadataPatch {
                tags: Some("My private tag".into()),
                notes: Some("Keep notes".into()),
                favorite: Some(true),
                color: Some("#123456".into()),
            },
        )
        .unwrap();
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for keyword in keywords {
        assert_eq!(search(&provider, keyword), 1, "after rebuild: {keyword}");
    }
    assert_eq!(scan().unchanged, 1);
    // Simulate a pre-upgrade record without changing the source size or timestamp.
    let connection = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection.execute("UPDATE audio_files SET metadata_json=json_set(json_remove(audiolib_metadata_json(metadata_json),'$.wav_metadata'),'$.plugin_private','preserved') WHERE id=?1", [file.id]).unwrap();
    let refreshed = scan();
    assert_eq!(refreshed.updated, 1);
    assert_eq!(refreshed.unchanged, 0);
    assert_eq!(scan().unchanged, 1);
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "Scanning never writes source WAV files"
    );
    let upgraded = provider.get_audio(file.id).unwrap();
    assert_eq!(upgraded.attributes["plugin_private"], "preserved");
    assert_eq!(upgraded.tags, "My private tag");
    assert_eq!(upgraded.notes, "Keep notes");
    assert!(upgraded.favorite);
    assert_eq!(upgraded.color.as_deref(), Some("#123456"));
    wav(&path, false);
    let removed = scan();
    assert_eq!(removed.updated, 1);
    for keyword in keywords {
        assert_eq!(search(&provider, keyword), 0, "stale: {keyword}");
    }
    let removed = provider.get_audio(file.id).unwrap();
    assert!(
        removed.attributes["wav_metadata"]["fields"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(removed.tags, "My private tag");
    assert_eq!(removed.attributes["plugin_private"], "preserved");
    assert_eq!(search(&provider, "private"), 1);
}

#[test]
fn incremental_scan_refreshes_unsupported_and_old_extraction_versions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let path = root.join("metadata.wav");
    wav(&path, true);
    let original = fs::read(&path).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let service = ScanService::new(provider.clone());
    let scan = || {
        service
            .scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    };
    assert_eq!(scan().added, 1);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                notes: Some("Retain my annotation".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let connection = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    let current_version = audiolib_audio::WAV_METADATA_VERSION;
    for marker in [
        format!(r#""{current_version}""#),
        format!("{current_version}.0"),
        "[]".into(),
        "{}".into(),
        "9223372036854775808".into(),
        "null".into(),
        (current_version - 1).to_string(),
    ] {
        // Imported extension Metadata is valid JSON, but need not have our owned types.
        connection
            .execute(
                "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),
                 '$.wav_metadata.version',json(?1),'$.plugin_private','keep') WHERE id=?2",
                rusqlite::params![marker, id],
            )
            .unwrap();
        let progress = scan();
        assert_eq!(progress.errors, 0, "marker {marker}");
        assert_eq!(progress.updated, 1, "marker {marker}");
        assert_eq!(progress.unchanged, 0, "marker {marker}");
        let audio = provider.get_audio(id).unwrap();
        assert_eq!(
            audio.attributes["wav_metadata"]["version"],
            audiolib_audio::WAV_METADATA_VERSION
        );
        assert_eq!(audio.attributes["plugin_private"], "keep");
        assert_eq!(audio.notes, "Retain my annotation");
        assert_eq!(search(&provider, "rarefoleykeyword"), 1);
        assert_eq!(scan().unchanged, 1);
    }
    assert_eq!(fs::read(path).unwrap(), original);
}
