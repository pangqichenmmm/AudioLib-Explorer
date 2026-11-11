// crates/audiolib-db/src/waveforms_db.rs
//! `waveforms.db` (doc/api/README.md §9.4.1): a separate SQLite file next to (never inside)
//! a library database or a server library's client cache, holding compact
//! `i8-minmax-base64` list Waveforms. WAL mode, `synchronous=NORMAL`, one row per record
//! written by per-row UPSERT. The library schema, its indexes, FTS5 and server snapshots
//! are not touched.
//!
//! The table layout matches the server's §9.4.1 file so that a downloaded bundle (§9.4.2)
//! can replace the local file after [`validate_bundle`]:
//! `list_waveforms(file_id, etag, src_mtime, src_size, encoding, n, blob)` with `blob`
//! exactly 256 bytes (128 × `min,max` int8, zero padded after `n` columns) and
//! `meta(key, value)`.
use crate::db_error;
use audiolib_core::{
    Error, LIST_WAVEFORM_COLUMNS, LIST_WAVEFORM_I8_MAX_BYTES, ListWaveformI8, Result,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::{BTreeSet, HashMap};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};

/// File name of the compact list Waveform store.
pub const WAVEFORMS_DB_FILE: &str = "waveforms.db";
/// Columns of `list_waveforms` (the §9.4.2 whitelist, exactly these).
pub const LIST_WAVEFORMS_COLUMNS: [&str; 7] = [
    "file_id",
    "etag",
    "src_mtime",
    "src_size",
    "encoding",
    "n",
    "blob",
];
/// Columns of `meta`.
pub const META_COLUMNS: [&str; 2] = ["key", "value"];

/// One stored compact list Waveform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveformsDbRow {
    pub file_id: i64,
    /// Server entity tag (full, including its `-l1i8` suffix) or the local zoom key.
    pub etag: String,
    /// Wire encoding the peaks were received or derived in.
    pub encoding: String,
    /// Source file mtime (Unix nanoseconds locally; 0 when unknown, as for server items).
    pub src_mtime: i64,
    /// Source file size in bytes (0 when unknown).
    pub src_size: i64,
    pub peaks: ListWaveformI8,
}

/// Handle of one `waveforms.db` file. Live handles of the same canonical path
/// share one serialized connection, including its close/replace/reopen boundary.
pub struct WaveformsDb {
    path: PathBuf,
    conn: Arc<SharedConnection>,
}

type ConnectionRegistry = HashMap<PathBuf, Arc<ConnectionEntry>>;

#[derive(Default)]
enum ConnectionState {
    #[default]
    Closed,
    Opening,
    Live(Weak<SharedConnection>),
}

#[derive(Default)]
struct ConnectionEntry {
    state: Mutex<ConnectionState>,
    changed: Condvar,
    #[cfg(test)]
    waiter: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

struct SharedConnection {
    // Field declaration order is part of the lifetime contract: SQLite and its
    // cached statements close before the marker permits another native open.
    conn: Mutex<Connection>,
    _release: ReleaseMarker,
    #[cfg(test)]
    before_close: Mutex<Option<BeforeConnectionClose>>,
}

impl Deref for SharedConnection {
    type Target = Mutex<Connection>;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

struct ReleaseMarker(Arc<ConnectionEntry>);

impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = ConnectionState::Closed;
        self.0.changed.notify_all();
    }
}

/// Reset a failed or panicking constructor before any caller can be stranded
/// waiting on Opening. Successful construction hands ownership to the marker.
struct OpeningGuard {
    entry: Arc<ConnectionEntry>,
    armed: bool,
}

impl OpeningGuard {
    fn release_marker(&mut self) -> ReleaseMarker {
        let marker = ReleaseMarker(self.entry.clone());
        self.armed = false;
        marker
    }
}

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut state = self
                .entry
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *state = ConnectionState::Closed;
            self.entry.changed.notify_all();
        }
    }
}

#[cfg(test)]
struct BeforeConnectionClose {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl Drop for SharedConnection {
    fn drop(&mut self) {
        if let Some(hook) = self
            .before_close
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = hook.entered.send(());
            let _ = hook.release.recv_timeout(std::time::Duration::from_secs(5));
        }
        // Automatic field drops still close conn before _release, as in production.
    }
}

/// Native canonical parents resolve relative/dot and filesystem case aliases.
/// Keep the lossless path: distinct non-UTF-8 POSIX entries must not collide.
fn connection_key(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .map_err(|error| Error::Database(format!("waveforms.db canonical parent: {error}")))?;
    Ok(parent.join(WAVEFORMS_DB_FILE))
}

fn shared_connection(path: &Path) -> Result<Arc<SharedConnection>> {
    static CONNECTIONS: OnceLock<Mutex<ConnectionRegistry>> = OnceLock::new();
    let key = connection_key(path)?;
    let entry = {
        let mut connections = CONNECTIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // A dead Weak alone is insufficient: its SQLite destructor may still
        // be running. Only the marker-confirmed Closed/map-only entry is idle.
        connections.retain(|_, entry| {
            Arc::strong_count(entry) > 1
                || !matches!(
                    &*entry.state.lock().unwrap_or_else(PoisonError::into_inner),
                    ConnectionState::Closed
                )
        });
        connections
            .entry(key)
            .or_insert_with(|| Arc::new(ConnectionEntry::default()))
            .clone()
    };
    loop {
        let mut state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let ConnectionState::Live(connection) = &*state
            && let Some(connection) = connection.upgrade()
        {
            return Ok(connection);
        }
        if matches!(&*state, ConnectionState::Closed) {
            *state = ConnectionState::Opening;
            drop(state);
            let mut opening = OpeningGuard {
                entry: entry.clone(),
                armed: true,
            };
            // No global or per-entry mutex is held during filesystem/SQLite I/O.
            remove_stale_bundle_files(path);
            let connection = open_connection(path)?;
            let connection = Arc::new(SharedConnection {
                conn: Mutex::new(connection),
                // conn has already moved into the first field before the guard
                // hands off. Every subsequent unwind closes it before notify.
                _release: opening.release_marker(),
                #[cfg(test)]
                before_close: Mutex::new(None),
            });
            let mut state = entry.state.lock().unwrap_or_else(PoisonError::into_inner);
            *state = ConnectionState::Live(Arc::downgrade(&connection));
            entry.changed.notify_all();
            return Ok(connection);
        }
        // Opening is single-flight. An expired Live Weak waits for the native
        // close marker, rather than racing that destructor with a new open.
        #[cfg(test)]
        if let Some(waiter) = entry
            .waiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = waiter.send(());
        }
        drop(
            entry
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
        );
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(db_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(db_error)?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(db_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::Database(format!(
            "waveforms.db journal mode is {mode}, expected wal"
        )));
    }
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS list_waveforms(
           file_id   INTEGER PRIMARY KEY,
           etag      TEXT    NOT NULL,
           src_mtime INTEGER NOT NULL,
           src_size  INTEGER NOT NULL,
           encoding  TEXT    NOT NULL,
           n         INTEGER NOT NULL,
           blob      BLOB    NOT NULL
         );
         CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(db_error)?;
    Ok(conn)
}

/// Rows per write transaction of [`WaveformsDb::upsert_many`].
pub const WRITE_BATCH_ROWS: usize = 256;
const UPSERT_SQL: &str =
    "INSERT INTO list_waveforms(file_id,etag,src_mtime,src_size,encoding,n,blob)
     VALUES(?1,?2,?3,?4,?5,?6,?7)
     ON CONFLICT(file_id) DO UPDATE SET etag=excluded.etag,src_mtime=excluded.src_mtime,
      src_size=excluded.src_size,encoding=excluded.encoding,n=excluded.n,
      blob=excluded.blob";

/// Zero-padded 256-byte blob of `peaks`.
fn padded_blob(peaks: &ListWaveformI8) -> Vec<u8> {
    let mut blob = peaks.to_bytes();
    blob.resize(LIST_WAVEFORM_I8_MAX_BYTES, 0);
    blob
}

impl WaveformsDb {
    /// `waveforms.db` in the directory of `main_db` (a sibling file, never the same file).
    pub fn path_beside(main_db: &Path) -> PathBuf {
        main_db
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(WAVEFORMS_DB_FILE)
    }

    /// Open or create the store at `path` (WAL mode, tables created if absent).
    pub fn open(path: &Path) -> Result<Self> {
        if path.file_name().and_then(|name| name.to_str()) != Some(WAVEFORMS_DB_FILE) {
            return Err(Error::Validation(format!(
                "list Waveform store must be named {WAVEFORMS_DB_FILE}"
            )));
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::Database(format!("{}: {error}", parent.display())))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            conn: shared_connection(path)?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert or replace one row (single-statement UPSERT, its own transaction).
    pub fn upsert(&self, row: &WaveformsDbRow) -> Result<()> {
        if row.peaks.values.len() > LIST_WAVEFORM_I8_MAX_BYTES {
            return Err(Error::Validation("i8 peaks exceed 256 bytes".into()));
        }
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.prepare_cached(UPSERT_SQL)
            .map_err(db_error)?
            .execute(rusqlite::params![
                row.file_id,
                row.etag,
                row.src_mtime,
                row.src_size,
                row.encoding,
                row.peaks.columns() as i64,
                padded_blob(&row.peaks)
            ])
            .map_err(db_error)?;
        Ok(())
    }

    /// Insert or replace many rows in transactions of [`WRITE_BATCH_ROWS`] rows, so a
    /// page of results costs one commit instead of one per row.
    pub fn upsert_many(&self, rows: &[WaveformsDbRow]) -> Result<usize> {
        if rows
            .iter()
            .any(|row| row.peaks.values.len() > LIST_WAVEFORM_I8_MAX_BYTES)
        {
            return Err(Error::Validation("i8 peaks exceed 256 bytes".into()));
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        for chunk in rows.chunks(WRITE_BATCH_ROWS) {
            let transaction = conn.transaction().map_err(db_error)?;
            {
                let mut statement = transaction.prepare_cached(UPSERT_SQL).map_err(db_error)?;
                for row in chunk {
                    statement
                        .execute(rusqlite::params![
                            row.file_id,
                            row.etag,
                            row.src_mtime,
                            row.src_size,
                            row.encoding,
                            row.peaks.columns() as i64,
                            padded_blob(&row.peaks)
                        ])
                        .map_err(db_error)?;
                }
            }
            transaction.commit().map_err(db_error)?;
        }
        Ok(rows.len())
    }

    /// The row of `file_id`. A row whose blob or `n` no longer validates is reported as
    /// absent (it is replaced by the next UPSERT).
    pub fn get(&self, file_id: i64) -> Result<Option<WaveformsDbRow>> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let row = conn
            .prepare_cached(
                "SELECT etag,src_mtime,src_size,encoding,n,blob FROM list_waveforms
                 WHERE file_id=?1",
            )
            .map_err(db_error)?
            .query_row([file_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            })
            .optional()
            .map_err(db_error)?;
        Ok(
            row.and_then(|(etag, src_mtime, src_size, encoding, n, blob)| {
                let n = usize::try_from(n).ok()?;
                if blob.len() != LIST_WAVEFORM_I8_MAX_BYTES || n > LIST_WAVEFORM_COLUMNS as usize {
                    return None;
                }
                let peaks = ListWaveformI8::from_bytes(&blob[..n * 2]).ok()?;
                Some(WaveformsDbRow {
                    file_id,
                    etag,
                    encoding,
                    src_mtime,
                    src_size,
                    peaks,
                })
            }),
        )
    }

    /// Rows of several ids (absent ones left out).
    pub fn get_many(&self, ids: &[i64]) -> Result<Vec<WaveformsDbRow>> {
        let mut rows = Vec::new();
        for id in ids {
            if let Some(row) = self.get(*id)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Number of stored rows.
    pub fn len(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("SELECT COUNT(*) FROM list_waveforms", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count.max(0) as u64)
        .map_err(db_error)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Atomically replace this store with an already validated bundle file; rows already
    /// in the store win over the bundle's (§9.4.2):
    /// the connection is closed, `validated` is renamed over `waveforms.db` (same
    /// directory) and the store is reopened in WAL mode. On a failed rename the previous
    /// file is reopened and kept.
    pub fn replace_with(&self, validated: &Path) -> Result<()> {
        if validated.parent() != self.path.parent() {
            return Err(Error::Validation(
                "a bundle must be staged in the waveforms.db directory".into(),
            ));
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        // Re-check under the store lock right before the swap: rows written by page
        // lookups while the bundle downloaded are newer than the bundle's, so they are
        // copied over it instead of being lost. Writers wait on this lock meanwhile.
        let local_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM list_waveforms", [], |row| row.get(0))
            .map_err(db_error)?;
        if local_rows > 0 {
            let target = validated
                .to_str()
                .ok_or_else(|| Error::Validation("bundle path is not UTF-8".into()))?;
            conn.execute("ATTACH DATABASE ?1 AS bundle", [target])
                .map_err(db_error)?;
            let merged = conn
                .execute_batch(
                    "INSERT OR REPLACE INTO bundle.list_waveforms
                       (file_id,etag,src_mtime,src_size,encoding,n,blob)
                     SELECT file_id,etag,src_mtime,src_size,encoding,n,blob
                     FROM main.list_waveforms;",
                )
                .map_err(db_error);
            conn.execute_batch("DETACH DATABASE bundle")
                .map_err(db_error)?;
            merged?;
        }
        // Every live handle of this path uses this slot and mutex. Closing the
        // only connection checkpoints the WAL and removes its sidecars; old
        // provider generations wait here and then observe the reopened store.
        close_or_restore(&mut conn, Connection::close)?;
        let renamed = std::fs::rename(validated, &self.path);
        *conn = open_connection(&self.path)?;
        renamed.map_err(|error| Error::Database(format!("waveforms.db replace: {error}")))
    }
}

/// Remove `waveforms.db.bundle-<pid>-<n>.tmp` downloads (and their SQLite sidecars)
/// left in the directory of `path` by an earlier process that stopped mid-import.
/// Files of this process are kept, as an import may be running. Best effort.
fn remove_stale_bundle_files(path: &Path) {
    let Some(directory) = path.parent() else {
        return;
    };
    let directory = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let prefix = format!("{WAVEFORMS_DB_FILE}.bundle-");
    let own = format!("{prefix}{}-", std::process::id());
    // `waveforms.db.etag-<pid>.tmp`: the bundle ETag being written by the remote client.
    let etag_prefix = format!("{WAVEFORMS_DB_FILE}.etag-");
    let own_etag = format!("{etag_prefix}{}.tmp", std::process::id());
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let stale_bundle =
            name.starts_with(&prefix) && name.contains(".tmp") && !name.starts_with(&own);
        let stale_etag =
            name.starts_with(&etag_prefix) && name.ends_with(".tmp") && name != own_etag;
        if stale_bundle || stale_etag {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => tracing::info!(file = name, "removed a stale waveform bundle download"),
                Err(error) => tracing::debug!(%error, file = name, "stale bundle file kept"),
            }
        }
    }
}

/// Close the connection in `slot` with `close`, leaving a placeholder in memory only
/// while it closes. When closing fails the original file connection is put back, so the
/// store never stays on an empty in-memory database.
fn close_or_restore(
    slot: &mut Connection,
    close: impl FnOnce(Connection) -> std::result::Result<(), (Connection, rusqlite::Error)>,
) -> Result<()> {
    let placeholder = Connection::open_in_memory().map_err(db_error)?;
    let old = std::mem::replace(slot, placeholder);
    match close(old) {
        Ok(()) => Ok(()),
        Err((old, error)) => {
            *slot = old;
            Err(db_error(error))
        }
    }
}

/// Validate a downloaded bundle (§9.4.2) before it may replace `waveforms.db`:
/// `PRAGMA integrity_check` is `ok`; exactly the tables `list_waveforms` and `meta` with
/// exactly the whitelisted columns (no views, triggers or extra indexes); every row's
/// `blob` is exactly 256 bytes, `encoding` equals `encoding`, `0 <= n <= 128`; and
/// `meta.library_id` equals `library_id`. Returns the row count. The file is opened
/// read-only and never modified.
pub fn validate_bundle(path: &Path, encoding: &str, library_id: &str) -> Result<u64> {
    let invalid =
        |message: String| Error::Validation(format!("waveform bundle rejected: {message}"));
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| invalid(error.to_string()))?;
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| invalid(error.to_string()))?;
    if check != "ok" {
        return Err(invalid(format!("integrity_check: {check}")));
    }
    let objects: Vec<(String, String)> = conn
        .prepare("SELECT type,name FROM sqlite_master")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .map_err(|error| invalid(error.to_string()))?;
    let mut tables = BTreeSet::new();
    for (kind, name) in &objects {
        match kind.as_str() {
            "table" => {
                tables.insert(name.as_str());
            }
            "index" if name.starts_with("sqlite_autoindex_") => {}
            _ => return Err(invalid(format!("unexpected {kind} {name}"))),
        }
    }
    if tables != BTreeSet::from(["list_waveforms", "meta"]) {
        return Err(invalid(format!("unexpected tables {tables:?}")));
    }
    for (table, expected) in [
        ("list_waveforms", &LIST_WAVEFORMS_COLUMNS[..]),
        ("meta", &META_COLUMNS[..]),
    ] {
        let columns: BTreeSet<String> = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
            .map_err(|error| invalid(error.to_string()))?;
        let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
        if columns != expected {
            return Err(invalid(format!("{table} columns {columns:?}")));
        }
    }
    let bad: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM list_waveforms WHERE typeof(blob)<>'blob' OR length(blob)<>?1
              OR encoding IS NOT ?2 OR typeof(n)<>'integer' OR n<0 OR n>?3
              OR typeof(etag)<>'text'",
            rusqlite::params![
                LIST_WAVEFORM_I8_MAX_BYTES as i64,
                encoding,
                i64::from(LIST_WAVEFORM_COLUMNS)
            ],
            |row| row.get(0),
        )
        .map_err(|error| invalid(error.to_string()))?;
    if bad != 0 {
        return Err(invalid(format!(
            "{bad} rows with a bad blob, encoding or n"
        )));
    }
    let meta_library: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key='library_id'", [], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|error| invalid(error.to_string()))?;
    if meta_library.as_deref() != Some(library_id) {
        return Err(invalid(format!(
            "meta.library_id {meta_library:?} is not the open library"
        )));
    }
    conn.query_row("SELECT COUNT(*) FROM list_waveforms", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|count| count.max(0) as u64)
    .map_err(|error| invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::LIST_WAVEFORM_ENCODING_I8;

    fn row(id: i64, etag: &str, values: Vec<i8>) -> WaveformsDbRow {
        WaveformsDbRow {
            file_id: id,
            etag: etag.into(),
            encoding: LIST_WAVEFORM_ENCODING_I8.into(),
            src_mtime: 11,
            src_size: 22,
            peaks: ListWaveformI8::new(values).unwrap(),
        }
    }

    #[test]
    fn upsert_replaces_rows_and_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let main = directory.path().join("library.db");
        let path = WaveformsDb::path_beside(&main);
        assert_eq!(path, directory.path().join("waveforms.db"));
        {
            let db = WaveformsDb::open(&path).unwrap();
            db.upsert(&row(7, "\"a-l1i8\"", vec![-1, 1])).unwrap();
            db.upsert(&row(8, "\"b-l1i8\"", vec![])).unwrap();
            db.upsert(&row(7, "\"c-l1i8\"", vec![-5, 5, 0, 1])).unwrap();
        }
        let db = WaveformsDb::open(&path).unwrap();
        let (mode, blob_len, n): (String, i64, i64) = {
            let conn = db.conn.lock().unwrap();
            let mode = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            let (blob_len, n) = conn
                .query_row(
                    "SELECT length(blob),n FROM list_waveforms WHERE file_id=7",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            (mode, blob_len, n)
        };
        assert_eq!(mode, "wal");
        assert_eq!((blob_len, n), (256, 2));
        let seven = db.get(7).unwrap().unwrap();
        assert_eq!(seven.etag, "\"c-l1i8\"");
        assert_eq!(seven.peaks.values, vec![-5, 5, 0, 1]);
        assert_eq!(seven.encoding, LIST_WAVEFORM_ENCODING_I8);
        assert_eq!(db.get(8).unwrap().unwrap().peaks.columns(), 0);
        assert!(db.get(9).unwrap().is_none());
        assert_eq!(db.get_many(&[9, 8, 7]).unwrap().len(), 2);
        assert_eq!(db.len().unwrap(), 2);
        assert!(WaveformsDb::open(&directory.path().join("library.db")).is_err());
    }

    #[test]
    fn upsert_many_writes_in_batches_and_rejects_oversized_rows_up_front() {
        let directory = tempfile::tempdir().unwrap();
        let db = WaveformsDb::open(&directory.path().join(WAVEFORMS_DB_FILE)).unwrap();
        let rows: Vec<_> = (0..WRITE_BATCH_ROWS as i64 * 2 + 7)
            .map(|id| row(id, "e", vec![-1, 1]))
            .collect();
        assert_eq!(db.upsert_many(&rows).unwrap(), rows.len());
        assert_eq!(db.len().unwrap(), rows.len() as u64);
        let mut too_big = row(9_999, "e", vec![]);
        too_big.peaks.values = vec![0; LIST_WAVEFORM_I8_MAX_BYTES + 2];
        assert!(
            db.upsert_many(&[row(10_000, "e", vec![]), too_big])
                .is_err()
        );
        assert!(db.get(10_000).unwrap().is_none(), "nothing written");
    }

    #[test]
    fn rows_written_during_a_bundle_download_survive_the_swap() {
        let directory = tempfile::tempdir().unwrap();
        let db = WaveformsDb::open(&directory.path().join(WAVEFORMS_DB_FILE)).unwrap();
        let staged = directory.path().join("waveforms.db.bundle-0-0.tmp");
        {
            let bundle = open_connection(&staged).unwrap();
            for (id, etag) in [(1, "old"), (2, "bundle")] {
                bundle
                    .execute(
                        "INSERT INTO list_waveforms VALUES(?1,?2,0,0,'i8-minmax-base64',1,?3)",
                        rusqlite::params![
                            id,
                            etag,
                            padded_blob(&ListWaveformI8::new(vec![-1, 1]).unwrap())
                        ],
                    )
                    .unwrap();
            }
            bundle.execute_batch("PRAGMA journal_mode=DELETE").unwrap();
        }
        // A page lookup wrote id 1 (newer) and id 3 while the bundle was downloading.
        db.upsert(&row(1, "page", vec![-2, 2])).unwrap();
        db.upsert(&row(3, "page", vec![])).unwrap();
        db.replace_with(&staged).unwrap();
        assert!(!staged.exists());
        assert_eq!(db.len().unwrap(), 3);
        assert_eq!(db.get(1).unwrap().unwrap().etag, "page");
        assert_eq!(db.get(2).unwrap().unwrap().etag, "bundle");
        assert_eq!(db.get(3).unwrap().unwrap().etag, "page");
    }

    #[test]
    fn surviving_stores_observe_bundle_replacement_through_canonical_parent_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cacheMixed");
        std::fs::create_dir(&cache).unwrap();
        let old = WaveformsDb::open(&cache.join(WAVEFORMS_DB_FILE)).unwrap();
        old.upsert(&row(1, "page", vec![-2, 2])).unwrap();
        old.upsert(&row(3, "page", vec![])).unwrap();
        let upper = directory.path().join("CACHEMIXED");
        #[cfg(windows)]
        let alias = if upper.exists() {
            // Only cacheMixed was created in this owned root. An existing
            // uppercase spelling is its native case-insensitive alias.
            assert_eq!(upper.canonicalize().unwrap(), cache.canonicalize().unwrap());
            cache.join("..").join("CACHEMIXED").join(WAVEFORMS_DB_FILE)
        } else {
            // Case-sensitive Windows directories have no uppercase alias.
            cache.join("..").join("cacheMixed").join(WAVEFORMS_DB_FILE)
        };
        #[cfg(not(windows))]
        let alias = cache.join("..").join("cacheMixed").join(WAVEFORMS_DB_FILE);
        let current = WaveformsDb::open(&alias).unwrap();
        // Different directory entries stay independent, including uppercase
        // on a case-sensitive filesystem. Case-insensitive volumes use another name.
        let other = if upper.exists() {
            directory.path().join("different-cache")
        } else {
            upper
        };
        let unrelated = WaveformsDb::open(&other.join(WAVEFORMS_DB_FILE)).unwrap();
        unrelated
            .upsert(&row(90, "unrelated", vec![-9, 9]))
            .unwrap();
        let staged = current.path().with_file_name("waveforms.db.bundle-0-0.tmp");
        {
            let bundle = open_connection(&staged).unwrap();
            for (id, etag) in [(1, "old-bundle"), (2, "bundle")] {
                bundle
                    .execute(
                        "INSERT INTO list_waveforms VALUES(?1,?2,0,0,'i8-minmax-base64',1,?3)",
                        rusqlite::params![
                            id,
                            etag,
                            padded_blob(&ListWaveformI8::new(vec![-1, 1]).unwrap())
                        ],
                    )
                    .unwrap();
            }
            bundle
                .execute(
                    "INSERT INTO meta VALUES('library_id','owned-shared-store')",
                    [],
                )
                .unwrap();
            bundle.execute_batch("PRAGMA journal_mode=DELETE").unwrap();
        }
        assert_eq!(
            validate_bundle(&staged, LIST_WAVEFORM_ENCODING_I8, "owned-shared-store").unwrap(),
            2
        );
        // Keep old alive throughout the actual close/rename/reopen. Before the
        // fix Windows denies rename; POSIX leaves old on the replaced inode.
        current.replace_with(&staged).unwrap();
        assert!(!staged.exists());
        for store in [&old, &current] {
            assert_eq!(store.len().unwrap(), 3);
            assert_eq!(store.get(1).unwrap().unwrap().etag, "page");
            assert_eq!(store.get(2).unwrap().unwrap().etag, "bundle");
            assert!(store.get(3).unwrap().is_some());
            assert!(store.get(90).unwrap().is_none());
        }
        old.upsert(&row(4, "after-replace", vec![-4, 4])).unwrap();
        assert_eq!(current.get(4).unwrap().unwrap().etag, "after-replace");
        assert_eq!(unrelated.len().unwrap(), 1);
        assert_eq!(unrelated.get(90).unwrap().unwrap().etag, "unrelated");
    }

    #[test]
    fn reopening_waits_for_the_last_native_connection_to_close_before_replacing_a_bundle() {
        use std::sync::mpsc::{self, TryRecvError};
        use std::time::Duration;

        struct ReleaseOnDrop(Option<mpsc::Sender<()>>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let deadline = Duration::from_secs(5);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(WAVEFORMS_DB_FILE);
        let old = WaveformsDb::open(&path).unwrap();
        old.upsert(&row(1, "page", vec![-2, 2])).unwrap();
        let entry = old.conn._release.0.clone();
        let (entered, did_enter) = mpsc::channel();
        let (release, can_close) = mpsc::channel();
        // Releases the destructor even if a later assertion unwinds.
        let release_on_unwind = ReleaseOnDrop(Some(release));
        *old.conn.before_close.lock().unwrap() = Some(BeforeConnectionClose {
            entered,
            release: can_close,
        });
        let (waiter, did_wait) = mpsc::channel();
        *entry.waiter.lock().unwrap() = Some(waiter);
        let (closed, did_close) = mpsc::channel();
        let dropping = std::thread::spawn(move || {
            drop(old);
            let _ = closed.send(());
        });
        // The final strong owner expired, but the real native connection is
        // deliberately still alive before its declared first field drops.
        did_enter.recv_timeout(deadline).unwrap();
        let (opened, did_open) = mpsc::channel();
        let opening = std::thread::spawn(move || {
            let result = WaveformsDb::open(&path);
            let _ = opened.send(result);
        });
        // This notification comes from the actual lifecycle wait, rather than
        // assuming that an unstarted or slow worker has already called open.
        did_wait.recv_timeout(deadline).unwrap();
        assert!(matches!(did_open.try_recv(), Err(TryRecvError::Empty)));
        drop(release_on_unwind);
        did_close.recv_timeout(deadline).unwrap();
        dropping.join().unwrap();
        let current = did_open.recv_timeout(deadline).unwrap().unwrap();
        opening.join().unwrap();
        let staged = directory.path().join("waveforms.db.bundle-0-0.tmp");
        {
            let bundle = open_connection(&staged).unwrap();
            bundle
                .execute(
                    "INSERT INTO list_waveforms VALUES(2,'bundle',0,0,'i8-minmax-base64',1,?1)",
                    [padded_blob(&ListWaveformI8::new(vec![-1, 1]).unwrap())],
                )
                .unwrap();
            bundle
                .execute(
                    "INSERT INTO meta VALUES('library_id','owned-retired-store')",
                    [],
                )
                .unwrap();
            bundle.execute_batch("PRAGMA journal_mode=DELETE").unwrap();
        }
        assert_eq!(
            validate_bundle(&staged, LIST_WAVEFORM_ENCODING_I8, "owned-retired-store").unwrap(),
            1
        );
        current.replace_with(&staged).unwrap();
        assert_eq!(current.get(1).unwrap().unwrap().etag, "page");
        assert_eq!(current.get(2).unwrap().unwrap().etag, "bundle");
        assert!(!staged.exists());
    }

    #[test]
    fn opening_removes_stale_bundle_downloads_of_other_processes_only() {
        let directory = tempfile::tempdir().unwrap();
        let dir = directory.path();
        let stale = dir.join("waveforms.db.bundle-0-3.tmp");
        let stale_journal = dir.join("waveforms.db.bundle-0-3.tmp-journal");
        let own = dir.join(format!("waveforms.db.bundle-{}-0.tmp", std::process::id()));
        let unrelated = dir.join("library.db");
        let stale_etag = dir.join("waveforms.db.etag-0.tmp");
        let own_etag = dir.join(format!("waveforms.db.etag-{}.tmp", std::process::id()));
        let etag = dir.join("waveforms.db.etag");
        for file in [
            &stale,
            &stale_journal,
            &own,
            &unrelated,
            &stale_etag,
            &own_etag,
            &etag,
        ] {
            std::fs::write(file, b"x").unwrap();
        }
        let _db = WaveformsDb::open(&dir.join(WAVEFORMS_DB_FILE)).unwrap();
        assert!(!stale.exists() && !stale_journal.exists() && !stale_etag.exists());
        assert!(own.exists() && unrelated.exists() && own_etag.exists() && etag.exists());
    }

    #[test]
    #[allow(clippy::result_large_err)] // mirrors rusqlite's `Connection::close` signature
    fn a_failed_close_keeps_the_file_connection() {
        let directory = tempfile::tempdir().unwrap();
        let db = WaveformsDb::open(&directory.path().join(WAVEFORMS_DB_FILE)).unwrap();
        db.upsert(&row(3, "e", vec![-1, 1])).unwrap();
        {
            let mut conn = db.conn.lock().unwrap();
            let failed =
                close_or_restore(&mut conn, |old| Err((old, rusqlite::Error::InvalidQuery)));
            assert!(failed.is_err());
        }
        assert_eq!(db.len().unwrap(), 1, "still the file store, not :memory:");
        assert_eq!(db.get(3).unwrap().unwrap().etag, "e");
    }

    #[test]
    fn rows_with_a_bad_blob_or_n_read_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let db = WaveformsDb::open(&directory.path().join(WAVEFORMS_DB_FILE)).unwrap();
        let conn = db.conn.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO list_waveforms VALUES(1,'x',0,0,'i8-minmax-base64',1,zeroblob(255));
             INSERT INTO list_waveforms VALUES(2,'x',0,0,'i8-minmax-base64',129,zeroblob(256));",
        )
        .unwrap();
        drop(conn);
        assert!(db.get(1).unwrap().is_none());
        assert!(db.get(2).unwrap().is_none());
    }
}
