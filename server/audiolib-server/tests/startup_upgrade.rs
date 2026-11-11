// server/audiolib-server/tests/startup_upgrade.rs
//! Server startup exposes synchronous library-upgrade progress in ordinary info logs.
use audiolib_core::{
    DataProvider,
    compatibility::{LibraryAccess, LibraryFormat},
};
use audiolib_db::{SnapshotUpgradeProgress, SqliteProvider, migration};
use audiolib_server::{AppState, Security, ServerConfig};
use rusqlite::Connection;
use std::{
    io::{self, Write},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

struct LogWriter(Arc<Mutex<Vec<u8>>>);

// The scoped log subscriber shares tracing's process-wide callsite interest cache
// with the other startup tests. Keep these opens sequential so a parallel open
// without that subscriber cannot suppress events being captured by the log test.
static STARTUP_TEST_LOCK: Mutex<()> = Mutex::new(());

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn server_startup_logs_upgrade_phases_rows_and_elapsed_time() {
    let _startup = STARTUP_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("library.db");
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(include_str!(
        "../../../crates/audiolib-db/tests/fixtures/schema-v5.sql"
    ))
    .unwrap();
    conn.execute_batch(
        "WITH RECURSIVE records(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM records WHERE id<5001)
         INSERT INTO audio_files(id,file_path,filename,user_notes)
         SELECT id,'root/clip-'||id||'.wav','clip-'||id||'.wav','retained notes' FROM records;
         UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';",
    )
    .unwrap();
    drop(conn);
    let config = ServerConfig::from_toml_str("library_db = 'library.db'", dir.path()).unwrap();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let output = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || LogWriter(output.clone()))
        .finish();
    let state =
        tracing::subscriber::with_default(subscriber, || AppState::open(&config, None)).unwrap();
    let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let expected = [
        "phase=\"preparing\"",
        "phase=\"copying\" completed=5000 total=5001",
        "phase=\"copying\" completed=5001 total=5001",
        "phase=\"indexing\" completed=5000 total=5001",
        "phase=\"indexing\" completed=5001 total=5001",
        "Library startup: database opened",
    ];
    let mut after = 0;
    for event in expected {
        let index = text[after..]
            .find(event)
            .unwrap_or_else(|| panic!("Missing {event:?} in {text}"));
        after += index + event.len();
    }
    assert!(text.contains("elapsed_seconds="), "{text}");
    assert!(text.contains("committed rows"), "{text}");
    assert_eq!(state.audio(1).unwrap().notes, "retained notes");
    assert!(SqliteProvider::has_current_search_state(&database).unwrap());
}

fn startup_fixture(format: &str) -> (tempfile::TempDir, ServerConfig) {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    if matches!(format, "p0" | "p1" | "z1") {
        drop(SqliteProvider::open(&database).unwrap());
    }
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    if let Some(version) = format.strip_prefix("schema") {
        conn.execute_batch(include_str!(
            "../../../crates/audiolib-db/tests/fixtures/schema-v5.sql"
        ))
        .unwrap();
        let version: i64 = version.parse().unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        if version < 5 {
            // Construct a supported historical layout, rather than claiming an
            // archived v3/v4 database: stable IDs and derived text were added later.
            conn.execute_batch(
                "DROP TRIGGER audiolib_audio_id_high_water;
                 DROP TABLE audiolib_audio_id_sequence;
                 DELETE FROM audiolib_schema_features WHERE feature='stable_audio_ids';
                 UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';
                 UPDATE library_meta SET value='1' WHERE key IN ('source_search_rev','source_tags_rev');",
            )
            .unwrap();
        }
    } else if format == "p0" {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS audiolib_album_audio_insert;
             DROP TRIGGER IF EXISTS audiolib_album_audio_update;
             DROP TRIGGER IF EXISTS audiolib_album_folder_delete;
             DROP TRIGGER IF EXISTS audiolib_album_id_high_water;
             DROP INDEX idx_audio_visible_album;
             DROP INDEX IF EXISTS idx_audio_album_members;
             ALTER TABLE audio_files DROP COLUMN album_id;
             DROP TABLE albums;
             DROP TABLE audiolib_album_id_sequence;
             ALTER TABLE audio_files ADD COLUMN album TEXT NOT NULL DEFAULT '';
             CREATE INDEX idx_audio_visible_album ON audio_files(album,id) WHERE missing=0;
             DELETE FROM audiolib_schema_features WHERE feature IN ('album_entities','metadata_storage');
             DELETE FROM library_meta WHERE key='metadata_storage_policy';
             PRAGMA user_version=7;
             ALTER TABLE audio_files ADD COLUMN last_played_at TEXT;
             UPDATE audiolib_schema_features SET version=1 WHERE feature='play_history_retired';",
        )
        .unwrap();
    }
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN startup_extension TEXT;
         INSERT INTO audio_files(id,file_path,filename,user_tags,user_rating,user_notes,favorite,metadata_json,startup_extension)
         VALUES(42,'owned/startup-metal.wav','startup-metal.wav','retained tags',4,'retained annotation',1,
                '{\"unknown_startup_extension\":{\"retained\":[1,true,\"value\"]}}','retained extension');",
    )
    .unwrap();
    if format.starts_with("schema") {
        conn.execute_batch(
            "UPDATE audio_files SET play_count=17,last_played_at='2026-10-21T09:00:00Z' WHERE id=42;
             INSERT INTO play_history(id,audio_file_id,played_at,filename,file_path)
             VALUES(7,42,'2026-10-21T09:00:00Z','startup-metal.wav','owned/startup-metal.wav');",
        )
        .unwrap();
    } else if format == "p0" {
        conn.execute_batch(
            "UPDATE audio_files SET last_played_at='2026-10-21T09:00:00Z' WHERE id=42;",
        )
        .unwrap();
    } else if format == "z1" {
        // Experimental z1 permits TEXT rows too. Preserve its real format marker
        // and native album domain when the current server prepares plain storage.
        conn.execute_batch(
            "INSERT INTO audiolib_schema_features(feature,version) VALUES('metadata_storage',1);
             UPDATE audiolib_album_id_sequence SET compression=1 WHERE singleton=1;",
        )
        .unwrap();
    }
    drop(conn);
    let config = ServerConfig::from_toml_str(
        "library_db='library.db'\nsnapshot_dir='snapshots'",
        root.path(),
    )
    .unwrap();
    (root, config)
}

#[test]
fn server_startup_auto_migrates_supported_formats_preserves_data_and_honors_rollback() {
    let _startup = STARTUP_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let cancel = AtomicBool::new(false);
    for format in ["schema3", "schema4", "schema5", "p0", "p1", "z1"] {
        let (_root, config) = startup_fixture(format);
        let original_album_domain = (format == "z1").then(|| {
            Connection::open(&config.library_db).unwrap().query_row(
                "SELECT album_id,namespace FROM audio_files,audiolib_album_id_sequence WHERE audio_files.id=42 AND singleton=1",
                [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            ).unwrap()
        });
        let source =
            migration::inspect(&config.library_db, LibraryAccess::Read, true, &cancel).unwrap();
        let mut phases = Vec::new();
        let state =
            AppState::open_with_progress(&config, Security::from_config(&config, None), |phase| {
                phases.push(phase)
            })
            .unwrap();
        assert_eq!(
            state.provider().library_info().unwrap().schema_version,
            audiolib_db::SCHEMA_VERSION,
            "{format}"
        );
        let audio = state.audio(42).unwrap();
        assert_eq!(audio.id, 42);
        assert_eq!(audio.notes, "retained annotation", "{format}");
        assert_eq!(audio.tags, "retained tags", "{format}");
        assert!(audio.favorite, "{format}");
        assert_eq!(
            audio.attributes["unknown_startup_extension"]["retained"],
            serde_json::json!([1, true, "value"])
        );
        let conn = Connection::open(&config.library_db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT startup_extension,user_rating FROM audio_files WHERE id=42",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
            ("retained extension".into(), 4),
            "{format}"
        );
        assert_eq!(
            conn.query_row(
                "SELECT typeof(metadata_json) FROM audio_files WHERE id=42",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "text",
            "{format}"
        );
        if let Some((album_id, namespace)) = original_album_domain {
            assert_eq!(audio.album_id, Some(album_id));
            assert_eq!(state.provider().album_namespace(), namespace);
        }
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM pragma_table_xinfo('audio_files') WHERE name IN ('play_count','last_played_at')", [], |row| row.get::<_, i64>(0)).unwrap(), 0, "{format}");
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='play_history'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "{format}"
        );
        drop(conn);
        state.shutdown(Duration::from_secs(5)).unwrap();
        drop(state);
        let complete = migration::discover(&config.library_db).unwrap();
        if format == "p1" {
            assert!(complete.is_none());
            assert!(
                phases
                    .iter()
                    .all(|phase| matches!(phase, SnapshotUpgradeProgress::Preparing))
            );
        } else {
            assert!(
                phases
                    .iter()
                    .any(|phase| matches!(phase, SnapshotUpgradeProgress::BackingUp)),
                "{format}"
            );
            let job = complete.as_ref().unwrap();
            assert_eq!(job.phase, migration::MigrationPhase::Complete, "{format}");
            assert!(
                job.plan.local_history.is_none(),
                "Server startup must not import client-local history"
            );
            assert!(job.directory.join("backup.db").is_file());
            migration::verify(&job.directory, &cancel).unwrap();
        }
        let target =
            migration::inspect(&config.library_db, LibraryAccess::Write, true, &cancel).unwrap();
        assert_eq!(target.format, LibraryFormat::target(), "{format}");
        let mut second_phases = Vec::new();
        let state =
            AppState::open_with_progress(&config, Security::from_config(&config, None), |phase| {
                second_phases.push(phase)
            })
            .unwrap();
        state.shutdown(Duration::from_secs(5)).unwrap();
        drop(state);
        assert!(
            second_phases
                .iter()
                .all(|phase| matches!(phase, SnapshotUpgradeProgress::Preparing)),
            "{format}: {second_phases:?}"
        );
        assert_eq!(
            migration::inspect(&config.library_db, LibraryAccess::Write, true, &cancel).unwrap(),
            target,
            "{format}"
        );
        assert_eq!(
            migration::discover(&config.library_db)
                .unwrap()
                .map(|job| job.id),
            complete.as_ref().map(|job| job.id.clone()),
            "{format}"
        );
        if let Some(job) = complete {
            migration::rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
            let restored =
                migration::inspect(&config.library_db, LibraryAccess::Read, true, &cancel).unwrap();
            assert_eq!(restored.format, source.format, "{format}");
            assert_eq!(restored.business, source.business, "{format}");
            assert!(
                matches!(
                    AppState::open(&config, None),
                    Err(audiolib_core::Error::Busy(_))
                ),
                "{format}: explicit rollback must prevent automatic re-upgrade"
            );
        }
    }
}

#[test]
fn server_startup_rejects_future_formats_without_migrating_the_database() {
    let _startup = STARTUP_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for future in ["schema", "known_feature", "unknown_feature"] {
        let (_root, config) = startup_fixture("p1");
        let conn = Connection::open(&config.library_db).unwrap();
        match future {
            "schema" => conn.pragma_update(None, "user_version", 99).unwrap(),
            "known_feature" => {
                conn.execute(
                    "UPDATE audiolib_schema_features SET version=99 WHERE feature='tokenization'",
                    [],
                )
                .unwrap();
            }
            "unknown_feature" => {
                conn.execute(
                    "INSERT INTO audiolib_schema_features VALUES('unknown_startup_feature',1)",
                    [],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        drop(conn);
        let bytes = std::fs::read(&config.library_db).unwrap();
        assert!(AppState::open(&config, None).is_err(), "{future}");
        assert_eq!(
            std::fs::read(&config.library_db).unwrap(),
            bytes,
            "{future}"
        );
        assert!(
            migration::discover(&config.library_db).unwrap().is_none(),
            "{future}"
        );
    }
}
