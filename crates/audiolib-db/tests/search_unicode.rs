// crates/audiolib-db/tests/search_unicode.rs
//! Scanner/provider regressions for Unicode word boundaries and derived-index upgrades.
mod support;
use audiolib_core::{DataProvider, MaintenanceAction, MetadataPatch, SearchRequest};
use audiolib_db::{
    ScanCancellation, ScanOptions, ScanService, SnapshotUpgradeProgress, SqliteProvider,
};
use rusqlite::Connection;
use std::{collections::BTreeSet, fs, path::Path, sync::Arc};

fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], value: &[u8]) {
    bytes.extend(name);
    bytes.extend((value.len() as u32).to_le_bytes());
    bytes.extend(value);
    if !value.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn wav(path: &Path, description: Option<&str>) {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut fmt = Vec::new();
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(48000_u32.to_le_bytes());
    fmt.extend(96000_u32.to_le_bytes());
    fmt.extend(2_u16.to_le_bytes());
    fmt.extend(16_u16.to_le_bytes());
    chunk(&mut bytes, b"fmt ", &fmt);
    chunk(&mut bytes, b"data", &[0; 128]);
    if let Some(description) = description {
        let mut info = b"INFO".to_vec();
        chunk(&mut info, b"ICMT", description.as_bytes());
        chunk(&mut bytes, b"LIST", &info);
    }
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn scan(provider: &Arc<SqliteProvider>, root: &Path) {
    let progress = ScanService::new(provider.clone())
        .scan(
            root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.errors, 0);
}

fn names(provider: &SqliteProvider, query: &str) -> BTreeSet<String> {
    provider
        .search(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .into_iter()
        .map(|file| file.into_data().filename)
        .collect()
}

#[test]
fn scanner_search_keeps_unicode_words_and_does_not_merge_separate_words() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let expected = BTreeSet::from([
        // Distinct prefixes keep all three spellings separate on APFS volumes
        // that fold both filename case and Unicode normalization.
        "plain-cafe.wav".to_string(),
        "nfc-café.wav".into(),
        "nfd-Cafe\u{301}.wav".into(),
    ]);
    for name in expected
        .iter()
        .map(String::as_str)
        .chain(["irrelevant.wav"])
    {
        wav(&root.join(name), None);
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    scan(&provider, &root);
    let irrelevant = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .into_iter()
        .find(|file| file.filename == "irrelevant.wav")
        .unwrap();
    provider
        .update_metadata(
            irrelevant.id,
            &MetadataPatch {
                tags: Some("caf e".into()),
                ..Default::default()
            },
        )
        .unwrap();
    for query in ["cafe", "café", "Cafe\u{301}", "\"café\"", "café*"] {
        assert_eq!(names(&provider, query), expected, "{query}");
    }
    // Rebuild and read-only access must retain the same Unicode semantics.
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for query in ["cafe", "café", "Cafe\u{301}"] {
        assert_eq!(names(&provider, query), expected);
    }
    let readonly = SqliteProvider::open_read_only(provider.database_path()).unwrap();
    assert_eq!(names(&readonly, "café"), expected);
}

#[test]
fn existing_library_upgrade_reports_rows_and_preserves_business_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("library.db");
    let provider = SqliteProvider::open(&database).unwrap();
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "WITH RECURSIVE records(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM records WHERE id<5001)
         INSERT INTO audio_files(id,file_path,filename,user_tags,user_notes,favorite,user_color,metadata_json)
         SELECT id,'root/clip-'||id||'.wav','clip-'||id||'.wav','retained tag',
         'retained notes',1,'#abcdef','{\"unknown_extension\":{\"retained\":true}}' FROM records;",
    )
    .unwrap();
    let before = serde_json::to_value(provider.get_audio(1).unwrap()).unwrap();
    drop(provider);
    conn.execute_batch(
        "UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';",
    )
    .unwrap();
    drop(conn);

    let mut progress = Vec::new();
    let upgraded = SqliteProvider::open_existing_with_progress(&database, |event| {
        progress.push(event);
    })
    .unwrap();
    assert_eq!(progress.first(), Some(&SnapshotUpgradeProgress::Preparing));
    for copying in [true, false] {
        let mut counts: Vec<_> = progress
            .iter()
            .filter_map(|e| match e {
                SnapshotUpgradeProgress::Copying { completed, total } if copying => {
                    Some((*completed, *total))
                }
                SnapshotUpgradeProgress::Indexing { completed, total } if !copying => {
                    Some((*completed, *total))
                }
                _ => None,
            })
            .collect();
        counts.dedup();
        assert_eq!(counts, [(5000, 5001), (5001, 5001)]);
    }
    let terminal: Vec<_> = progress
        .iter()
        .filter(|e| {
            matches!(
                e,
                SnapshotUpgradeProgress::Validating
                    | SnapshotUpgradeProgress::Compacting
                    | SnapshotUpgradeProgress::Publishing
                    | SnapshotUpgradeProgress::Checkpoint
            )
        })
        .copied()
        .collect();
    assert_eq!(
        terminal,
        [
            SnapshotUpgradeProgress::Validating,
            SnapshotUpgradeProgress::Compacting,
            SnapshotUpgradeProgress::Publishing,
            SnapshotUpgradeProgress::Checkpoint
        ]
    );
    assert_eq!(
        serde_json::to_value(upgraded.get_audio(1).unwrap()).unwrap(),
        before
    );
    assert_eq!(upgraded.stats().unwrap().audio_files, 5001);
    assert_eq!(
        upgraded
            .search(&SearchRequest {
                query: "tain".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        5001
    );
    assert!(SqliteProvider::has_current_search_state(&database).unwrap());
    drop(upgraded);
    progress.clear();
    let unchanged = SqliteProvider::open_existing_with_progress(&database, |event| {
        progress.push(event);
    })
    .unwrap();
    assert_eq!(progress, [SnapshotUpgradeProgress::Preparing]);
    assert_eq!(
        serde_json::to_value(unchanged.get_audio(1).unwrap()).unwrap(),
        before
    );
}

#[test]
fn embedded_source_words_and_chinese_search_survive_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    wav(
        &root.join("source.wav"),
        Some("café 爆炸声音 数字音频工作站"),
    );
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    scan(&provider, &root);
    for rebuild in [false, true] {
        if rebuild {
            provider
                .maintain_library(MaintenanceAction::RebuildSearch)
                .unwrap();
        }
        for query in [
            "cafe",
            "café",
            "Cafe\u{301}",
            "爆炸",
            "声音",
            "数字音频工作站",
            "音频",
            "café,爆炸",
            "(café) \"爆炸\"*",
        ] {
            assert_eq!(
                names(&provider, query),
                BTreeSet::from(["source.wav".into()]),
                "{query}"
            );
        }
        assert!(
            names(&provider, "café OR 爆炸").is_empty(),
            "OR remains a literal word"
        );
        assert!(names(&provider, "caf e").is_empty());
    }
}

#[test]
fn internal_punctuation_search_is_and_across_nonadjacent_tokens_and_metadata_fields() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    for name in [
        "separated.wav",
        "adjacent.wav",
        "cross-field.wav",
        "accent.wav",
    ] {
        wav(&root.join(name), None);
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    scan(&provider, &root);
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        let (tags, notes) = match file.filename.as_str() {
            "separated.wav" => ("foo middle bar don middle t 1 middle 2", ""),
            "adjacent.wav" => ("foo bar don t 1 2", ""),
            "cross-field.wav" => ("foo don 1", "bar t 2"),
            "accent.wav" => ("café middle door", ""),
            other => panic!("Unexpected fixture {other}"),
        };
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(tags.into()),
                    notes: Some(notes.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let all = BTreeSet::from([
        "separated.wav".into(),
        "adjacent.wav".into(),
        "cross-field.wav".into(),
    ]);
    for query in [
        "foo:bar",
        "foo·bar",
        "foo：bar",
        "foo‿bar",
        "don't",
        "don’t",
        "1,2",
    ] {
        assert_eq!(names(&provider, query), all, "{query}");
    }
    for query in ["café:door", "Cafe\u{301}:door"] {
        assert_eq!(
            names(&provider, query),
            BTreeSet::from(["accent.wav".into()]),
            "{query}"
        );
    }
    // Dot compounds and decimal searches were already phrases in the old compiler.
    for query in ["foo.bar", "foo_bar", "1.2"] {
        assert_eq!(
            names(&provider, query),
            BTreeSet::from(["adjacent.wav".into()]),
            "{query}"
        );
    }
    let readonly = SqliteProvider::open_read_only(provider.database_path()).unwrap();
    assert_eq!(names(&readonly, "foo:bar"), all);
}

#[test]
fn existing_v3_rebuilds_old_derived_words_once_without_changing_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let audio = root.join("embedded.wav");
    wav(&audio, Some("café 爆炸声音"));
    let original = fs::read(&audio).unwrap();
    let database = temp.path().join("current.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    scan(&provider, &root);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("Retain 私人".into()),
                notes: Some("Keep notes".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let before = provider.get_audio(id).unwrap();
    drop(provider);
    let legacy = temp.path().join("library.db");
    support::legacy_copy(&database, &legacy);
    let database = legacy;
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    // Simulate the previous v3 tokenizer's derived postings and absent feature marker.
    conn.execute_batch(
        "UPDATE audio_files SET bwf_description_tokenized='caf é 爆炸 声音';
        DELETE FROM audiolib_schema_features WHERE feature='tokenization';",
    )
    .unwrap();
    drop(conn);
    let old_source_bytes = fs::read(&database).unwrap();
    let readonly = SqliteProvider::open_read_only(&database).unwrap();
    assert_ne!(readonly.database_path(), database.as_path());
    let owned_directory = readonly.database_path().parent().unwrap().to_path_buf();
    for query in ["cafe", "café", "Cafe\u{301}", "爆炸"] {
        assert_eq!(
            names(&readonly, query),
            BTreeSet::from(["embedded.wav".into()]),
            "readonly: {query}"
        );
    }
    assert_eq!(
        serde_json::to_value(readonly.get_audio(id).unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    assert_eq!(fs::read(&database).unwrap(), old_source_bytes);
    assert_eq!(fs::read(&audio).unwrap(), original);
    assert!(owned_directory.is_dir());
    drop(readonly);
    assert!(
        !owned_directory.exists(),
        "Reader connections must close before owned snapshot cleanup"
    );
    assert_eq!(fs::read(&database).unwrap(), old_source_bytes);
    let provider = SqliteProvider::open_existing(&database).unwrap();
    for query in ["cafe", "café", "Cafe\u{301}", "爆炸"] {
        assert_eq!(
            names(&provider, query),
            BTreeSet::from(["embedded.wav".into()]),
            "{query}"
        );
    }
    assert_eq!(
        serde_json::to_value(provider.get_audio(id).unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(fs::read(&audio).unwrap(), original);
    drop(provider);
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let rootpage: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='audio_search_data'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        audiolib_db::SCHEMA_VERSION
    );
    assert_eq!(
        conn.query_row(
            "SELECT version FROM audiolib_schema_features WHERE feature='tokenization'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    drop(conn);
    drop(SqliteProvider::open_existing(&database).unwrap());
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='audio_search_data'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        rootpage
    );
}

fn recreate_legacy_porter_index(path: &Path, source_words: &str) {
    let mut conn = Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let table: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='audio_search'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let triggers: Vec<String> = conn
        .prepare(
            "SELECT sql FROM sqlite_schema WHERE type='trigger'
             AND name IN ('audio_files_ai','audio_files_au','audio_files_ad') ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(triggers.len(), 3);
    let columns = table.split_once("content='audio_files'").unwrap().0;
    let legacy_table = format!(
        "{columns}content='audio_files',content_rowid='id',
         tokenize='porter unicode61 remove_diacritics 2');"
    );
    let tx = conn.transaction().unwrap();
    tx.execute_batch(
        "DROP TRIGGER audio_files_ai; DROP TRIGGER audio_files_au;
         DROP TRIGGER audio_files_ad; DROP TABLE audio_search;",
    )
    .unwrap();
    tx.execute(
        "UPDATE audio_files SET filename_tokenized=filename,
         user_tags_tokenized=user_tags,user_notes_tokenized=user_notes,
         bwf_description_tokenized=?1,ixml_scene_tokenized=ixml_scene,
         ixml_take_tokenized=ixml_take",
        [source_words],
    )
    .unwrap();
    tx.execute_batch(&legacy_table).unwrap();
    for trigger in triggers {
        tx.execute_batch(&trigger).unwrap();
    }
    tx.execute(
        "INSERT INTO audio_search(audio_search) VALUES('rebuild')",
        [],
    )
    .unwrap();
    tx.execute(
        "UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization'",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

fn retained_business_data(provider: &SqliteProvider, id: i64) -> serde_json::Value {
    serde_json::json!({
        "audio": provider.get_audio(id).unwrap(),
        "scans": provider.scan_history(10, 0).unwrap(),
    })
}

fn assert_explosion_fragments(provider: &SqliteProvider) {
    for end in 2..="explosion".len() {
        let query = &"explosion"[..end];
        assert_eq!(
            names(provider, query),
            BTreeSet::from(["explosion.wav".into()]),
            "{query}"
        );
    }
    for query in ["plos", "sion"] {
        assert_eq!(
            names(provider, query),
            BTreeSet::from(["explosion.wav".into()]),
            "{query}"
        );
    }
}

#[test]
fn real_porter_v1_index_upgrades_owned_readers_and_writers_without_business_data_loss() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let audio = root.join("explosion.wav");
    wav(&audio, Some("café 爆炸声音"));
    let source_audio = fs::read(&audio).unwrap();
    let database = temp.path().join("current.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    scan(&provider, &root);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("Retain 私人".into()),
                notes: Some("Keep notes".into()),
                favorite: Some(true),
                color: Some("#AABBCC".into()),
            },
        )
        .unwrap();

    let original = retained_business_data(&provider, id);
    drop(provider);
    let legacy = temp.path().join("library.db");
    support::legacy_copy(&database, &legacy);
    let database = legacy;
    recreate_legacy_porter_index(&database, "café 爆炸 声音");
    let source_database = fs::read(&database).unwrap();
    assert!(!SqliteProvider::has_current_search_text(&database).unwrap());
    assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());

    let readonly = SqliteProvider::open_read_only(&database).unwrap();
    assert_ne!(readonly.database_path(), database.as_path());
    assert!(SqliteProvider::has_current_search_text(readonly.database_path()).unwrap());
    let owned_directory = readonly.database_path().parent().unwrap().to_path_buf();
    assert_explosion_fragments(&readonly);
    assert_eq!(retained_business_data(&readonly, id), original);
    assert_eq!(fs::read(&database).unwrap(), source_database);
    assert_eq!(fs::read(&audio).unwrap(), source_audio);
    drop(readonly);
    assert!(!owned_directory.exists());
    assert_eq!(fs::read(&database).unwrap(), source_database);

    let provider = SqliteProvider::open_existing(&database).unwrap();
    assert_explosion_fragments(&provider);
    assert_eq!(retained_business_data(&provider, id), original);
    assert!(provider.check_library().unwrap().is_ok());
    assert!(SqliteProvider::has_current_search_text(&database).unwrap());
    assert_eq!(fs::read(&audio).unwrap(), source_audio);
    drop(provider);
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let table_before: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='audio_search'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rootpage: i64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='audio_search_data'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!table_before.contains("porter"));
    assert!(!table_before.contains("prefix="));
    assert_eq!(
        conn.query_row(
            "SELECT version FROM audiolib_schema_features WHERE feature='tokenization'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        3
    );
    drop(conn);
    let reopened = SqliteProvider::open_existing(&database).unwrap();
    assert_eq!(retained_business_data(&reopened, id), original);
    assert_explosion_fragments(&reopened);
    drop(reopened);
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name='audio_search_data'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        rootpage
    );
    assert_eq!(
        conn.query_row(
            "SELECT sql FROM sqlite_schema WHERE name='audio_search'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
        table_before
    );
}

#[test]
fn restore_checks_legacy_porter_triggers_before_rebuilding_the_index() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("current.db");
    let provider = SqliteProvider::open(&database).unwrap();
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,filename,
         user_tags,user_notes,favorite,metadata_json)
         VALUES(42,'fixture/explosion.wav','explosion.wav',
         'retain','private notes',1,'{\"private\":\"keep\"}');",
    )
    .unwrap();
    drop(conn);
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();

    let original = retained_business_data(&provider, 42);
    let legacy = temp.path().join("legacy.db");
    support::legacy_copy(&database, &legacy);
    recreate_legacy_porter_index(&legacy, "");
    let legacy_bytes = fs::read(&legacy).unwrap();

    for modified in [false, true] {
        let broken = temp.path().join(format!("broken-{modified}.db"));
        fs::copy(&legacy, &broken).unwrap();
        let conn = Connection::open(&broken).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        conn.execute_batch("DROP TRIGGER audio_files_au;").unwrap();
        if modified {
            conn.execute_batch(
                "CREATE TRIGGER audio_files_au AFTER UPDATE ON audio_files
                 BEGIN SELECT 1; END;",
            )
            .unwrap();
        }
        drop(conn);
        let broken_bytes = fs::read(&broken).unwrap();
        let target_bytes = fs::read(&database).unwrap();
        let report = SqliteProvider::check_owned_file(&broken).unwrap();
        assert!(!report.is_ok());
        assert!(report.errors.iter().any(|error| error.contains("triggers")));
        let error = provider.restore_library(&broken).unwrap_err();
        assert!(error.to_string().contains("triggers"), "{error}");
        assert_eq!(fs::read(&broken).unwrap(), broken_bytes);
        assert_eq!(fs::read(&database).unwrap(), target_bytes);
        assert_eq!(retained_business_data(&provider, 42), original);
        assert_explosion_fragments(&provider);
    }

    provider.restore_library(&legacy).unwrap();
    assert!(SqliteProvider::has_current_search_text(&database).unwrap());
    assert_eq!(retained_business_data(&provider, 42), original);
    assert_explosion_fragments(&provider);
    assert_eq!(fs::read(&legacy).unwrap(), legacy_bytes);
}

#[test]
fn letter_fragments_require_contiguous_order_within_one_word_and_metadata_field() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    for name in [
        "target.wav",
        "split-words.wav",
        "split-fields.wav",
        "wrong-order.wav",
        "separated.wav",
        "encoded.wav",
    ] {
        wav(&root.join(name), None);
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    scan(&provider, &root);
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        let (tags, notes) = match file.filename.as_str() {
            "target.wav" => ("explosion", ""),
            "split-words.wav" => ("pl lo os si io on", ""),
            "split-fields.wav" => ("pl si", "lo os io on"),
            "wrong-order.wav" => ("os lo pl on io si", ""),
            "separated.wav" => ("plxloxos sixioxon", ""),
            // A user's text resembling internal tokens must stay ordinary text.
            "encoded.wav" => ("g00007000006c g00006c00006f g00006f000073", ""),
            other => panic!("Unexpected fixture {other}"),
        };
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(tags.into()),
                    notes: Some(notes.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    for query in ["plos", "sion", "plos sion"] {
        assert_eq!(
            names(&provider, query),
            BTreeSet::from(["target.wav".into()]),
            "{query}"
        );
    }
}

#[test]
fn letter_fragments_fold_case_accents_and_combining_marks_consistently() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    for name in ["a.wav", "b.wav", "c.wav", "d.wav", "e.wav", "f.wav"] {
        wav(&root.join(name), None);
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    scan(&provider, &root);
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        let tags = match file.filename.as_str() {
            "a.wav" => "café",
            "b.wav" => "Cafe\u{301}",
            "c.wav" => "CAFETERIA",
            "d.wav" => "Straße",
            "e.wav" => "STRASSE",
            "f.wav" => "ΑΘΉΝΑ",
            other => panic!("Unexpected fixture {other}"),
        };
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(tags.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let accents = BTreeSet::from(["a.wav".into(), "b.wav".into(), "c.wav".into()]);
    for query in ["af", "AF", "afé", "afe\u{301}", "FÉ", "fe\u{301}"] {
        assert_eq!(names(&provider, query), accents, "{query}");
    }
    let expanding_case = BTreeSet::from(["d.wav".into(), "e.wav".into()]);
    for query in ["rass", "raß", "RAẞ", "aße"] {
        assert_eq!(names(&provider, query), expanding_case, "{query}");
    }
    for query in ["θήν", "ΘΗΝ", "θη\u{301}ν"] {
        assert_eq!(
            names(&provider, query),
            BTreeSet::from(["f.wav".into()]),
            "{query}"
        );
    }
    let readonly = SqliteProvider::open_read_only(provider.database_path()).unwrap();
    assert_eq!(names(&readonly, "AFÉ"), accents);
    assert_eq!(names(&readonly, "rass"), expanding_case);
}
