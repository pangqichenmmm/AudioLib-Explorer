// crates/audiolib-db/src/search_tracking.rs
//! Bounded changes for supported `audio_files` writes, using built-in SQL only.
//!
//! Normal foreign/older writers execute the same capture without registering a
//! function. Direct FTS writes that bypass audio_files and the administration
//! lease are not covered. Managed FTS-only rebuild/restore must drop/reset this
//! accelerator. Callers retain their opaque data_version observer fallback for
//! observable unknown commits; a sequence is never a SQLite commit counter.
use audiolib_core::{Error, Result};
use rusqlite::{Connection, params};
use std::{collections::BTreeMap, sync::LazyLock};

/// Maximum retained mutation-sequence window; older cache prefixes must recompute.
pub(crate) const HISTORY_LIMIT: u64 = 131_072;
const MAX_SEQUENCE: i64 = i64::MAX;
const STATE: &str = "audiolib_search_state";
const CHANGES: &str = "audiolib_search_changes";
const STATE_SQL: &str = "CREATE TABLE audiolib_search_state(
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 cookie TEXT NOT NULL CHECK(typeof(cookie)='text' AND length(cookie)=32 AND cookie NOT GLOB '*[^0-9a-f]*'),
 sequence INTEGER NOT NULL CHECK(typeof(sequence)='integer' AND sequence>=0),
 floor INTEGER NOT NULL CHECK(typeof(floor)='integer' AND floor>=0 AND floor<=sequence),
 schema_cookie INTEGER NOT NULL CHECK(typeof(schema_cookie)='integer' AND schema_cookie>=0),
 source_recipe TEXT NOT NULL,
 retained INTEGER NOT NULL CHECK(typeof(retained)='integer' AND retained>=0 AND retained<=262144))";
const CHANGES_SQL: &str = "CREATE TABLE audiolib_search_changes(
 id INTEGER PRIMARY KEY,
 sequence INTEGER NOT NULL CHECK(typeof(sequence)='integer' AND sequence>0))";

/// Identity of a complete tracked audio_files prefix in the caller's snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Epoch {
    /// Opaque incarnation; reset/reinstall and sequence overflow rotate it.
    pub cookie: String,
    /// Audio row mutation sequence, independent of SQLite commit counting.
    pub sequence: u64,
}

struct Definition {
    name: &'static str,
    kind: &'static str,
    table: &'static str,
    sql: String,
}

fn capture_body(ids: &[(&str, &str)]) -> String {
    let mut body = format!(
        "DELETE FROM {CHANGES} WHERE (SELECT sequence FROM {STATE} WHERE singleton=1)={MAX_SEQUENCE};
         UPDATE {STATE} SET
          cookie=CASE WHEN sequence={MAX_SEQUENCE} THEN lower(hex(randomblob(16))) ELSE cookie END,
          floor=CASE WHEN sequence={MAX_SEQUENCE} THEN 0 ELSE max(floor,sequence+1-{HISTORY_LIMIT}) END,
          retained=CASE WHEN sequence={MAX_SEQUENCE} THEN 0 ELSE retained END,
          sequence=CASE WHEN sequence={MAX_SEQUENCE} THEN 1 ELSE sequence+1 END WHERE singleton=1;
         DELETE FROM {CHANGES} WHERE sequence<=(SELECT floor FROM {STATE} WHERE singleton=1);
         UPDATE {STATE} SET retained=retained-changes() WHERE singleton=1;"
    );
    for (id, condition) in ids {
        body.push_str(&format!(
            "UPDATE {STATE} SET retained=retained+(NOT EXISTS(SELECT 1 FROM {CHANGES} WHERE id={id}))
             WHERE singleton=1 AND ({condition});
             INSERT INTO {CHANGES}(id,sequence) SELECT {id},sequence FROM {STATE}
             WHERE singleton=1 AND ({condition}) ON CONFLICT(id) DO UPDATE SET sequence=excluded.sequence;"
        ));
    }
    body
}

static DEFINITIONS: LazyLock<Vec<Definition>> = LazyLock::new(|| {
    let mut definitions = vec![
        Definition {
            name: STATE,
            kind: "table",
            table: STATE,
            sql: STATE_SQL.into(),
        },
        Definition {
            name: CHANGES,
            kind: "table",
            table: CHANGES,
            sql: CHANGES_SQL.into(),
        },
        Definition {
            name: "audiolib_search_changes_seq",
            kind: "index",
            table: CHANGES,
            sql: "CREATE INDEX audiolib_search_changes_seq ON audiolib_search_changes(sequence)"
                .into(),
        },
    ];
    for (name, event, ids) in [
        (
            "audiolib_search_capture_insert",
            "INSERT",
            vec![("new.id", "1")],
        ),
        (
            "audiolib_search_capture_update",
            "UPDATE",
            vec![("old.id", "old.id IS NOT new.id"), ("new.id", "1")],
        ),
        (
            "audiolib_search_capture_delete",
            "DELETE",
            vec![("old.id", "1")],
        ),
    ] {
        definitions.push(Definition {
            name,
            kind: "trigger",
            table: "audio_files",
            sql: format!(
                "CREATE TRIGGER {name} AFTER {event} ON audio_files BEGIN {} END",
                capture_body(&ids)
            ),
        });
    }
    definitions
});

fn db_error(error: rusqlite::Error) -> Error {
    Error::Database(error.to_string())
}

fn invalid() -> Error {
    Error::Validation(
        "Reserved search-tracking objects have an unsupported owner or definition".into(),
    )
}

fn sql_equal(left: &str, right: &str) -> bool {
    left.trim().trim_end_matches(';').trim() == right.trim().trim_end_matches(';').trim()
}

// No permissive SQL rewriting: the installer owns only its exact definitions.
fn objects(conn: &Connection) -> Result<Option<BTreeMap<String, String>>> {
    let mut found = BTreeMap::new();
    let mut query = conn.prepare_cached(
        "SELECT name,type,tbl_name,sql FROM sqlite_schema WHERE name COLLATE NOCASE IN
         ('audiolib_search_state','audiolib_search_changes','audiolib_search_changes_seq',
          'audiolib_search_capture_insert','audiolib_search_capture_update','audiolib_search_capture_delete')",
    ).map_err(db_error)?;
    let mut rows = query.query([]).map_err(db_error)?;
    while let Some(row) = rows.next().map_err(db_error)? {
        let (name, kind, table, sql): (String, String, String, Option<String>) = (
            row.get(0).map_err(db_error)?,
            row.get(1).map_err(db_error)?,
            row.get(2).map_err(db_error)?,
            row.get(3).map_err(db_error)?,
        );
        let Some(expected) = DEFINITIONS
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(&name))
        else {
            return Ok(None);
        };
        let Some(sql) = sql else { return Ok(None) };
        if kind != expected.kind
            || !table.eq_ignore_ascii_case(expected.table)
            || !sql_equal(&sql, &expected.sql)
            || found.insert(expected.name.into(), sql).is_some()
        {
            return Ok(None);
        }
    }
    // Dropping an owned table would also drop these extensions. Never adopt or
    // silently remove an unrelated index/trigger attached to our reserved tables.
    let extra: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE tbl_name IN
         ('audiolib_search_state','audiolib_search_changes') AND sql IS NOT NULL
         AND name NOT IN ('audiolib_search_state','audiolib_search_changes','audiolib_search_changes_seq'))",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    Ok((!extra).then_some(found))
}

fn source_recipe(conn: &Connection) -> Result<Option<String>> {
    let mut query = conn.prepare_cached(
        "SELECT name,type,tbl_name,rootpage,sql FROM sqlite_schema WHERE name IN
         ('audio_files','audio_search','audio_files_ai','audio_files_au','audio_files_ad') ORDER BY name",
    ).map_err(db_error)?;
    let values = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    if values.len() != 5 {
        return Ok(None);
    }
    for (name, kind, table, _, sql) in &values {
        let Some(sql) = sql else { return Ok(None) };
        if name == "audio_files" {
            if kind != "table" || sql.to_ascii_lowercase().contains("virtual table") {
                return Ok(None);
            }
        } else if name == "audio_search" {
            let normalized = sql
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();
            if kind != "table" || !normalized.contains("usingfts5(") {
                return Ok(None);
            }
        } else if kind != "trigger" || table != "audio_files" {
            return Ok(None);
        }
    }
    let keys: Vec<(String, String, i64)> = conn
        .prepare_cached("SELECT name,type,pk FROM pragma_table_xinfo('audio_files') WHERE pk<>0")
        .map_err(db_error)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)?;
    if !matches!(keys.as_slice(), [(name,kind,1)] if name == "id" && kind.eq_ignore_ascii_case("INTEGER"))
    {
        return Ok(None);
    }
    serde_json::to_string(&values)
        .map(Some)
        .map_err(|_| invalid())
}

struct State {
    epoch: Epoch,
    floor: u64,
    retained: u64,
}

fn read_state(conn: &Connection) -> Result<Option<State>> {
    let Some(found) = objects(conn)? else {
        return Ok(None);
    };
    if found.len() != DEFINITIONS.len() {
        return Ok(None);
    }
    let Some(recipe) = source_recipe(conn)? else {
        return Ok(None);
    };
    let schema: i64 = conn
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .map_err(db_error)?;
    let mut query = conn.prepare_cached("SELECT cookie,sequence,floor,schema_cookie,source_recipe,retained FROM audiolib_search_state").map_err(db_error)?;
    let mut rows = query.query([]).map_err(db_error)?;
    let Some(row) = rows.next().map_err(db_error)? else {
        return Ok(None);
    };
    let parsed: rusqlite::Result<(String, i64, i64, i64, String, i64)> = (|| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })();
    let Ok((cookie, sequence, floor, saved_schema, saved_recipe, retained)) = parsed else {
        return Ok(None);
    };
    if rows.next().map_err(db_error)?.is_some()
        || cookie.len() != 32
        || !cookie
            .bytes()
            .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))
        || sequence < 0
        || floor < 0
        || floor > sequence
        || !(0..=262144).contains(&retained)
        || schema != saved_schema
        || recipe != saved_recipe
    {
        return Ok(None);
    }
    Ok(Some(State {
        epoch: Epoch {
            cookie,
            sequence: sequence as u64,
        },
        floor: floor as u64,
        retained: retained as u64,
    }))
}

fn history_complete(conn: &Connection, state: &State) -> Result<bool> {
    let count: i64 = conn
        .query_row("SELECT count(*) FROM audiolib_search_changes", [], |row| {
            row.get(0)
        })
        .map_err(db_error)?;
    let invalid: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM audiolib_search_changes WHERE sequence<=?1)
         OR EXISTS(SELECT 1 FROM audiolib_search_changes WHERE sequence>?2)",
            params![state.floor as i64, state.epoch.sequence as i64],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(count >= 0 && count as u64 == state.retained && !invalid)
}

fn snapshot<T>(conn: &Connection, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction().map_err(db_error)?;
        let result = read(&tx)?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    } else {
        read(conn)
    }
}

/// Read an epoch without creating, repairing, or scanning the bounded journal.
/// DDL/recipe changes and partial/foreign objects return None. The read is pinned.
pub(crate) fn epoch(conn: &Connection) -> Result<Option<Epoch>> {
    snapshot(conn, |conn| Ok(read_state(conn)?.map(|state| state.epoch)))
}

/// Return sorted changed IDs only for a complete retained prefix ending at the
/// caller's current pinned epoch. Wrong incarnation, truncation, TTL gap or DDL
/// change returns None and requires the full observer/recompute fallback.
pub(crate) fn delta(conn: &Connection, from: &Epoch, to: &Epoch) -> Result<Option<Vec<i64>>> {
    snapshot(conn, |conn| {
        let Some(state) = read_state(conn)? else {
            return Ok(None);
        };
        if &state.epoch != to
            || from.cookie != to.cookie
            || from.sequence > to.sequence
            || from.sequence < state.floor
            || to.sequence - from.sequence > HISTORY_LIMIT
            || !history_complete(conn, &state)?
        {
            return Ok(None);
        }
        let mut query=conn.prepare_cached("SELECT id FROM audiolib_search_changes WHERE sequence>?1 AND sequence<=?2 ORDER BY id").map_err(db_error)?;
        query
            .query_map(params![from.sequence as i64, to.sequence as i64], |row| {
                row.get(0)
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map(Some)
            .map_err(db_error)
    })
}

fn savepoint(conn: &Connection, write: impl FnOnce() -> Result<()>) -> Result<()> {
    conn.execute_batch("SAVEPOINT audiolib_install_search_tracking")
        .map_err(db_error)?;
    let result = write().and_then(|()| {
        conn.execute_batch("RELEASE audiolib_install_search_tracking")
            .map_err(db_error)
    });
    if result.is_err() {
        let _=conn.execute_batch("ROLLBACK TO audiolib_install_search_tracking; RELEASE audiolib_install_search_tracking");
    }
    result
}

/// Install/reset only from an explicitly admitted business writer, after its last
/// schema change. Never changes DEFENSIVE/trusted_schema/trigger configuration.
/// Unrelated source formats are left untracked; foreign reserved objects refuse.
pub(crate) fn install(conn: &Connection) -> Result<()> {
    if conn.is_readonly("main").map_err(db_error)? {
        return Err(Error::Validation(
            "Search tracking installation requires a writer".into(),
        ));
    }
    savepoint(conn, || {
        let Some(found) = objects(conn)? else {
            return Err(invalid());
        };
        let Some(recipe) = source_recipe(conn)? else {
            return Ok(());
        };
        if let Some(state) = read_state(conn)?
            && history_complete(conn, &state)?
        {
            return Ok(());
        }
        for definition in DEFINITIONS.iter() {
            if !found.contains_key(definition.name) {
                conn.execute_batch(&definition.sql).map_err(db_error)?;
            }
        }
        let schema: i64 = conn
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .map_err(db_error)?;
        conn.execute("DELETE FROM audiolib_search_changes", [])
            .map_err(db_error)?;
        conn.execute("DELETE FROM audiolib_search_state", [])
            .map_err(db_error)?;
        conn.execute(
            "INSERT INTO audiolib_search_state VALUES(1,lower(hex(randomblob(16))),0,0,?1,?2,0)",
            params![schema, recipe],
        )
        .map_err(db_error)?;
        Ok(())
    })
}

/// Remove only exact owned capture/queue objects inside a managed rebuild or
/// owned snapshot operation. Foreign collisions/extensions are preserved/refused.
pub(crate) fn drop_owned(conn: &Connection) -> Result<()> {
    savepoint(conn, || {
        let Some(found) = objects(conn)? else {
            return Err(invalid());
        };
        for definition in DEFINITIONS.iter().rev() {
            if found.contains_key(definition.name) {
                conn.execute_batch(&format!("DROP {} {}", definition.kind, definition.name))
                    .map_err(db_error)?;
            }
        }
        Ok(())
    })
}
