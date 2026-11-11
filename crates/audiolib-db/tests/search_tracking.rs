// crates/audiolib-db/tests/search_tracking.rs
//! Native capture, ownership, snapshot and bounded-history regressions.
#[path = "../src/search_tracking.rs"]
mod search_tracking;
use rusqlite::{Connection, config::DbConfig};
use search_tracking::{Epoch, HISTORY_LIMIT, delta, drop_owned, epoch, install};

const SOURCE: &str = "
CREATE TABLE audio_files(id INTEGER PRIMARY KEY,body TEXT);
CREATE VIRTUAL TABLE audio_search USING fts5(body,content='',contentless_delete=1,columnsize=1);
CREATE TRIGGER audio_files_ai AFTER INSERT ON audio_files BEGIN
 INSERT INTO audio_search(rowid,body) VALUES(new.id,new.body); END;
CREATE TRIGGER audio_files_au AFTER UPDATE ON audio_files BEGIN
 DELETE FROM audio_search WHERE rowid=old.id;
 INSERT INTO audio_search(rowid,body) VALUES(new.id,new.body); END;
CREATE TRIGGER audio_files_ad AFTER DELETE ON audio_files BEGIN
 DELETE FROM audio_search WHERE rowid=old.id; END;";

fn source(conn: &Connection) {
    // Keep the native SQL recipe explicit; application functions are never registered.
    conn.execute_batch(SOURCE).unwrap();
}

fn tracked() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    source(&conn);
    install(&conn).unwrap();
    conn
}

const PROTOTYPE: &str = "
CREATE VIRTUAL TABLE audio_search USING fts5(body,content='',contentless_delete=1,columnsize=1);
CREATE TABLE capture(seq INTEGER PRIMARY KEY, id INTEGER NOT NULL, action TEXT NOT NULL);
CREATE TRIGGER capture_insert AFTER INSERT ON audio_search_docsize BEGIN
 INSERT INTO capture(id,action) VALUES(new.id,'insert'); END;
CREATE TRIGGER capture_update AFTER UPDATE ON audio_search_docsize BEGIN
 INSERT INTO capture(id,action) VALUES(old.id,'update-old');
 INSERT INTO capture(id,action) VALUES(new.id,'update-new'); END;
CREATE TRIGGER capture_delete AFTER DELETE ON audio_search_docsize BEGIN
 INSERT INTO capture(id,action) VALUES(old.id,'delete'); END;";

#[test]
fn bundled_shadow_triggers_capture_fts_dml_without_registered_functions() {
    assert_eq!(rusqlite::version(), "3.54.1");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prototype.db");
    let conn = Connection::open(&path).unwrap();
    assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
    conn.execute_batch(PROTOTYPE).unwrap();
    // A fresh foreign connection has no application functions or hooks.
    let foreign = Connection::open(&path).unwrap();
    foreign
        .execute_batch(
            "INSERT INTO audio_search(rowid,body) VALUES(7,'vehicle');
        UPDATE audio_search SET body='metal' WHERE rowid=7;
        DELETE FROM audio_search WHERE rowid=7;
        INSERT INTO audio_search(rowid,body) VALUES(8,'wind'),(9,'water');
        INSERT INTO audio_search(audio_search) VALUES('delete-all');",
        )
        .unwrap();
    let rows: Vec<(i64, String)> = conn
        .prepare("SELECT id,action FROM capture ORDER BY seq")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    println!("native shadow capture: {rows:?}");
    for id in [7, 8, 9] {
        assert!(rows.iter().any(|row| row.0 == id && row.1 == "insert"));
        assert!(rows.iter().any(|row| row.0 == id && row.1 == "delete"));
    }
}

#[test]
fn persistent_shadow_capture_is_rejected_as_a_defensive_compatibility_strategy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("defensive.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(PROTOTYPE).unwrap();
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .unwrap();
    conn.execute_batch("INSERT INTO audio_search(rowid,body) VALUES(10,'wind')")
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM capture WHERE id=10", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
    let foreign = Connection::open(&path).unwrap();
    foreign
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .unwrap();
    // This native limitation is why production installs only on audio_files.
    let error = foreign
        .execute_batch("INSERT INTO audio_search(rowid,body) VALUES(11,'water')")
        .unwrap_err();
    assert!(matches!(&error,rusqlite::Error::SqliteFailure(code,_) if code.extended_code==11));
    assert!(
        error
            .to_string()
            .contains("cannot create triggers on shadow tables")
    );
    assert!(
        foreign
            .db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
            .unwrap()
    );
}

#[test]
fn native_af_capture_works_for_foreign_writers_and_defensive_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tracked.db");
    let conn = Connection::open(&path).unwrap();
    source(&conn);
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .unwrap();
    install(&conn).unwrap();
    let from = epoch(&conn).unwrap().unwrap();
    let foreign = Connection::open(&path).unwrap();
    foreign
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .unwrap();
    foreign
        .execute_batch(
            "INSERT INTO audio_files VALUES(-9223372036854775808,'wind'),(1,'water');
      UPDATE audio_files SET body='vehicle' WHERE id=1;
      UPDATE audio_files SET id=2 WHERE id=1;
      DELETE FROM audio_files WHERE id=2;",
        )
        .unwrap();
    let to = epoch(&foreign).unwrap().unwrap();
    assert_eq!(to.sequence, 5);
    assert_eq!(
        delta(&conn, &from, &to).unwrap().unwrap(),
        vec![i64::MIN, 1, 2]
    );
    assert!(
        foreign
            .db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)
            .unwrap()
    );
    assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
    let same = epoch(&conn).unwrap().unwrap();
    install(&conn).unwrap();
    assert_eq!(
        epoch(&conn).unwrap().unwrap(),
        same,
        "idempotent installation must not discard a live prefix"
    );
    drop_owned(&conn).unwrap();
    assert!(epoch(&conn).unwrap().is_none());
    assert_eq!(
        conn.query_row("SELECT count(*) FROM audio_files", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn reads_are_pinned_and_rollback_does_not_publish_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.db");
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    source(&writer);
    install(&writer).unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let before = epoch(&reader).unwrap().unwrap();
    writer
        .execute_batch("BEGIN; INSERT INTO audio_files VALUES(1,'wind'); ROLLBACK")
        .unwrap();
    assert_eq!(epoch(&writer).unwrap().unwrap(), before);
    writer
        .execute_batch("INSERT INTO audio_files VALUES(2,'water')")
        .unwrap();
    assert_eq!(epoch(&reader).unwrap().unwrap(), before);
    assert_eq!(
        delta(&reader, &before, &before).unwrap().unwrap(),
        Vec::<i64>::new()
    );
    reader.execute_batch("COMMIT").unwrap();
    let after = epoch(&reader).unwrap().unwrap();
    assert_eq!(delta(&reader, &before, &after).unwrap().unwrap(), vec![2]);
    assert!(
        delta(&reader, &before, &before).unwrap().is_none(),
        "a historical to epoch is not the current pinned snapshot"
    );
}

#[test]
fn partial_ddl_foreign_collisions_and_changed_recipes_never_become_trusted() {
    let conn = tracked();
    let original = epoch(&conn).unwrap().unwrap();
    conn.execute_batch("DROP TRIGGER audiolib_search_capture_update")
        .unwrap();
    assert!(epoch(&conn).unwrap().is_none());
    install(&conn).unwrap();
    let repaired = epoch(&conn).unwrap().unwrap();
    assert_ne!(original.cookie, repaired.cookie);
    assert!(delta(&conn, &original, &repaired).unwrap().is_none());
    conn.execute_batch("DROP TRIGGER audiolib_search_capture_insert;
      CREATE TRIGGER audiolib_search_capture_insert AFTER INSERT ON audio_files BEGIN SELECT 1; END").unwrap();
    assert!(epoch(&conn).unwrap().is_none());
    assert!(install(&conn).is_err());
    assert!(drop_owned(&conn).is_err());
    assert!(conn.query_row("SELECT sql LIKE '%SELECT 1%' FROM sqlite_schema WHERE name='audiolib_search_capture_insert'",[],|r|r.get::<_,bool>(0)).unwrap());
    let conn = tracked();
    conn.execute_batch("CREATE INDEX foreign_extension ON audiolib_search_changes(id,sequence)")
        .unwrap();
    assert!(epoch(&conn).unwrap().is_none());
    assert!(
        drop_owned(&conn).is_err(),
        "must not silently drop a foreign extension with its parent table"
    );
    let conn = tracked();
    conn.execute_batch("DROP TABLE audio_search;
      CREATE VIRTUAL TABLE audio_search USING fts5(body,content='',contentless_delete=1,columnsize=1)").unwrap();
    assert!(epoch(&conn).unwrap().is_none());
    install(&conn).unwrap();
    assert!(epoch(&conn).unwrap().is_some());
}

#[test]
fn truncation_ttl_and_cookie_rollover_require_a_full_fallback() {
    let conn = tracked();
    let from = epoch(&conn).unwrap().unwrap();
    conn.execute_batch("INSERT INTO audio_files VALUES(1,'wind'),(2,'water')")
        .unwrap();
    let to = epoch(&conn).unwrap().unwrap();
    conn.execute("DELETE FROM audiolib_search_changes WHERE id=1", [])
        .unwrap();
    assert!(
        delta(&conn, &from, &to).unwrap().is_none(),
        "truncated history must not silently lose a changed ID"
    );
    install(&conn).unwrap();
    let reset = epoch(&conn).unwrap().unwrap();
    assert_ne!(from.cookie, reset.cookie);
    // Construct the valid compact state of many repeated updates to a single ID,
    // avoiding 131k redundant fixture transactions merely to cross the TTL edge.
    conn.execute_batch("UPDATE audio_files SET body='vehicle' WHERE id=1")
        .unwrap();
    conn.execute(
        "UPDATE audiolib_search_state SET sequence=?1",
        [HISTORY_LIMIT as i64],
    )
    .unwrap();
    conn.execute(
        "UPDATE audiolib_search_changes SET sequence=?1",
        [HISTORY_LIMIT as i64],
    )
    .unwrap();
    conn.execute_batch("INSERT INTO audio_files VALUES(3,'metal')")
        .unwrap();
    let expired = epoch(&conn).unwrap().unwrap();
    assert!(delta(&conn, &reset, &expired).unwrap().is_none());
    let retained = Epoch {
        cookie: expired.cookie.clone(),
        sequence: 1,
    };
    assert_eq!(
        delta(&conn, &retained, &expired).unwrap().unwrap(),
        vec![1, 3]
    );
    conn.execute(
        "UPDATE audiolib_search_state SET sequence=?1,floor=?2",
        rusqlite::params![i64::MAX, i64::MAX - HISTORY_LIMIT as i64],
    )
    .unwrap();
    conn.execute("UPDATE audiolib_search_changes SET sequence=?1", [i64::MAX])
        .unwrap();
    let maximum = epoch(&conn).unwrap().unwrap();
    conn.execute_batch("UPDATE audio_files SET body='new wind' WHERE id=1")
        .unwrap();
    let wrapped = epoch(&conn).unwrap().unwrap();
    assert_ne!(maximum.cookie, wrapped.cookie);
    assert_eq!(wrapped.sequence, 1);
    assert!(delta(&conn, &maximum, &wrapped).unwrap().is_none());
    assert_eq!(
        delta(
            &conn,
            &Epoch {
                cookie: wrapped.cookie.clone(),
                sequence: 0
            },
            &wrapped
        )
        .unwrap()
        .unwrap(),
        vec![1]
    );
}

#[test]
fn readonly_untracked_and_unknown_fts_paths_remain_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly.db");
    let writer = Connection::open(&path).unwrap();
    source(&writer);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(epoch(&readonly).unwrap().is_none());
    assert!(install(&readonly).is_err());
    install(&writer).unwrap();
    let before = epoch(&readonly).unwrap().unwrap();
    let data_version: i64 = readonly
        .pragma_query_value(None, "data_version", |r| r.get(0))
        .unwrap();
    writer
        .execute_batch(
            "INSERT INTO audio_search(rowid,body) VALUES(99,'unsupported direct FTS write')",
        )
        .unwrap();
    assert_eq!(
        epoch(&readonly).unwrap().unwrap(),
        before,
        "AF capture must not pretend to see bypass writes"
    );
    assert_ne!(
        readonly
            .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        data_version,
        "caller must retain its unknown-observer fallback"
    );
    assert_eq!(
        delta(&readonly, &before, &before).unwrap().unwrap(),
        Vec::<i64>::new()
    );
}
