// crates/audiolib-db/src/migration/inspect.rs
//! Pinned, read-only observations and typed business-data digests.
use crate::{db_error, schema, search_storage};
use audiolib_core::{
    Error, Result,
    compatibility::{
        CompatibilityDecision, LibraryAccess, LibraryFormat, MigrationEvidence, PendingMigration,
        SearchStorage, assess_library,
    },
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// Stable typed digest of one business table, preserving NULLs and raw Metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TableDigest {
    /// Actual table name; no SQL identifier is interpolated without quoting.
    pub table: String,
    /// Compared columns, excluding explicitly regenerated derived values.
    pub columns: Vec<String>,
    /// Number of rows actually traversed.
    pub rows: u64,
    /// SHA256 of typed, length-delimited values in stable identity order.
    pub sha256: String,
}

/// Business-data evidence suitable for comparing source, candidate and rollback.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BusinessDigest {
    /// Each ordinary business table, including unknown extensions.
    pub tables: Vec<TableDigest>,
    /// Digest of the ordered table descriptors and digests.
    pub sha256: String,
}

/// Owned inspection result. A plan must revalidate it after obtaining write exclusion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibraryInspection {
    /// Canonical source identity; local reports only, never a remote credential.
    pub source: PathBuf,
    /// Actual persistent format, including unknown feature names.
    pub format: LibraryFormat,
    /// Reasons supporting the compatibility classification.
    pub evidence: MigrationEvidence,
    /// Classifies the requested read or write access.
    pub decision: CompatibilityDecision,
    /// Present only when a complete business scan was requested and succeeded.
    pub business: Option<BusinessDigest>,
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1 COLLATE NOCASE)",
        [table],
        |row| row.get(0),
    )
    .map_err(db_error)
}

fn revision(conn: &Connection, key: &str) -> Result<Option<u32>> {
    if !table_exists(conn, "library_meta")? {
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM library_meta WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    value
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| Error::Validation(format!("Invalid revision value for {key}")))
        })
        .transpose()
}

pub(super) fn ordinary_tables(conn: &Connection) -> Result<Vec<String>> {
    conn.prepare(
        "SELECT name FROM pragma_table_list WHERE schema='main' AND type='table'
         AND name NOT GLOB 'sqlite_*' ORDER BY name",
    )
    .map_err(db_error)?
    .query_map([], |row| row.get(0))
    .map_err(db_error)?
    .collect::<rusqlite::Result<_>>()
    .map_err(db_error)
}

fn identified(conn: &Connection, version: u32) -> Result<bool> {
    if schema::validate_library_identity(conn).is_err() {
        return Ok(false);
    }
    if version <= 2 {
        return Ok(true);
    }
    // A table merely named audio_files is insufficient authority to replace a file.
    for name in [
        "folders",
        "folder_closure",
        "playlists",
        "playlist_items",
        "project_references",
        "waveform_cache",
        "scan_sessions",
    ] {
        if !table_exists(conn, name)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Inspect a fixed SQLite read snapshot without creating or upgrading the source.
/// Deep inspection reads all business values; shallow inspection never asserts
/// that unread business data is sound. Cancellation does not mutate the source.
pub fn inspect(
    path: &Path,
    access: LibraryAccess,
    deep: bool,
    cancel: &AtomicBool,
) -> Result<LibraryInspection> {
    check_cancel(cancel)?;
    let source = path.canonicalize()?;
    let conn =
        Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_error)?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN; SELECT COUNT(*) FROM sqlite_schema;")
        .map_err(db_error)?;
    inspect_connection(&conn, source, access, deep, cancel)
}

/// Observe only identity/format metadata; no business row or ID-watermark reads.
pub(super) fn observe_format(
    conn: &Connection,
) -> Result<(LibraryFormat, bool, Option<PendingMigration>)> {
    let schema_version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    let identified_library = identified(conn, schema_version)?;
    let features: BTreeMap<String, u32> = if table_exists(conn, "audiolib_schema_features")? {
        conn.prepare("SELECT feature,version FROM audiolib_schema_features ORDER BY feature")
            .map_err(db_error)?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error)?
    } else {
        BTreeMap::new()
    };
    let pending = if schema::pending(conn)? {
        Some(
            conn.query_row(
                "SELECT protocol,target FROM audiolib_search_upgrade WHERE singleton=1",
                [],
                |row| {
                    Ok(PendingMigration {
                        protocol: "search_text".into(),
                        revision: row.get(0)?,
                        target: row.get(1)?,
                    })
                },
            )
            .map_err(db_error)?,
        )
    } else if table_exists(conn, "audiolib_storage_migration")? {
        Some(
            conn.query_row(
                "SELECT protocol,target FROM audiolib_storage_migration WHERE singleton=1",
                [],
                |row| {
                    Ok(PendingMigration {
                        protocol: "search_storage".into(),
                        revision: row.get(0)?,
                        target: row.get(1)?,
                    })
                },
            )
            .map_err(db_error)?,
        )
    } else {
        None
    };
    let storage = if identified_library && search_storage::structure_matches(conn)? {
        SearchStorage::ContentlessDelete
    } else if identified_library && schema::triggers_match(conn)? {
        SearchStorage::ExternalContent
    } else {
        SearchStorage::Unknown
    };
    let format = LibraryFormat {
        schema_version,
        storage,
        features,
        source_search_revision: revision(conn, "source_search_rev")?,
        source_tags_revision: revision(conn, "source_tags_rev")?,
    };
    Ok((format, identified_library, pending))
}

pub(super) fn inspect_connection(
    conn: &Connection,
    source: PathBuf,
    access: LibraryAccess,
    deep: bool,
    cancel: &AtomicBool,
) -> Result<LibraryInspection> {
    let (format, identified_library, pending) = observe_format(conn)?;
    let columns = search_storage::columns(conn)?;
    let can_regenerate_search = [
        "id",
        "filename",
        "user_tags",
        "user_notes",
        "bwf_description",
        "metadata_json",
        "ixml_scene",
        "ixml_take",
    ]
    .iter()
    .all(|required| {
        columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(required))
    });
    let retirement_ready = !matches!(format.features.get("play_history_retired"), Some(1 | 2))
        || (!columns.iter().any(|column| {
            column.eq_ignore_ascii_case("play_count")
                || (format.features.get("play_history_retired") == Some(&2)
                    && column.eq_ignore_ascii_case("last_played_at"))
        }) && !table_exists(conn, "play_history")?);
    let search_ready = pending.is_none()
        && retirement_ready
        && format.snapshot_format_id().is_some()
        && schema::search_state_is_current(conn)?;
    let mut evidence = MigrationEvidence {
        identified_library,
        business_integrity: None,
        can_regenerate_search,
        search_ready,
        pending,
    };
    // Do not reinterpret a future format as damaged because this binary cannot
    // understand it. Only a positively identified, understood source is traversed.
    let preliminary = assess_library(&format, &evidence, access);
    let business = if deep
        && identified_library
        && !preliminary
            .reasons
            .contains(&audiolib_core::compatibility::CompatibilityReason::ToolUpgradeRequired)
    {
        let digest = business_digest(conn, cancel)?;
        evidence.business_integrity = Some(true);
        Some(digest)
    } else {
        None
    };
    let decision = assess_library(&format, &evidence, access);
    Ok(LibraryInspection {
        source,
        format,
        evidence,
        decision,
        business,
    })
}

pub(super) fn business_digest(conn: &Connection, cancel: &AtomicBool) -> Result<BusinessDigest> {
    business_digest_matching(conn, None, cancel)
}

/// Compare the original projection after additive legacy normalization. Additional
/// schema columns/tables are allowed, but every original business value must survive.
pub(super) fn business_digest_matching(
    conn: &Connection,
    reference: Option<&BusinessDigest>,
    cancel: &AtomicBool,
) -> Result<BusinessDigest> {
    business_digest_retained(conn, reference, &[], cancel)
}

pub(super) fn business_digest_retained(
    conn: &Connection,
    reference: Option<&BusinessDigest>,
    retired_tables: &[String],
    cancel: &AtomicBool,
) -> Result<BusinessDigest> {
    business_digest_impl(conn, reference, retired_tables, cancel, None)
}

/// Reconstruct only the two explicitly admitted local identity values while
/// hashing every retained business byte; never remove namespace from the digest.
pub(super) fn business_digest_before_import(
    conn: &Connection,
    reference: &BusinessDigest,
    namespace: &str,
    legacy_scope: Option<&str>,
    cancel: &AtomicBool,
) -> Result<BusinessDigest> {
    business_digest_impl(
        conn,
        Some(reference),
        &[],
        cancel,
        Some((namespace, legacy_scope)),
    )
}

fn business_digest_impl(
    conn: &Connection,
    reference: Option<&BusinessDigest>,
    retired_tables: &[String],
    cancel: &AtomicBool,
    identity: Option<(&str, Option<&str>)>,
) -> Result<BusinessDigest> {
    let mut tables = Vec::new();
    let names = match reference {
        Some(reference) => reference
            .tables
            .iter()
            .map(|table| table.table.clone())
            .collect(),
        None => {
            let mut names = ordinary_tables(conn)?;
            if table_exists(conn, "sqlite_sequence")? {
                names.push("sqlite_sequence".into());
                names.sort();
            }
            names
        }
    };
    for table in names {
        check_cancel(cancel)?;
        if [
            "audiolib_schema_features",
            "audiolib_search_upgrade",
            "audiolib_storage_migration",
            "audiolib_migration_receipts",
        ]
        .contains(&table.as_str())
        {
            continue;
        }
        let info = conn
            .prepare("SELECT name,pk FROM pragma_table_xinfo(?1) ORDER BY cid")
            .map_err(db_error)?
            .query_map([&table], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let mut columns: Vec<String> = info
            .iter()
            .filter(|(name, _)| {
                (!table.eq_ignore_ascii_case("audio_files")
                    || (!search_storage::is_derived_column(name)
                        && !name.eq_ignore_ascii_case("source_tags_text")
                        && !name.eq_ignore_ascii_case("album")))
                    && !(table.eq_ignore_ascii_case("audiolib_album_id_sequence")
                        && name.eq_ignore_ascii_case("compression"))
            })
            .map(|(name, _)| name.clone())
            .collect();
        let mut primary: Vec<_> = info.iter().filter(|(_, order)| *order > 0).collect();
        primary.sort_by_key(|(_, order)| *order);
        let without_rowid: bool = conn
            .query_row(
                "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
                [&table],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let order = if !without_rowid {
            let hidden = ["rowid", "_rowid_", "oid"]
                .into_iter()
                .find(|name| {
                    !info
                        .iter()
                        .any(|(column, _)| column.eq_ignore_ascii_case(name))
                })
                .ok_or_else(|| {
                    Error::Validation(format!(
                        "Cannot address implicit identity for table {table}"
                    ))
                })?;
            columns.insert(0, hidden.into());
            vec![hidden.to_owned()]
        } else {
            primary.iter().map(|(name, _)| name.clone()).collect()
        };
        if let Some(reference) = reference {
            columns = reference
                .tables
                .iter()
                .find(|entry| entry.table.eq_ignore_ascii_case(&table))
                .ok_or_else(|| Error::Validation("Missing digest reference".into()))?
                .columns
                .clone();
        }
        let quote_list = |names: &[String]| {
            names
                .iter()
                .map(|name| search_storage::quote_identifier(name))
                .collect::<Vec<_>>()
                .join(",")
        };
        let filter = if table.eq_ignore_ascii_case("library_meta") {
            " WHERE key NOT IN ('source_search_rev','source_tags_rev','source_tags_progress','source_tags_progress_rev')".to_owned()
        } else if table.eq_ignore_ascii_case("sqlite_sequence") && !retired_tables.is_empty() {
            format!(
                " WHERE name COLLATE NOCASE NOT IN ({})",
                std::iter::repeat_n("?", retired_tables.len())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {} FROM {}{filter} ORDER BY {}",
            quote_list(&columns),
            search_storage::quote_identifier(&table),
            quote_list(&order)
        );
        let mut statement = conn.prepare(&sql).map_err(db_error)?;
        let bindings = if table.eq_ignore_ascii_case("sqlite_sequence") {
            retired_tables
        } else {
            &[]
        };
        let mut rows = statement
            .query(rusqlite::params_from_iter(bindings))
            .map_err(db_error)?;
        let mut hash = Sha256::new();
        let mut count = 0u64;
        while let Some(row) = rows.next().map_err(db_error)? {
            if count.is_multiple_of(1024) {
                check_cancel(cancel)?;
            }
            hash.update([0xff]);
            for (i, column) in columns.iter().enumerate() {
                let value = if table.eq_ignore_ascii_case("audiolib_album_id_sequence") {
                    match (identity, column.as_str()) {
                        (Some((namespace, _)), "namespace") => ValueRef::Text(namespace.as_bytes()),
                        (Some((_, scope)), "legacy_scope") => {
                            scope.map_or(ValueRef::Null, |value| ValueRef::Text(value.as_bytes()))
                        }
                        _ => row.get_ref(i).map_err(db_error)?,
                    }
                } else {
                    row.get_ref(i).map_err(db_error)?
                };
                if table.eq_ignore_ascii_case("audio_files")
                    && column.eq_ignore_ascii_case("metadata_json")
                    && matches!(value, ValueRef::Blob(_))
                {
                    let decoded = crate::metadata_storage::decode_value(value)?;
                    hash.update([3]);
                    hash.update((decoded.len() as u64).to_le_bytes());
                    hash.update(decoded.as_bytes());
                    continue;
                }
                match value {
                    ValueRef::Null => hash.update([0]),
                    ValueRef::Integer(value) => {
                        hash.update([1]);
                        hash.update(value.to_le_bytes());
                    }
                    ValueRef::Real(value) => {
                        hash.update([2]);
                        hash.update(value.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(value) => {
                        hash.update([3]);
                        hash.update((value.len() as u64).to_le_bytes());
                        hash.update(value);
                    }
                    ValueRef::Blob(value) => {
                        hash.update([4]);
                        hash.update((value.len() as u64).to_le_bytes());
                        hash.update(value);
                    }
                }
            }
            count += 1;
        }
        tables.push(TableDigest {
            table,
            columns,
            rows: count,
            sha256: format!("{:x}", hash.finalize()),
        });
    }
    let encoded =
        serde_json::to_vec(&tables).map_err(|error| Error::Database(error.to_string()))?;
    Ok(BusinessDigest {
        tables,
        sha256: format!("{:x}", Sha256::digest(encoded)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_database_is_not_accepted_from_its_version_or_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path,filename); PRAGMA user_version=5;").unwrap();
        drop(conn);
        let result = inspect(&path, LibraryAccess::Write, true, &AtomicBool::new(false)).unwrap();
        assert!(!result.evidence.identified_library);
        assert_eq!(result.decision.disposition, None);
    }

    #[test]
    fn deep_inspection_preserves_source_and_hashes_unknown_business_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let provider = crate::SqliteProvider::open(&path).unwrap();
        drop(provider);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE extension_data(id INTEGER PRIMARY KEY,payload BLOB,notes TEXT);
             INSERT INTO extension_data VALUES(-7,x'00ff','first');",
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&path).unwrap();
        let first = inspect(&path, LibraryAccess::Write, true, &AtomicBool::new(false)).unwrap();
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert_eq!(first.evidence.business_integrity, Some(true));
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE extension_data SET notes='second'", [])
            .unwrap();
        drop(conn);
        let second = inspect(&path, LibraryAccess::Write, true, &AtomicBool::new(false)).unwrap();
        assert_ne!(
            first.business.unwrap().sha256,
            second.business.unwrap().sha256
        );
    }
}
