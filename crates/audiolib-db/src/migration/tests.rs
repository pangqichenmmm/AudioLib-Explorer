// crates/audiolib-db/src/migration/tests.rs
//! Migration tests use only owned temporary libraries.
use super::*;
use crate::{load_tokenizer, schema};
use audiolib_core::Error;
use rusqlite::{Connection, params};
use std::sync::atomic::AtomicBool;

// Fault injection is confined to a closed DELETE-journal temporary fixture.
fn corrupt_table_root(path: &std::path::Path, table: &str) {
    use std::io::{Seek, SeekFrom, Write};
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
    let page: u64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |row| crate::unsigned(row, 0),
        )
        .unwrap();
    let size: u64 = conn
        .pragma_query_value(None, "page_size", |row| crate::unsigned(row, 0))
        .unwrap();
    assert!(page > 1);
    drop(conn);
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start((page - 1) * size)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn diagnosis_requires_actual_business_corruption_and_never_infers_it_from_versions() {
    use audiolib_core::compatibility::{CompatibilityReason, ExecutionState, MigrationDisposition};
    for future in [
        None,
        Some("PRAGMA user_version=99;"),
        Some("INSERT INTO audiolib_schema_features VALUES('future_required_feature',1);"),
    ] {
        let (_directory, path) = current_definition_fixture();
        if let Some(change) = future {
            Connection::open(&path)
                .unwrap()
                .execute_batch(change)
                .unwrap();
        }
        corrupt_table_root(&path, "audio_files");
        let bytes = std::fs::read(&path).unwrap();
        let report = diagnose(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            report.decision.execution_state,
            ExecutionState::Blocked,
            "{report:?}"
        );
        assert!(!report.automatic_rebuild_allowed);
        if future.is_some() {
            assert_eq!(report.decision.disposition, None, "{report:?}");
            assert!(
                report
                    .decision
                    .reasons
                    .contains(&CompatibilityReason::ToolUpgradeRequired)
            );
            assert!(report.findings.is_empty());
        } else {
            assert_eq!(
                report.decision.disposition,
                Some(MigrationDisposition::RebuildRequired),
                "{report:?}"
            );
            assert_eq!(
                report.findings[0].code,
                DiagnosticCode::BusinessRowsUnreadable
            );
            assert_eq!(report.findings[0].object.as_deref(), Some("audio_files"));
            assert!(report.findings[0].sqlite_extended_code.is_some());
        }
        assert!(plan(&path, &AtomicBool::new(false)).is_err());
        assert!(discover(&path).unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn diagnosis_blocks_broken_references_and_retired_corruption_without_rebuild() {
    use audiolib_core::compatibility::ExecutionState;
    for retired in [false, true] {
        let (_directory, path) = fixture();
        if retired {
            corrupt_table_root(&path, "play_history");
        } else {
            Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "PRAGMA foreign_keys=OFF; UPDATE playlist_items SET audio_file_id=321;",
                )
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let report = diagnose(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(report.decision.disposition, None, "{report:?}");
        assert_eq!(report.decision.execution_state, ExecutionState::Blocked);
        assert_eq!(
            report.findings[0].code,
            if retired {
                DiagnosticCode::RetiredDataUnreadable
            } else {
                DiagnosticCode::BusinessReferencesInvalid
            }
        );
        assert!(!report.automatic_rebuild_allowed);
        assert!(plan(&path, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn diagnosis_distinguishes_compatible_unsafe_identity_and_cancellation() {
    use audiolib_core::compatibility::{ExecutionState, MigrationDisposition};
    let (_directory, path) = current_definition_fixture();
    let cancel = AtomicBool::new(false);
    let report = diagnose(&path, &cancel).unwrap();
    assert_eq!(
        report.decision.disposition,
        Some(MigrationDisposition::Compatible)
    );
    assert!(report.findings.is_empty());
    Connection::open(&path)
        .unwrap()
        .execute_batch("UPDATE audiolib_audio_id_sequence SET high_water=0;")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let report = diagnose(&path, &cancel).unwrap();
    assert_eq!(report.decision.disposition, None);
    assert_eq!(report.decision.execution_state, ExecutionState::Blocked);
    assert_eq!(report.findings[0].code, DiagnosticCode::UnsafeSchemaRepair);
    assert!(matches!(
        diagnose(&path, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn diagnosis_treats_unavailable_extension_functions_as_incomplete_evidence() {
    use rusqlite::functions::FunctionFlags;
    let (_directory, path) = current_definition_fixture();
    let conn = Connection::open(&path).unwrap();
    conn.create_scalar_function(
        "extension_only",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| ctx.get::<String>(0),
    )
    .unwrap();
    conn.execute_batch("CREATE TABLE extension_generated(value TEXT, computed TEXT GENERATED ALWAYS AS (extension_only(value)) VIRTUAL); INSERT INTO extension_generated(value) VALUES('preserve');").unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let report = diagnose(&path, &AtomicBool::new(false)).unwrap();
    assert_eq!(report.decision.disposition, None, "{report:?}");
    assert!(!report.automatic_rebuild_allowed);
    assert_eq!(
        report.findings[0].code,
        DiagnosticCode::InspectionUnavailable
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn diagnosis_allows_retiring_readable_orphan_history_and_preserves_it_for_rollback() {
    use audiolib_core::compatibility::MigrationDisposition;
    let (_directory, path) = fixture();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF; UPDATE play_history SET audio_file_id=321;")
        .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    let cancel = AtomicBool::new(false);
    let report = diagnose(&path, &cancel).unwrap();
    assert_eq!(
        report.decision.disposition,
        Some(MigrationDisposition::Migratable),
        "{report:?}"
    );
    assert!(report.findings.is_empty());
    let reviewed = plan(&path, &cancel).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
    verify(&job.directory, &cancel).unwrap();
    rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT audio_file_id FROM play_history", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        321
    );
    assert_eq!(
        conn.query_row("SELECT SUM(play_count) FROM audio_files", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        68
    );
}

#[test]
fn sqlite_like_extension_names_are_hashed_copied_and_restored() {
    let (_directory, path) = fixture();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE sqliteX_private(value BLOB);
        INSERT INTO sqliteX_private(rowid,value) VALUES(-321,x'00ff'),(99,NULL);
        CREATE TABLE SQLiteZ_identity(id INTEGER PRIMARY KEY AUTOINCREMENT,value TEXT);
        INSERT INTO SQLiteZ_identity VALUES(4,'retained');
        UPDATE sqlite_sequence SET seq=2000 WHERE name='SQLiteZ_identity';",
    )
    .unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let before = std::fs::read(&path).unwrap();
    let reviewed = plan(&path, &cancel).unwrap();
    for name in ["sqliteX_private", "SQLiteZ_identity"] {
        assert!(
            reviewed
                .source
                .business
                .as_ref()
                .unwrap()
                .tables
                .iter()
                .any(|table| table.table == name)
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
    verify(&job.directory, &cancel).unwrap();
    for restored in [false, true] {
        if restored {
            rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        let values = conn
            .prepare("SELECT rowid,value FROM sqliteX_private ORDER BY rowid")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(values, vec![(-321, Some(vec![0, 255])), (99, None)]);
        assert_eq!(
            conn.query_row("SELECT value FROM SQLiteZ_identity WHERE id=4", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "retained"
        );
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='SQLiteZ_identity'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2000
        );
    }
}

#[test]
fn diagnosis_repairs_fts_damage_and_can_restore_the_original_source() {
    use audiolib_core::compatibility::{ExecutionState, MigrationDisposition};
    for physical in [false, true] {
        let (_directory, path) = current_definition_fixture();
        let cancel = AtomicBool::new(false);
        if physical {
            corrupt_table_root(&path, "audio_search_data");
        } else {
            Connection::open(&path)
                .unwrap()
                .execute("DELETE FROM audio_search_docsize WHERE id=1", [])
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let report = diagnose(&path, &cancel).unwrap();
        assert_eq!(
            report.decision.disposition,
            Some(MigrationDisposition::Migratable),
            "{report:?}"
        );
        assert_eq!(report.decision.execution_state, ExecutionState::Ready);
        assert_eq!(report.findings[0].code, DiagnosticCode::DerivedDataInvalid);
        let reviewed = plan(&path, &cancel).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        let repaired = diagnose(&path, &cancel).unwrap();
        assert_eq!(
            repaired.decision.disposition,
            Some(MigrationDisposition::Compatible),
            "{repaired:?}"
        );
        assert_eq!(
            repaired.inspection.unwrap().business,
            reviewed.source.business
        );
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        let restored = diagnose(&path, &cancel).unwrap();
        assert_eq!(
            restored.findings[0].code,
            DiagnosticCode::DerivedDataInvalid
        );
    }
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let mut conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    schema::migrate(&mut conn).unwrap();
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN extension_value TEXT;
         CREATE TABLE extension_rows(key TEXT, payload BLOB);
         INSERT INTO extension_rows(rowid,key,payload) VALUES(-100,'payload',x'0001ff'),(99,NULL,NULL);
         CREATE TABLE extension_composite(a TEXT,b INTEGER,payload TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;
         INSERT INTO extension_composite VALUES('key',1,'retained');
         CREATE TABLE extension_identity(id INTEGER PRIMARY KEY AUTOINCREMENT,value TEXT);
         INSERT INTO extension_identity VALUES(11,'retained');
         UPDATE sqlite_sequence SET seq=1000000 WHERE name='extension_identity';",
    ).unwrap();
    for id in [i64::MIN, -1, 1, i64::MAX] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,user_notes,play_count,last_played_at,extension_value)
             VALUES(?1,?2,'metal.wav','user annotation',17,'2026-10-20',?3)",
            params![id,id.to_string(),format!("keep-{id}")],
        ).unwrap();
    }
    conn.execute("INSERT INTO play_history(audio_file_id,played_at,filename,file_path) VALUES(1,'2026-10-20','metal.wav','1')", []).unwrap();
    conn.execute_batch(
        "INSERT INTO playlists(id,name,description) VALUES(7,'retained playlist','keep description');
         INSERT INTO playlist_items(id,playlist_id,audio_file_id,position,notes)
           VALUES(17,7,1,4,'keep item');
         INSERT INTO project_references(id,audio_file_id,project_path,project_type,reference_count)
           VALUES(19,1,'owned/project.fixture','fixture',3);
         INSERT INTO waveform_cache(id,audio_file_id,zoom_level,samples_per_pixel,min_values,max_values)
           VALUES(23,1,'retained',64,x'0080ff',x'ff7f00');",
    ).unwrap();
    drop(conn);
    (directory, path)
}

fn identifier_case_fixture(mixed: bool) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let mut sql = include_str!("../../tests/fixtures/schema-v5.sql").replace("\r\n", "\n");
    let history = if mixed {
        "Play_History"
    } else {
        "PLAY_HISTORY"
    };
    let count = if mixed { "Play_Count" } else { "PLAY_COUNT" };
    sql = sql.replace("play_count INTEGER", &format!("{count} INTEGER"));
    let last = if mixed {
        "Last_Played_At"
    } else {
        "LAST_PLAYED_AT"
    };
    sql = sql.replace("last_played_at TEXT", &format!("{last} TEXT"));
    sql = sql.replace(
        "CREATE TABLE play_history (\n        id INTEGER PRIMARY KEY,",
        &format!("CREATE TABLE {history} (\n        id INTEGER PRIMARY KEY AUTOINCREMENT,"),
    );
    assert!(sql.contains(&format!("CREATE TABLE {history}")));
    for (column, mixed_name) in crate::search_storage::DERIVED_COLUMNS.iter().zip([
        "Filename_Tokenized",
        "User_Tags_Tokenized",
        "Bwf_Description_Tokenized",
        "Ixml_Scene_Tokenized",
        "Ixml_Take_Tokenized",
        "User_Notes_Tokenized",
    ]) {
        let alias = if mixed {
            mixed_name.to_owned()
        } else {
            column.to_ascii_uppercase()
        };
        sql = sql.replace(&format!("{column} TEXT"), &format!("{alias} TEXT"));
    }
    let conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    conn.execute_batch(&sql).unwrap();
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN play_count_note TEXT;
         ALTER TABLE audio_files ADD COLUMN last_played_at_note TEXT DEFAULT 'keep last note';
         ALTER TABLE audio_files ADD COLUMN filename_tokenized_note TEXT;
         INSERT INTO audio_files(id,file_path,filename,play_count,last_played_at,play_count_note,filename_tokenized_note)
           VALUES(1,'owned.wav','metal.wav',17,'2026-10-21T09:30:00Z','keep count note','keep token note');
         INSERT INTO play_history(id,audio_file_id,played_at,filename,file_path)
           VALUES(5,1,'2026-10-20','metal.wav','owned.wav');
         UPDATE sqlite_sequence SET seq=700 WHERE name='play_history' COLLATE NOCASE;
         CREATE TABLE play_history_archive(id INTEGER PRIMARY KEY AUTOINCREMENT,payload BLOB);
         INSERT INTO play_history_archive VALUES(44,x'0017ff');
         UPDATE sqlite_sequence SET seq=900 WHERE name='play_history_archive';",
    ).unwrap();
    drop(conn);
    (directory, path)
}

#[test]
fn identifier_case_variants_retire_exact_fields_and_restore_original_schema_and_values() {
    let cancel = AtomicBool::new(false);
    for mixed in [false, true] {
        let (_directory, path) = identifier_case_fixture(mixed);
        let conn = Connection::open(&path).unwrap();
        let original_schema = super::plan::schema_digest(&conn).unwrap();
        drop(conn);
        let reviewed = plan(&path, &cancel).unwrap();
        let original_business = reviewed.source.business.clone().unwrap();
        let audio = original_business
            .tables
            .iter()
            .find(|table| table.table == "audio_files")
            .unwrap();
        assert!(
            audio
                .columns
                .iter()
                .any(|column| column.eq_ignore_ascii_case("play_count"))
        );
        assert!(
            !audio
                .columns
                .iter()
                .any(|column| crate::search_storage::is_derived_column(column))
        );
        let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert!(schema::current_format_ready(&conn).unwrap());
        let columns = crate::search_storage::columns(&conn).unwrap();
        assert!(
            !columns
                .iter()
                .any(|column| column.eq_ignore_ascii_case("play_count")
                    || column.eq_ignore_ascii_case("last_played_at")
                    || crate::search_storage::is_derived_column(column))
        );
        assert!(!inspect::table_exists(&conn, "play_history").unwrap());
        assert_eq!(conn.query_row(
            "SELECT last_played_at_note,play_count_note,filename_tokenized_note FROM audio_files WHERE id=1", [],
            |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?)),
        ).unwrap(), ("keep last note".into(),"keep count note".into(),"keep token note".into()));
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_sequence WHERE name='play_history' COLLATE NOCASE",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='play_history_archive'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            900
        );
        assert_eq!(
            conn.query_row(
                "SELECT payload FROM play_history_archive WHERE id=44",
                [],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            vec![0, 23, 255]
        );
        drop(conn);
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(super::plan::schema_digest(&conn).unwrap(), original_schema);
        assert_eq!(digest_business(&conn, &cancel).unwrap(), original_business);
        assert_eq!(
            conn.query_row("SELECT play_count FROM audio_files WHERE id=1", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            17
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM play_history WHERE id=5", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='play_history' COLLATE NOCASE",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            700
        );
    }
}

#[test]
fn identifier_case_dependencies_block_without_modifying_source() {
    let cancel = AtomicBool::new(false);
    for dependency in [
        "CREATE VIEW extension_dependency AS SELECT PLAY_COUNT FROM audio_files;",
        "CREATE VIEW extension_dependency AS SELECT FILENAME_TOKENIZED FROM audio_files;",
        "CREATE VIEW extension_dependency AS SELECT LAST_PLAYED_AT FROM audio_files;",
        "CREATE VIEW extension_dependency AS SELECT * FROM play_history;",
        "CREATE TRIGGER extension_dependency AFTER UPDATE OF PLAY_COUNT ON audio_files BEGIN SELECT 1; END;",
        "CREATE TABLE extension_dependency(id INTEGER PRIMARY KEY,play_id INTEGER REFERENCES play_history(id)); INSERT INTO extension_dependency VALUES(1,5);",
    ] {
        let (_directory, path) = identifier_case_fixture(true);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(dependency).unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        assert!(plan(&path, &cancel).is_err(), "{dependency}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn schema6_identifier_case_aliases_require_owned_repair_and_rollback_preserves_aliases() {
    let cancel = AtomicBool::new(false);
    for mixed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.db");
        drop(crate::SqliteProvider::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        let count = if mixed { "Play_Count" } else { "PLAY_COUNT" };
        let history = if mixed {
            "Play_History"
        } else {
            "PLAY_HISTORY"
        };
        conn.execute_batch(&format!(
            "ALTER TABLE audio_files ADD COLUMN {count} INTEGER DEFAULT 0;
             ALTER TABLE audio_files ADD COLUMN Last_Played_At TEXT;
             ALTER TABLE audio_files ADD COLUMN last_played_at_note TEXT DEFAULT 'retained time note';
             ALTER TABLE audio_files ADD COLUMN Filename_Tokenized TEXT;
             ALTER TABLE audio_files ADD COLUMN play_count_note TEXT;
             CREATE TABLE {history}(id INTEGER PRIMARY KEY AUTOINCREMENT,audio_file_id INTEGER REFERENCES audio_files(id));
             INSERT INTO audio_files(id,file_path,filename,play_count,last_played_at,play_count_note,Filename_Tokenized)
               VALUES(1,'owned.wav','metal.wav',17,'2026-10-21','retained note','obsolete');
             INSERT INTO play_history VALUES(5,1);"
        )).unwrap();
        let original_schema = super::plan::schema_digest(&conn).unwrap();
        let original_business = digest_business(&conn, &cancel).unwrap();
        assert!(!schema::current_format_ready(&conn).unwrap());
        assert!(!crate::search_storage::structure_matches(&conn).unwrap());
        drop(conn);
        let inspection = inspect(
            &path,
            audiolib_core::compatibility::LibraryAccess::Write,
            true,
            &cancel,
        )
        .unwrap();
        assert!(!inspection.evidence.search_ready);
        assert_eq!(
            inspection.decision.disposition,
            Some(audiolib_core::compatibility::MigrationDisposition::Migratable)
        );
        let job = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert!(schema::current_format_ready(&conn).unwrap());
        assert!(!inspect::table_exists(&conn, "play_history").unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT last_played_at_note,play_count_note FROM audio_files WHERE id=1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
            ("retained time note".into(), "retained note".into())
        );
        drop(conn);
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(super::plan::schema_digest(&conn).unwrap(), original_schema);
        assert_eq!(digest_business(&conn, &cancel).unwrap(), original_business);
        assert_eq!(
            conn.query_row("SELECT play_count FROM audio_files WHERE id=1", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            17
        );
        assert_eq!(
            conn.query_row(
                "SELECT Filename_Tokenized FROM audio_files WHERE id=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "obsolete"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM play_history WHERE id=5", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
    }
}

#[test]
fn full_job_retires_only_authorized_objects_and_rollback_restores_history() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let plan = plan(&path, &cancel).unwrap();
    let before = plan.source.business.clone().unwrap();
    let job = apply(&plan, &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(job.phase, MigrationPhase::Complete);
    let conn = Connection::open(&path).unwrap();
    assert!(!inspect::table_exists(&conn, "play_history").unwrap());
    assert!(
        !crate::search_storage::columns(&conn)
            .unwrap()
            .contains(&"play_count".into())
    );
    assert!(
        !conn
            .column_exists(None, "audio_files", "last_played_at")
            .unwrap()
    );
    let retained: (i64, i64, i64, String, Vec<u8>) = conn
        .query_row(
            "SELECT pi.audio_file_id,pi.position,pr.reference_count,pi.notes,w.min_values
         FROM playlist_items pi JOIN playlists p ON p.id=pi.playlist_id
         JOIN project_references pr ON pr.audio_file_id=pi.audio_file_id
         JOIN waveform_cache w ON w.audio_file_id=pi.audio_file_id WHERE p.id=7",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        retained,
        (1, 4, 3, "keep item".into(), vec![0x00, 0x80, 0xff])
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM extension_rows WHERE rowid IN (-100,99)",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    drop(conn);
    verify(&job.directory, &cancel).unwrap();
    let restored = rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(restored.phase, MigrationPhase::RolledBack);
    assert_eq!(
        inspect(
            &path,
            audiolib_core::compatibility::LibraryAccess::Write,
            true,
            &cancel
        )
        .unwrap()
        .business,
        Some(before)
    );
    assert_eq!(
        resume(&job.directory, &cancel, &mut |_| Ok(()))
            .unwrap()
            .phase,
        MigrationPhase::RolledBack
    );
}

#[test]
fn interrupted_copy_blocks_legacy_writes_and_resumes_without_duplicates() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let plan = plan(&path, &cancel).unwrap();
    let mut saved = None;
    let result = apply(&plan, &cancel, &mut |event| {
        saved = Some(event.directory);
        if event.phase == MigrationPhase::Copying
            && event.table.as_deref() == Some("audio_files")
            && event.completed > 0
        {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    let conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    assert!(
        conn.execute("UPDATE audio_files SET user_notes='lost' WHERE id=1", [])
            .is_err()
    );
    drop(conn);
    let job = resume(&saved.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(job.phase, MigrationPhase::Complete);
    verify(&job.directory, &cancel).unwrap();
}

#[test]
fn committed_index_cursor_recovers_a_stale_outer_copy_phase_without_recopying() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let mut directory = None;
    let result = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |event| {
        directory = Some(event.directory);
        if event.phase == MigrationPhase::Indexing && event.completed > 0 {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    let mut job = job::load(&directory.unwrap()).unwrap();
    assert_eq!(job.phase, MigrationPhase::Indexing);
    let candidate = job::open_read(&job.artifact("candidate.db").unwrap()).unwrap();
    let (phase, completed): (String, i64) = candidate
        .query_row(
            "SELECT phase,completed FROM audiolib_migration_copy WHERE name=''",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((phase.as_str(), completed), ("index", 4));
    drop(candidate);
    // Crash window: the candidate committed index state but job.sqlite still
    // reports Copying. Candidate watermarks, not an outer label, decide replay.
    job.phase = MigrationPhase::Copying;
    job.save().unwrap();
    let mut recopied = false;
    let completed = resume(&job.directory, &cancel, &mut |event| {
        recopied |= event.phase == MigrationPhase::Copying && event.table.is_some();
        Ok(())
    })
    .unwrap();
    assert!(!recopied);
    assert_eq!(completed.phase, MigrationPhase::Complete);
    verify(&job.directory, &cancel).unwrap();
}

#[test]
fn changed_source_and_later_annotations_are_never_silently_replaced() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let old_plan = plan(&path, &cancel).unwrap();
    let conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    conn.execute("UPDATE audio_files SET user_notes='newer' WHERE id=1", [])
        .unwrap();
    drop(conn);
    assert!(apply(&old_plan, &cancel, &mut |_| Ok(())).is_err());
    let job = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    let conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    conn.execute(
        "UPDATE audio_files SET user_notes='after migration' WHERE id=1",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(rollback(&job.directory, &cancel, &mut |_| Ok(())).is_err());
}

#[test]
fn retired_field_dependency_blocks_planning_and_added_defaults_are_explicit() {
    let (_directory, path) = fixture();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE VIEW retained_stats AS SELECT play_count FROM audio_files;")
        .unwrap();
    assert!(plan(&path, &AtomicBool::new(false)).is_err());
    conn.execute_batch("DROP VIEW retained_stats;").unwrap();
    let mut changes = recipe::changes();
    changes.push(SchemaChange::AddColumn {
        table: "audio_files".into(),
        column: "future_label".into(),
        definition: "TEXT NOT NULL DEFAULT 'legacy'".into(),
    });
    let blueprint = crate::search_storage::blueprint_with(&conn, &changes).unwrap();
    let candidate = Connection::open_in_memory().unwrap();
    for (_, sql) in &blueprint.tables {
        crate::search_storage::execute_schema(&candidate, sql).unwrap();
    }
    candidate
        .execute(
            "INSERT INTO audio_files(id,file_path,filename) VALUES(1,'x','x')",
            [],
        )
        .unwrap();
    assert_eq!(
        candidate
            .query_row("SELECT future_label FROM audio_files", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "legacy"
    );
}

#[test]
fn update_of_removed_columns_blocks_before_a_job_and_keeps_unrelated_triggers() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let conn = Connection::open(&path).unwrap();
    for columns in [
        "play_count",
        "last_played_at",
        "\"LAST_PLAYED_AT\"",
        "\"PLAY_COUNT\"",
        "`filename_tokenized`",
        "[play_count],user_notes",
        "'play_count'",
        "user_notes,/* dependency */play_count",
    ] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER extension_dependency AFTER UPDATE OF {columns} ON audio_files
             BEGIN INSERT INTO extension_rows(key) VALUES('audit'); END;"
        ))
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = plan(&path, &cancel).unwrap_err().to_string();
        assert!(error.contains("extension_dependency"), "{columns}: {error}");
        assert!(error.contains("UPDATE OF"), "{columns}: {error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            !std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".audiolib-migrate-")
                })
        );
        conn.execute_batch("DROP TRIGGER extension_dependency;")
            .unwrap();
    }
    // Removed-column words in names, string literals and comments are not
    // dependencies. A similar retained identifier must not be treated as a prefix.
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN play_count_note TEXT;
         CREATE TRIGGER \"play_count AFTER UPDATE OF filename_tokenized\"
         AFTER UPDATE /* play_count */ OF play_count_note,extension_value ON audio_files
         BEGIN INSERT INTO extension_rows(key) VALUES('UPDATE OF play_count'); END;",
    )
    .unwrap();
    drop(conn);
    let reviewed = plan(&path, &cancel).unwrap();
    let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
    verify(&job.directory, &cancel).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE audio_files SET play_count_note='changed' WHERE id=1",
        [],
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM extension_rows WHERE key='UPDATE OF play_count'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
}

#[test]
fn resume_at_every_outer_boundary_and_after_publication_receipt() {
    for phase in [
        MigrationPhase::Backup,
        MigrationPhase::Guarding,
        MigrationPhase::Normalizing,
        MigrationPhase::Copying,
        MigrationPhase::Indexing,
        MigrationPhase::Validating,
        MigrationPhase::Compacting,
        MigrationPhase::Publishing,
        MigrationPhase::Checkpoint,
    ] {
        let (_directory, path) = fixture();
        let cancel = AtomicBool::new(false);
        let mut saved = None;
        let result = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |event| {
            saved = Some(event.directory);
            if event.phase == phase {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "{phase:?}: {result:?}"
        );
        let mut job = resume(&saved.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(job.phase, MigrationPhase::Complete);
        // Simulate committed publication with a manifest that still says Publishing.
        job.phase = MigrationPhase::Publishing;
        job.save().unwrap();
        assert_eq!(
            resume(&job.directory, &cancel, &mut |_| Ok(()))
                .unwrap()
                .phase,
            MigrationPhase::Complete
        );
        verify(&job.directory, &cancel).unwrap();
    }
}

#[test]
fn corrupted_backup_and_later_schema_change_block_rollback() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let job = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE INDEX extension_added_after_migration ON audio_files(extension_value);",
    )
    .unwrap();
    drop(conn);
    assert!(rollback(&job.directory, &cancel, &mut |_| Ok(())).is_err());
    let conn = Connection::open(job.artifact("backup.db").unwrap()).unwrap();
    conn.execute_batch("CREATE TABLE unexpected_change(value);")
        .unwrap();
    drop(conn);
    assert!(verify(&job.directory, &cancel).is_err());
}

#[test]
fn inconsistent_candidate_checkpoints_block_resume_before_copying_or_publishing() {
    for (phase, damage) in [
        (
            MigrationPhase::Copying,
            "DELETE FROM audiolib_migration_copy WHERE name=''",
        ),
        (
            MigrationPhase::Copying,
            "INSERT INTO audiolib_migration_copy VALUES('unknown','table',NULL,0,0)",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET completed=-1 WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET completed='invalid' WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET done=1 WHERE name='extension_rows'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET last_id=1 WHERE name='extension_composite'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audio_files SET id=-2 WHERE id=-1",
        ),
        (
            MigrationPhase::Indexing,
            "UPDATE audio_search_docsize SET id=-2 WHERE id=-1",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET completed=3 WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET last_id=-1 WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET done=2 WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET phase='unknown' WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Copying,
            "UPDATE audiolib_migration_copy SET completed=1 WHERE name=''",
        ),
        (
            MigrationPhase::Copying,
            "DELETE FROM audiolib_migration_copy WHERE name='extension_rows'",
        ),
        (
            MigrationPhase::Indexing,
            "UPDATE audiolib_migration_copy SET completed=3 WHERE name=''",
        ),
        (
            MigrationPhase::Indexing,
            "UPDATE audiolib_migration_copy SET last_id=-1 WHERE name=''",
        ),
        (
            MigrationPhase::Indexing,
            "UPDATE audiolib_migration_copy SET done=2 WHERE name=''",
        ),
        (
            MigrationPhase::Indexing,
            "UPDATE audiolib_migration_copy SET done=0 WHERE name='audio_files'",
        ),
        (
            MigrationPhase::Indexing,
            "DELETE FROM audio_search_docsize WHERE id=1",
        ),
    ] {
        let (_directory, path) = fixture();
        let cancel = AtomicBool::new(false);
        let reviewed = plan(&path, &cancel).unwrap();
        let interrupted = apply(&reviewed, &cancel, &mut |event| {
            if event.phase == phase
                && event.table.as_deref() == Some("audio_files")
                && event.completed > 0
            {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(matches!(interrupted, Err(Error::Cancelled)));
        let job = discover(&path).unwrap().unwrap();
        let candidate = job.directory.join("candidate.db");
        Connection::open(&candidate)
            .unwrap()
            .execute_batch(damage)
            .unwrap();
        let before = std::fs::read(&candidate).unwrap();
        let error = resume(&job.directory, &cancel, &mut |_| Ok(())).unwrap_err();
        assert!(
            matches!(&error, Error::Validation(message) if message.contains("checkpoint")),
            "{phase:?} {damage}: {error:?}"
        );
        assert_eq!(std::fs::read(&candidate).unwrap(), before, "{damage}");
        assert_eq!(
            inspect(
                &path,
                audiolib_core::compatibility::LibraryAccess::Read,
                true,
                &cancel
            )
            .unwrap()
            .business,
            reviewed.source.business
        );
        assert_eq!(discover(&path).unwrap().unwrap().phase, phase);
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(
            digest_business(&Connection::open(&path).unwrap(), &cancel).unwrap(),
            reviewed.source.business.unwrap()
        );
    }
}

fn current_definition_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN extension_blob BLOB;
         ALTER TABLE audio_files ADD COLUMN last_played_at_note TEXT;
         INSERT INTO audio_files(id,file_path,filename,user_notes,last_played_at_note,extension_blob)
         VALUES(1,'1.wav','metal.wav','preserved','2026-10-20',x'00ff'),
               (2,'2.wav','water.wav','preserved',NULL,NULL);
         UPDATE audiolib_audio_id_sequence SET high_water=900;
         CREATE TABLE extension_rows(key TEXT,payload BLOB);
         INSERT INTO extension_rows(rowid,key,payload) VALUES(33,'private',x'0001');
         CREATE INDEX extension_index ON extension_rows(key COLLATE NOCASE);
         PRAGMA journal_mode=DELETE;",
    )
    .unwrap();
    drop(conn);
    (directory, path)
}

#[test]
fn damaged_owned_definitions_repair_in_candidate_and_rollback_restores_original_schema() {
    for damage in [
        "DROP INDEX idx_audio_search_facets; CREATE INDEX idx_audio_search_facets ON audio_files(filename)",
        "DROP INDEX idx_audio_files_not_present; CREATE INDEX idx_audio_files_not_present ON audio_files(id) WHERE missing=0",
        "DROP INDEX idx_audio_visible_name; CREATE INDEX idx_audio_visible_name ON audio_files(filename COLLATE BINARY,id) WHERE missing=0",
        "DROP INDEX idx_audio_visible_tagged_folders; CREATE INDEX idx_audio_visible_tagged_folders ON audio_files(folder_id,id,user_tags,source_tags_text) WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>' ')",
        "DROP TRIGGER audiolib_audio_id_high_water; CREATE TRIGGER audiolib_audio_id_high_water AFTER INSERT ON audio_files BEGIN UPDATE audiolib_audio_id_sequence SET high_water=high_water WHERE singleton=1; END",
        "DROP INDEX idx_audio_visible_name",
    ] {
        let (_directory, path) = current_definition_fixture();
        let conn = Connection::open(&path).unwrap();
        assert!(schema::current_format_ready(&conn).unwrap());
        conn.execute_batch(damage).unwrap();
        assert!(!schema::current_format_ready(&conn).unwrap(), "{damage}");
        let original_schema = super::plan::schema_digest(&conn).unwrap();
        drop(conn);
        let original_bytes = std::fs::read(&path).unwrap();
        let cancel = AtomicBool::new(false);
        let plan = plan(&path, &cancel).unwrap();
        assert!(std::fs::read(&path).unwrap() == original_bytes);
        let business = plan.source.business.clone().unwrap();
        let job = apply(&plan, &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        let conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        assert!(schema::current_format_ready(&conn).unwrap(), "{damage}");
        assert_eq!(
            inspect::business_digest_matching(&conn, Some(&business), &cancel).unwrap(),
            business
        );
        assert_eq!(
            conn.query_row(
                "SELECT high_water FROM audiolib_audio_id_sequence",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            900
        );
        assert_eq!(
            conn.query_row(
                "SELECT payload FROM extension_rows WHERE rowid=33",
                [],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            vec![0, 1]
        );
        // Exercise the repaired trigger, then roll the probe back before migration rollback.
        conn.execute_batch("BEGIN; INSERT INTO audio_files(id,file_path,filename) VALUES(950,'950.wav','probe.wav');").unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT high_water FROM audiolib_audio_id_sequence",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            950
        );
        conn.execute_batch("ROLLBACK").unwrap();
        drop(conn);
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(super::plan::schema_digest(&conn).unwrap(), original_schema);
        assert_eq!(
            inspect::business_digest_matching(&conn, Some(&business), &cancel).unwrap(),
            business
        );
    }
}

#[test]
fn ambiguous_objects_and_lost_identity_evidence_block_planning_without_mutation() {
    for damage in [
        "DROP INDEX idx_audio_search_facets; CREATE INDEX idx_audio_search_facets ON extension_rows(key)",
        "DROP INDEX idx_audio_search_facets; CREATE TABLE idx_audio_search_facets(value TEXT); INSERT INTO idx_audio_search_facets VALUES('private')",
        "DROP TRIGGER audiolib_audio_id_high_water; DROP TABLE audiolib_audio_id_sequence",
        "UPDATE audiolib_audio_id_sequence SET high_water=0",
        "PRAGMA ignore_check_constraints=ON; INSERT INTO audiolib_audio_id_sequence VALUES(2,900)",
        "DROP TRIGGER audiolib_audio_id_high_water; DROP TABLE audiolib_audio_id_sequence; CREATE TABLE audiolib_audio_id_sequence(singleton,high_water); INSERT INTO audiolib_audio_id_sequence VALUES(1,'900')",
    ] {
        let (directory, path) = current_definition_fixture();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(damage).unwrap();
        assert!(!schema::current_format_ready(&conn).unwrap(), "{damage}");
        drop(conn);
        let original = std::fs::read(&path).unwrap();
        let names = || {
            let mut names = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = names();
        let error = plan(&path, &AtomicBool::new(false)).unwrap_err();
        assert!(
            matches!(&error, Error::Validation(message) if message.contains("Audio ID") || message.contains("Reserved schema object")),
            "{damage}: {error}"
        );
        assert!(std::fs::read(&path).unwrap() == original);
        assert_eq!(names(), before);
    }
}

#[test]
fn added_default_and_nullable_fields_use_the_generic_copy_executor() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let mut plan = plan(&path, &cancel).unwrap();
    plan.schema_changes.push(SchemaChange::AddColumn {
        table: "audio_files".into(),
        column: "future_label".into(),
        definition: "TEXT NOT NULL DEFAULT 'legacy'".into(),
    });
    plan.schema_changes.push(SchemaChange::AddColumn {
        table: "audio_files".into(),
        column: "future_optional".into(),
        definition: "INTEGER".into(),
    });
    // This synthetic compiled-step test calls only the private stage engine.
    // Public apply rejects edits to a plan's registered recipe.
    assert!(apply(&plan, &cancel, &mut |_| Ok(())).is_err());
    let owned = tempfile::tempdir_in(path.parent().unwrap()).unwrap();
    let mut job = MigrationJob {
        directory: owned.path().canonicalize().unwrap(),
        id: "test-add-fields".into(),
        plan,
        phase: MigrationPhase::Normalizing,
        backup_sha256: None,
        target_business: None,
        target_schema_sha256: None,
        import_identity: None,
    };
    let mut source = job::open_read(&path).unwrap();
    crate::management::snapshot_to_file_cancellable(
        &mut source,
        &job.artifact("backup.db").unwrap(),
        &cancel,
    )
    .unwrap();
    job.target_business = Some(stage::normalize(&job, &cancel, &mut |_| Ok(())).unwrap());
    let manifest = Connection::open(job.artifact("job.sqlite").unwrap()).unwrap();
    manifest.execute_batch("CREATE TABLE job(singleton INTEGER PRIMARY KEY,payload TEXT NOT NULL); INSERT INTO job VALUES(1,'{}');").unwrap();
    drop(manifest);
    stage::build(&mut job, &cancel, &mut |_| Ok(())).unwrap();
    stage::validate(&job, &cancel).unwrap();
    let target = Connection::open(job.artifact("candidate.db").unwrap()).unwrap();
    assert_eq!(target.query_row("SELECT COUNT(*) FROM audio_files WHERE future_label='legacy' AND future_optional IS NULL",[],|row|row.get::<_,i64>(0)).unwrap(),4);
}

#[test]
fn pending_job_is_reported_and_table_dependencies_are_not_silently_dropped() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let mut saved = None;
    let result = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |event| {
        saved = Some(event.directory);
        if event.phase == MigrationPhase::Normalizing {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    let observed = inspect(
        &path,
        audiolib_core::compatibility::LibraryAccess::Write,
        true,
        &cancel,
    )
    .unwrap();
    assert_eq!(
        observed.evidence.pending.unwrap().protocol,
        "search_storage"
    );
    assert!(!observed.decision.direct_use);
    rollback(&saved.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE dependent_history(value INTEGER REFERENCES play_history(id));",
    )
    .unwrap();
    assert!(plan(&path, &cancel).is_err());
}

#[test]
fn wal_publication_is_resumable_when_an_old_read_snapshot_blocks_checkpoint() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("PRAGMA journal_mode=WAL; BEGIN; SELECT COUNT(*) FROM audio_files;")
        .unwrap();
    let mut saved = None;
    let result = apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |event| {
        saved = Some(event.directory);
        Ok(())
    });
    assert!(matches!(result, Err(Error::Busy(_))), "{result:?}");
    // The old snapshot sees original history while the new snapshot is published.
    assert_eq!(
        reader
            .query_row("SELECT COUNT(*) FROM play_history", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(reader);
    let job = resume(&saved.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(job.phase, MigrationPhase::Complete);
    verify(&job.directory, &cancel).unwrap();
}

#[test]
fn cancel_before_backup_can_be_rolled_back_without_creating_a_backup() {
    let (_directory, path) = fixture();
    let cancel = AtomicBool::new(false);
    let mut saved = None;
    assert!(matches!(
        apply(&plan(&path, &cancel).unwrap(), &cancel, &mut |event| {
            saved = Some(event.directory);
            Err(Error::Cancelled)
        }),
        Err(Error::Cancelled)
    ));
    let restored = rollback(&saved.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(restored.phase, MigrationPhase::RolledBack);
    verify(&restored.directory, &cancel).unwrap();
}

#[test]
fn historical_schema_and_derived_revisions_normalize_only_in_owned_copies() {
    for version in [3, 4, 5] {
        let (_directory, path) = fixture();
        let cancel = AtomicBool::new(false);
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        conn.execute_batch(
            "DROP TRIGGER audiolib_audio_id_high_water;
             DROP TABLE audiolib_audio_id_sequence;
             DELETE FROM audiolib_schema_features WHERE feature='stable_audio_ids';
             UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';
             UPDATE library_meta SET value='1' WHERE key IN ('source_search_rev','source_tags_rev');",
        ).unwrap();
        drop(conn);
        let plan = plan(&path, &cancel).unwrap();
        assert!(plan.space.normalizes_legacy);
        let before = plan.source.business.clone();
        let job = apply(&plan, &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(
            inspect(
                &path,
                audiolib_core::compatibility::LibraryAccess::Write,
                true,
                &cancel
            )
            .unwrap()
            .business,
            before
        );
    }
}

#[test]
fn historical_interrupted_revision_three_protocol_is_resumed_in_owned_copy() {
    use crate::SnapshotUpgradeProgress;
    for stop in ["tokenizing", "indexing", "validating", "checkpoint"] {
        let (_directory, path) = fixture();
        let cancel = AtomicBool::new(false);
        let mut conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        conn.execute(
            "UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization'",
            [],
        )
        .unwrap();
        let result = schema::migrate_resumable(&mut conn, &path, &cancel, &mut |event| {
            let reached = match event {
                SnapshotUpgradeProgress::Tokenizing { completed, .. } => {
                    stop == "tokenizing" && completed > 0
                }
                SnapshotUpgradeProgress::Indexing { completed, .. } => {
                    stop == "indexing" && completed > 0
                }
                SnapshotUpgradeProgress::Validating => stop == "validating",
                SnapshotUpgradeProgress::Checkpoint => stop == "checkpoint",
                _ => false,
            };
            if reached {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "{stop}: {result:?}"
        );
        drop(conn);
        let plan = plan(&path, &cancel).unwrap();
        assert_eq!(
            plan.source.evidence.pending.as_ref().unwrap().protocol,
            "search_text"
        );
        assert_eq!(plan.source.format.features.get("tokenization"), Some(&4));
        let job = apply(&plan, &cancel, &mut |_| Ok(())).unwrap();
        verify(&job.directory, &cancel).unwrap();
        rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert!(
            inspect(
                &path,
                audiolib_core::compatibility::LibraryAccess::Write,
                true,
                &cancel
            )
            .unwrap()
            .evidence
            .pending
            .is_some()
        );
    }
}

#[test]
fn current_provider_creation_metadata_snapshot_patch_and_play_noop_share_the_target() {
    use audiolib_core::{
        BatchMetadataRequest, DataProvider, MetadataPatch, SearchRequest, TagEditMode,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("current.db");
    let provider = crate::SqliteProvider::open(&path).unwrap();
    assert_eq!(provider.library_info().unwrap().schema_version, 8);
    provider.writer().unwrap().execute(
        "INSERT INTO audio_files(id,file_path,filename,user_notes) VALUES(1,'one.wav','metal.wav','old annotation')",[],
    ).unwrap();
    provider
        .update_metadata(
            1,
            &MetadataPatch {
                tags: Some("木头 uniqueannotation".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let request = SearchRequest {
        query: "uniqueannotation".into(),
        ..Default::default()
    };
    assert_eq!(provider.search(&request).unwrap().total_count, 1);
    provider
        .update_metadata_batch(&BatchMetadataRequest {
            ids: vec![1],
            patch: MetadataPatch {
                notes: Some("batchchanged".into()),
                ..Default::default()
            },
            tag_mode: TagEditMode::Replace,
        })
        .unwrap();
    let mut patched = provider.get_audio(1).unwrap();
    patched.tags = "remotechanged".into();

    crate::SqliteProvider::apply_snapshot_records(&path, &[patched]).unwrap();

    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "remotechanged".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    assert_eq!(provider.search(&request).unwrap().total_count, 0);
    let dto = serde_json::to_value(provider.get_audio(1).unwrap()).unwrap();
    assert!(dto.get("play_count").is_none());
    assert!(dto["attributes"].get("last_played_at").is_none());

    assert!(matches!(provider.get_audio(99), Err(Error::NotFound(_))));
    provider.check_library().unwrap();
    assert!(crate::search_storage::structure_matches(&provider.writer().unwrap()).unwrap());
}

#[test]
fn current_provider_auto_migrates_old_library_but_honors_explicit_rollback() {
    use audiolib_core::DataProvider;
    let (_directory, path) = fixture();
    let legacy = crate::SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(legacy.get_audio(1).unwrap().notes, "user annotation");
    drop(legacy);
    let provider = crate::SqliteProvider::open_existing(&path).unwrap();

    assert_eq!(provider.library_info().unwrap().schema_version, 8);
    drop(provider);
    let job = discover(&path).unwrap().unwrap();
    assert_eq!(job.phase, MigrationPhase::Complete);
    rollback(&job.directory, &AtomicBool::new(false), &mut |_| Ok(())).unwrap();
    assert!(matches!(
        crate::SqliteProvider::open_existing(&path),
        Err(Error::Busy(_))
    ));
    let legacy = crate::SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(legacy.get_audio(1).unwrap().notes, "user annotation");
    drop(legacy);
    let requested = plan(&path, &AtomicBool::new(false)).unwrap();
    apply(&requested, &AtomicBool::new(false), &mut |_| Ok(())).unwrap();
    crate::SqliteProvider::open_existing(&path).unwrap();
}

#[test]
fn current_import_restore_maintenance_and_removal_preserve_unretired_business() {
    use audiolib_core::{DataProvider, MaintenanceAction, MetadataPatch, SearchRequest};
    let (directory, source) = fixture();
    let source_bytes = std::fs::read(&source).unwrap();
    let destination = directory.path().join("imported.db");
    crate::import_legacy(&source, &destination).unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
    let provider = crate::SqliteProvider::open_existing(&destination).unwrap();
    assert_eq!(provider.library_info().unwrap().schema_version, 8);

    assert_eq!(provider.get_audio(1).unwrap().notes, "user annotation");
    assert!(provider.check_library().unwrap().is_ok());
    let before = digest_business(&provider.writer().unwrap(), &AtomicBool::new(false)).unwrap();
    for action in [
        MaintenanceAction::Optimize,
        MaintenanceAction::RebuildSearch,
    ] {
        provider.maintain_library(action).unwrap();
        assert_eq!(
            digest_business(&provider.writer().unwrap(), &AtomicBool::new(false)).unwrap(),
            before
        );
        assert!(provider.check_library().unwrap().is_ok());
    }
    let backup = directory.path().join("backup.db");
    provider.backup_library(&backup).unwrap();
    let backup_bytes = std::fs::read(&backup).unwrap();
    provider
        .update_metadata(
            1,
            &MetadataPatch {
                notes: Some("laterannotation".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "laterannotation".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    provider.restore_library(&backup).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), backup_bytes);
    assert_eq!(provider.get_audio(1).unwrap().notes, "user annotation");
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "laterannotation".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
    assert_eq!(provider.remove_audio(&[1]).unwrap(), 1);
    assert!(matches!(provider.get_audio(1), Err(Error::NotFound(_))));
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        3
    );
    assert!(provider.check_library().unwrap().is_ok());
    assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
}

#[test]
fn discovery_rejects_unowned_registry_without_modifying_it() {
    let (_directory, path) = fixture();
    let registry_path = path.with_file_name("library.db.migration-job");
    let conn = Connection::open(&registry_path).unwrap();
    conn.execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('keep');")
        .unwrap();
    drop(conn);
    let before = std::fs::read(&registry_path).unwrap();
    assert!(discover(&path).is_err());
    assert_eq!(std::fs::read(&registry_path).unwrap(), before);
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        5
    );
}

#[test]
fn current_derived_repair_recomputes_tags_without_replaying_user_update_triggers() {
    use audiolib_core::{DataProvider, SearchRequest};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("stale.db");
    let provider = crate::SqliteProvider::open(&path).unwrap();
    provider.writer().unwrap().execute_batch(
        r#"INSERT INTO audio_files(id,file_path,filename,source_tags_text,metadata_json)
           VALUES(1,'source.wav','source.wav','stale','{"wav_metadata":{"fields":{"keywords":"freshkeyword"}}}');
           DELETE FROM library_meta WHERE key IN ('source_search_rev','source_tags_rev');
           CREATE TABLE custom_update_audit(value INTEGER);
           INSERT INTO custom_update_audit VALUES(0);
           CREATE TRIGGER custom_tag_update AFTER UPDATE OF source_tags_text ON audio_files BEGIN
             UPDATE custom_update_audit SET value=value+1;
           END;"#,
    ).unwrap();
    drop(provider);
    let cancel = AtomicBool::new(false);
    let reviewed = plan(&path, &cancel).unwrap();
    let job = apply(&reviewed, &cancel, &mut |_| Ok(())).unwrap();
    verify(&job.directory, &cancel).unwrap();
    let provider = crate::SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "freshkeyword".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    let conn = provider.writer().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT source_tags_text FROM audio_files WHERE id=1",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "freshkeyword"
    );
    assert_eq!(
        conn.query_row("SELECT value FROM custom_update_audit", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
