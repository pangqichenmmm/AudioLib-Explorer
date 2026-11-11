// crates/audiolib-db/src/dir_search.rs
//! Directory-segment path search for `scope=all` and `scope=path`.
//!
//! `audio_dir_search` stores one FTS row per folder (rowid = `folders.id`). Only that
//! folder's own name is indexed, after NFC and fullwidth-to-halfwidth normalization.
//! Library roots (`parent_id IS NULL`) keep a row but store no name, so a query that
//! only matches the mount prefix does not hit every file. `audio_dir_runs` stores
//! maximal consecutive file-id ranges for each folder. A file path-matches a term when
//! its folder or any ancestor segment matches.
//!
//! Ranking for `scope=all` is three tiers: main-field bm25 (the s8 order), then mixed
//! matches by file id, then path-only matches by file id. `scope=path` omits the pure
//! main tier. When a term's directory set is empty and that term also misses the main
//! FTS index (`LIMIT 1`), the page is empty and the full main scan is not run.

use crate::db_error;
use crate::search_text::{compile_query, compile_query_columns, query_terms};
use audiolib_core::{Error, FileVisibility, Result, SearchScope};
use jieba_rs::Jieba;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const PATH_RAW: &str = "{path_tokens}";
const PATH_DERIVED: &str = "{path_tokens_tokenized}";
/// Directory-segment index layout. Cursors carry this value; a mismatch is stale.
pub(crate) const SEARCH_PATH_REV: i64 = 1;
/// At or below this many estimated files, only that folder subtree's runs are scanned.
const SUBTREE_FILE_LIMIT: i64 = 50_000;

pub(crate) const CREATE_DIR_FTS: &str = "
CREATE VIRTUAL TABLE audio_dir_search USING fts5(
 path_tokens,path_tokens_tokenized,
 content='',contentless_delete=1,detail=full,columnsize=1,
 tokenize='unicode61 remove_diacritics 2');";

pub(crate) const CREATE_RUNS: &str = "
CREATE TABLE audio_dir_runs(
 dir_id INTEGER NOT NULL,
 lo INTEGER NOT NULL,
 hi INTEGER NOT NULL,
 n INTEGER NOT NULL,
 PRIMARY KEY(dir_id, lo)
) WITHOUT ROWID;";

const INSERT_SEGMENTS: &str = "
INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized)
SELECT id,
 CASE WHEN parent_id IS NULL THEN '' ELSE t3_norm(COALESCE(folder_name,'')) END,
 CASE WHEN parent_id IS NULL THEN '' ELSE audiolib_tokenize(t3_norm(COALESCE(folder_name,''))) END
FROM folders;";

pub(crate) const INSERT_RUNS: &str = "
INSERT INTO audio_dir_runs
SELECT folder_id, min(id), max(id), count(*)
FROM (
 SELECT id, folder_id, sum(chg) OVER (ORDER BY id) AS g
 FROM (
  SELECT id, folder_id,
   CASE WHEN folder_id IS lag(folder_id) OVER (ORDER BY id) THEN 0 ELSE 1 END AS chg
  FROM audio_files
 )
)
WHERE folder_id IS NOT NULL
GROUP BY g;";

/// NFC, then map ideographic space and fullwidth ASCII into their halfwidth forms.
pub(crate) fn t3_norm(value: &str) -> String {
    let nfc = icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    nfc.normalize(value)
        .chars()
        .map(|character| match character as u32 {
            0x3000 => ' ',
            unit @ 0xFF01..=0xFF5E => char::from_u32(unit - 0xFEE0).unwrap_or(character),
            _ => character,
        })
        .collect()
}

pub(crate) fn register_t3(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "t3_norm",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value = context.get::<Option<String>>(0)?.unwrap_or_default();
            Ok(t3_norm(&value))
        },
    )
    .map_err(db_error)
}

/// A path set wider than half the library is ignored and the term stays on the main index.
pub(crate) fn path_set_too_wide(estimated_files: i64, total_files: i64) -> bool {
    total_files > 0 && estimated_files.saturating_mul(2) > total_files
}

/// Subtree runs are a worthwhile filter only while the estimate stays modest.
pub(crate) fn restrict_to_subtree(estimated_files: i64) -> bool {
    estimated_files <= SUBTREE_FILE_LIMIT
}

const DIRTY_TABLE: &str = "CREATE TABLE audiolib_dir_dirty(
 dir_id INTEGER PRIMARY KEY,segment INTEGER NOT NULL CHECK(segment IN (0,1)),
 files INTEGER NOT NULL CHECK(files IN (0,1)))";
// These triggers deliberately use only SQLite built-ins. Older schema-8 writers
// can keep scanning without knowing the optional directory-index implementation.
const DIRTY_TRIGGERS: &[(&str, &str, &str)] = &[
    ("audiolib_dir_folder_insert", "folders", "CREATE TRIGGER audiolib_dir_folder_insert AFTER INSERT ON folders BEGIN
     INSERT INTO audiolib_dir_dirty VALUES(new.id,1,1) ON CONFLICT(dir_id) DO UPDATE SET segment=1,files=1; END"),
    ("audiolib_dir_folder_delete", "folders", "CREATE TRIGGER audiolib_dir_folder_delete AFTER DELETE ON folders BEGIN
     INSERT INTO audiolib_dir_dirty VALUES(old.id,1,1) ON CONFLICT(dir_id) DO UPDATE SET segment=1,files=1; END"),
    ("audiolib_dir_folder_update", "folders", "CREATE TRIGGER audiolib_dir_folder_update AFTER UPDATE OF id,folder_name,parent_id ON folders
     WHEN old.id IS NOT new.id OR old.folder_name IS NOT new.folder_name OR old.parent_id IS NOT new.parent_id BEGIN
     INSERT INTO audiolib_dir_dirty VALUES(old.id,1,CASE WHEN old.id IS NOT new.id THEN 1 ELSE 0 END)
      ON CONFLICT(dir_id) DO UPDATE SET segment=1,files=max(files,excluded.files);
     INSERT INTO audiolib_dir_dirty VALUES(new.id,1,CASE WHEN old.id IS NOT new.id THEN 1 ELSE 0 END)
      ON CONFLICT(dir_id) DO UPDATE SET segment=1,files=max(files,excluded.files); END"),
    ("audiolib_dir_folder_stats", "folders", "CREATE TRIGGER audiolib_dir_folder_stats AFTER UPDATE OF total_file_count ON folders
     WHEN old.total_file_count IS NOT new.total_file_count BEGIN
     UPDATE library_meta SET value=CAST(min(CAST(value AS INTEGER),9223372036854775806)+1 AS TEXT)
      WHERE key='dir_index_gen'; END"),
    ("audiolib_dir_audio_insert", "audio_files", "CREATE TRIGGER audiolib_dir_audio_insert AFTER INSERT ON audio_files BEGIN
     INSERT INTO audiolib_dir_dirty SELECT dir_id,0,1 FROM audio_dir_runs WHERE new.id BETWEEN lo AND hi
      ON CONFLICT(dir_id) DO UPDATE SET files=1;
     INSERT INTO audiolib_dir_dirty SELECT new.folder_id,0,1 WHERE new.folder_id IS NOT NULL
      ON CONFLICT(dir_id) DO UPDATE SET files=1; END"),
    ("audiolib_dir_audio_delete", "audio_files", "CREATE TRIGGER audiolib_dir_audio_delete AFTER DELETE ON audio_files WHEN old.folder_id IS NOT NULL BEGIN
     INSERT INTO audiolib_dir_dirty VALUES(old.folder_id,0,1) ON CONFLICT(dir_id) DO UPDATE SET files=1; END"),
    ("audiolib_dir_audio_update", "audio_files", "CREATE TRIGGER audiolib_dir_audio_update AFTER UPDATE OF id,folder_id ON audio_files
     WHEN old.id IS NOT new.id OR old.folder_id IS NOT new.folder_id BEGIN
     INSERT INTO audiolib_dir_dirty SELECT dir_id,0,1 FROM audio_dir_runs WHERE new.id BETWEEN lo AND hi
      ON CONFLICT(dir_id) DO UPDATE SET files=1;
     INSERT INTO audiolib_dir_dirty SELECT old.folder_id,0,1 WHERE old.folder_id IS NOT NULL
      ON CONFLICT(dir_id) DO UPDATE SET files=1;
     INSERT INTO audiolib_dir_dirty SELECT new.folder_id,0,1 WHERE new.folder_id IS NOT NULL
      ON CONFLICT(dir_id) DO UPDATE SET files=1; END"),
];
const DIRTY_OBJECTS: &str = "'audiolib_dir_dirty','audiolib_dir_folder_insert','audiolib_dir_folder_delete',
 'audiolib_dir_folder_update','audiolib_dir_folder_stats','audiolib_dir_audio_insert','audiolib_dir_audio_delete','audiolib_dir_audio_update'";

fn tracking_present(conn: &Connection) -> Result<bool> {
    let present: bool = conn
        .query_row(
            &format!(
                "SELECT count(*)=8 AND sum(type='table')=1 AND sum(type='trigger')=7
        FROM sqlite_schema WHERE name IN ({DIRTY_OBJECTS})"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if !present {
        return Ok(false);
    }
    for (name, kind, table, sql) in std::iter::once((
        "audiolib_dir_dirty",
        "table",
        "audiolib_dir_dirty",
        DIRTY_TABLE,
    ))
    .chain(
        DIRTY_TRIGGERS
            .iter()
            .map(|(name, table, sql)| (*name, "trigger", *table, *sql)),
    ) {
        if !crate::schema::definition::matches_sql(conn, name, kind, table, sql)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn install_tracking(conn: &Connection) -> Result<()> {
    for (name, kind, table, sql) in std::iter::once((
        "audiolib_dir_dirty",
        "table",
        "audiolib_dir_dirty",
        DIRTY_TABLE,
    ))
    .chain(
        DIRTY_TRIGGERS
            .iter()
            .map(|(name, table, sql)| (*name, "trigger", *table, *sql)),
    ) {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                [name],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !exists {
            conn.execute_batch(sql).map_err(db_error)?;
        } else if !crate::schema::definition::matches_sql(conn, name, kind, table, sql)? {
            return Err(Error::Validation(format!(
                "Directory change tracker differs from its supported definition: {name}"
            )));
        }
    }
    Ok(())
}

/// Remove only the optional directory index's capture objects in the caller's transaction.
pub(crate) fn drop_tracking(conn: &Connection) -> Result<()> {
    // A reserved name alone does not prove ownership. Refuse collisions before
    // dropping any object, including during a failed migration's cancellation.
    for (name, kind, table, sql) in std::iter::once((
        "audiolib_dir_dirty",
        "table",
        "audiolib_dir_dirty",
        DIRTY_TABLE,
    ))
    .chain(
        DIRTY_TRIGGERS
            .iter()
            .map(|(name, table, sql)| (*name, "trigger", *table, *sql)),
    ) {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                [name],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if exists && !crate::schema::definition::matches_sql(conn, name, kind, table, sql)? {
            return Err(Error::Validation(format!(
                "Directory change tracker ownership cannot be established: {name}"
            )));
        }
    }
    for (name, _, _) in DIRTY_TRIGGERS {
        conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {name}"))
            .map_err(db_error)?;
    }
    conn.execute_batch("DROP TABLE IF EXISTS audiolib_dir_dirty")
        .map_err(db_error)
}

/// Replay changes made during a full build before its runs are regenerated.
/// The caller must publish the complete index and clear this queue in one transaction.
pub(crate) fn finish_tracking(conn: &Connection) -> Result<()> {
    install_tracking(conn)?;
    let changed = conn
        .prepare("SELECT dir_id FROM audiolib_dir_dirty WHERE segment=1 ORDER BY dir_id")
        .map_err(db_error)?
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    for id in changed {
        reindex_segment(conn, id)?;
    }
    conn.execute("DELETE FROM audiolib_dir_dirty", [])
        .map_err(db_error)?;
    Ok(())
}

fn index_published(conn: &Connection) -> Result<bool> {
    let tables: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='audio_dir_search')
             AND EXISTS(SELECT 1 FROM sqlite_schema WHERE name='audio_dir_runs')",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if !tables {
        return Ok(false);
    }
    Ok(meta_i64(conn, "search_path_rev")? == Some(SEARCH_PATH_REV))
}

pub(crate) fn ready(conn: &Connection) -> Result<bool> {
    if !index_published(conn)? {
        return Ok(false);
    }
    if tracking_present(conn)? {
        return conn
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM audiolib_dir_dirty)",
                [],
                |row| row.get(0),
            )
            .map_err(db_error);
    }
    // Historical schema-9 snapshots predate capture; old schema-8 binaries cannot
    // write them. Keep those read-only inputs usable, but require preparation of
    // untracked schema-8 indexes before claiming path-search readiness.
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    let any: bool = conn
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name IN ({DIRTY_OBJECTS}))"),
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(version == 9 && !any)
}

const CHECKPOINT_KEY: &str = "v9_dir_checkpoint";
const BACKUP_KEY: &str = "v9_dir_backup";

/// Resumable schema 9 path-index build (design §5.2), read without the writer.
pub(crate) struct UpgradeState {
    pub ready: bool,
    /// Last folder id indexed; `Some` while a build is under way.
    pub checkpoint: Option<i64>,
    pub done: u64,
    pub total: u64,
    pub files: u64,
    pub backup: Option<String>,
}

pub(crate) fn upgrade_state(conn: &Connection) -> Result<UpgradeState> {
    let ready = ready(conn)?;
    let checkpoint = meta_i64(conn, CHECKPOINT_KEY)?;
    let count = |sql: &str, arg: i64| -> Result<u64> {
        conn.query_row(sql, [arg], |row| row.get::<_, i64>(0))
            .map(|n| n.max(0) as u64)
            .map_err(db_error)
    };
    let total = count("SELECT count(*) FROM folders WHERE ?1=?1", 0)?;
    let done = match checkpoint {
        Some(last) => count("SELECT count(*) FROM folders WHERE id<=?1", last)?,
        None => 0,
    };
    let files = count("SELECT count(*) FROM audio_files WHERE ?1=?1", 0)?;
    let backup = conn
        .query_row(
            "SELECT value FROM library_meta WHERE key=?1",
            [BACKUP_KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    Ok(UpgradeState {
        ready,
        checkpoint,
        done,
        total,
        files,
        backup,
    })
}

fn in_tx<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let own = conn.is_autocommit();
    if own {
        conn.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
    }
    let result = body();
    if own {
        if result.is_ok() {
            conn.execute_batch("COMMIT").map_err(db_error)?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
}

/// Step 1: add the empty tables and the checkpoint (fast, one transaction).
pub(crate) fn upgrade_begin(conn: &Connection, backup: Option<&str>) -> Result<()> {
    in_tx(conn, || {
        conn.execute_batch(
            "DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs;",
        )
        .map_err(db_error)?;
        conn.execute_batch(CREATE_DIR_FTS).map_err(db_error)?;
        conn.execute_batch(CREATE_RUNS).map_err(db_error)?;
        install_tracking(conn)?;
        conn.execute("DELETE FROM audiolib_dir_dirty", [])
            .map_err(db_error)?;
        conn.execute("DELETE FROM library_meta WHERE key='search_path_rev'", [])
            .map_err(db_error)?;
        write_meta(conn, CHECKPOINT_KEY, "0")?;
        match backup {
            Some(path) => write_meta(conn, BACKUP_KEY, path),
            None => Ok(()),
        }
    })
}

/// Step 2: index up to `limit` folders after the checkpoint and move the checkpoint in
/// the same transaction. `false` once every folder is indexed.
pub(crate) fn upgrade_batch(conn: &Connection, limit: i64) -> Result<bool> {
    in_tx(conn, || {
        let last = meta_i64(conn, CHECKPOINT_KEY)?
            .ok_or_else(|| Error::Database("Path index upgrade was not started".into()))?;
        let next: Option<i64> = conn
            .query_row(
                "SELECT max(id) FROM (SELECT id FROM folders WHERE id>?1 ORDER BY id LIMIT ?2)",
                params![last, limit.max(1)],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let Some(next) = next else {
            return Ok(false);
        };
        conn.execute(
            "INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized)
             SELECT id,
              CASE WHEN parent_id IS NULL THEN '' ELSE t3_norm(COALESCE(folder_name,'')) END,
              CASE WHEN parent_id IS NULL THEN '' ELSE audiolib_tokenize(t3_norm(COALESCE(folder_name,''))) END
             FROM folders WHERE id>?1 AND id<=?2",
            params![last, next],
        )
        .map_err(db_error)?;
        write_meta(conn, CHECKPOINT_KEY, &next.to_string())?;
        Ok(true)
    })
}

/// Steps 3-4: runs, optimize, then publish `search_path_rev` (the switch).
pub(crate) fn upgrade_finish(conn: &Connection) -> Result<()> {
    in_tx(conn, || {
        finish_tracking(conn)?;
        conn.execute("DELETE FROM audio_dir_runs", [])
            .map_err(db_error)?;
        conn.execute_batch(INSERT_RUNS).map_err(db_error)?;
        conn.execute(
            "INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize')",
            [],
        )
        .map_err(db_error)?;
        write_meta(conn, "search_path_rev", &SEARCH_PATH_REV.to_string())?;
        write_snapshot_format(conn)?;
        let generation = meta_i64(conn, "dir_index_gen")?
            .unwrap_or(0)
            .saturating_add(1);
        write_meta(conn, "dir_index_gen", &generation.to_string())?;
        conn.execute("DELETE FROM library_meta WHERE key=?1", [CHECKPOINT_KEY])
            .map_err(db_error)?;
        Ok(())
    })
}

// Preparing a derived index does not upgrade the business schema. Only the
// dedicated schema-9 migration may publish user_version=9 and its required features.
fn write_snapshot_format(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    let extended = match crate::schema::feature_version(
        conn,
        audiolib_core::compatibility::AUDIO_SOURCES_FEATURE,
    )? {
        None => false,
        Some(1) => true,
        Some(_) => {
            return Err(Error::Validation(
                "Unsupported extended audio source revision".into(),
            ));
        }
    };
    let format = match (version, extended) {
        (8, false) => audiolib_core::compatibility::PLAN_B_FORMAT,
        (8, true) => audiolib_core::compatibility::PLAN_B_AUDIO_FORMAT,
        (9, false) => audiolib_core::compatibility::PLAN_B_V9_FORMAT,
        (9, true) => audiolib_core::compatibility::PLAN_B_V9_AUDIO_FORMAT,
        _ => {
            return Err(Error::Validation(
                "Directory index requires schema 8 or 9".into(),
            ));
        }
    };
    write_meta(conn, "snapshot_format", format)
}

/// Drop and rebuild both directory tables. Caller holds the library writer.
pub(crate) fn rebuild(conn: &Connection) -> Result<()> {
    let own = conn.is_autocommit();
    if own {
        conn.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
    }
    let result = (|| -> Result<()> {
        conn.execute_batch(
            "DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs;",
        )
        .map_err(db_error)?;
        conn.execute_batch(CREATE_DIR_FTS).map_err(db_error)?;
        conn.execute_batch(CREATE_RUNS).map_err(db_error)?;
        install_tracking(conn)?;
        conn.execute_batch(INSERT_SEGMENTS).map_err(db_error)?;
        conn.execute_batch(INSERT_RUNS).map_err(db_error)?;
        conn.execute("DELETE FROM audiolib_dir_dirty", [])
            .map_err(db_error)?;
        conn.execute(
            "INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize')",
            [],
        )
        .map_err(db_error)?;
        write_meta(conn, "search_path_rev", &SEARCH_PATH_REV.to_string())?;
        write_snapshot_format(conn)?;
        let generation = meta_i64(conn, "dir_index_gen")?
            .unwrap_or(0)
            .saturating_add(1);
        write_meta(conn, "dir_index_gen", &generation.to_string())?;
        conn.execute("DELETE FROM library_meta WHERE key=?1", [CHECKPOINT_KEY])
            .map_err(db_error)?;
        Ok(())
    })();
    if own {
        if result.is_ok() {
            conn.execute_batch("COMMIT").map_err(db_error)?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
    result
}

/// Reindex renamed or deleted folders and recompute runs whose files changed.
/// No-op until [`rebuild`] has published `search_path_rev`.
pub(crate) fn maintain(
    conn: &Connection,
    dirs_changed: &[i64],
    dirs_files_changed: &[i64],
) -> Result<()> {
    in_tx(conn, || {
        if !index_published(conn)? {
            // Mid-upgrade: keep already indexed segments current; runs are rebuilt at the end.
            if let Some(last) = meta_i64(conn, CHECKPOINT_KEY)? {
                for id in dirs_changed.iter().filter(|id| **id <= last) {
                    reindex_segment(conn, *id)?;
                }
            }
            return Ok(());
        }
        if !tracking_present(conn)? {
            // An index predating capture cannot prove that older writers kept it
            // current. Rebuild once while installing capture, never on every write.
            return rebuild(conn);
        }
        let mut segments = dirs_changed.iter().copied().collect::<HashSet<_>>();
        let mut files = dirs_files_changed.iter().copied().collect::<HashSet<_>>();
        let pending = conn
            .prepare("SELECT dir_id,segment,files FROM audiolib_dir_dirty")
            .map_err(db_error)?
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        for (id, segment, file) in pending {
            if segment {
                segments.insert(id);
            }
            if file {
                files.insert(id);
            }
        }
        if segments.is_empty() && files.is_empty() {
            return Ok(());
        }
        for id in segments {
            reindex_segment(conn, id)?;
        }
        conn.execute(
            "DELETE FROM audio_dir_runs WHERE dir_id NOT IN (SELECT id FROM folders)",
            [],
        )
        .map_err(db_error)?;
        for id in files {
            conn.execute("DELETE FROM audio_dir_runs WHERE dir_id=?1", [id])
                .map_err(db_error)?;
            conn.execute(
                "INSERT INTO audio_dir_runs
             SELECT folder_id, min(id), max(id), count(*)
             FROM (
              SELECT id, folder_id, sum(chg) OVER (ORDER BY id) AS g
              FROM (
               SELECT id, folder_id,
                CASE WHEN folder_id IS lag(folder_id) OVER (ORDER BY id) THEN 0 ELSE 1 END AS chg
               FROM audio_files
               WHERE id BETWEEN
                (SELECT coalesce(min(id), 0) FROM audio_files WHERE folder_id=?1)
                AND (SELECT coalesce(max(id), -1) FROM audio_files WHERE folder_id=?1)
              )
             )
             WHERE folder_id=?1
             GROUP BY g",
                [id],
            )
            .map_err(db_error)?;
        }
        let generation = meta_i64(conn, "dir_index_gen")?
            .unwrap_or(0)
            .saturating_add(1);
        conn.execute("DELETE FROM audiolib_dir_dirty", [])
            .map_err(db_error)?;
        write_meta(conn, "dir_index_gen", &generation.to_string())
    })
}

#[cfg(feature = "scanner")]
pub(crate) fn bump_generation(conn: &Connection) -> Result<()> {
    // Reparenting can enqueue a segment before this existing cache-invalidation
    // hook is called. Consume it incrementally rather than leaving a new scan dirty.
    maintain(conn, &[], &[])?;
    if !ready(conn)? {
        return Ok(());
    }
    let generation = meta_i64(conn, "dir_index_gen")?
        .unwrap_or(0)
        .saturating_add(1);
    write_meta(conn, "dir_index_gen", &generation.to_string())
}

#[derive(Clone, Copy)]
struct Run {
    lo: i64,
    hi: i64,
    count: i64,
    dense: bool,
    dir: i64,
}

struct RunIdx {
    all: Vec<Run>,
    by_dir: HashMap<i64, Vec<usize>>,
    parent: HashMap<i64, i64>,
    children: HashMap<i64, Vec<i64>>,
}

#[derive(Default)]
pub(crate) struct DirIndexCache {
    inner: Mutex<Option<(i64, Arc<RunIdx>)>>,
}

pub(crate) struct DirHit {
    pub tier: u8,
    pub score: f64,
    pub id: i64,
}

pub(crate) struct DirPage {
    pub hits: Vec<DirHit>,
    pub short_circuited: bool,
    pub has_more: bool,
    /// Hits actually observed, including skipped rows and the lookahead row.
    pub observed_count: usize,
}

#[derive(Clone, Copy)]
struct Cursor {
    tier: u8,
    score: f64,
    id: i64,
    rev: i64,
}

pub(crate) fn encode_cursor(tier: u8, score: f64, id: i64, rev: i64) -> String {
    let raw = format!("v1|{tier}|{}|{id}|{rev}", score.to_bits());
    base64url(raw.as_bytes())
}

fn decode_cursor(value: &str) -> Result<Cursor> {
    let bytes = base64url_decode(value)
        .map_err(|_| Error::Validation("Search cursor is invalid".into()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Validation("Search cursor is invalid".into()))?;
    let mut parts = text.split('|');
    let invalid = || Error::Validation("Search cursor is invalid".into());
    if parts.next() != Some("v1") {
        return Err(invalid());
    }
    let tier: u8 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(invalid)?;
    let bits: u64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(invalid)?;
    let id: i64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(invalid)?;
    let rev: i64 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(invalid)?;
    if parts.next().is_some() || tier > 2 {
        return Err(invalid());
    }
    Ok(Cursor {
        tier,
        score: f64::from_bits(bits),
        id,
        rev,
    })
}

/// Inputs for one directory-aware page. `offset` is ignored when `cursor` is set.
#[derive(Clone, Copy)]
pub(crate) struct DirSearch<'a> {
    pub conn: &'a Connection,
    pub cache: &'a DirIndexCache,
    pub jieba: &'a Jieba,
    pub query: &'a str,
    pub scope: SearchScope,
    pub visibility: FileVisibility,
    pub cursor: Option<&'a str>,
    pub offset: usize,
    pub limit: usize,
}

/// One page of directory-aware hits.
pub(crate) fn search(request: &DirSearch<'_>) -> Result<DirPage> {
    let DirSearch {
        conn,
        cache,
        jieba,
        query,
        scope,
        visibility,
        cursor,
        offset,
        limit,
    } = *request;
    let full_match = compile_query(jieba, query)?;
    let terms = query_terms(jieba, query);
    if terms.is_empty() {
        return Err(Error::Validation("Search requires a word or number".into()));
    }
    let cursor = cursor.map(decode_cursor).transpose()?;
    if let Some(cursor) = cursor
        && cursor.rev != SEARCH_PATH_REV
    {
        return Err(Error::Validation(format!(
            "CURSOR_STALE: search_path_rev {} != {SEARCH_PATH_REV}",
            cursor.rev
        )));
    }
    let index = load_index(conn, cache)?;
    let mut sets = Vec::with_capacity(terms.len());
    for term in &terms {
        let matched = compile_query_columns(jieba, &t3_norm(term), PATH_RAW, PATH_DERIVED)?;
        sets.push(segment_matches(conn, &matched)?);
    }
    // Unassigned files are library members too. Count in this pinned read snapshot,
    // independently of legacy folder statistics or the directory-index cache.
    let total_files: i64 = conn
        .query_row("SELECT COUNT(*) FROM audio_files", [], |row| row.get(0))
        .map_err(db_error)?;
    for set in &mut sets {
        if path_set_too_wide(estimate(set, &index), total_files) {
            set.clear();
        }
    }
    // A term with no directory hit can still be satisfied by the main index. One
    // posting is enough to prove that. Zero postings means every tier is empty,
    // so the full main AND and the nopath scan never start.
    for (index_term, term) in terms.iter().enumerate() {
        if !sets[index_term].is_empty() {
            continue;
        }
        let main = compile_query(jieba, term)?;
        if !main_has_hit(conn, &main)? {
            return Ok(DirPage {
                hits: Vec::new(),
                short_circuited: true,
                has_more: false,
                observed_count: 0,
            });
        }
    }

    let skip = if cursor.is_some() { 0 } else { offset };
    let want = skip.saturating_add(limit).saturating_add(1);
    let mut produced = Vec::new();
    if scope == SearchScope::All && cursor.map(|item| item.tier).unwrap_or(0) == 0 {
        let after = cursor
            .filter(|item| item.tier == 0)
            .map(|item| (item.score, item.id));
        produced.extend(tier0(conn, &full_match, after, want, visibility)?);
    }
    if produced.len() < want {
        let term_match = terms
            .iter()
            .map(|term| compile_query(jieba, term))
            .collect::<Result<Vec<_>>>()?;
        for tier in [1_u8, 2] {
            if scope == SearchScope::Path && tier == 1 {
                continue;
            }
            if produced.len() >= want {
                break;
            }
            if cursor.is_some_and(|item| item.tier > tier) {
                continue;
            }
            let after_id = cursor
                .filter(|item| item.tier == tier)
                .map(|item| item.id)
                .unwrap_or(i64::MIN);
            let ids = tier_ids(
                &TierQuery {
                    conn,
                    index: &index,
                    sets: &sets,
                    term_match: &term_match,
                    full_match: &full_match,
                    scope,
                    visibility,
                },
                tier,
                after_id,
                want - produced.len(),
            )?;
            let score = if tier == 1 { 1.0 } else { 2.0 };
            produced.extend(ids.into_iter().map(|id| DirHit { tier, score, id }));
        }
    }
    let observed_count = produced.len();
    let mut rest = produced.into_iter().skip(skip).collect::<Vec<_>>();
    let has_more = rest.len() > limit;
    rest.truncate(limit);
    Ok(DirPage {
        hits: rest,
        short_circuited: false,
        has_more,
        observed_count,
    })
}

fn tier0(
    conn: &Connection,
    full_match: &str,
    after: Option<(f64, i64)>,
    limit: usize,
    visibility: FileVisibility,
) -> Result<Vec<DirHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let visibility = visibility_sql(visibility);
    let (sql, owned_params): (String, Vec<rusqlite::types::Value>) =
        if let Some((score, id)) = after {
            (
                format!(
                    "SELECT s, id FROM (
                    SELECT rowid AS id, bm25(audio_search) AS s
                    FROM audio_search WHERE audio_search MATCH ?1
                 ) ranked
                 WHERE (s > ?2 OR (s = ?2 AND id > ?3))
                   AND EXISTS (
                    SELECT 1 FROM audio_files af WHERE af.id = ranked.id AND {visibility}
                   )
                 ORDER BY s, id LIMIT ?4"
                ),
                vec![
                    rusqlite::types::Value::Text(full_match.to_owned()),
                    rusqlite::types::Value::Real(score),
                    rusqlite::types::Value::Integer(id),
                    rusqlite::types::Value::Integer(limit as i64),
                ],
            )
        } else {
            (
                format!(
                    "SELECT s, id FROM (
                    SELECT rowid AS id, bm25(audio_search) AS s
                    FROM audio_search WHERE audio_search MATCH ?1
                 ) ranked
                 WHERE EXISTS (
                    SELECT 1 FROM audio_files af WHERE af.id = ranked.id AND {visibility}
                 )
                 ORDER BY s, id LIMIT ?2"
                ),
                vec![
                    rusqlite::types::Value::Text(full_match.to_owned()),
                    rusqlite::types::Value::Integer(limit as i64),
                ],
            )
        };
    let mut statement = conn.prepare_cached(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(owned_params), |row| {
            Ok(DirHit {
                tier: 0,
                score: row.get(0)?,
                id: row.get(1)?,
            })
        })
        .map_err(db_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(db_error)
}

#[derive(Clone, Copy)]
struct TierQuery<'a> {
    conn: &'a Connection,
    index: &'a RunIdx,
    sets: &'a [HashSet<i64>],
    term_match: &'a [String],
    full_match: &'a str,
    scope: SearchScope,
    visibility: FileVisibility,
}

fn tier_ids(query: &TierQuery<'_>, tier: u8, after_id: i64, limit: usize) -> Result<Vec<i64>> {
    let TierQuery {
        conn,
        index,
        sets,
        term_match,
        full_match,
        scope,
        visibility,
    } = *query;
    if limit == 0 {
        return Ok(Vec::new());
    }
    // Path is a strict directory-only AND; mixed sources belong to All only.
    if tier == 1 && scope == SearchScope::Path {
        return Ok(Vec::new());
    }
    if tier == 1 && (term_match.len() < 2 || sets.iter().all(HashSet::is_empty)) {
        return Ok(Vec::new());
    }
    if tier == 2 && sets.iter().any(HashSet::is_empty) {
        return Ok(Vec::new());
    }
    if tier == 2 {
        let Some(smallest) = smallest_set(sets, index) else {
            return Ok(Vec::new());
        };
        let runs = runs_for(index, &sets[smallest]);
        let mut main = MainPostings::new(full_match, after_id, i64::MAX);
        return stream_runs(
            conn,
            &runs,
            after_id,
            limit,
            visibility,
            |dir| path_full(sets, index, dir),
            // Path scope retains full path hits even when main also matches.
            // All scope excludes its earlier tier-0 hits with one advancing stream.
            |id| Ok(scope != SearchScope::All || !main.contains(conn, id)?),
        );
    }
    stream_mixed(query, after_id, limit)
}

/// A bounded rowid-ordered FTS stream. No posting list is materialized in full,
/// and each refill advances its lower bound instead of restarting MATCH per file.
struct MainPostings<'a> {
    expression: &'a str,
    after: i64,
    upper: i64,
    ids: Vec<i64>,
    position: usize,
    done: bool,
}

impl<'a> MainPostings<'a> {
    fn new(expression: &'a str, after: i64, upper: i64) -> Self {
        Self {
            expression,
            after,
            upper,
            ids: Vec::new(),
            position: 0,
            done: false,
        }
    }

    fn head(&mut self, conn: &Connection) -> Result<Option<i64>> {
        if self.position == self.ids.len() {
            if self.done {
                return Ok(None);
            }
            let mut statement = conn
                .prepare_cached(
                    "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1
                     AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT 256",
                )
                .map_err(db_error)?;
            self.ids = statement
                .query_map(params![self.expression, self.after, self.upper], |row| {
                    row.get(0)
                })
                .map_err(db_error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db_error)?;
            self.position = 0;
            self.done = self.ids.len() < 256;
            if let Some(last) = self.ids.last() {
                self.after = *last;
            }
        }
        Ok(self.ids.get(self.position).copied())
    }

    fn advance(&mut self) {
        self.position += 1;
    }

    /// Call with increasing IDs; skip untouched gaps before refilling a batch.
    fn contains(&mut self, conn: &Connection, id: i64) -> Result<bool> {
        self.position = self.ids.partition_point(|candidate| *candidate < id);
        if self.position == self.ids.len() {
            self.after = self.after.max(id.saturating_sub(1));
        }
        Ok(self.head(conn)? == Some(id))
    }
}

fn stream_mixed(query: &TierQuery<'_>, after_id: i64, limit: usize) -> Result<Vec<i64>> {
    let TierQuery {
        conn,
        index,
        sets,
        term_match,
        full_match,
        visibility,
        ..
    } = *query;
    let mut missing_by_dir = HashMap::new();
    let mut needed = vec![false; sets.len()];
    let mut low = i64::MAX;
    let mut high = i64::MIN;
    for (&dir, slots) in &index.by_dir {
        let missed = sets
            .iter()
            .enumerate()
            .filter_map(|(term, set)| (!ancestor_has(set, index, dir)).then_some(term))
            .collect::<Vec<_>>();
        // Pure main and complete path directories cannot contribute to tier 1.
        // In particular, a full-path page need not open the main FTS table at all.
        if missed.is_empty() || missed.len() == sets.len() {
            continue;
        }
        let mut has_remaining_run = false;
        for &slot in slots {
            let run = index.all[slot];
            if run.hi > after_id {
                low = low.min(run.lo);
                high = high.max(run.hi);
                has_remaining_run = true;
            }
        }
        if has_remaining_run {
            for &term in &missed {
                needed[term] = true;
            }
            missing_by_dir.insert(dir, missed);
        }
    }
    if missing_by_dir.is_empty() {
        return Ok(Vec::new());
    }
    let lower = after_id.max(low.saturating_sub(1));
    let mut streams = term_match
        .iter()
        .enumerate()
        .filter(|(term, _)| needed[*term])
        .map(|(term, expression)| (term, MainPostings::new(expression, lower, high)))
        .collect::<Vec<_>>();
    let mut main_full = MainPostings::new(full_match, lower, high);
    let mut main_terms = vec![false; sets.len()];
    let mut visible = conn
        .prepare_cached(&format!(
            "SELECT 1 FROM audio_files WHERE id=?1 AND folder_id=?2 AND {}",
            visibility_sql(visibility)
        ))
        .map_err(db_error)?;
    let mut out = Vec::new();
    while out.len() < limit {
        let mut next = None;
        for (_, stream) in &mut streams {
            if let Some(id) = stream.head(conn)? {
                next = Some(next.map_or(id, |previous: i64| previous.min(id)));
            }
        }
        let Some(id) = next else { break };
        main_terms.fill(false);
        for (term, stream) in &mut streams {
            if stream.head(conn)? == Some(id) {
                main_terms[*term] = true;
                stream.advance();
            }
        }
        let Some(dir) = dir_of(index, id) else {
            continue;
        };
        let Some(missed) = missing_by_dir.get(&dir) else {
            continue;
        };
        if missed.iter().all(|term| main_terms[*term])
            && !main_full.contains(conn, id)?
            && visible.exists(params![id, dir]).map_err(db_error)?
        {
            out.push(id);
        }
    }
    Ok(out)
}

fn stream_runs(
    conn: &Connection,
    runs: &[Run],
    after_id: i64,
    limit: usize,
    visibility: FileVisibility,
    mut accept_dir: impl FnMut(i64) -> bool,
    mut accept_id: impl FnMut(i64) -> Result<bool>,
) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    let mut run_index = runs.partition_point(|run| run.hi <= after_id);
    while run_index < runs.len() && out.len() < limit {
        let run = runs[run_index];
        run_index += 1;
        if !accept_dir(run.dir) {
            continue;
        }
        let mut from = after_id.saturating_add(1).max(run.lo);
        while from <= run.hi && out.len() < limit {
            let mut ids = Vec::new();
            expand(conn, &run, from, 256, visibility, &mut ids)?;
            let Some(last) = ids.last().copied() else {
                break;
            };
            for id in ids {
                if accept_id(id)? {
                    out.push(id);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            from = last.saturating_add(1);
        }
    }
    Ok(out)
}

fn expand(
    conn: &Connection,
    run: &Run,
    from: i64,
    limit: usize,
    visibility: FileVisibility,
    out: &mut Vec<i64>,
) -> Result<()> {
    let lo = run.lo.max(from);
    if lo > run.hi || limit == 0 {
        return Ok(());
    }
    if run.dense && visibility == FileVisibility::All {
        let mut id = lo;
        while id <= run.hi && out.len() < limit {
            out.push(id);
            id += 1;
        }
        return Ok(());
    }
    let sql = format!(
        "SELECT id FROM audio_files WHERE id>=?1 AND id<=?2 AND folder_id=?4 AND {} ORDER BY id LIMIT ?3",
        visibility_sql(visibility)
    );
    let mut statement = conn.prepare_cached(&sql).map_err(db_error)?;
    let rows = statement
        .query_map(params![lo, run.hi, limit as i64, run.dir], |row| row.get(0))
        .map_err(db_error)?;
    for id in rows {
        out.push(id.map_err(db_error)?);
    }
    Ok(())
}

fn visibility_sql(visibility: FileVisibility) -> &'static str {
    match visibility {
        FileVisibility::Present => "missing=0",
        FileVisibility::Missing => "missing=1",
        FileVisibility::All => "1=1",
    }
}

fn path_full(sets: &[HashSet<i64>], index: &RunIdx, dir: i64) -> bool {
    sets.iter().all(|set| ancestor_has(set, index, dir))
}

fn ancestor_has(set: &HashSet<i64>, index: &RunIdx, mut dir: i64) -> bool {
    for _ in 0..=index.parent.len() {
        if set.contains(&dir) {
            return true;
        }
        match index.parent.get(&dir) {
            Some(&parent) if parent != dir => dir = parent,
            _ => return false,
        }
    }
    false
}

fn dir_of(index: &RunIdx, id: i64) -> Option<i64> {
    let position = index.all.partition_point(|run| run.hi < id);
    index
        .all
        .get(position)
        .filter(|run| run.lo <= id)
        .map(|run| run.dir)
}

fn top_dirs(set: &HashSet<i64>, index: &RunIdx) -> Vec<i64> {
    set.iter()
        .copied()
        .filter(|&dir| {
            let mut cursor = dir;
            for _ in 0..=index.parent.len() {
                match index.parent.get(&cursor) {
                    Some(&parent) if parent != cursor => {
                        if set.contains(&parent) {
                            return false;
                        }
                        cursor = parent;
                    }
                    _ => return true,
                }
            }
            true
        })
        .collect()
}

fn subtree_dirs(set: &HashSet<i64>, index: &RunIdx) -> Vec<i64> {
    let mut out = Vec::new();
    let mut stack = top_dirs(set, index);
    while let Some(dir) = stack.pop() {
        out.push(dir);
        if let Some(children) = index.children.get(&dir) {
            stack.extend(children);
        }
    }
    out
}

fn subtree_files(index: &RunIdx, dir: i64) -> i64 {
    let mut count = 0;
    let mut stack = vec![dir];
    let mut seen = HashSet::new();
    while let Some(dir) = stack.pop() {
        if !seen.insert(dir) {
            continue;
        }
        if let Some(runs) = index.by_dir.get(&dir) {
            for &slot in runs {
                let run = index.all[slot];
                count += run.count;
            }
        }
        if let Some(children) = index.children.get(&dir) {
            stack.extend(children);
        }
    }
    count
}

fn estimate(set: &HashSet<i64>, index: &RunIdx) -> i64 {
    top_dirs(set, index)
        .into_iter()
        .map(|dir| subtree_files(index, dir))
        .sum()
}

fn smallest_set(sets: &[HashSet<i64>], index: &RunIdx) -> Option<usize> {
    sets.iter()
        .enumerate()
        .filter(|(_, set)| !set.is_empty())
        .min_by_key(|(_, set)| estimate(set, index))
        .map(|(slot, _)| slot)
}

fn runs_for(index: &RunIdx, set: &HashSet<i64>) -> Vec<Run> {
    if set.is_empty() {
        return Vec::new();
    }
    if !restrict_to_subtree(estimate(set, index)) {
        return index.all.clone();
    }
    let mut runs = Vec::new();
    for dir in subtree_dirs(set, index) {
        if let Some(slots) = index.by_dir.get(&dir) {
            for &slot in slots {
                runs.push(index.all[slot]);
            }
        }
    }
    runs.sort_unstable_by_key(|run| run.lo);
    runs
}

fn segment_matches(conn: &Connection, match_expr: &str) -> Result<HashSet<i64>> {
    let mut statement = conn
        .prepare_cached("SELECT rowid FROM audio_dir_search WHERE audio_dir_search MATCH ?1")
        .map_err(db_error)?;
    statement
        .query_map(params![match_expr], |row| row.get(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)
}

fn main_has_hit(conn: &Connection, match_expr: &str) -> Result<bool> {
    let mut statement = conn
        .prepare_cached("SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 LIMIT 1")
        .map_err(db_error)?;
    statement.exists(params![match_expr]).map_err(db_error)
}

fn load_index(conn: &Connection, cache: &DirIndexCache) -> Result<Arc<RunIdx>> {
    if !ready(conn)? {
        return Err(Error::Busy(
            "Directory path index needs preparation before search".into(),
        ));
    }
    let generation = meta_i64(conn, "dir_index_gen")?.unwrap_or(0);
    let mut guard = cache
        .inner
        .lock()
        .map_err(|_| Error::Database("Directory index cache lock was poisoned".into()))?;
    if let Some((cached, index)) = guard.as_ref()
        && *cached == generation
    {
        return Ok(Arc::clone(index));
    }
    let index = Arc::new(read_index(conn)?);
    *guard = Some((generation, Arc::clone(&index)));
    Ok(index)
}

fn read_index(conn: &Connection) -> Result<RunIdx> {
    let mut statement = conn
        .prepare("SELECT lo, hi, n, dir_id FROM audio_dir_runs ORDER BY lo")
        .map_err(db_error)?;
    let all = statement
        .query_map([], |row| {
            let (lo, hi, count): (i64, i64, i64) = (row.get(0)?, row.get(1)?, row.get(2)?);
            Ok(Run {
                lo,
                hi,
                count,
                dense: hi.checked_sub(lo).and_then(|span| span.checked_add(1)) == Some(count),
                dir: row.get(3)?,
            })
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let mut by_dir: HashMap<i64, Vec<usize>> = HashMap::new();
    for (slot, run) in all.iter().enumerate() {
        by_dir.entry(run.dir).or_default().push(slot);
    }
    let mut parent = HashMap::new();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut folders = conn
        .prepare("SELECT id, parent_id FROM folders")
        .map_err(db_error)?;
    let rows = folders
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .map_err(db_error)?;
    for row in rows {
        let (id, parent_id) = row.map_err(db_error)?;
        if let Some(parent_id) = parent_id {
            parent.insert(id, parent_id);
            children.entry(parent_id).or_default().push(id);
        }
    }
    Ok(RunIdx {
        all,
        by_dir,
        parent,
        children,
    })
}

pub(crate) fn reindex_segment(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM audio_dir_search WHERE rowid=?1", [id])
        .map_err(db_error)?;
    conn.execute(
        "INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized)
         SELECT id,
          CASE WHEN parent_id IS NULL THEN '' ELSE t3_norm(COALESCE(folder_name,'')) END,
          CASE WHEN parent_id IS NULL THEN '' ELSE audiolib_tokenize(t3_norm(COALESCE(folder_name,''))) END
         FROM folders WHERE id=?1",
        [id],
    )
    .map_err(db_error)?;
    Ok(())
}

fn meta_i64(conn: &Connection, key: &str) -> Result<Option<i64>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM library_meta WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    value
        .map(|text| {
            text.parse().map_err(|_| {
                Error::Database(format!("Directory index metadata {key} is not an integer"))
            })
        })
        .transpose()
}

fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO library_meta(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )
    .map_err(db_error)?;
    Ok(())
}

pub(crate) fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let block = u32::from(bytes[index]) << 16
            | u32::from(bytes[index + 1]) << 8
            | u32::from(bytes[index + 2]);
        out.push(TABLE[((block >> 18) & 63) as usize] as char);
        out.push(TABLE[((block >> 12) & 63) as usize] as char);
        out.push(TABLE[((block >> 6) & 63) as usize] as char);
        out.push(TABLE[(block & 63) as usize] as char);
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let block = u32::from(bytes[index]) << 16;
        out.push(TABLE[((block >> 18) & 63) as usize] as char);
        out.push(TABLE[((block >> 12) & 63) as usize] as char);
    } else if rest == 2 {
        let block = u32::from(bytes[index]) << 16 | u32::from(bytes[index + 1]) << 8;
        out.push(TABLE[((block >> 18) & 63) as usize] as char);
        out.push(TABLE[((block >> 12) & 63) as usize] as char);
        out.push(TABLE[((block >> 6) & 63) as usize] as char);
    }
    out
}

pub(crate) fn base64url_decode(value: &str) -> std::result::Result<Vec<u8>, ()> {
    fn value_of(byte: u8) -> std::result::Result<u8, ()> {
        Ok(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(()),
        })
    }
    let bytes = value.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(());
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let block = (u32::from(value_of(bytes[index])?) << 18)
            | (u32::from(value_of(bytes[index + 1])?) << 12)
            | (u32::from(value_of(bytes[index + 2])?) << 6)
            | u32::from(value_of(bytes[index + 3])?);
        out.push((block >> 16) as u8);
        out.push((block >> 8) as u8);
        out.push(block as u8);
        index += 4;
    }
    let rest = bytes.len() - index;
    if rest == 2 {
        let block = (u32::from(value_of(bytes[index])?) << 18)
            | (u32::from(value_of(bytes[index + 1])?) << 12);
        out.push((block >> 16) as u8);
    } else if rest == 3 {
        let block = (u32::from(value_of(bytes[index])?) << 18)
            | (u32::from(value_of(bytes[index + 1])?) << 12)
            | (u32::from(value_of(bytes[index + 2])?) << 6);
        out.push((block >> 16) as u8);
        out.push((block >> 8) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataProvider, SqliteProvider};
    use audiolib_core::SearchRequest;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn normalization_folds_fullwidth_and_keeps_halfwidth() {
        assert_eq!(t3_norm("Ｋｉｔ"), "Kit");
        assert_eq!(t3_norm("Kit"), "Kit");
        assert_eq!(t3_norm("Ａ　Ｂ"), "A B");
        assert!(path_set_too_wide(3, 4));
        assert!(!path_set_too_wide(1, 4));
        assert!(restrict_to_subtree(50_000));
        assert!(!restrict_to_subtree(50_001));
    }

    #[test]
    fn cursor_roundtrip_keeps_tier_score_and_rev() {
        let encoded = encode_cursor(1, 1.0, 42, SEARCH_PATH_REV);
        let cursor = decode_cursor(&encoded).unwrap();
        assert_eq!(cursor.tier, 1);
        assert_eq!(cursor.id, 42);
        assert_eq!(cursor.rev, SEARCH_PATH_REV);
        assert!(decode_cursor("not-a-cursor").is_err());
    }

    fn library() -> (tempfile::TempDir, SqliteProvider) {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name,parent_id,total_file_count) VALUES
                 (1,'/lib','LibraryRoot',NULL,5),
                 (2,'/lib/Kit','Kit',1,2),
                 (3,'/lib/Kit/Explosion','Explosion',2,1),
                 (4,'/lib/Other','Other',1,1),
                 (5,'/lib/Doors','Doors',1,1);
                 INSERT INTO folder_closure(ancestor_id,descendant_id,depth) VALUES
                 (1,1,0),(2,2,0),(3,3,0),(4,4,0),(5,5,0),(1,2,1),(1,3,2),(2,3,1),(1,4,1),(1,5,1);
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
                 (9,'/lib/explosion kit.wav','explosion kit.wav',1,0),
                 (10,'/lib/Kit/explosion.wav','explosion.wav',2,0),
                 (11,'/lib/Kit/Explosion/quiet.wav','quiet.wav',3,0),
                 (12,'/lib/Other/explosion_b.wav','explosion_b.wav',4,0),
                 (20,'/lib/kit explosion.wav','kit explosion.wav',1,0),
                 (30,'/lib/Doors/hinge.wav','hinge.wav',5,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        let format: String = provider
            .writer()
            .unwrap()
            .query_row(
                "SELECT value FROM library_meta WHERE key='snapshot_format'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(format, audiolib_core::compatibility::PLAN_B_FORMAT);
        (dir, provider)
    }

    #[test]
    fn publishing_a_directory_index_preserves_the_actual_library_format() {
        let (_dir, provider) = library();
        let path = provider.database_path().to_path_buf();
        let finish = |provider: &SqliteProvider| {
            let conn = provider.writer().unwrap();
            upgrade_begin(&conn, None).unwrap();
            while upgrade_batch(&conn, 2).unwrap() {}
            upgrade_finish(&conn).unwrap();
        };
        let check = |provider: &SqliteProvider, schema: i64, expected: &str| {
            let conn = provider.reader().unwrap();
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                schema
            );
            assert_eq!(
                conn.query_row(
                    "SELECT value FROM library_meta WHERE key='snapshot_format'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                expected
            );
        };
        finish(&provider);
        check(&provider, 8, audiolib_core::compatibility::PLAN_B_FORMAT);
        drop(provider);
        let local = SqliteProvider::open_existing(&path).unwrap();
        check(&local, 8, audiolib_core::compatibility::PLAN_B_FORMAT);
        assert_eq!(ids(&local, "Kit", SearchScope::Path), vec![10, 11]);
        drop(local);
        let report = crate::migration9::execute(
            &path,
            crate::migration9::Action::Start,
            &crate::migration9::Options::default(),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert_eq!(report.state, crate::migration9::State::Done, "{report:?}");
        let server = SqliteProvider::open_server_existing(
            &path,
            &std::sync::atomic::AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        server.rebuild_directory_search().unwrap();
        check(&server, 9, audiolib_core::compatibility::PLAN_B_V9_FORMAT);
        finish(&server);
        check(&server, 9, audiolib_core::compatibility::PLAN_B_V9_FORMAT);
        assert_eq!(ids(&server, "Kit", SearchScope::Path), vec![10, 11]);
    }

    #[test]
    fn directory_lookup_handles_ids_outside_and_between_runs() {
        let (_temp, provider) = library();
        let index = read_index(&provider.reader().unwrap()).unwrap();
        for id in [i64::MIN, 8, 13, 29, 31, i64::MAX] {
            assert_eq!(dir_of(&index, id), None, "{id}");
        }
        assert_eq!(dir_of(&index, 30), Some(5));
    }

    fn ids(provider: &SqliteProvider, query: &str, scope: SearchScope) -> Vec<i64> {
        provider
            .search(&SearchRequest {
                query: query.into(),
                scope,
                limit: 20,
                ..Default::default()
            })
            .unwrap()
            .results
            .into_iter()
            .map(|file| file.id)
            .collect()
    }

    #[test]
    fn layered_results_keep_s8_prefix_then_path_hits_by_id() {
        let (_dir, provider) = library();
        let main = ids(&provider, "explosion Kit", SearchScope::Main);
        let all = ids(&provider, "explosion Kit", SearchScope::All);
        assert!(!main.is_empty(), "main query should hit filenames");
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 11]);
        let path = ids(&provider, "Kit", SearchScope::Path);
        assert_eq!(path, vec![10, 11]);
        assert!(ids(&provider, "LibraryRoot", SearchScope::Path).is_empty());
        assert!(ids(&provider, "LibraryRoot", SearchScope::All).is_empty());
    }

    #[test]
    fn empty_path_term_without_main_hit_returns_empty_without_full_scan() {
        let (_dir, provider) = library();
        assert!(!ids(&provider, "explosion", SearchScope::All).is_empty());
        let conn = provider.reader().unwrap();
        let page = search(&DirSearch {
            conn: &conn,
            cache: &provider.dir_index,
            jieba: &provider.tokenizer,
            query: "zzznopath explosion",
            scope: SearchScope::All,
            visibility: FileVisibility::Present,
            cursor: None,
            offset: 0,
            limit: 20,
        })
        .unwrap();
        assert!(page.short_circuited);
        assert!(page.hits.is_empty());
        assert!(!page.has_more);
        let via_api = provider
            .search(&SearchRequest {
                query: "zzznopath explosion".into(),
                scope: SearchScope::All,
                ..Default::default()
            })
            .unwrap();
        assert!(via_api.results.is_empty());
        assert_eq!(via_api.total_count, 0);
        assert!(via_api.next_cursor.is_none());
    }

    #[test]
    fn keyset_cursor_pages_tiers_without_duplicates() {
        let (_dir, provider) = library();
        let full = ids(&provider, "explosion Kit", SearchScope::All);
        let mut seen = Vec::new();
        let mut cursor = None;
        for _ in 0..8 {
            let page = provider
                .search(&SearchRequest {
                    query: "explosion Kit".into(),
                    scope: SearchScope::All,
                    limit: 1,
                    cursor: cursor.clone(),
                    // A cursor replaces offset. The first page has no cursor.
                    offset: if cursor.is_some() { 50 } else { 0 },
                    ..Default::default()
                })
                .unwrap();
            if page.results.is_empty() {
                assert!(page.next_cursor.is_none());
                break;
            }
            assert_eq!(page.results.len(), 1);
            seen.push(page.results[0].id);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen, full);
        assert!(cursor.is_none());
    }

    #[test]
    fn stale_cursor_is_rejected() {
        let (_dir, provider) = library();
        let error = provider
            .search(&SearchRequest {
                query: "Kit".into(),
                scope: SearchScope::Path,
                cursor: Some(encode_cursor(2, 2.0, 1, SEARCH_PATH_REV + 1)),
                ..Default::default()
            })
            .unwrap_err();
        assert!(error.to_string().contains("CURSOR_STALE"), "{error}");
    }

    #[test]
    fn wide_directory_set_is_dropped_and_rename_updates_one_segment() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
                 (1,'/lib','LibraryRoot',NULL),
                 (2,'/lib/Wide','Wide',1),
                 (3,'/lib/Other','Other',1);
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
                 (1,'/lib/Wide/a.wav','a.wav',2,0),
                 (2,'/lib/Wide/b.wav','b.wav',2,0),
                 (3,'/lib/Wide/c.wav','c.wav',2,0),
                 (4,'/lib/Other/d.wav','d.wav',3,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        assert!(ids(&provider, "Wide", SearchScope::Path).is_empty());

        let (_dir, provider) = library();
        {
            let writer = provider.writer().unwrap();
            writer
                .execute("UPDATE folders SET folder_name='Metalbox' WHERE id=2", [])
                .unwrap();
            maintain(&writer, &[2], &[]).unwrap();
        }
        // A name that still begins with Kit would stay a letter-fragment hit.
        assert!(ids(&provider, "Kit", SearchScope::Path).is_empty());
        assert_eq!(ids(&provider, "Metalbox", SearchScope::Path), vec![10, 11]);
    }

    #[test]
    fn main_scope_ignores_directory_names() {
        let (_dir, provider) = library();
        assert!(ids(&provider, "Doors", SearchScope::Main).is_empty());
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30]);
        let main = ids(&provider, "explosion", SearchScope::Main);
        let all = ids(&provider, "explosion", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert!(all.contains(&11));
        assert!(!main.contains(&11));
    }

    fn legacy_connection(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        crate::register_library_functions(&conn).unwrap();
        // Schema-8 main indexing knows these existing functions, but not t3_norm.
        conn.remove_function("t3_norm", 1).unwrap();
        conn
    }

    #[test]
    fn older_scans_invalidate_readiness_and_pending_members_are_consumed_incrementally() {
        let (_dir, provider) = library();
        let path = provider.database_path().to_path_buf();
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30]);
        drop(provider);
        let legacy = legacy_connection(&path);
        legacy.execute_batch("BEGIN;
            INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES(6,'/lib/LegacyWind','LegacyWind',1);
            INSERT INTO folder_closure VALUES(6,6,0),(1,6,1);
            INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(31,'/lib/LegacyWind/quiet.wav','quiet.wav',6,0);
            COMMIT;").unwrap();
        let provider = SqliteProvider::open_existing(&path).unwrap();
        assert!(!provider.path_search_ready().unwrap());
        assert!(provider.local_upgrade_plan_impl().unwrap().is_some());
        provider
            .run_local_upgrade_impl(&std::sync::atomic::AtomicBool::new(false), &mut |_| {})
            .unwrap();
        assert!(provider.path_search_ready().unwrap());
        assert_eq!(ids(&provider, "LegacyWind", SearchScope::Path), vec![31]);
        provider
            .update_metadata(
                9,
                &audiolib_core::MetadataPatch {
                    notes: Some("annotation only".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            provider.path_search_ready().unwrap(),
            "annotations must not dirty directory membership"
        );
        legacy
            .execute_batch(
                "BEGIN;
            UPDATE audio_files SET folder_id=4 WHERE id=31;
            DELETE FROM audio_files WHERE id=10;
            UPDATE folders SET folder_name='GentleWind',parent_id=4 WHERE id=6;
            COMMIT;",
            )
            .unwrap();
        assert!(!provider.path_search_ready().unwrap());
        assert!(
            load_index(&provider.reader().unwrap(), &provider.dir_index).is_err(),
            "a warm cache must not bypass pending changes"
        );
        {
            let conn = provider.writer().unwrap();
            conn.execute_batch("CREATE TRIGGER preserve_unaffected_runs BEFORE DELETE ON audio_dir_runs WHEN old.dir_id=1 BEGIN SELECT RAISE(ABORT,'unaffected directory'); END;
                INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(32,'/lib/Doors/latch.wav','latch.wav',5,0);").unwrap();
            maintain(&conn, &[], &[5]).unwrap();
            assert!(ready(&conn).unwrap());
            assert_eq!(
                conn.query_row("SELECT count(*) FROM audiolib_dir_dirty", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert!(
                conn.execute("DELETE FROM audio_dir_runs WHERE dir_id=1", [])
                    .is_err(),
                "incremental upkeep must preserve unrelated index extensions"
            );
        }
        assert_eq!(ids(&provider, "Other", SearchScope::Path), vec![12, 31]);
        assert_eq!(ids(&provider, "Kit", SearchScope::Path), vec![11]);
        assert!(ids(&provider, "GentleWind", SearchScope::Path).is_empty());
        legacy
            .execute("DELETE FROM folders WHERE id=6", [])
            .unwrap();
        {
            let conn = provider.writer().unwrap();
            maintain(&conn, &[], &[]).unwrap();
            assert!(ready(&conn).unwrap());
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM audio_dir_search_docsize WHERE id=6",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
        }
    }

    #[test]
    fn sparse_runs_and_wide_sets_use_actual_members_including_unassigned_files() {
        let (_dir, provider) = library();
        let legacy = legacy_connection(provider.database_path());
        legacy.execute("INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(40,'/lib/Doors/latch.wav','latch.wav',5,0)", []).unwrap();
        maintain(&provider.writer().unwrap(), &[], &[]).unwrap();
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30, 40]);
        legacy.execute("INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(35,'/unassigned.wav','unassigned.wav',NULL,0)", []).unwrap();
        assert!(!provider.path_search_ready().unwrap());
        maintain(&provider.writer().unwrap(), &[], &[]).unwrap();
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30, 40]);
        provider.rebuild_directory_search().unwrap();
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30, 40]);
        legacy
            .execute("UPDATE folders SET total_file_count=6 WHERE id=5", [])
            .unwrap();
        assert_eq!(
            ids(&provider, "Doors", SearchScope::Path),
            vec![30, 40],
            "stale legacy folder statistics must not replace actual run membership"
        );
        legacy.execute_batch("BEGIN; UPDATE audio_files SET missing=1 WHERE id=40; UPDATE folders SET total_file_count=1 WHERE id=5; COMMIT;").unwrap();
        assert!(
            provider.path_search_ready().unwrap(),
            "statistics need cache refresh, not an index rebuild"
        );
        assert_eq!(ids(&provider, "Doors", SearchScope::Path), vec![30]);
        for id in 41..=44 {
            legacy.execute("INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(?1,?2,'latch.wav',5,0)",
                params![id,format!("/lib/Doors/latch-{id}.wav")]).unwrap();
        }
        maintain(&provider.writer().unwrap(), &[], &[]).unwrap();
        assert_eq!(
            ids(&provider, "Doors", SearchScope::Path),
            vec![30, 41, 42, 43, 44],
            "six path members of twelve total (including an unassigned row) are exactly half, not too wide"
        );
        legacy.execute("INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(45,'/lib/Doors/latch-45.wav','latch.wav',5,0)",[]).unwrap();
        maintain(&provider.writer().unwrap(), &[], &[]).unwrap();
        assert!(
            ids(&provider, "Doors", SearchScope::Path).is_empty(),
            "seven path members of thirteen total must still obey the >50% wide-set policy"
        );
    }

    #[test]
    fn untracked_local_indexes_and_forged_capture_objects_are_not_ready() {
        let (_dir, provider) = library();
        {
            let conn = provider.writer().unwrap();
            drop_tracking(&conn).unwrap();
            assert!(
                !ready(&conn).unwrap(),
                "an untracked schema-8 index may be stale"
            );
        }
        provider.rebuild_directory_search().unwrap();
        let conn = provider.writer().unwrap();
        conn.execute_batch("DROP TRIGGER audiolib_dir_audio_insert;
            CREATE TRIGGER audiolib_dir_audio_insert AFTER INSERT ON audio_files BEGIN SELECT 1; END;").unwrap();
        assert!(!ready(&conn).unwrap());
        assert!(
            drop_tracking(&conn).is_err(),
            "a name collision is not ownership"
        );
        assert!(install_tracking(&conn).is_err());
    }

    #[test]
    fn resumable_upgrade_cancels_between_batches_and_resumes_without_a_second_backup() {
        use audiolib_core::local_upgrade::{LocalUpgradeOutcome, LocalUpgradePhase};
        use std::sync::atomic::{AtomicBool, Ordering};
        let (dir, provider) = library();
        provider
            .writer()
            .unwrap()
            .execute("DELETE FROM library_meta WHERE key='search_path_rev'", [])
            .unwrap();
        assert!(!provider.path_search_ready().unwrap());
        let plan = provider.local_upgrade_plan_impl().unwrap().expect("plan");
        assert!(!plan.resuming);
        assert!(plan.total > 0);
        let backups = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains(".pre-schema9-")
                })
                .count()
        };
        let cancel = AtomicBool::new(false);
        let outcome = provider
            .run_local_upgrade_impl(&cancel, &mut |progress| {
                if matches!(progress.phase, LocalUpgradePhase::PathBuild) {
                    cancel.store(true, Ordering::Release);
                }
            })
            .unwrap();
        assert_eq!(outcome, LocalUpgradeOutcome::Cancelled);
        assert!(!provider.path_search_ready().unwrap());
        assert_eq!(backups(), 1);
        let plan = provider
            .local_upgrade_plan_impl()
            .unwrap()
            .expect("resume plan");
        assert!(plan.resuming, "the checkpoint survives a cancel");
        let again = AtomicBool::new(false);
        let outcome = provider
            .run_local_upgrade_impl(&again, &mut |_| {})
            .unwrap();
        assert!(
            matches!(outcome, LocalUpgradeOutcome::Done { backup: Some(ref path) } if path.exists())
        );
        assert_eq!(backups(), 1, "a resume never backs up again");
        assert!(provider.path_search_ready().unwrap());
        assert!(provider.local_upgrade_plan_impl().unwrap().is_none());
        assert_eq!(ids(&provider, "Kit", SearchScope::Path), vec![10, 11]);
    }
    fn overlapping_library() -> (tempfile::TempDir, SqliteProvider) {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name,parent_id,total_file_count) VALUES
                 (1,'/lib','LibraryRoot',NULL,12),
                 (2,'/lib/Borg','Borg',1,5),
                 (3,'/lib/Borg/Sound','Sound',2,3),
                 (4,'/lib/Other','Other',1,6),
                 (5,'/lib/Sound','Sound',1,1);
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
                 (2,'/lib/Borg/Sound/borg sound.wav','borg sound.wav',3,0),
                 (4,'/lib/Borg/Sound/quiet.wav','quiet.wav',3,1),
                 (8,'/lib/Borg/Sound/quiet2.wav','quiet2.wav',3,0),
                 (10,'/lib/Borg/sound.wav','sound.wav',2,0),
                 (12,'/lib/Borg/borg sound.wav','borg sound.wav',2,0),
                 (14,'/lib/Sound/borg.wav','borg.wav',5,0);
                 WITH RECURSIVE n(id) AS (VALUES(20) UNION ALL SELECT id+1 FROM n WHERE id<25)
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
                 SELECT id,'/lib/Other/'||id||'.wav','quiet.wav',4,0 FROM n;",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        (dir, provider)
    }

    #[test]
    fn overlapping_terms_keep_native_offset_and_cursor_counts_observed() {
        let (_dir, provider) = overlapping_library();
        let main = ids(&provider, "Borg Sound", SearchScope::Main);
        assert_eq!(main, vec![2, 12]);
        let all = ids(&provider, "Borg Sound", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14, 8]);
        // Full path hits remain in path scope even when main also matches (2).
        // Mixed hits belong to All; Path retains only the full directory AND.
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);

        for scope in [SearchScope::Path, SearchScope::All] {
            let full = ids(&provider, "Borg Sound", scope);
            let mut cursor = None;
            let mut paged = Vec::new();
            for (page_index, expected) in full.chunks(2).enumerate() {
                let request = SearchRequest {
                    query: "Borg Sound".into(),
                    scope,
                    limit: 2,
                    ..Default::default()
                };
                let offset_page = provider
                    .search_scoped(
                        &SearchRequest {
                            offset: page_index * 2,
                            ..request.clone()
                        },
                        None,
                    )
                    .unwrap();
                let cursor_page = provider
                    .search_scoped(
                        &SearchRequest {
                            offset: if cursor.is_some() { 999 } else { 0 },
                            cursor: cursor.clone(),
                            ..request
                        },
                        None,
                    )
                    .unwrap();
                let returned = cursor_page
                    .results
                    .iter()
                    .map(|file| file.id)
                    .collect::<Vec<_>>();
                assert_eq!(returned, expected);
                assert_eq!(
                    offset_page
                        .results
                        .iter()
                        .map(|file| file.id)
                        .collect::<Vec<_>>(),
                    returned
                );
                let remaining = full.len() - page_index * 2;
                assert_eq!(cursor_page.total_count, remaining.min(3) as u64);
                assert_eq!(
                    offset_page.total_count,
                    (page_index * 2 + remaining.min(3)) as u64
                );
                paged.extend(returned);
                cursor = cursor_page.next_cursor;
                assert_eq!(cursor.is_some(), remaining > 2);
            }
            assert_eq!(paged, full);
            let beyond_end = provider
                .search_scoped(
                    &SearchRequest {
                        query: "Borg Sound".into(),
                        scope,
                        offset: 999,
                        limit: 2,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            assert!(beyond_end.results.is_empty());
            assert_eq!(beyond_end.total_count, 0);
            assert!(!beyond_end.total_count_exact);
            assert!(beyond_end.next_cursor.is_none());
        }

        for (visibility, expected) in [
            (FileVisibility::Missing, vec![4]),
            (FileVisibility::All, vec![2, 4, 8]),
        ] {
            let mut request = SearchRequest {
                query: "Borg Sound".into(),
                scope: SearchScope::Path,
                ..Default::default()
            };
            request.filters.visibility = visibility;
            let page = provider.search(&request).unwrap();
            assert_eq!(
                page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                expected
            );
        }
    }

    fn add_stream_refill_rows(provider: &SqliteProvider) {
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "WITH RECURSIVE n(id) AS (VALUES(100) UNION ALL SELECT id+1 FROM n WHERE id<699)
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
                 SELECT id,'/lib/Borg/'||id||'.wav',
                        CASE WHEN id%2=0 THEN 'borg sound.wav' ELSE 'sound.wav' END,2,0 FROM n;
                 WITH RECURSIVE n(id) AS (VALUES(2000) UNION ALL SELECT id+1 FROM n WHERE id<2599)
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
                 SELECT id,'/lib/Other/'||id||'.wav','quiet.wav',4,0 FROM n;",
            )
            .unwrap();
        // Keep the real Borg membership below half of the whole library. Do not
        // substitute stale folder statistics for C's actual-member wide-set rule.
        provider.rebuild_directory_search().unwrap();
    }

    #[test]
    fn all_mixed_stream_requires_missing_terms_while_path_stays_directory_only() {
        let (_dir, provider) = overlapping_library();
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);
        let main = ids(&provider, "Borg Sound", SearchScope::Main);
        let all = ids(&provider, "Borg Sound", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14, 8]);
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "UPDATE audio_files SET filename='sound indoor.wav' WHERE id=10;
                 UPDATE audio_files SET filename='borg indoor.wav' WHERE id=14;
                 UPDATE audio_files SET filename='borg sound indoor.wav' WHERE id=2;
                 INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
                 (11,'/lib/folderless-gap.wav','sound indoor.wav',NULL,0),
                 (30,'/lib/folderless.wav','borg sound indoor.wav',NULL,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        assert!(ids(&provider, "Borg Sound Indoor", SearchScope::Path).is_empty());
        let main = ids(&provider, "Borg Sound Indoor", SearchScope::Main);
        assert!(main.contains(&30));
        let all = ids(&provider, "Borg Sound Indoor", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14]);
        assert!(
            !all.contains(&11),
            "a folderless partial main hit is not mixed"
        );
    }

    #[test]
    fn all_posting_refills_preserve_reference_order_and_cursor_local_counts() {
        let (_dir, provider) = overlapping_library();
        add_stream_refill_rows(&provider);
        let mut expected = provider
            .search(&SearchRequest {
                query: "Borg Sound".into(),
                scope: SearchScope::Main,
                limit: 1000,
                ..Default::default()
            })
            .unwrap()
            .results
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>();
        assert!(
            expected.len() > 256,
            "fixture must refill the full-main exclusion stream"
        );
        expected.extend([10, 14]);
        expected.extend((100..700).filter(|id| id % 2 != 0));
        expected.push(8);
        let request = SearchRequest {
            query: "Borg Sound".into(),
            scope: SearchScope::All,
            limit: 1000,
            ..Default::default()
        };
        let complete = provider.search(&request).unwrap();
        assert_eq!(
            complete
                .results
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(complete.total_count, expected.len() as u64);
        assert!(complete.total_count_exact);
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);
        let mut cursor = None;
        let mut walked = Vec::new();
        loop {
            let before = walked.len();
            let page = provider
                .search(&SearchRequest {
                    limit: 137,
                    offset: if cursor.is_some() { 999 } else { 0 },
                    cursor,
                    ..request.clone()
                })
                .unwrap();
            assert_eq!(page.returned_count, page.results.len());
            let end = (before + 137).min(expected.len());
            assert_eq!(
                page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                expected[before..end]
            );
            walked.extend(page.results.iter().map(|file| file.id));
            let observed = page.results.len() + usize::from(page.next_cursor.is_some());
            assert_eq!(page.total_count, observed as u64);
            assert_eq!(page.total_count_exact, page.next_cursor.is_none());
            if page.next_cursor.is_none() {
                break;
            }
            cursor = page.next_cursor;
        }
        assert_eq!(walked, expected);
    }

    fn all_directory_first_page_vm_work(files: i64, partial: bool) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        {
            let writer = provider.writer().unwrap();
            writer
                .execute_batch(
                    "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
                     (1,'/lib','LibraryRoot',NULL),
                     (2,'/lib/Borg Sound','Borg Sound',1),
                     (3,'/lib/Other','Other',1);",
                )
                .unwrap();
            writer
                .execute(
                    "WITH RECURSIVE n(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM n WHERE id<?1)
                     INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
                     SELECT id*2,'/lib/Borg Sound/'||id||'.wav','quiet.wav',2,0 FROM n
                     UNION ALL
                     SELECT ?1*2+id,'/lib/Other/'||id||'.wav','quiet.wav',3,0 FROM n",
                    [files],
                )
                .unwrap();
            if partial {
                writer
                    .execute_batch(
                        "UPDATE folders SET folder_name='Borg' WHERE id=2;
                         UPDATE folders SET folder_name='Sound' WHERE id=3;
                         UPDATE audio_files SET filename='sound.wav' WHERE id=2;
                         UPDATE audio_files SET filename='borg.wav'
                         WHERE id=(SELECT min(id) FROM audio_files WHERE folder_id=3);",
                    )
                    .unwrap();
            }
        }
        provider.rebuild_directory_search().unwrap();
        let conn = provider.reader().unwrap();
        load_index(&conn, &provider.dir_index).unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&ticks);
        conn.progress_handler(
            100,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let page = search(&DirSearch {
            conn: &conn,
            cache: &provider.dir_index,
            jieba: &provider.tokenizer,
            query: "Borg Sound",
            scope: SearchScope::All,
            visibility: FileVisibility::Present,
            cursor: None,
            offset: 0,
            limit: 5,
        });
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        let page = page.unwrap();
        let expected = if partial {
            vec![2, files * 2 + 1]
        } else {
            vec![2, 4, 6, 8, 10]
        };
        assert_eq!(
            page.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(page.observed_count, if partial { 2 } else { 6 });
        assert_eq!(page.has_more, !partial);
        let work = ticks.load(Ordering::Relaxed);
        if !partial {
            // C's strict Path route must still avoid the main FTS table entirely
            // for a complete directory AND; All legitimately uses its main tier.
            conn.authorizer(Some(|context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Read { table_name, .. }
                    if table_name == "audio_search")
                {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
            let path = search(&DirSearch {
                conn: &conn,
                cache: &provider.dir_index,
                jieba: &provider.tokenizer,
                query: "Borg Sound",
                scope: SearchScope::Path,
                visibility: FileVisibility::Present,
                cursor: None,
                offset: 0,
                limit: 5,
            });
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert_eq!(
                path.unwrap()
                    .hits
                    .iter()
                    .map(|hit| hit.id)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        drop(conn);
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "Borg Sound".into(),
                    scope: SearchScope::All,
                    limit: 5,
                    ..Default::default()
                })
                .unwrap()
                .results
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            expected
        );
        work
    }

    #[test]
    fn all_complete_path_first_page_avoids_exhaustive_mixed_expansion() {
        let small = all_directory_first_page_vm_work(512, false);
        let large = all_directory_first_page_vm_work(8192, false);
        assert!(small > 0);
        assert!(
            large <= small * 2,
            "complete-path All VM callbacks grew from {small} to {large}"
        );
    }

    #[test]
    fn all_sparse_mixed_first_page_uses_bounded_posting_streams() {
        let small = all_directory_first_page_vm_work(512, true);
        let large = all_directory_first_page_vm_work(8192, true);
        assert!(small > 0);
        assert!(
            large <= small * 2,
            "mixed All VM callbacks grew from {small} to {large}"
        );
    }

    #[test]
    fn posting_refill_interrupts_and_does_not_poison_the_next_reader() {
        let (_dir, provider) = overlapping_library();
        add_stream_refill_rows(&provider);
        let cancel = Arc::new(AtomicBool::new(false));
        let conn = provider.search_reader(Some(cancel.clone())).unwrap();
        let expression = compile_query(&provider.tokenizer, "sound").unwrap();
        let mut stream = MainPostings::new(&expression, i64::MIN, i64::MAX);
        let mut previous = i64::MIN;
        for _ in 0..256 {
            let id = stream.head(&conn).unwrap().unwrap();
            assert!(id > previous);
            previous = id;
            stream.advance();
        }
        let signal = cancel.clone();
        conn.progress_handler(1, Some(move || signal.load(Ordering::Acquire)))
            .unwrap();
        cancel.store(true, Ordering::Release);
        assert!(matches!(stream.head(&conn), Err(Error::Cancelled)));
        drop(conn);
        // The flag intentionally remains set. ReadLease must remove its hook;
        // a later real query on the pool must not inherit cancellation.
        let page = provider
            .search(&SearchRequest {
                query: "sound".into(),
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.results.len(), 5);
    }

    #[test]
    fn sparse_expansion_checks_the_recorded_folder_and_keeps_null_main_hits() {
        let (_dir, provider) = overlapping_library();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
             (3,'/lib/folderless-missing.wav','quiet.wav',NULL,1),
             (6,'/lib/folderless-main.wav','borg sound.wav',NULL,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        let conn = provider.reader().unwrap();
        let mut expanded = Vec::new();
        // Directly exercise expansion of a legacy sparse interval. Normal C run
        // construction and snapshot validation already split/refuse foreign gaps.
        expand(
            &conn,
            &Run {
                lo: 2,
                hi: 8,
                count: 3,
                dense: false,
                dir: 3,
            },
            2,
            20,
            FileVisibility::All,
            &mut expanded,
        )
        .unwrap();
        assert_eq!(expanded, vec![2, 4, 8]);
        drop(conn);
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);
        let main = ids(&provider, "Borg Sound", SearchScope::Main);
        assert!(main.contains(&6));
        let all = ids(&provider, "Borg Sound", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14, 8]);
        let mut request = SearchRequest {
            query: "Borg Sound".into(),
            scope: SearchScope::Path,
            ..Default::default()
        };
        request.filters.visibility = FileVisibility::Missing;
        assert_eq!(
            provider
                .search(&request)
                .unwrap()
                .results
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            vec![4]
        );
        request.filters.visibility = FileVisibility::All;
        assert_eq!(
            provider
                .search(&request)
                .unwrap()
                .results
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            vec![2, 4, 8]
        );
    }

    #[test]
    fn overlapping_path_terms_keep_directory_and_main_pages_distinct() {
        let (_dir, provider) = overlapping_library();
        let main = ids(&provider, "Borg Sound", SearchScope::Main);
        assert_eq!(main, vec![2, 12]);
        let all = ids(&provider, "Borg Sound", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14, 8]);
        // Full path hits remain in path scope even when main also matches (2).
        // A filename cannot complete a partial directory match (10, 12, 14).
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);

        for scope in [SearchScope::Path, SearchScope::All] {
            let full = ids(&provider, "Borg Sound", scope);
            let mut cursor = None;
            let mut paged = Vec::new();
            for (page_index, expected) in full.chunks(1).enumerate() {
                let request = SearchRequest {
                    query: "Borg Sound".into(),
                    scope,
                    limit: 1,
                    ..Default::default()
                };
                // Exercise native suffix counts; the presentation cache may
                // replace a repeated first page's cursor with a materialized one.
                let offset_page = provider
                    .search_scoped(
                        &SearchRequest {
                            offset: page_index,
                            ..request.clone()
                        },
                        None,
                    )
                    .unwrap();
                let cursor_page = provider
                    .search_scoped(
                        &SearchRequest {
                            offset: if cursor.is_some() { 999 } else { 0 },
                            cursor: cursor.clone(),
                            ..request
                        },
                        None,
                    )
                    .unwrap();
                let returned = cursor_page
                    .results
                    .iter()
                    .map(|file| file.id)
                    .collect::<Vec<_>>();
                assert_eq!(returned, expected);
                assert_eq!(
                    offset_page
                        .results
                        .iter()
                        .map(|file| file.id)
                        .collect::<Vec<_>>(),
                    returned
                );
                let remaining = full.len() - page_index;
                assert_eq!(cursor_page.total_count, remaining.min(2) as u64);
                assert_eq!(cursor_page.total_count_exact, remaining <= 1);
                assert_eq!(
                    offset_page.total_count,
                    (page_index + remaining.min(2)) as u64
                );
                paged.extend(returned);
                cursor = cursor_page.next_cursor;
                assert_eq!(cursor.is_some(), remaining > 1);
            }
            assert_eq!(paged, full);
            let beyond_end = provider
                .search(&SearchRequest {
                    query: "Borg Sound".into(),
                    scope,
                    offset: 999,
                    limit: 2,
                    ..Default::default()
                })
                .unwrap();
            assert!(beyond_end.results.is_empty());
            assert_eq!(beyond_end.total_count, 0);
            assert!(!beyond_end.total_count_exact);
            assert!(beyond_end.next_cursor.is_none());
        }

        for (visibility, expected) in [
            (FileVisibility::Missing, vec![4]),
            (FileVisibility::All, vec![2, 4, 8]),
        ] {
            let mut request = SearchRequest {
                query: "Borg Sound".into(),
                scope: SearchScope::Path,
                ..Default::default()
            };
            request.filters.visibility = visibility;
            let page = provider.search(&request).unwrap();
            assert_eq!(
                page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                expected
            );
        }
    }

    fn directory_first_page_vm_work(files: usize, partial: bool) -> usize {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        {
            let writer = provider.writer().unwrap();
            writer
                .execute_batch(
                    "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
                     (1,'/lib','LibraryRoot',NULL),
                     (2,'/lib/Borg Sound','Borg Sound',1),
                     (3,'/lib/Other','Other',1);",
                )
                .unwrap();
            writer
                .execute(
                    "WITH RECURSIVE n(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM n WHERE id<?1)
                     INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
                     SELECT id*2,'/lib/Borg Sound/'||id||'.wav','quiet.wav',2,0 FROM n
                     UNION ALL
                     SELECT ?1*2+id,'/lib/Other/'||id||'.wav','quiet.wav',3,0 FROM n",
                    [files as i64],
                )
                .unwrap();
            writer
                .execute(
                    "UPDATE folders SET total_file_count=CASE WHEN id=1 THEN ?1*2 ELSE ?1 END",
                    [files as i64],
                )
                .unwrap();
            if partial {
                writer
                    .execute_batch(
                        "UPDATE folders SET folder_name='Borg' WHERE id=2;
                         UPDATE folders SET folder_name='Sound' WHERE id=3;
                         UPDATE audio_files SET filename='sound.wav' WHERE id=2;
                         UPDATE audio_files SET filename='borg.wav'
                         WHERE id=(SELECT min(id) FROM audio_files WHERE folder_id=3);",
                    )
                    .unwrap();
            }
        }
        provider.rebuild_directory_search().unwrap();
        let conn = provider.reader().unwrap();
        // Separate one-time RunIdx loading from page work. The path run is sparse
        // and visibility is checked in SQLite, so expanding the whole subtree
        // produces VM work proportional to its size even without main probes.
        load_index(&conn, &provider.dir_index).unwrap();
        conn.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read { table_name, .. }
                if table_name == "audio_search")
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&ticks);
        conn.progress_handler(
            100,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let result = search(&DirSearch {
            conn: &conn,
            cache: &provider.dir_index,
            jieba: &provider.tokenizer,
            query: "Borg Sound",
            scope: SearchScope::Path,
            visibility: FileVisibility::Present,
            cursor: None,
            offset: 0,
            limit: 5,
        });
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let page = result.unwrap();
        let expected = if partial {
            Vec::new()
        } else {
            vec![2, 4, 6, 8, 10]
        };
        assert_eq!(
            page.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(page.observed_count, if partial { 0 } else { 6 });
        assert_eq!(page.has_more, !partial);
        ticks.load(Ordering::Relaxed)
    }

    #[test]
    fn all_path_first_page_does_not_scan_subtree_or_probe_main() {
        let small = directory_first_page_vm_work(512, false);
        let large = directory_first_page_vm_work(8192, false);
        assert!(small > 0);
        // The same five-row page may have small B-tree-depth overhead, but 16x
        // more subtree rows must not cause an exhaustive tier-1 file scan.
        assert!(
            large <= small * 2,
            "page VM callbacks grew from {small} to {large}"
        );
    }

    #[test]
    fn partial_directory_matches_do_not_probe_main_or_scan_each_path_file() {
        let small = directory_first_page_vm_work(512, true);
        let large = directory_first_page_vm_work(8192, true);
        assert!(small > 0);
        assert!(
            large <= small * 2,
            "partial-directory Path VM callbacks grew from {small} to {large}"
        );
    }

    #[test]
    fn path_requires_every_term_in_directories_while_all_can_complete_from_main() {
        let (_dir, provider) = overlapping_library();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "UPDATE audio_files SET filename='sound indoor.wav' WHERE id=10;
             UPDATE audio_files SET filename='borg indoor.wav' WHERE id=14;
             UPDATE audio_files SET filename='borg sound indoor.wav' WHERE id=2;
             INSERT INTO folders(id,folder_path,folder_name,parent_id,total_file_count) VALUES
             (6,'/lib/Borg/Sound/Indoor','Indoor',3,1);
             INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
             (11,'/lib/folderless-gap.wav','sound indoor.wav',NULL,0),
             (30,'/lib/folderless.wav','borg sound indoor.wav',NULL,0),
             (32,'/lib/Borg/Sound/Indoor/quiet.wav','quiet.wav',6,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        assert_eq!(
            ids(&provider, "Borg Sound Indoor", SearchScope::Path),
            vec![32]
        );
        let all = ids(&provider, "Borg Sound Indoor", SearchScope::All);
        let main = ids(&provider, "Borg Sound Indoor", SearchScope::Main);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert!(main.contains(&30), "folderless full-main hits remain valid");
        assert_eq!(&all[main.len()..], &[10, 14, 32]);
    }

    #[test]
    fn directory_pages_and_all_posting_batches_survive_cursor_boundaries() {
        let (_dir, provider) = overlapping_library();
        add_stream_refill_rows(&provider);
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "WITH RECURSIVE n(id) AS (VALUES(1000) UNION ALL SELECT id+1 FROM n WHERE id<1599)
             INSERT INTO audio_files(id,file_path,filename,folder_id,missing)
             SELECT id,
                    CASE WHEN id%2=0 THEN '/lib/Borg/Sound/' ELSE '/lib/Other/' END||id||'.wav',
                    'quiet.wav',CASE WHEN id%2=0 THEN 3 ELSE 4 END,0 FROM n;",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        let main = provider
            .search(&SearchRequest {
                query: "Borg Sound".into(),
                scope: SearchScope::Main,
                limit: 1000,
                ..Default::default()
            })
            .unwrap()
            .results
            .into_iter()
            .map(|file| file.id)
            .collect::<Vec<_>>();
        assert!(main.len() > 256, "full-main exclusions must refill");
        for scope in [SearchScope::Path, SearchScope::All] {
            let request = SearchRequest {
                query: "Borg Sound".into(),
                scope,
                limit: 1000,
                ..Default::default()
            };
            let full = provider
                .search(&request)
                .unwrap()
                .results
                .into_iter()
                .map(|file| file.id)
                .collect::<Vec<_>>();
            if scope == SearchScope::Path {
                let expected = [2, 8]
                    .into_iter()
                    .chain((1000..1600).step_by(2))
                    .collect::<Vec<_>>();
                assert_eq!(full, expected);
            } else {
                let expected = main
                    .iter()
                    .copied()
                    .chain([10, 14])
                    .chain((100..700).filter(|id| id % 2 != 0))
                    .chain([8])
                    .chain((1000..1600).step_by(2))
                    .collect::<Vec<_>>();
                assert_eq!(full, expected);
            }
            let mut cursor = None;
            let mut paged = Vec::new();
            loop {
                let page = provider
                    .search(&SearchRequest {
                        limit: 137,
                        offset: if cursor.is_some() { 999 } else { 0 },
                        cursor,
                        ..request.clone()
                    })
                    .unwrap();
                let observed = page.results.len() + usize::from(page.next_cursor.is_some());
                assert_eq!(page.total_count, observed as u64);
                assert_eq!(page.total_count_exact, page.next_cursor.is_none());
                paged.extend(page.results.into_iter().map(|file| file.id));
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(paged, full);
        }
    }

    #[test]
    fn full_path_sparse_runs_do_not_assign_folderless_gaps_to_the_directory() {
        let (_dir, provider) = overlapping_library();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES
                 (3,'/lib/folderless-missing.wav','quiet.wav',NULL,1),
                 (6,'/lib/folderless-main.wav','borg sound.wav',NULL,0);",
            )
            .unwrap();
        provider.rebuild_directory_search().unwrap();
        assert_eq!(ids(&provider, "Borg Sound", SearchScope::Path), vec![2, 8]);
        let main = ids(&provider, "Borg Sound", SearchScope::Main);
        assert!(main.contains(&6));
        let all = ids(&provider, "Borg Sound", SearchScope::All);
        assert_eq!(&all[..main.len()], main.as_slice());
        assert_eq!(&all[main.len()..], &[10, 14, 8]);
        for (visibility, expected) in [
            (FileVisibility::Missing, vec![4]),
            (FileVisibility::All, vec![2, 4, 8]),
        ] {
            let mut request = SearchRequest {
                query: "Borg Sound".into(),
                scope: SearchScope::Path,
                ..Default::default()
            };
            request.filters.visibility = visibility;
            let page = provider.search(&request).unwrap();
            assert_eq!(
                page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                expected
            );
        }
    }
}
