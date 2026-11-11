// crates/audiolib-db/tests/library.rs
mod support;
use audiolib_core::{
    DataProvider, MetadataPatch, ScanState, SearchFilters, SearchRequest, SortOrder,
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider, import_legacy};
use rusqlite::params;
use std::{fs, path::Path, sync::Arc};

fn legacy(path: &Path, audio_path: &Path) {
    let conn = support::connection(path).unwrap();
    conn.execute_batch("CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT UNIQUE NOT NULL,filename TEXT,
        file_size INTEGER,sample_rate INTEGER,bit_depth INTEGER,channels INTEGER,duration REAL,
        user_tags TEXT,user_rating INTEGER,user_color TEXT,user_notes TEXT,
        filename_tokenized TEXT,user_tags_tokenized TEXT,bwf_description TEXT,
        created_at TEXT,modified_at TEXT,last_played_at TEXT,play_count INTEGER,folder_id INTEGER,custom_user_field TEXT);
        CREATE TABLE folders(id INTEGER PRIMARY KEY,folder_path TEXT,folder_name TEXT,parent_id INTEGER,
            depth INTEGER,file_count INTEGER,total_file_count INTEGER,total_duration REAL,total_size INTEGER,
            created_at TEXT,updated_at TEXT,last_scanned TEXT);
        CREATE TABLE folder_closure(ancestor_id INTEGER,descendant_id INTEGER,depth INTEGER,PRIMARY KEY(ancestor_id,descendant_id));
        CREATE TABLE playlists(id INTEGER PRIMARY KEY,name TEXT,description TEXT,created_at TEXT,modified_at TEXT);
        CREATE TABLE playlist_items(id INTEGER PRIMARY KEY,playlist_id INTEGER,audio_file_id INTEGER,position INTEGER,added_at TEXT,notes TEXT);
        CREATE TABLE project_references(id INTEGER PRIMARY KEY,audio_file_id INTEGER,project_path TEXT,project_type TEXT,reference_count INTEGER,last_seen TEXT);
        CREATE TABLE waveform_cache(id INTEGER PRIMARY KEY,audio_file_id INTEGER,zoom_level TEXT,samples_per_pixel INTEGER,min_values BLOB,max_values BLOB,rms_values BLOB,color_values BLOB,created_at TEXT);
        CREATE TABLE private_plugin_state(name TEXT,value BLOB);
        CREATE VIRTUAL TABLE audio_search USING fts5(filename,filename_tokenized UNINDEXED,user_tags,user_tags_tokenized UNINDEXED,bwf_description,
            content='audio_files',content_rowid='id');
        INSERT INTO folders VALUES(7,'library','library',NULL,0,1,1,2.5,100,'2024-01-01','2024-01-02',NULL);
        INSERT INTO folder_closure VALUES(7,7,0);
        INSERT INTO playlists VALUES(9,'Favorites','keep me','2024-02-01','2024-02-02');
        INSERT INTO playlist_items VALUES(17,9,42,3,'2024-02-03','item notes');
        INSERT INTO project_references VALUES(19,42,'project.rpp','Reaper',4,'2024-02-04');
        INSERT INTO waveform_cache VALUES(21,42,'overview',64,X'0000803F',X'00000040',X'0000003F',X'FF1020','2024-02-05');
        INSERT INTO private_plugin_state VALUES('keep',X'012345');").unwrap();
    conn.execute(
        "INSERT INTO audio_files VALUES(42,?1,'爆炸声音.wav',100,48000,24,2,2.5,
        '火焰,oldtag',4,'#123456','车辆发动机的环境声',NULL,NULL,'远处爆炸',
        '2024-01-01 01:02:03','2024-01-02 02:03:04','2024-01-03 03:04:05',12,7,'custom untouched')",
        [audio_path.to_string_lossy().as_ref()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO audio_search(audio_search) VALUES('rebuild')",
        [],
    )
    .unwrap();
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
fn imports_legacy_losslessly_and_rebuilds_chinese_fts() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("legacy.db");
    let target = temp.path().join("rust.db");
    legacy(&source, &temp.path().join("爆炸声音.wav"));
    let before = fs::read(&source).unwrap();
    let report = import_legacy(&source, &target).unwrap();
    assert_eq!(report.audio_files, 1);
    assert!(report.source_preserved);
    assert_eq!(fs::read(&source).unwrap(), before);
    assert!(import_legacy(&source, &target).is_err());
    let provider = SqliteProvider::open(&target).unwrap();
    let audio = provider.get_audio(42).unwrap();
    assert_eq!(audio.tags, "火焰,oldtag");
    // Legacy rating data stays in the column (read by nothing in the product).
    let legacy_rating: i64 = support::connection(&target)
        .unwrap()
        .query_row("SELECT user_rating FROM audio_files WHERE id=42", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(legacy_rating, 4);

    assert_eq!(audio.color.as_deref(), Some("#123456"));
    assert_eq!(audio.notes, "车辆发动机的环境声");
    let dto = serde_json::to_value(&audio).unwrap();
    assert!(dto.get("play_count").is_none());
    assert!(audio.attributes.get("last_played_at").is_none());
    assert!(
        !support::connection(&target)
            .unwrap()
            .column_exists(None, "audio_files", "last_played_at")
            .unwrap()
    );
    for query in ["爆炸", "火焰", "发动机", "oldtag"] {
        assert_eq!(search(&provider, query), 1, "{query}");
    }
    let conn = support::connection(&target).unwrap();
    for (sql, expected) in [
        (
            "SELECT custom_user_field FROM audio_files WHERE id=42",
            "custom untouched",
        ),
        (
            "SELECT created_at FROM audio_files WHERE id=42",
            "2024-01-01 01:02:03",
        ),
        ("SELECT notes FROM playlist_items WHERE id=17", "item notes"),
        (
            "SELECT project_path FROM project_references WHERE id=19",
            "project.rpp",
        ),
        (
            "SELECT hex(min_values) FROM waveform_cache WHERE id=21",
            "0000803F",
        ),
        (
            "SELECT hex(color_values) FROM waveform_cache WHERE id=21",
            "FF1020",
        ),
        ("SELECT hex(value) FROM private_plugin_state", "012345"),
    ] {
        assert_eq!(
            conn.query_row(sql, [], |row| row.get::<_, String>(0))
                .unwrap(),
            expected
        );
    }
    provider.check_integrity().unwrap();
}

#[test]
fn backup_reads_committed_wal_without_writing_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.db");
    let target = temp.path().join("target.db");
    legacy(&source, &temp.path().join("audio.wav"));
    let conn = support::connection(&source).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;UPDATE audio_files SET user_notes='committed wal';",
    )
    .unwrap();
    let bytes = fs::read(&source).unwrap();
    let report = import_legacy(&source, &target).unwrap();
    assert_eq!(report.audio_files, 1);
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(
        SqliteProvider::open(&target)
            .unwrap()
            .get_audio(42)
            .unwrap()
            .notes,
        "committed wal"
    );
}

#[test]
fn failed_import_does_not_publish_a_destination_or_leave_staging_files() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("bad.db");
    let target = temp.path().join("target.db");
    let conn = support::connection(&source).unwrap();
    conn.execute_batch("PRAGMA user_version=999;CREATE TABLE private_data(value TEXT);INSERT INTO private_data VALUES('keep');").unwrap();
    drop(conn);
    let before = fs::read(&source).unwrap();
    assert!(import_legacy(&source, &target).is_err());
    assert!(!target.exists());
    assert_eq!(fs::read(&source).unwrap(), before);
    let mut names: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [".audiolib-import-leases", "bad.db"].map(std::ffi::OsString::from),
        "Only the untouched source and persistent import lease directory may remain; owned staging db, WAL and SHM must disappear"
    );
    let leases = temp.path().join(".audiolib-import-leases");
    assert!(leases.is_dir());
    let lease_names: Vec<_> = fs::read_dir(&leases)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        lease_names,
        [std::ffi::OsString::from("target.db")],
        "The persistent lease directory must contain only this destination's lock, without staging db, WAL or SHM"
    );
    assert!(leases.join("target.db").is_file());
}

#[test]
fn metadata_updates_fts_old_terms_and_deletion_leave_no_stale_postings() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.db");
    let target = temp.path().join("new.db");
    legacy(&source, &temp.path().join("file.wav"));
    import_legacy(&source, &target).unwrap();
    let provider = SqliteProvider::open(&target).unwrap();
    let returned = provider
        .update_metadata(
            42,
            &MetadataPatch {
                tags: Some("newtag 森林".into()),
                notes: Some("鸟鸣".into()),
                favorite: Some(true),
                color: Some("".into()),
            },
        )
        .unwrap();
    assert_eq!(returned.tags, "newtag 森林");
    assert!(returned.favorite);
    assert_eq!(returned.color, None);
    assert_eq!(search(&provider, "oldtag"), 0);
    assert_eq!(search(&provider, "发动机"), 0);
    for query in ["newtag", "森林", "鸟鸣"] {
        assert_eq!(search(&provider, query), 1);
    }

    provider.delete_audio(42).unwrap();
    assert_eq!(search(&provider, "newtag"), 0);
    let conn = support::connection(&target).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH 'newtag'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM playlist_items", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    provider.check_integrity().unwrap();
}

fn write_wav(path: &Path, frames: u32) {
    let bytes = frames * 2;
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + bytes).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&48000u32.to_le_bytes());
    data.extend_from_slice(&96000u32.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&bytes.to_le_bytes());
    data.resize(data.len() + bytes as usize, 0);
    fs::write(path, data).unwrap();
}

#[test]
fn scanner_incremental_rescan_keeps_annotations_and_filters_counts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(root.join("child")).unwrap();
    write_wav(&root.join("wind.wav"), 480);
    write_wav(&root.join("child").join("rain.wav"), 960);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("lib.db")).unwrap());
    let scanner = ScanService::new(provider.clone());
    let result = scanner
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.added, 2);
    assert_eq!(result.errors, 0);
    assert_eq!(result.state, ScanState::Completed);
    let all = provider.search(&SearchRequest::default()).unwrap();
    let id = all
        .results
        .iter()
        .find(|row| row.filename == "wind.wav")
        .unwrap()
        .id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("foley".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    // Background probing cannot erase extension/plugin fields it does not understand.
    let extra = support::connection(provider.database_path()).unwrap();
    extra
        .execute("UPDATE audio_files SET user_rating=4 WHERE id=?1", [id])
        .unwrap();
    extra.execute("UPDATE audio_files SET metadata_json=json_patch(audiolib_metadata_json(metadata_json),?2),bwf_originator='legacy originator',ixml_tape='legacy tape' WHERE id=?1",
        params![id,r#"{"private_plugin_attribute":"preserve","ixml_scene":"scene preserved"}"#]).unwrap();
    extra.execute("INSERT INTO waveform_cache(audio_file_id,zoom_level,min_values,max_values) VALUES(?1,'overview',X'0000803F',X'00000040')", [id]).unwrap();
    drop(extra);
    let again = scanner
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!((again.added, again.updated, again.unchanged), (0, 0, 2));
    assert_eq!(provider.get_audio(id).unwrap().tags, "foley");
    let cache_count = || {
        support::connection(provider.database_path())
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM waveform_cache WHERE audio_file_id=?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(cache_count(), 1);
    scanner
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        cache_count(),
        1,
        "Full scans must keep caches for unchanged fingerprints"
    );
    let roots = provider.folders(None).unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].file_count, 2);
    let page = provider
        .search(&SearchRequest {
            folder_id: Some(roots[0].id),
            include_subfolders: false,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total_count, 1);
    let page = provider
        .search(&SearchRequest {
            filters: SearchFilters {
                duration_min: Some(0.0),
                duration_max: Some(0.02),
                sample_rate: Some(48000),
                bit_depth: Some(16),
                channels: Some(1),
                favorites_only: true,
                tagged_only: true,
                ..Default::default()
            },
            order_by: SortOrder::Filename,
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results[0].id, id);
    write_wav(&root.join("wind.wav"), 1440);
    let changed = scanner
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(changed.updated, 1);
    assert_eq!(
        cache_count(),
        0,
        "A changed size/mtime fingerprint invalidates Waveform cache"
    );
    let kept_rating: i64 = support::connection(provider.database_path())
        .unwrap()
        .query_row(
            "SELECT user_rating FROM audio_files WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept_rating, 4, "rescan keeps legacy rating data");
    let stored = provider.get_audio(id).unwrap();
    assert_eq!(stored.attributes["private_plugin_attribute"], "preserve");
    assert!(stored.attributes.get("bwf_originator").is_none());
    assert!(stored.attributes.get("ixml_tape").is_none());
    provider.check_integrity().unwrap();
}

#[test]
fn cancelled_and_failed_file_scans_do_not_clean_up_missing_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    write_wav(&root.join("old.wav"), 480);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("lib.db")).unwrap());
    let scanner = ScanService::new(provider.clone());
    scanner
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                notes: Some("retain this".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fs::remove_file(root.join("old.wav")).unwrap();
    for index in 0..10 {
        write_wav(&root.join(format!("new{index}.wav")), 480);
    }
    let cancel = ScanCancellation::default();
    let signal = cancel.clone();
    let result = scanner
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                batch_size: 1,
                ..Default::default()
            },
            &cancel,
            move |p| {
                if p.processed >= 1 {
                    signal.cancel();
                }
            },
        )
        .unwrap();
    assert_eq!(result.state, ScanState::Cancelled);
    assert_eq!(search(&provider, "old"), 1);
    fs::write(root.join("broken.wav"), b"bad header").unwrap();
    let result = scanner
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.errors, 1);
    assert_eq!(search(&provider, "old"), 1);
    fs::remove_file(root.join("broken.wav")).unwrap();
    scanner
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(search(&provider, "old"), 0);
    assert_eq!(provider.get_audio(id).unwrap().notes, "retain this");
    write_wav(&root.join("old.wav"), 480);
    scanner
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(search(&provider, "old"), 1);
    assert_eq!(provider.get_audio(id).unwrap().notes, "retain this");
    assert!(
        scanner
            .scan(
                &temp.path().join("absent"),
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {}
            )
            .is_err()
    );
}

#[test]
fn query_validation_and_pagination_are_stable() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("lib.db")).unwrap();
    let conn = support::connection(provider.database_path()).unwrap();
    for id in 1..=3 {
        conn.execute("INSERT INTO audio_files(id,file_path,filename,user_rating,sample_rate,duration) VALUES(?1,?2,'same.wav',4,48000,0)",params![id,format!("{id}.wav")]).unwrap();
    }
    for offset in 0..3 {
        let page = provider
            .search(&SearchRequest {
                query: "same".into(),
                offset,
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total_count, 3);
        assert_eq!(page.results[0].id, offset as i64 + 1);
    }
    assert!(
        provider
            .search(&SearchRequest {
                query: "same; DROP TABLE audio_files".into(),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        provider
            .search(&SearchRequest {
                query: "\" ( )".into(),
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        provider
            .update_metadata(
                1,
                &MetadataPatch {
                    color: Some("red".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
fn concurrent_readers_see_consistent_count_and_owned_results_while_tags_change() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.db");
    let target = temp.path().join("new.db");
    legacy(&source, &temp.path().join("file.wav"));
    import_legacy(&source, &target).unwrap();
    let provider = Arc::new(SqliteProvider::open(&target).unwrap());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let provider = provider.clone();
            scope.spawn(move || {
                for _ in 0..30 {
                    let page = provider
                        .search(&SearchRequest {
                            query: "changing".into(),
                            ..Default::default()
                        })
                        .unwrap();
                    assert_eq!(page.total_count, page.results.len() as u64);
                    if let Some(item) = page.results.first() {
                        assert!(item.tags.contains("changing"));
                    }
                }
            });
        }
        let provider = provider.clone();
        scope.spawn(move || {
            for index in 0..30 {
                provider
                    .update_metadata(
                        42,
                        &MetadataPatch {
                            tags: Some(if index % 2 == 0 { "changing" } else { "other" }.into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        });
    });
    provider.check_integrity().unwrap();
}

#[test]
fn missing_search_index_is_repaired_without_changing_retained_legacy_data() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.db");
    let target = temp.path().join("new.db");
    legacy(&source, &temp.path().join("file.wav"));
    import_legacy(&source, &target).unwrap();
    let conn = support::connection(&target).unwrap();
    conn.execute_batch("DROP INDEX idx_audio_visible_name; DELETE FROM audiolib_schema_features WHERE feature='search_indexes';")
        .unwrap();
    drop(conn);
    let provider = SqliteProvider::open(&target).unwrap();
    assert_eq!(provider.get_audio(42).unwrap().notes, "车辆发动机的环境声");
    assert_eq!(search(&provider, "发动机"), 1);
    let conn = support::connection(&target).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        audiolib_db::SCHEMA_VERSION
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='idx_audio_visible_name'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM playlist_items WHERE audio_file_id=42",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn read_only_provider_searches_but_cannot_edit_or_migrate() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lib.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = support::connection(&path).unwrap();
    conn.execute(
        "INSERT INTO audio_files(file_path,filename) VALUES('metal.wav','metal.wav')",
        [],
    )
    .unwrap();
    drop(conn);
    drop(provider);
    let original = std::fs::read(&path).unwrap();
    let readonly = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(search(&readonly, "metal"), 1);
    assert!(
        readonly
            .update_metadata(
                1,
                &MetadataPatch {
                    notes: Some("must fail".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    drop(readonly);
    assert_eq!(original, std::fs::read(&path).unwrap());
    let conn = support::connection(&path).unwrap();
    conn.execute_batch("PRAGMA user_version=1;").unwrap();
    drop(conn);
    assert!(SqliteProvider::open_read_only(&path).is_err());
}

#[test]
fn literal_tag_and_color_filters_share_count_and_pagination_predicates() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("lib.db")).unwrap();
    let conn = support::connection(provider.database_path()).unwrap();
    for (id, tags, color) in [
        (1, "Metal_100%", "#abcdef"),
        (2, "METAL_100% outdoor", "#ABCDEF"),
        (3, "metalX100more", "#ABCDEF"),
        (4, "Metal_100%", "#123456"),
    ] {
        conn.execute("INSERT INTO audio_files(id,file_path,filename,user_tags,user_color) VALUES(?1,?2,'metal.wav',?3,?4)", params![id,format!("{id}.wav"),tags,color]).unwrap();
    }
    let request = SearchRequest {
        query: "metal".into(),
        filters: SearchFilters {
            tag_contains: Some("metal_100%".into()),
            color: Some("#aBcDeF".into()),
            ..Default::default()
        },
        limit: 1,
        offset: 1,
        ..Default::default()
    };
    let page = provider.search(&request).unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].id, 2);
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: String::new(),
                offset: 0,
                ..request.clone()
            })
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(
        provider
            .search(&SearchRequest {
                filters: SearchFilters {
                    tag_contains: Some("%'; OR 1=1 --".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
    assert!(
        provider
            .search(&SearchRequest {
                filters: SearchFilters {
                    color: Some("ABCDEF".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .is_err()
    );
}

#[test]
fn relevance_with_missing_records_keeps_exact_count_and_bm25_order() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("lib.db")).unwrap();
    let conn = support::connection(provider.database_path()).unwrap();
    for id in 1..=6 {
        conn.execute("INSERT INTO audio_files(id,file_path,filename,user_tags,missing) VALUES(?1,?2,'metal.wav',?3,?4)",params![id,format!("{id}.wav"),if id%2==0 {"metal metal"} else {"wood"},id==2]).unwrap();
    }
    let expected:Vec<i64>=conn.prepare("SELECT af.id FROM audio_files af JOIN audio_search ON audio_search.rowid=af.id WHERE missing=0 AND audio_search MATCH 'metal' ORDER BY bm25(audio_search),af.id").unwrap().query_map([],|row|row.get(0)).unwrap().map(Result::unwrap).collect();
    let page = provider
        .search(&SearchRequest {
            query: "metal".into(),
            limit: 2,
            offset: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total_count, 5);
    assert_eq!(
        page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
        expected[1..3]
    );
    conn.execute("UPDATE audio_files SET missing=0 WHERE id=2", [])
        .unwrap();
    let expected:Vec<i64>=conn.prepare("SELECT rowid FROM audio_search WHERE audio_search MATCH 'metal' ORDER BY bm25(audio_search),rowid LIMIT 2 OFFSET 1").unwrap().query_map([],|row|row.get(0)).unwrap().map(Result::unwrap).collect();
    let page = provider
        .search(&SearchRequest {
            query: "metal".into(),
            limit: 2,
            offset: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total_count, 6);
    assert_eq!(
        page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
        expected
    );
}
