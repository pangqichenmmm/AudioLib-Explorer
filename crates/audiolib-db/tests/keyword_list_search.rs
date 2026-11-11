// crates/audiolib-db/tests/keyword_list_search.rs
//! Regression: every item of a `;`-separated embedded keyword list is full-text searchable.
mod support;
use audiolib_core::{DataProvider, MaintenanceAction, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{fs, path::Path, sync::Arc};

fn append(bytes: &mut Vec<u8>, name: &[u8; 4], value: &[u8]) {
    bytes.extend(name);
    bytes.extend((value.len() as u32).to_le_bytes());
    bytes.extend(value);
    if !value.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn wav(path: &Path, keywords: &str) {
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
    let mut info = b"INFO".to_vec();
    append(&mut info, b"IKEY", format!("{keywords}\0").as_bytes());
    append(&mut bytes, b"LIST", &info);
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
fn semicolon_keyword_list_items_are_all_searchable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    wav(&root.join("a.wav"), "Zebragroan; thunderclap");
    wav(&root.join("b.wav"), "Zebragroan; thunderclap");
    let source_bytes: Vec<_> = ["a.wav", "b.wav"]
        .map(|name| (root.join(name), fs::read(root.join(name)).unwrap()))
        .into();
    let db = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&db).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let source_metadata: Vec<_> = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .into_iter()
        .map(|file| {
            let file = file.into_data();
            (file.id, file.attributes)
        })
        .collect();
    let check = |stage: &str| {
        for term in [
            "zebragroan",
            "thunderclap",
            "Thunderclap",
            "zebra",
            "groan",
            "under",
            "clap",
        ] {
            assert_eq!(search(&provider, term), 2, "{stage}: search {term}");
        }
        for query in ["zebragroan thunderclap", "groan clap"] {
            assert_eq!(search(&provider, query), 2, "{stage}: {query}");
        }
        for (id, metadata) in &source_metadata {
            assert_eq!(&provider.get_audio(*id).unwrap().attributes, metadata);
        }
        for (path, bytes) in &source_bytes {
            assert_eq!(&fs::read(path).unwrap(), bytes);
        }
    };
    check("after scan");
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    check("after rebuild");
}

#[test]
fn sql_seeded_metadata_json_keywords_are_searchable_after_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("library.db");
    let provider = SqliteProvider::open(&db).unwrap();
    let json = r#"{"wav_metadata":{"fields":{"keywords":"Zebragroan; thunderclap"}}}"#;
    {
        let conn = support::connection(&db).unwrap();
        for id in [364, 2104] {
            conn.execute(
                "INSERT INTO audio_files(id,file_path,filename,metadata_json,missing) VALUES(?1,?2,?2,?3,0)",
                rusqlite::params![id, format!("{id}.wav"), json],
            )
            .unwrap();
        }
    }
    // Seeded rows carry no derived search text until the maintenance rebuild fills it.
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for term in ["zebragroan", "thunderclap", "groan", "under", "groan clap"] {
        assert_eq!(search(&provider, term), 2, "{term}");
    }
    for id in [364, 2104] {
        let file = provider.get_audio(id).unwrap();
        assert_eq!(
            file.attributes,
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
        assert_eq!(file.tags, "");
    }
}
