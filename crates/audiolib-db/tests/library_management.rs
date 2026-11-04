// crates/audiolib-db/tests/library_management.rs
mod support;
use audiolib_core::*;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{fs, path::Path, sync::Arc};

fn wav(path: &Path, rate: u32) {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend(38u32.to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(rate.to_le_bytes());
    bytes.extend((rate * 2).to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(2u32.to_le_bytes());
    bytes.extend(0i16.to_le_bytes());
    fs::write(path, bytes).unwrap();
}
struct Fixture {
    temp: tempfile::TempDir,
    provider: Arc<SqliteProvider>,
    ids: Vec<i64>,
    paths: Vec<std::path::PathBuf>,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(root.join("child")).unwrap();
    let paths = vec![root.join("metal.wav"), root.join("child").join("wood.wav")];
    for path in &paths {
        wav(path, 48000);
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let ids = paths
        .iter()
        .map(|path| provider.find_audio_by_path(path).unwrap().unwrap().id)
        .collect();
    Fixture {
        temp,
        provider,
        ids,
        paths,
    }
}
fn tags(f: &Fixture, id: i64) -> String {
    f.provider.get_audio(id).unwrap().tags
}
fn patch(tags: &str) -> MetadataPatch {
    MetadataPatch {
        tags: Some(tags.into()),
        ..Default::default()
    }
}

#[test]
fn older_schema_upgrade_retires_counts_without_inventing_events() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("older.db");
    let conn = support::connection(&path).unwrap();
    // Historical additive layout, before playback history was present.
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,filename,play_count,last_played_at)
        VALUES(1,'old.wav','old.wav',7,'2024-01-02');
        DROP TABLE play_history; PRAGMA user_version=2;",
    )
    .unwrap();
    drop(conn);
    let original = fs::read(&path).unwrap();
    let owned = temp.path().join("imported.db");
    audiolib_db::import_legacy(&path, &owned).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    let upgraded = SqliteProvider::open_existing(&owned).unwrap();
    assert_eq!(
        upgraded.library_info().unwrap().schema_version,
        audiolib_db::SCHEMA_VERSION
    );

    let conn = support::connection(&owned).unwrap();
    let before: i64 = conn
        .query_row("PRAGMA data_version", [], |r| r.get(0))
        .unwrap();

    assert!(matches!(upgraded.get_audio(99999), Err(Error::NotFound(_))));
    assert_eq!(
        conn.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
    assert!(
        !conn
            .column_exists(None, "audio_files", "last_played_at")
            .unwrap()
    );
    assert!(!conn.table_exists(None, "play_history").unwrap());
    assert!(
        !conn
            .column_exists(None, "audio_files", "play_count")
            .unwrap()
    );
    assert!(upgraded.scan_history(0, 0).is_err());
}

#[test]
fn missing_visibility_lookup_and_stats_cover_hidden_records() {
    let f = fixture();
    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute("UPDATE audio_files SET missing=1 WHERE id=?1", [f.ids[0]])
        .unwrap();
    assert!(
        f.provider
            .find_audio_by_path(&f.paths[0])
            .unwrap()
            .unwrap()
            .missing
    );
    assert_eq!(f.provider.stats().unwrap().missing_files, 1);
    for (visibility, count) in [
        (FileVisibility::Present, 0),
        (FileVisibility::Missing, 1),
        (FileVisibility::All, 1),
    ] {
        let page = f
            .provider
            .search(&SearchRequest {
                query: "metal".into(),
                filters: SearchFilters {
                    visibility,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total_count, count);
        if count > 0 {
            assert!(page.results[0].missing);
        }
    }
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                filters: SearchFilters {
                    visibility: FileVisibility::All,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
}

#[test]
fn batch_edits_are_atomic_deduplicated_and_unicode_token_aware() {
    let f = fixture();
    let id = f.ids[0];
    f.provider
        .update_metadata(id, &patch("ÉCHO, stone"))
        .unwrap();
    assert!(
        f.provider
            .update_metadata_batch(&BatchMetadataRequest {
                ids: vec![id, 99999],
                patch: patch("must roll back"),
                ..Default::default()
            })
            .is_err()
    );
    assert_eq!(tags(&f, id), "ÉCHO, stone");
    let result = f
        .provider
        .update_metadata_batch(&BatchMetadataRequest {
            ids: vec![id, id],
            patch: patch("écho; metal"),
            tag_mode: TagEditMode::Append,
        })
        .unwrap();
    assert_eq!(result.updated, 1);
    assert_eq!(tags(&f, id), "ÉCHO, stone, metal");
    f.provider
        .update_metadata_batch(&BatchMetadataRequest {
            ids: vec![id],
            patch: patch("écho"),
            tag_mode: TagEditMode::Remove,
        })
        .unwrap();
    assert_eq!(tags(&f, id), "stone, metal");
    assert!(
        f.provider
            .update_metadata_batch(&BatchMetadataRequest {
                ids: vec![id],
                patch: MetadataPatch {
                    color: Some("bad".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .is_err()
    );
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "stone".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    assert!(fs::read(&f.paths[0]).unwrap().starts_with(b"RIFF"));
}

#[test]
fn backups_restore_wal_annotations_with_warm_readers_and_keep_rollback_copy() {
    let f = fixture();
    let id = f.ids[0];
    let original = fs::read(&f.paths[0]).unwrap();
    f.provider.update_metadata(id, &patch("snapshot")).unwrap();

    let backup = f.temp.path().join("backup.db");
    let report = f.provider.backup_library(&backup).unwrap();
    assert_eq!(report.total_records, 2);
    assert!(report.bytes > 0);
    let backup_bytes = fs::read(&backup).unwrap();
    assert!(f.provider.backup_library(&backup).is_err());
    assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
    f.provider.update_metadata(id, &patch("later")).unwrap();

    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "later".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    let restored = f.provider.restore_library(&backup).unwrap();
    assert!(restored.rollback_backup.is_file());
    assert_eq!(tags(&f, id), "snapshot");

    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "later".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "snapshot".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    let rollback = SqliteProvider::open_read_only(&restored.rollback_backup).unwrap();
    assert_eq!(rollback.get_audio(id).unwrap().tags, "later");

    assert!(f.provider.check_library().unwrap().is_ok());
    assert_eq!(fs::read(&f.paths[0]).unwrap(), original);
}

#[test]
fn readonly_check_export_and_backup_do_not_change_source_and_reject_admin_writes() {
    let f = fixture();
    let snapshot = f.temp.path().join("readonly.db");
    f.provider.backup_library(&snapshot).unwrap();
    let before = fs::read(&snapshot).unwrap();
    let readonly = SqliteProvider::open_read_only(&snapshot).unwrap();
    assert!(readonly.check_library().unwrap().is_ok());
    readonly
        .export_library(
            &f.temp.path().join("readonly.jsonl"),
            LibraryExportFormat::JsonLines,
        )
        .unwrap();
    readonly
        .backup_library(&f.temp.path().join("readonly-copy.db"))
        .unwrap();
    assert!(
        readonly
            .restore_library(f.provider.database_path())
            .is_err()
    );
    assert!(
        readonly
            .maintain_library(MaintenanceAction::Optimize)
            .is_err()
    );
    assert_eq!(fs::read(&snapshot).unwrap(), before);
}

#[test]
fn invalid_or_future_restore_sources_preserve_current_library_and_existing_open_rejects_legacy() {
    let f = fixture();
    let id = f.ids[0];
    f.provider.update_metadata(id, &patch("safe")).unwrap();
    let corrupt = f.temp.path().join("corrupt.db");
    fs::write(&corrupt, b"not sqlite").unwrap();
    assert!(f.provider.restore_library(&corrupt).is_err());
    assert_eq!(tags(&f, id), "safe");
    let future = f.temp.path().join("future.db");
    f.provider.backup_library(&future).unwrap();
    support::connection(&future)
        .unwrap()
        .execute_batch("PRAGMA user_version=999;")
        .unwrap();
    let before = fs::read(&future).unwrap();
    assert!(f.provider.restore_library(&future).is_err());
    assert!(SqliteProvider::open_existing(&future).is_err());
    assert_eq!(tags(&f, id), "safe");
    assert_eq!(fs::read(&future).unwrap(), before);
    let legacy = f.temp.path().join("legacy.db");
    support::connection(&legacy)
        .unwrap()
        .execute_batch("CREATE TABLE private(value TEXT);INSERT INTO private VALUES('keep');")
        .unwrap();
    let before = fs::read(&legacy).unwrap();
    assert!(SqliteProvider::open_existing(&legacy).is_err());
    assert!(f.provider.restore_library(&legacy).is_err());
    assert_eq!(tags(&f, id), "safe");
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);
    assert_eq!(fs::read(&legacy).unwrap(), before);
    let empty = f.temp.path().join("empty.db");
    support::connection(&empty)
        .unwrap()
        .execute_batch("VACUUM;")
        .unwrap();
    let before = fs::read(&empty).unwrap();
    assert!(f.provider.restore_library(&empty).is_err());
    assert_eq!(fs::read(&empty).unwrap(), before);
    assert_eq!(tags(&f, id), "safe");
}

#[test]
fn fts_contentless_storage_and_trigger_damage_is_detected_and_rebuilt_with_rollback() {
    let f = fixture();
    let id = f.ids[0];
    f.provider.update_metadata(id, &patch("known")).unwrap();
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "known".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute(
        "INSERT INTO audio_search(audio_search) VALUES('delete-all')",
        [],
    )
    .unwrap();
    let check = f.provider.check_library().unwrap();
    assert!(check.sqlite_ok);
    assert!(!check.fts_ok);
    assert!(!check.is_ok());
    let damaged = f.temp.path().join("damaged.db");
    f.provider.backup_library(&damaged).unwrap();
    assert!(f.provider.restore_library(&damaged).is_err());
    assert!(
        f.provider
            .maintain_library(MaintenanceAction::Optimize)
            .is_err()
    );
    assert!(!f.provider.check_library().unwrap().fts_ok);
    let repaired = f
        .provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    assert!(repaired.rollback_backup.is_file());
    assert!(repaired.integrity.is_ok());
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "known".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    conn.execute_batch("DROP TRIGGER audio_files_au;").unwrap();
    let check = f.provider.check_library().unwrap();
    assert!(!check.fts_ok);
    assert!(check.errors.iter().any(|error| error.contains("triggers")));
    assert!(
        f.provider
            .maintain_library(MaintenanceAction::RebuildSearch)
            .unwrap()
            .integrity
            .is_ok()
    );
    f.provider.update_metadata(id, &patch("new term")).unwrap();
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "known".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
    assert_eq!(
        f.provider
            .search(&SearchRequest {
                query: "term".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    assert!(
        f.provider
            .maintain_library(MaintenanceAction::Optimize)
            .unwrap()
            .integrity
            .is_ok()
    );
    // A standalone FTS5 replacement must not pass contentless-delete validation.
    let original_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='audio_search'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let standalone = original_sql.replace("content='',contentless_delete=1,", "");
    assert_ne!(standalone, original_sql);
    conn.execute_batch(&format!("DROP TABLE audio_search; {standalone};"))
        .unwrap();
    assert!(!f.provider.check_library().unwrap().fts_ok);
    assert!(
        f.provider
            .maintain_library(MaintenanceAction::RebuildSearch)
            .unwrap()
            .integrity
            .is_ok()
    );
}

#[test]
fn relink_refreshes_headers_preserves_identity_annotations_and_rejects_collisions() {
    let f = fixture();
    let id = f.ids[0];
    let source_bytes = fs::read(&f.paths[0]).unwrap();
    f.provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("keep".into()),
                notes: Some("notes".into()),
                favorite: Some(true),
                color: Some("#123456".into()),
            },
        )
        .unwrap();

    assert!(f.provider.relink_audio(id, &f.paths[1]).is_err());
    assert_eq!(f.provider.get_audio(id).unwrap().sample_rate, 48000);
    let target = f.temp.path().join("moved.wav");
    wav(&target, 96000);
    let target_bytes = fs::read(&target).unwrap();
    let updated = f.provider.relink_audio(id, &target).unwrap();
    assert_eq!(updated.id, id);
    assert_eq!(updated.sample_rate, 96000);
    assert_eq!(updated.tags, "keep");

    assert!(!updated.missing);
    assert_eq!(
        f.provider.find_audio_by_path(&target).unwrap().unwrap().id,
        id
    );
    assert!(
        f.provider
            .find_audio_by_path(&f.paths[0])
            .unwrap()
            .is_none()
    );
    let invalid = f.temp.path().join("bad.wav");
    fs::write(&invalid, b"broken").unwrap();
    assert!(f.provider.relink_audio(id, &invalid).is_err());
    assert_eq!(
        f.provider.find_audio_by_path(&target).unwrap().unwrap().id,
        id
    );
    assert_eq!(fs::read(&f.paths[0]).unwrap(), source_bytes);
    assert_eq!(fs::read(&target).unwrap(), target_bytes);
    assert!(f.provider.check_library().unwrap().is_ok());
}

#[test]
fn recursive_folder_removal_cleans_relations_and_keeps_source_wavs() {
    let f = fixture();
    let child = f.provider.get_audio(f.ids[1]).unwrap().folder_id.unwrap();
    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute_batch("INSERT INTO playlists(id,name) VALUES(1,'keep playlist');")
        .unwrap();
    conn.execute(
        "INSERT INTO playlist_items(playlist_id,audio_file_id) VALUES(1,?1)",
        [f.ids[1]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO project_references(audio_file_id,project_path) VALUES(?1,'demo.rpp')",
        [f.ids[1]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO waveform_cache(audio_file_id,zoom_level) VALUES(?1,'overview')",
        [f.ids[1]],
    )
    .unwrap();

    assert_eq!(f.provider.remove_folder(child).unwrap(), 1);
    assert!(f.provider.get_audio(f.ids[1]).is_err());
    for table in ["playlist_items", "project_references", "waveform_cache"] {
        assert_eq!(
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(f.provider.stats().unwrap().audio_files, 1);
    assert!(f.paths.iter().all(|path| path.is_file()));
    assert_eq!(f.provider.remove_audio(&[f.ids[0], f.ids[0]]).unwrap(), 1);
    assert_eq!(f.provider.stats().unwrap().audio_files, 0);
    assert!(f.provider.check_library().unwrap().is_ok());
}

#[test]
fn full_stream_exports_and_jsonl_csv_annotation_roundtrips_are_transactional() {
    let f = fixture();
    let id = f.ids[0];
    let notes = "line one\r\nline two, \"quoted\" 中文";
    f.provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("engine, 金属".into()),
                notes: Some(notes.into()),
                favorite: Some(true),
                color: Some("#abcdef".into()),
            },
        )
        .unwrap();
    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute("UPDATE audio_files SET missing=1 WHERE id=?1", [f.ids[1]])
        .unwrap();
    for (extension, format) in [
        ("jsonl", LibraryExportFormat::JsonLines),
        ("csv", LibraryExportFormat::Csv),
    ] {
        let output = f.temp.path().join(format!("export-{extension}.data"));
        let report = f.provider.export_library(&output, format).unwrap();
        assert_eq!(report.records, 2);
        let exported = fs::read(&output).unwrap();
        assert!(f.provider.export_library(&output, format).is_err());
        assert_eq!(fs::read(&output).unwrap(), exported);
        f.provider
            .update_metadata(
                id,
                &MetadataPatch {
                    tags: Some("changed".into()),
                    notes: Some("changed".into()),
                    favorite: Some(false),
                    color: Some(String::new()),
                },
            )
            .unwrap();
        let imported = f.provider.import_annotations(&output).unwrap();
        assert_eq!(imported.records, 2);
        let audio = f.provider.get_audio(id).unwrap();
        assert_eq!(audio.notes, notes);
        assert_eq!(audio.tags, "engine, 金属");
        assert!(audio.favorite);
        assert_eq!(audio.color.as_deref(), Some("#abcdef"));
    }
    let bad = f.temp.path().join("bad.jsonl");
    let row = serde_json::json!({"id":f.ids[1],"file_path":f.provider.get_audio(id).unwrap().file_path,"tags":"rollback"});
    fs::write(&bad, format!("{row}\n{{invalid\n")).unwrap();
    assert!(f.provider.import_annotations(&bad).is_err());
    assert_eq!(tags(&f, id), "engine, 金属");
    let skipped = f.temp.path().join("skipped.jsonl");
    fs::write(
        &skipped,
        "{\"file_path\":\"unknown.wav\",\"tags\":\"skip\"}\n",
    )
    .unwrap();
    assert_eq!(f.provider.import_annotations(&skipped).unwrap().skipped, 1);
}

#[test]
fn administration_shares_scan_lock_while_snapshot_reads_continue() {
    let f = fixture();
    let mut name = f
        .provider
        .database_path()
        .file_name()
        .unwrap()
        .to_os_string();
    name.push(".scan.lock");
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.provider.database_path().with_file_name(name))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(matches!(
        f.provider.remove_audio(&[f.ids[0]]),
        Err(Error::Busy(_))
    ));
    assert!(matches!(
        f.provider.update_metadata_batch(&BatchMetadataRequest {
            ids: vec![f.ids[0]],
            patch: patch("busy"),
            ..Default::default()
        }),
        Err(Error::Busy(_))
    ));
    assert!(matches!(
        f.provider.maintain_library(MaintenanceAction::Optimize),
        Err(Error::Busy(_))
    ));
    f.provider
        .backup_library(&f.temp.path().join("while-scanning.db"))
        .unwrap();
    f.provider
        .export_library(
            &f.temp.path().join("while-scanning.jsonl"),
            LibraryExportFormat::JsonLines,
        )
        .unwrap();
    lock.unlock().unwrap();
    assert_eq!(f.provider.remove_audio(&[f.ids[0]]).unwrap(), 1);
}

#[test]
fn root_relocation_refuses_existing_indexed_destinations_and_precommit_cancellation() {
    let f = fixture();
    let source = f.temp.path().join("audio");
    let destination = f.temp.path().join("destination");
    fs::create_dir_all(destination.join("child")).unwrap();
    fs::copy(&f.paths[0], destination.join("metal.wav")).unwrap();
    fs::copy(&f.paths[1], destination.join("child").join("wood.wav")).unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert!(matches!(
        f.provider
            .relink_folder_cancellable(&source, &destination, cancel),
        Err(Error::Cancelled)
    ));
    ScanService::new(f.provider.clone())
        .scan(
            &destination,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert!(matches!(
        f.provider.relink_folder(&source, &destination),
        Err(Error::Validation(_))
    ));
    for (id, path) in f.ids.iter().zip(&f.paths) {
        assert_eq!(
            f.provider.get_audio(*id).unwrap().file_path,
            audiolib_db::display_path(&path.canonicalize().unwrap())
        );
    }
    assert_eq!(f.provider.stats().unwrap().audio_files, 4);
}

#[test]
fn root_relocation_honors_precancelled_aliases_and_unindexed_roots() {
    let f = fixture();
    let source = f.temp.path().join("audio").canonicalize().unwrap();
    let alias = source.join("");
    assert!(alias.is_dir());
    let unindexed = f.temp.path().join("unindexed");
    let destination = f.temp.path().join("destination");
    fs::create_dir_all(&unindexed).unwrap();
    fs::create_dir_all(&destination).unwrap();
    f.provider
        .update_metadata(
            f.ids[0],
            &MetadataPatch {
                notes: Some("keep after cancellation".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| f.provider.get_audio(*id).unwrap())
        .collect();
    let roots = f.provider.folders(None).unwrap();
    let root_id = roots[0].id;
    let children = f.provider.folders(Some(root_id)).unwrap();
    let original: Vec<_> = f.paths.iter().map(|path| fs::read(path).unwrap()).collect();
    for (source, target) in [(&source, &alias), (&unindexed, &destination)] {
        let result = f.provider.relink_folder_cancellable(
            source,
            target,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    }
    // A request cancelled before admission must also avoid acquiring OS leases.
    let database = f.provider.database_path().canonicalize().unwrap();
    let mut name = database.file_name().unwrap().to_os_string();
    name.push(".scan.lock");
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(database.with_file_name(name))
        .unwrap();
    lock.try_lock().unwrap();
    let result = f.provider.relink_folder_cancellable(
        &source,
        &destination,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    lock.unlock().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    for (old, bytes) in before.iter().zip(original) {
        assert_eq!(
            serde_json::to_value(f.provider.get_audio(old.id).unwrap()).unwrap(),
            serde_json::to_value(old).unwrap()
        );
        assert_eq!(fs::read(&old.file_path).unwrap(), bytes);
    }
    assert_eq!(
        serde_json::to_value(f.provider.folders(None).unwrap()).unwrap(),
        serde_json::to_value(roots).unwrap()
    );
    assert_eq!(
        serde_json::to_value(f.provider.folders(Some(root_id)).unwrap()).unwrap(),
        serde_json::to_value(children).unwrap()
    );
    assert!(!f.temp.path().join("backups").exists());
}

#[test]
fn root_relocation_after_a_case_only_rename_preserves_records_and_source_audio() {
    let f = fixture();
    let source = f.temp.path().join("audio");
    let target = f.temp.path().join("Audio");
    let case_insensitive = target.exists();
    let original: Vec<_> = f.paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let first = f.provider.get_audio(f.ids[0]).unwrap();
    f.provider
        .update_metadata(
            first.id,
            &MetadataPatch {
                tags: Some("retained tag".into()),
                notes: Some("retained note".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute(
        "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.unknown_extension','keep') WHERE id=?1",
        [first.id],
    )
    .unwrap();
    drop(conn);
    let root_id = f.provider.folders(None).unwrap()[0].id;
    let child_id = f.provider.folders(Some(root_id)).unwrap()[0].id;
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| serde_json::to_value(f.provider.get_audio(*id).unwrap()).unwrap())
        .collect();
    let before_root = f.provider.get_folder(root_id).unwrap().path;

    fs::rename(&source, &target).unwrap();
    assert_eq!(source.exists(), case_insensitive);
    // On APFS the old spelling may still resolve, and realpath may already return
    // the new spelling. A precancelled request must still leave every record alone.
    assert!(matches!(
        f.provider.relink_folder_cancellable(
            &source,
            &target,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        ),
        Err(Error::Cancelled)
    ));
    for (id, preserved) in f.ids.iter().zip(&before) {
        assert_eq!(
            serde_json::to_value(f.provider.get_audio(*id).unwrap()).unwrap(),
            *preserved
        );
    }
    assert_eq!(f.provider.get_folder(root_id).unwrap().path, before_root);
    assert!(!f.temp.path().join("backups").exists());
    if cfg!(windows) {
        // Windows aliases remain a no-op even after a physical case-only rename.
        assert_eq!(f.provider.relink_folder(&source, &target).unwrap(), 0);
        for ((id, preserved), (relative, bytes)) in f
            .ids
            .iter()
            .zip(&before)
            .zip(["metal.wav", "child/wood.wav"].into_iter().zip(&original))
        {
            assert_eq!(
                serde_json::to_value(f.provider.get_audio(*id).unwrap()).unwrap(),
                *preserved
            );
            assert_eq!(fs::read(target.join(relative)).unwrap(), *bytes);
        }
        return;
    }
    assert_eq!(f.provider.relink_folder(&source, &target).unwrap(), 2);
    for ((id, relative), bytes) in f
        .ids
        .iter()
        .zip(["metal.wav", "child/wood.wav"])
        .zip(original)
    {
        let moved = target.join(relative);
        let audio = f.provider.get_audio(*id).unwrap();
        assert_eq!(
            audio.file_path,
            audiolib_db::display_path(&moved.canonicalize().unwrap())
        );
        assert_eq!(fs::read(&moved).unwrap(), bytes);
    }
    let moved = f.provider.get_audio(first.id).unwrap();
    assert_eq!(moved.folder_id, first.folder_id);
    assert_eq!(
        (moved.tags.as_str(), moved.notes.as_str(), moved.favorite),
        ("retained tag", "retained note", true)
    );
    assert_eq!(moved.attributes["unknown_extension"], "keep");
    assert_eq!(
        f.provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&target.canonicalize().unwrap())
    );
    assert_eq!(f.provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);

    let rescanned = ScanService::new(f.provider.clone())
        .scan(
            &target,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(rescanned.added, 0);
    assert_eq!(
        f.provider.get_audio(first.id).unwrap().notes,
        "retained note"
    );
}

#[test]
fn root_relocation_rejects_ambiguous_indexed_source_aliases() {
    let f = fixture();
    let source = f.temp.path().join("audio");
    let target = f.temp.path().join("destination");
    fs::create_dir(&target).unwrap();
    let canonical = source.canonicalize().unwrap();
    // Two stored spellings are aliases only when the volume resolves both names
    // to the same directory. A case-sensitive volume keeps them distinct.
    if !canonical.with_file_name("AUDIO").exists() {
        eprintln!("case-insensitive filesystem unavailable; not run");
        return;
    }
    let root_id = f.provider.folders(None).unwrap()[0].id;
    let conn = support::connection(f.provider.database_path()).unwrap();
    conn.execute(
        "UPDATE folders SET folder_path=?2 WHERE id=?1",
        rusqlite::params![
            root_id,
            audiolib_db::folder_path_text(&canonical.with_file_name("AUDIO"))
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO folders(folder_path,folder_name) VALUES(?1,'alias')",
        [audiolib_db::folder_path_text(
            &canonical.with_file_name("AuDiO"),
        )],
    )
    .unwrap();
    drop(conn);
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| serde_json::to_value(f.provider.get_audio(*id).unwrap()).unwrap())
        .collect();
    let original: Vec<_> = f.paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let error = f.provider.relink_folder(&source, &target).unwrap_err();
    assert!(matches!(error, Error::Validation(_)), "{error:?}");
    assert!(
        error.to_string().contains("more than one indexed folder"),
        "{error}"
    );
    for ((id, preserved), (path, bytes)) in
        f.ids.iter().zip(&before).zip(f.paths.iter().zip(&original))
    {
        assert_eq!(
            serde_json::to_value(f.provider.get_audio(*id).unwrap()).unwrap(),
            *preserved
        );
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
    assert!(!f.temp.path().join("backups").exists());
}

#[cfg(unix)]
#[test]
fn root_relocation_resolves_parent_aliases_after_unicode_case_only_rename() {
    let temp = tempfile::tempdir().unwrap();
    let actual_parent = temp.path().join("actual");
    let alias_parent = temp.path().join("alias");
    let source = actual_parent.join("Äudio");
    let target = actual_parent.join("äudio");
    fs::create_dir_all(source.join("child")).unwrap();
    std::os::unix::fs::symlink(&actual_parent, &alias_parent).unwrap();
    let requested_source = alias_parent.join("Äudio");
    let case_insensitive = target.exists();
    let paths = [source.join("metal.wav"), source.join("child/wood.wav")];
    for path in &paths {
        wav(path, 48_000);
    }
    let original: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &source,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let ids: Vec<_> = paths
        .iter()
        .map(|path| provider.find_audio_by_path(path).unwrap().unwrap().id)
        .collect();
    provider
        .update_metadata(
            ids[0],
            &MetadataPatch {
                tags: Some("retained Unicode tag".into()),
                notes: Some("retained alias note".into()),
                favorite: Some(true),
                color: Some("#abcdef".into()),
            },
        )
        .unwrap();

    let before: Vec<_> = ids
        .iter()
        .map(|id| provider.get_audio(*id).unwrap())
        .collect();
    let root_id = provider.folders(None).unwrap()[0].id;
    let child_id = provider.folders(Some(root_id)).unwrap()[0].id;
    fs::rename(&source, &target).unwrap();
    assert_eq!(source.exists(), case_insensitive);
    assert!(matches!(
        provider.relink_folder_cancellable(
            &requested_source,
            &target,
            Arc::new(std::sync::atomic::AtomicBool::new(true))
        ),
        Err(Error::Cancelled)
    ));
    for previous in &before {
        assert_eq!(
            serde_json::to_value(provider.get_audio(previous.id).unwrap()).unwrap(),
            serde_json::to_value(previous).unwrap()
        );
    }
    assert!(!temp.path().join("backups").exists());
    assert_eq!(
        provider.relink_folder(&requested_source, &target).unwrap(),
        2
    );
    for ((previous, relative), bytes) in before
        .iter()
        .zip(["metal.wav", "child/wood.wav"])
        .zip(&original)
    {
        let moved = target.join(relative);
        let current = provider.get_audio(previous.id).unwrap();
        assert_eq!(
            current.file_path,
            audiolib_db::display_path(&moved.canonicalize().unwrap())
        );
        assert_eq!(current.folder_id, previous.folder_id);
        assert_eq!(
            (
                &current.tags,
                &current.notes,
                current.favorite,
                &current.color
            ),
            (
                &previous.tags,
                &previous.notes,
                previous.favorite,
                &previous.color
            )
        );
        assert_eq!(fs::read(&moved).unwrap(), *bytes);
    }
    assert_eq!(
        provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&target.canonicalize().unwrap())
    );
    assert_eq!(provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(provider.stats().unwrap().audio_files, 2);

    let rescanned = ScanService::new(provider.clone())
        .scan(
            &target,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(rescanned.added, 0);
    assert_eq!(
        provider.get_audio(ids[0]).unwrap().notes,
        "retained alias note"
    );
}

#[cfg(windows)]
#[test]
fn root_relocation_keeps_windows_case_aliases_as_a_noop() {
    let f = fixture();
    let source = f.temp.path().join("audio");
    let equivalent = std::path::PathBuf::from(source.to_string_lossy().to_uppercase());
    assert!(equivalent.is_dir());
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| f.provider.get_audio(*id).unwrap().file_path)
        .collect();
    assert_eq!(f.provider.relink_folder(&source, &equivalent).unwrap(), 0);
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);
    for (id, path) in f.ids.iter().zip(before) {
        assert_eq!(f.provider.get_audio(*id).unwrap().file_path, path);
    }
}

#[test]
fn root_relocation_resolves_existing_source_aliases() {
    let f = fixture();
    // This alias reproduces the raw/canonical identity mismatch on every OS,
    // including macOS temporary paths behind the /var -> /private/var symlink.
    let source = f.temp.path().join("audio").join("..").join("audio");
    assert!(source.is_dir());
    let canonical_source = source.canonicalize().unwrap();
    assert_ne!(
        audiolib_db::folder_path_text(&source),
        audiolib_db::folder_path_text(&canonical_source)
    );
    let root_id = f.provider.folders(None).unwrap()[0].id;
    let child_id = f.provider.folders(Some(root_id)).unwrap()[0].id;
    f.provider
        .update_metadata(
            f.ids[0],
            &MetadataPatch {
                tags: Some("retained tag".into()),
                notes: Some("retained alias note".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();

    let extension = serde_json::json!({ "marker": "keep", "values": [1, 2] });
    let conn = support::connection(f.provider.database_path()).unwrap();
    for id in &f.ids {
        conn.execute(
            "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.unknown_extension',json(?1)) WHERE id=?2",
            rusqlite::params![extension.to_string(), id],
        )
        .unwrap();
    }
    drop(conn);
    let preserved_attributes = |audio: &AudioFile| {
        let mut attributes = audio.attributes.clone();
        // Relocation updates this derived library timestamp; every other source
        // and extension Metadata field must remain unchanged.
        assert!(
            attributes
                .as_object_mut()
                .unwrap()
                .remove("modified_at")
                .is_some()
        );
        attributes
    };
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| f.provider.get_audio(*id).unwrap())
        .collect();
    let original: Vec<_> = f.paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let occupied = f.temp.path().join("occupied");
    let target = f.temp.path().join("relocated");
    for destination in [&occupied, &target] {
        fs::create_dir_all(destination.join("child")).unwrap();
        fs::copy(&f.paths[0], destination.join("metal.wav")).unwrap();
        fs::copy(&f.paths[1], destination.join("child/wood.wav")).unwrap();
    }
    ScanService::new(f.provider.clone())
        .scan(
            &occupied,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let result = f.provider.relink_folder(&source, &occupied);
    assert!(matches!(result, Err(Error::Validation(_))), "{result:?}");
    for old in &before {
        assert_eq!(
            serde_json::to_value(f.provider.get_audio(old.id).unwrap()).unwrap(),
            serde_json::to_value(old).unwrap()
        );
    }
    assert_eq!(f.provider.relink_folder(&source, &target).unwrap(), 2);
    for ((old, relative), bytes) in before
        .iter()
        .zip(["metal.wav", "child/wood.wav"])
        .zip(original)
    {
        let moved = target.join(relative);
        let current = f.provider.get_audio(old.id).unwrap();
        assert_eq!(
            current.file_path,
            audiolib_db::display_path(&moved.canonicalize().unwrap())
        );
        assert_eq!(current.folder_id, old.folder_id);
        assert_eq!(current.tags, old.tags);
        assert_eq!(current.notes, old.notes);
        assert_eq!(current.favorite, old.favorite);

        assert_eq!(preserved_attributes(&current), preserved_attributes(old));
        assert_eq!(current.attributes["unknown_extension"], extension);
        assert_eq!(fs::read(&old.file_path).unwrap(), bytes);
        assert_eq!(fs::read(moved).unwrap(), bytes);
    }
    assert_eq!(
        f.provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&target.canonicalize().unwrap())
    );
    assert_eq!(f.provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(f.provider.stats().unwrap().audio_files, 4);

    // A subsequent physical move leaves the stored canonical source missing.
    // It must still be usable to relocate the same records rather than fail its
    // canonicalization or silently return zero.
    let old_target = target.canonicalize().unwrap();
    let moved_again = f.temp.path().join("moved-again");
    fs::rename(&target, &moved_again).unwrap();
    assert!(!old_target.exists());
    assert_eq!(
        f.provider.relink_folder(&old_target, &moved_again).unwrap(),
        2
    );
    for (old, relative) in before.iter().zip(["metal.wav", "child/wood.wav"]) {
        let current = f.provider.get_audio(old.id).unwrap();
        assert_eq!(
            current.file_path,
            audiolib_db::display_path(&moved_again.join(relative).canonicalize().unwrap())
        );
        assert_eq!(current.folder_id, old.folder_id);
        assert_eq!(current.tags, old.tags);
        assert_eq!(current.notes, old.notes);
        assert_eq!(current.favorite, old.favorite);

        assert_eq!(preserved_attributes(&current), preserved_attributes(old));
        assert_eq!(current.attributes["unknown_extension"], extension);
    }
    assert_eq!(
        f.provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&moved_again.canonicalize().unwrap())
    );
}

#[cfg(unix)]
#[test]
fn root_relocation_accepts_native_separators_but_rejects_linked_children() {
    let f = fixture();
    let source = f.temp.path().join("audio").canonicalize().unwrap();
    let target = source.parent().unwrap().join("relocated");
    let original: Vec<_> = f.paths.iter().map(|path| fs::read(path).unwrap()).collect();
    f.provider
        .update_metadata(
            f.ids[0],
            &MetadataPatch {
                notes: Some("preserved after link rejection".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let before: Vec<_> = f
        .ids
        .iter()
        .map(|id| f.provider.get_audio(*id).unwrap())
        .collect();
    let root_id = f.provider.folders(None).unwrap()[0].id;
    let child_id = f.provider.folders(Some(root_id)).unwrap()[0].id;
    fs::create_dir_all(target.join("actual-child")).unwrap();
    fs::copy(&f.paths[0], target.join("metal.wav")).unwrap();
    fs::copy(&f.paths[1], target.join("actual-child/wood.wav")).unwrap();
    std::os::unix::fs::symlink("actual-child", target.join("child")).unwrap();
    // The link stays inside the allowed root, but cannot change the stored tree's
    // identity. Reject it before committing any root or audio path updates.
    assert!(matches!(
        f.provider.relink_folder(&source, &target),
        Err(Error::Validation(message))
            if message == "Relocation folders must use real paths without directory links"
    ));
    for old in &before {
        let current = f.provider.get_audio(old.id).unwrap();
        assert_eq!(current.file_path, old.file_path);
        assert_eq!(current.folder_id, old.folder_id);
        assert_eq!(current.notes, old.notes);
    }
    assert_eq!(
        f.provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&source)
    );
    // Unlink only the owned fixture link; no source WAV is removed or changed.
    fs::remove_file(target.join("child")).unwrap();
    fs::rename(target.join("actual-child"), target.join("child")).unwrap();
    assert_eq!(
        f.provider.relink_folder(&source, &target.join("")).unwrap(),
        2
    );
    for (((old, relative), path), bytes) in before
        .iter()
        .zip(["metal.wav", "child/wood.wav"])
        .zip(&f.paths)
        .zip(original)
    {
        let moved = target.join(relative);
        let current = f.provider.get_audio(old.id).unwrap();
        assert_eq!(
            current.file_path,
            audiolib_db::display_path(&moved.canonicalize().unwrap())
        );
        assert_eq!(current.folder_id, old.folder_id);
        assert_eq!(current.notes, old.notes);
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::read(moved).unwrap(), bytes);
    }
    assert_eq!(
        f.provider.get_folder(root_id).unwrap().path,
        audiolib_db::folder_path_text(&target.canonicalize().unwrap())
    );
    assert_eq!(f.provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);
}

#[test]
fn root_relocation_obeys_cross_process_scan_and_administration_leases() {
    let f = fixture();
    let source = f.temp.path().join("audio");
    let target = f.temp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    for suffix in [".scan.lock", ".admin.lock"] {
        let database = f.provider.database_path().canonicalize().unwrap();
        let mut name = database.file_name().unwrap().to_os_string();
        name.push(suffix);
        let lock = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(database.with_file_name(name))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            matches!(
                f.provider.relink_folder(&source, &target),
                Err(Error::Busy(_))
            ),
            "{suffix}"
        );
        lock.unlock().unwrap();
    }
    assert!(source.join("metal.wav").is_file());
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);
}

#[test]
fn root_relocation_rejects_volume_roots_before_changing_any_paths() {
    let f = fixture();
    let filesystem_root = f.paths[0].ancestors().last().unwrap();
    assert!(matches!(
        f.provider
            .relink_folder(filesystem_root, &f.temp.path().join("audio")),
        Err(Error::Validation(_))
    ));
    assert_eq!(f.provider.stats().unwrap().audio_files, 2);
}
