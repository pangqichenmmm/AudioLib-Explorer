// crates/audiolib-db/src/albums.rs
//! Shared album identities and folder membership, independent of individual WAV covers.
use super::*;
use audiolib_core::{Album, AlbumFacet, AlbumKind, MAX_ALBUM_MEMBERS};

/// Restrict the candidate scan to one parent directory before applying the limit.
/// The folder index orders equal folder IDs by rowid, so unrelated members of a
/// large shared album cannot turn this into an album-wide scan. Materialization
/// keeps full Metadata hydration limited to the selected present candidates.
pub(crate) fn members_in_folder_sql(conn: &Connection) -> Result<String> {
    let projection = audio_projection(conn)?;
    Ok(format!(
        "WITH artwork_candidates AS MATERIALIZED (
           SELECT id FROM audio_files INDEXED BY idx_audio_files_folder
           WHERE folder_id=?1 AND album_id=?2 AND missing=0 ORDER BY id LIMIT ?3
         )
         SELECT {projection} FROM artwork_candidates candidates
         JOIN audio_files af ON af.id=candidates.id ORDER BY af.id"
    ))
}

/// Path keys only narrow the scan; the source identity predicates preserve case
/// and literal POSIX backslashes before the limit. Legacy NULL keys use the
/// NULL-folder index range. Each branch needs only its first `limit` identities
/// before merging, and full Metadata is hydrated only after the final limit.
pub(crate) fn unassigned_directory_members_sql(
    conn: &Connection,
    directory: &str,
) -> Result<String> {
    let projection = audio_projection(conn)?;
    let candidates = if directory.is_empty() {
        "SELECT id FROM audio_files INDEXED BY idx_audio_files_folder
         WHERE folder_id IS NULL AND album_id=?1 AND missing=0
           AND length(audiolib_path_identity(file_path))>0
           AND instr(audiolib_path_identity(file_path),'/')=0
         ORDER BY id LIMIT ?2"
    } else {
        "SELECT id FROM (
           SELECT id FROM audio_files INDEXED BY idx_audio_files_path_key
           WHERE path_key>=?1 COLLATE BINARY AND path_key<?2 COLLATE BINARY
             AND folder_id IS NULL AND album_id=?3 AND missing=0
             AND substr(audiolib_path_identity(file_path),1,length(?4))=?4 COLLATE BINARY
             AND length(audiolib_path_identity(file_path))>length(?4)
             AND instr(substr(audiolib_path_identity(file_path),length(?4)+1),'/')=0
           ORDER BY id LIMIT ?5
         )
         UNION
         SELECT id FROM (
           SELECT id FROM audio_files INDEXED BY idx_audio_files_folder
           WHERE folder_id IS NULL AND path_key IS NULL AND album_id=?3 AND missing=0
             AND substr(audiolib_path_identity(file_path),1,length(?4))=?4 COLLATE BINARY
             AND length(audiolib_path_identity(file_path))>length(?4)
             AND instr(substr(audiolib_path_identity(file_path),length(?4)+1),'/')=0
           ORDER BY id LIMIT ?5
         )
         ORDER BY id LIMIT ?5"
    };
    Ok(format!(
        "WITH artwork_candidates AS MATERIALIZED ({candidates})
         SELECT {projection} FROM artwork_candidates candidates
         JOIN audio_files af ON af.id=candidates.id ORDER BY af.id"
    ))
}

fn unassigned_directory_values(id: i64, directory: &str, limit: usize) -> Vec<Value> {
    if directory.is_empty() {
        return vec![Value::Integer(id), Value::Integer(limit as i64)];
    }
    let prefix = if directory == "/" {
        "/".to_owned()
    } else {
        format!("{directory}/")
    };
    // Match path_key's folding without dropping separators that originated from
    // a literal POSIX backslash at the end of the directory name.
    let index_prefix = prefix.replace('\\', "/").to_lowercase();
    let upper = format!("{}0", index_prefix.strip_suffix('/').unwrap());
    vec![
        Value::Text(index_prefix),
        Value::Text(upper),
        Value::Integer(id),
        Value::Text(prefix),
        Value::Integer(limit as i64),
    ]
}

/// Unknown SQL may expect JSON text even when it still compiles with a BLOB.
/// Keep the whole library in text mode rather than changing those semantics.
pub(crate) fn dependency_safe(conn: &Connection) -> Result<bool> {
    // STRICT TEXT cannot store a compressed BLOB even without a CHECK clause.
    // ANY remains eligible, subject to the extension-dependency checks below.
    let strict_text: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_list t JOIN pragma_table_xinfo('audio_files') c WHERE t.schema='main' AND t.name='audio_files' COLLATE NOCASE AND t.strict=1 AND c.name='metadata_json' COLLATE NOCASE AND c.type='TEXT' COLLATE NOCASE)",
        [],|row|row.get(0),
    ).map_err(db_error)?;
    if strict_text {
        return Ok(false);
    }
    // UNIQUE/PRIMARY KEY constraints may be represented by autoindexes whose SQL
    // is NULL. BLOB equality must not replace the original TEXT/collation rules.
    let constrained_metadata: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('audio_files','main')
           WHERE name='metadata_json' COLLATE NOCASE AND pk>0)
         OR EXISTS(SELECT 1 FROM pragma_index_list('audio_files','main') i
           JOIN pragma_index_xinfo(i.name,'main') c
           WHERE c.name='metadata_json' COLLATE NOCASE AND c.\"key\"=1
             AND (i.\"unique\"=1 OR COALESCE(c.coll,'BINARY')<>'BINARY' COLLATE NOCASE))",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if constrained_metadata {
        return Ok(false);
    }
    let mut statement = conn
        .prepare(
            "SELECT name,type,tbl_name,COALESCE(sql,'') FROM sqlite_schema WHERE sql IS NOT NULL",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(db_error)?;
    for row in rows {
        let (name, kind, table, sql) = row.map_err(db_error)?;
        let sql = sql.to_ascii_lowercase();
        let owned = [
            "audio_files_ai",
            "audio_files_au",
            "audio_files_ad",
            "audiolib_audio_id_high_water",
            "audiolib_album_id_high_water",
            "audiolib_album_audio_insert",
            "audiolib_album_audio_update",
            "audiolib_album_folder_delete",
        ];
        if owned.contains(&name.as_str()) {
            continue;
        }
        if (kind == "trigger" && (table == "audio_files" || sql.contains("audio_files")))
            || (kind == "view" && sql.contains("audio_files"))
            || (kind == "index" && sql.contains("metadata_json"))
            || (kind == "table"
                && sql.contains("metadata_json")
                && (sql.contains("check") || sql.contains("generated") || sql.contains("collate")))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn configure_compression(conn: &Connection, enabled: bool) -> Result<()> {
    conn.execute(
        "UPDATE audiolib_album_id_sequence SET compression=?1 WHERE singleton=1",
        [i64::from(enabled)],
    )
    .map(drop)
    .map_err(db_error)
}

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audiolib_album_id_sequence (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), high_water INTEGER NOT NULL CHECK(high_water>=0), namespace TEXT NOT NULL, legacy_scope TEXT, compression INTEGER NOT NULL DEFAULT 0 CHECK(compression IN (0,1)));

CREATE TABLE IF NOT EXISTS albums (
 id INTEGER PRIMARY KEY, album_key TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('metadata','folder')), metadata_key TEXT,
 folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL);
CREATE INDEX IF NOT EXISTS idx_albums_folder ON albums(folder_id);
CREATE TRIGGER IF NOT EXISTS audiolib_album_id_high_water AFTER INSERT ON albums BEGIN
 UPDATE audiolib_album_id_sequence SET high_water=MAX(high_water,new.id) WHERE singleton=1;
END;";

fn bucket(alias: &str) -> String {
    format!(
        "COALESCE((SELECT ancestor_id FROM folder_closure WHERE descendant_id={alias}.folder_id ORDER BY depth DESC LIMIT 1 OFFSET 1),{alias}.folder_id)"
    )
}
fn key_expression(alias: &str) -> String {
    format!(
        "CASE WHEN audiolib_album_key({alias}.metadata_json)<>'' THEN 'metadata:'||audiolib_album_key({alias}.metadata_json) ELSE COALESCE('folder:'||{},'folder:unassigned') END",
        bucket(alias)
    )
}
fn upsert(alias: &str) -> String {
    let bucket = bucket(alias);
    format!("INSERT OR IGNORE INTO albums(id,album_key,name,kind,metadata_key,folder_id)
 SELECT (SELECT high_water+1 FROM audiolib_album_id_sequence WHERE singleton=1),{},
 CASE WHEN audiolib_album_key({alias}.metadata_json)<>'' THEN audiolib_album_key({alias}.metadata_json) ELSE COALESCE((SELECT folder_name FROM folders WHERE id={bucket}),'Unassigned library') END,
 CASE WHEN audiolib_album_key({alias}.metadata_json)<>'' THEN 'metadata' ELSE 'folder' END,
 NULLIF(audiolib_album_key({alias}.metadata_json),''),
 CASE WHEN audiolib_album_key({alias}.metadata_json)='' THEN {bucket} ELSE NULL END;",key_expression(alias))
}

pub(crate) fn install_triggers(conn: &Connection) -> Result<()> {
    let upsert = upsert("new");
    let key = key_expression("new");
    conn.execute_batch(&format!("
DROP TRIGGER IF EXISTS main.audiolib_album_audio_insert;
DROP TRIGGER IF EXISTS main.audiolib_album_audio_update;
DROP TRIGGER IF EXISTS main.audiolib_album_folder_delete;
CREATE TRIGGER audiolib_album_audio_insert AFTER INSERT ON audio_files BEGIN
 {upsert} UPDATE audio_files SET album_id=(SELECT id FROM albums WHERE album_key={key}) WHERE id=new.id;
END;
CREATE TRIGGER audiolib_album_audio_update AFTER UPDATE OF metadata_json,folder_id ON audio_files BEGIN
 {upsert} UPDATE audio_files SET album_id=(SELECT id FROM albums WHERE album_key={key}) WHERE id=new.id;
END;
CREATE TRIGGER audiolib_album_folder_delete BEFORE DELETE ON folders BEGIN
 UPDATE albums SET album_key='retired:'||id,folder_id=NULL WHERE folder_id=old.id;
END;" )).map_err(db_error)
}

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).map_err(db_error)?;
    conn.execute("INSERT OR IGNORE INTO audiolib_album_id_sequence(singleton,high_water,namespace) VALUES(1,0,?1)",[uuid::Uuid::new_v4().to_string()]).map(drop).map_err(db_error)
}
pub(crate) fn namespace(conn: &Connection) -> Result<String> {
    conn.query_row(
        "SELECT namespace FROM audiolib_album_id_sequence WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}
pub(crate) fn legacy_scope(conn: &Connection) -> Result<Option<String>> {
    conn.query_row(
        "SELECT legacy_scope FROM audiolib_album_id_sequence WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

pub(crate) fn high_water(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT MAX(high_water,COALESCE((SELECT MAX(id) FROM albums),0)) FROM audiolib_album_id_sequence WHERE singleton=1",[],|row|row.get(0)).map_err(db_error)
}
pub(crate) fn retain_high_water(conn: &Connection, value: i64) -> Result<()> {
    conn.execute(
        "UPDATE audiolib_album_id_sequence SET high_water=MAX(high_water,?1) WHERE singleton=1",
        [value],
    )
    .map(drop)
    .map_err(db_error)
}

/// Fill only an owned staging database before installing extension triggers.
/// Existing entity identities survive repair and search-index rebuilds.
pub(crate) fn backfill(conn: &Connection, cancel: &AtomicBool) -> Result<()> {
    initialize(conn)?;
    let has: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('audio_files') WHERE name='album_id')",
            [],
            |r| r.get(0),
        )
        .map_err(db_error)?;
    if !has {
        conn.execute_batch(
            "ALTER TABLE audio_files ADD COLUMN album_id INTEGER REFERENCES albums(id)",
        )
        .map_err(db_error)?;
    }
    let mut cursor = None;
    let mut cache = std::collections::HashMap::<String, i64>::new();
    let mut cache_bytes = 0usize;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let lower = if cursor.is_some() { ">" } else { ">=" };
        let sql = format!(
            "SELECT af.id,audiolib_album_key(af.metadata_json),{} FROM audio_files af WHERE af.album_id IS NULL AND af.id{lower}?1 ORDER BY af.id LIMIT 5000",
            bucket("af")
        );
        let rows = conn
            .prepare_cached(&sql)
            .map_err(db_error)?
            .query_map([cursor.unwrap_or(i64::MIN)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if rows.is_empty() {
            break;
        }
        for (id, library, folder) in rows {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            let key = if library.is_empty() {
                folder.map_or_else(|| "folder:unassigned".into(), |id| format!("folder:{id}"))
            } else {
                format!("metadata:{library}")
            };
            let album = if let Some(id) = cache.get(&key) {
                *id
            } else {
                let existing: Option<i64> = conn
                    .prepare_cached("SELECT id FROM albums WHERE album_key=?1")
                    .map_err(db_error)?
                    .query_row([&key], |row| row.get(0))
                    .optional()
                    .map_err(db_error)?;
                let album = if let Some(id) = existing {
                    id
                } else {
                    let next:i64=conn.prepare_cached("UPDATE audiolib_album_id_sequence SET high_water=high_water+1 WHERE singleton=1 AND high_water<9223372036854775807 RETURNING high_water").map_err(db_error)?.query_row([],|row|row.get(0)).optional().map_err(db_error)?.ok_or_else(||Error::Validation("Album identity space exhausted".into()))?;
                    let name = if library.is_empty() {
                        folder
                            .map(|folder| {
                                conn.query_row(
                                    "SELECT folder_name FROM folders WHERE id=?1",
                                    [folder],
                                    |row| row.get::<_, String>(0),
                                )
                                .optional()
                                .map_err(db_error)
                            })
                            .transpose()?
                            .flatten()
                            .unwrap_or_else(|| "Unassigned library".into())
                    } else {
                        library.clone()
                    };
                    conn.prepare_cached("INSERT INTO albums(id,album_key,name,kind,metadata_key,folder_id) VALUES(?1,?2,?3,?4,?5,?6)").map_err(db_error)?.execute(params![next,&key,name,if library.is_empty(){"folder"}else{"metadata"},if library.is_empty(){None}else{Some(library.as_str())},if library.is_empty(){folder}else{None}]).map_err(db_error)?;
                    next
                };
                if cache.len() >= 4096 || cache_bytes.saturating_add(key.len()) > 16 * 1024 * 1024 {
                    cache.clear();
                    cache_bytes = 0;
                }
                if key.len() <= 1024 * 1024 {
                    cache_bytes += key.len();
                    cache.insert(key, album);
                }
                album
            };
            conn.prepare_cached("UPDATE audio_files SET album_id=?2 WHERE id=?1")
                .map_err(db_error)?
                .execute(params![id, album])
                .map_err(db_error)?;
            cursor = Some(id);
        }
    }
    conn.execute_batch(schema::ALBUM_INDEX).map_err(db_error)?;
    Ok(())
}

/// Refresh membership after a folder has been attached beneath a wider scan root.
#[cfg(feature = "scanner")]
pub(crate) fn refresh_folder_membership(conn: &Connection, folder: i64) -> Result<()> {
    conn.execute("UPDATE audio_files SET folder_id=folder_id WHERE folder_id IN (SELECT descendant_id FROM folder_closure WHERE ancestor_id=?1) AND album_id IN (SELECT id FROM albums WHERE kind='folder')",[folder]).map(drop).map_err(db_error)?;
    conn.execute("UPDATE albums SET name=COALESCE((SELECT folder_name FROM folders WHERE id=albums.folder_id),name) WHERE kind='folder'",[]).map(drop).map_err(db_error)
}

pub(crate) fn ready(conn: &Connection) -> Result<bool> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='albums') AND EXISTS(SELECT 1 FROM pragma_table_xinfo('audio_files') WHERE name='album_id') AND EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_album_id_sequence')",[],|r|r.get(0)).map_err(db_error)?;
    if !exists {
        return Ok(false);
    }
    let has_scope:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('audiolib_album_id_sequence') WHERE name='legacy_scope')",[],|row|row.get(0)).map_err(db_error)?;
    if !has_scope {
        return Ok(false);
    }
    // A ready marker without membership triggers would admit new orphan records.
    // Compare only owned objects; unrelated extension indexes remain untouched.
    let expected = Connection::open_in_memory().map_err(db_error)?;
    expected.execute_batch("CREATE TABLE folders(id INTEGER PRIMARY KEY,folder_name TEXT); CREATE TABLE folder_closure(ancestor_id,descendant_id,depth); CREATE TABLE audio_files(id INTEGER PRIMARY KEY,metadata_json,folder_id,album_id,missing);").map_err(db_error)?;
    expected.execute_batch(SCHEMA).map_err(db_error)?;
    expected
        .execute_batch(schema::ALBUM_INDEX)
        .map_err(db_error)?;
    install_triggers(&expected)?;
    for name in [
        "audiolib_album_id_high_water",
        "audiolib_album_audio_insert",
        "audiolib_album_audio_update",
        "audiolib_album_folder_delete",
        "idx_albums_folder",
        "idx_audio_visible_album",
        "idx_audio_album_members",
    ] {
        let (kind, table, sql): (String, String, String) = expected
            .query_row(
                "SELECT type,tbl_name,sql FROM sqlite_schema WHERE name=?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(db_error)?;
        if !schema::definition::matches_sql(conn, name, &kind, &table, &sql)? {
            return Ok(false);
        }
    }
    let invalid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM audio_files af LEFT JOIN albums al ON al.id=af.album_id WHERE al.id IS NULL) OR NOT EXISTS(SELECT 1 FROM audiolib_album_id_sequence WHERE singleton=1 AND high_water>=COALESCE((SELECT MAX(id) FROM albums),0))",[],|r|r.get(0)).map_err(db_error)?;
    if invalid {
        return Ok(false);
    }
    let compression: bool = conn
        .query_row(
            "SELECT compression FROM audiolib_album_id_sequence WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if compression && !dependency_safe(conn)? {
        return Ok(false);
    }
    let namespace = namespace(conn)?;
    Ok(uuid::Uuid::parse_str(&namespace).is_ok())
}

pub(crate) const SELECT: &str = "al.id,al.name,al.kind,al.metadata_key,al.folder_id,(SELECT folder_path FROM folders WHERE id=al.folder_id),(SELECT id FROM audio_files WHERE album_id=al.id AND missing=0 ORDER BY id LIMIT 1)";
fn from_row(row: &Row<'_>) -> rusqlite::Result<Album> {
    let kind: String = row.get(2)?;
    Ok(Album {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: if kind == "metadata" {
            AlbumKind::Metadata
        } else {
            AlbumKind::Folder
        },
        metadata_key: row.get(3)?,
        folder_id: row.get(4)?,
        folder_path: row.get(5)?,
        representative_audio_id: row.get(6)?,
    })
}
pub(crate) fn entity_json_expression() -> String {
    "(SELECT json_object('id',al.id,'name',al.name,'kind',al.kind,'metadata_key',al.metadata_key,'folder_id',al.folder_id,'folder_path',(SELECT folder_path FROM folders WHERE id=al.folder_id),'representative_audio_id',(SELECT id FROM audio_files WHERE album_id=al.id AND missing=0 ORDER BY id LIMIT 1)) FROM albums al WHERE al.id=af.album_id)".into()
}

impl SqliteProvider {
    /// Indexed identity lookup for a saved album filter crossing library scopes.
    /// Folder lookup uses stored path identity only, never source filesystem I/O;
    /// a missing folder path means the shared unassigned group, not a retired row.
    pub fn find_album_by_semantic(
        &self,
        kind: AlbumKind,
        metadata_key: Option<&str>,
        folder_path: Option<&str>,
    ) -> Result<Option<Album>> {
        let reader = self.reader()?;
        let key = match kind {
            AlbumKind::Metadata => {
                let Some(key) = metadata_key.filter(|key| !key.is_empty()) else {
                    return Ok(None);
                };
                format!("metadata:{key}")
            }
            AlbumKind::Folder => match folder_path {
                Some(path) => {
                    let Some(id) = path_identity::folder_id_with_alias(&reader, path, false)?
                    else {
                        return Ok(None);
                    };
                    format!("folder:{id}")
                }
                None => "folder:unassigned".into(),
            },
        };
        reader
            .query_row(
                &format!("SELECT {SELECT} FROM albums al WHERE al.album_key=?1 AND al.kind=?2"),
                params![
                    key,
                    match kind {
                        AlbumKind::Metadata => "metadata",
                        AlbumKind::Folder => "folder",
                    }
                ],
                from_row,
            )
            .optional()
            .map_err(db_error)
    }
    /// Reuse client-local identities across legacy-server snapshot revisions.
    /// The destination must be an unpublished owned copy with no active readers.
    /// The previous snapshot is opened read-only; native schema-8 server IDs must
    /// never be passed through this adapter.
    pub fn preserve_local_album_ids(
        previous: &Path,
        owned_new: &Path,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if previous.canonicalize()? == owned_new.canonicalize()? {
            return Err(Error::Validation(
                "Album identity adaptation requires a separate owned snapshot".into(),
            ));
        }
        let old = Connection::open_with_flags(previous, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        let mut new = Connection::open_with_flags(owned_new, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        if !ready(&old)? || !ready(&new)? {
            return Err(Error::Validation(
                "Album identity adaptation requires complete local entities".into(),
            ));
        }
        let semantic = |album: &Album| match album.kind {
            AlbumKind::Metadata => format!(
                "metadata:{}",
                album.metadata_key.as_deref().unwrap_or(&album.name)
            ),
            AlbumKind::Folder => format!(
                "folder:{}",
                album
                    .folder_path
                    .as_deref()
                    .map(path_identity::comparison_key)
                    .unwrap_or_default()
            ),
        };
        let load = |conn: &Connection| -> Result<Vec<Album>> {
            conn.prepare(&format!("SELECT {SELECT} FROM albums al ORDER BY al.id"))
                .map_err(db_error)?
                .query_map([], from_row)
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)
        };
        let old_albums = load(&old)?;
        let old_by_key: std::collections::HashMap<_, _> = old_albums
            .iter()
            .map(|album| (semantic(album), album.id))
            .collect();
        let new_albums = load(&new)?;
        let mut high = high_water(&old)?.max(high_water(&new)?);
        new.execute_batch("PRAGMA foreign_keys=OFF;")
            .map_err(db_error)?;
        let tx = new.transaction().map_err(db_error)?;
        tx.execute_batch("CREATE TEMP TABLE album_identity_map(before INTEGER PRIMARY KEY,after INTEGER NOT NULL UNIQUE);").map_err(db_error)?;
        for album in new_albums {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            let id = if let Some(id) = old_by_key.get(&semantic(&album)) {
                *id
            } else {
                high = high
                    .checked_add(1)
                    .ok_or_else(|| Error::Validation("Album identity space exhausted".into()))?;
                high
            };
            tx.execute(
                "INSERT INTO album_identity_map VALUES(?1,?2)",
                params![album.id, id],
            )
            .map_err(db_error)?;
        }
        tx.execute("UPDATE audio_files SET album_id=(SELECT after FROM album_identity_map WHERE before=audio_files.album_id)",[]).map_err(db_error)?;
        tx.execute("UPDATE albums SET id=-id", [])
            .map_err(db_error)?;
        tx.execute(
            "UPDATE albums SET id=(SELECT after FROM album_identity_map WHERE before=-albums.id)",
            [],
        )
        .map_err(db_error)?;
        tx.execute("UPDATE audiolib_album_id_sequence SET high_water=?1,namespace=?2,legacy_scope=?3 WHERE singleton=1",params![high,namespace(&old)?,legacy_scope(&old)?]).map_err(db_error)?;
        if !ready(&tx)? {
            return Err(Error::Validation(
                "Adapted album membership is incomplete".into(),
            ));
        }
        tx.execute_batch("DROP TABLE album_identity_map;")
            .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
    /// True only when the actual snapshot owns complete shared album identities.
    pub fn has_current_album_state(path: &Path) -> Result<bool> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        ready(&conn)
    }
    /// Resolve the identity independently of a current search result.
    pub fn get_album(&self, id: i64) -> Result<Album> {
        if id <= 0 {
            return Err(Error::Validation("Album identity must be positive".into()));
        }
        let reader = self.reader()?;
        reader
            .query_row(
                &format!("SELECT {SELECT} FROM albums al WHERE al.id=?1"),
                [id],
                from_row,
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| Error::NotFound(format!("Album {id}")))
    }
    /// Present candidates from the complete album in stable ID order.
    pub fn album_members(
        &self,
        id: i64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        if id <= 0 || limit == 0 || limit > MAX_ALBUM_MEMBERS {
            return Err(Error::Validation(
                "Album candidates require a positive ID and limit 1..=64".into(),
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let reader = self.search_reader(Some(cancel.clone()))?;
        let projection = audio_projection(&reader)?;
        let files=reader.prepare(&format!("SELECT {projection} FROM audio_files af WHERE af.album_id=?1 AND af.missing=0 ORDER BY af.id LIMIT ?2")).map_err(db_error)?.query_map(params![id,limit as i64],audio_from_row).map_err(db_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?;
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        Ok(files)
    }
    /// Present candidates from one exact WAV parent directory, in stable ID order.
    /// Empty or unknown directories stay empty; other album members are never used.
    pub fn album_members_in_folder(
        &self,
        id: i64,
        folder_id: i64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        if id <= 0 || limit == 0 || limit > MAX_ALBUM_MEMBERS {
            return Err(Error::Validation(
                "Album candidates require a positive ID and limit 1..=64".into(),
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let reader = self.search_reader(Some(cancel.clone()))?;
        let files = reader
            .prepare(&members_in_folder_sql(&reader)?)
            .map_err(db_error)?
            .query_map(params![folder_id, id, limit as i64], audio_from_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        Ok(files)
    }
    /// Present members without a folder ID, restricted to one source parent path.
    /// Directory syntax is normalized without I/O; no other directory is a fallback.
    pub fn album_members_in_unassigned_directory(
        &self,
        id: i64,
        directory: &str,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        if id <= 0
            || limit == 0
            || limit > MAX_ALBUM_MEMBERS
            || directory.len() > 4096
            || directory.contains('\0')
        {
            return Err(Error::Validation(
                "Album candidates require a positive ID, limit 1..=64 and a NUL-free directory up to 4096 UTF-8 bytes".into(),
            ));
        }
        let directory = normalize_source_path_identity(directory);
        if directory.len() > 4096 {
            return Err(Error::Validation(
                "Normalized source directory exceeds 4096 UTF-8 bytes".into(),
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let reader = self.search_reader(Some(cancel.clone()))?;
        let files = reader
            .prepare(&unassigned_directory_members_sql(&reader, &directory)?)
            .map_err(db_error)?
            .query_map(
                params_from_iter(unassigned_directory_values(id, &directory, limit)),
                audio_from_row,
            )
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        Ok(files)
    }
    /// Covering-index pages of all identities, including missing members.
    pub fn album_member_ids(
        &self,
        id: i64,
        after_audio_id: Option<i64>,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<i64>> {
        if id <= 0 || limit == 0 || limit > 4096 {
            return Err(Error::Validation(
                "Album identity pages require a positive album ID and limit 1..=4096".into(),
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let reader = self.search_reader(Some(cancel.clone()))?;
        let sql = if after_audio_id.is_some() {
            "SELECT id FROM audio_files WHERE album_id=?1 AND id>?2 ORDER BY id LIMIT ?3"
        } else {
            "SELECT id FROM audio_files WHERE album_id=?1 AND id>=?2 ORDER BY id LIMIT ?3"
        };
        let ids = reader
            .prepare(sql)
            .map_err(db_error)?
            .query_map(
                params![id, after_audio_id.unwrap_or(i64::MIN), limit as i64],
                |row| row.get(0),
            )
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(db_error)?;
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        Ok(ids)
    }
    pub(crate) fn entity_facets(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AlbumFacet>> {
        request.validate()?;
        let reader = self.search_reader(Some(cancel.clone()))?;
        let sql = search_sql(
            &reader,
            &self.tokenizer,
            request,
            true,
            self.derived_tags.load(Ordering::Acquire),
        )?;
        let counts = format!(
            "SELECT af.album_id,COUNT(*) AS matches FROM {} WHERE {} GROUP BY af.album_id",
            sql.from,
            sql.conditions.join(" AND ")
        );
        let mut statement=reader.prepare(&format!("SELECT {SELECT},c.matches FROM albums al JOIN ({counts}) c ON c.album_id=al.id ORDER BY c.matches DESC,al.name COLLATE NOCASE,al.id")).map_err(db_error)?;
        let rows = statement
            .query_map(params_from_iter(&sql.values), |row| {
                Ok(AlbumFacet {
                    album: from_row(row)?,
                    matching_count: unsigned(row, 7)?,
                })
            })
            .map_err(db_error)?;
        let mut result = Vec::new();
        for row in rows {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            result.push(row.map_err(db_error)?);
        }
        Ok(result)
    }
}
