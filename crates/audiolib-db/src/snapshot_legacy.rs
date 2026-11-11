// crates/audiolib-db/src/snapshot_legacy.rs
//! A schema-8 projection of an exclusively owned schema-9 snapshot, never a live library.
use crate::{SqliteProvider, db_error, dir_search, owned_cancellation, schema};
use audiolib_core::{Error, LibraryInfo, Result, compatibility::PLAN_B_FORMAT};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, sync::atomic::AtomicBool};

// Keep only genuine WAV-family rows. A trusted container or extractor marker also
// excludes renamed MP3/AIFF sources whose historical filename still ends in .wav.
const EXCLUDED_AUDIO: &str = "lower(substr(filename,-4))<>'.wav'
    OR upper(COALESCE(json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.container'),'')) IN ('MP3','AIFF','AIFC')
    OR (json_type(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.mp3_version')='integer'
        AND typeof(json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.mp3_version'))='integer'
        AND json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.mp3_version')>0)
    OR (json_type(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.aiff_version')='integer'
        AND typeof(json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.aiff_version'))='integer'
        AND json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json))
        THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.aiff_version')>0)";

impl SqliteProvider {
    /// Read the WAV-only statistics for a prospective legacy representation without
    /// copying a live schema-9 library. All aggregates share one consistent read
    /// transaction; the caller binds the returned values to its source revision.
    pub fn legacy_snapshot_info_cancellable(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<LibraryInfo> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        crate::register_library_functions(&conn)?;
        owned_cancellation::run(&mut conn, cancel, |conn| {
            let tx = conn.transaction().map_err(db_error)?;
            crate::snapshot_schema9::validate_read(&tx)?;
            let stats = tx
                .query_row(
                    &format!(
                        "SELECT count(*) FILTER(WHERE missing=0),
                 coalesce(sum(duration) FILTER(WHERE missing=0),0),
                 coalesce(sum(file_size) FILTER(WHERE missing=0),0),
                 (SELECT count(*) FROM folders),count(*) FILTER(WHERE missing=1)
                 FROM audio_files WHERE NOT ({EXCLUDED_AUDIO})"
                    ),
                    [],
                    |row| {
                        Ok(audiolib_core::LibraryStats {
                            audio_files: crate::unsigned(row, 0)?,
                            total_duration: row.get(1)?,
                            total_bytes: crate::unsigned(row, 2)?,
                            folders: crate::unsigned(row, 3)?,
                            missing_files: crate::unsigned(row, 4)?,
                        })
                    },
                )
                .map_err(db_error)?;
            let sqlite_version = tx
                .query_row("SELECT sqlite_version()", [], |row| row.get(0))
                .map_err(db_error)?;
            tx.commit().map_err(db_error)?;
            let mut wal = path.as_os_str().to_os_string();
            wal.push("-wal");
            Ok(LibraryInfo {
                database: path.to_path_buf(),
                schema_version: 8,
                sqlite_version,
                database_bytes: std::fs::metadata(path)?.len(),
                wal_bytes: std::fs::metadata(Path::new(&wal))
                    .map(|meta| meta.len())
                    .unwrap_or(0),
                stats,
            })
        })
    }

    /// Downgrade an exclusively owned, unpublished schema-9 backup for legacy readers.
    ///
    /// The caller must own the staging file and discard it on any error. Source audio
    /// and the original library are never opened. WAV business identities and user
    /// annotations and existing main-index text survive; non-WAV rows and newer
    /// normalized storage are excluded. New Metadata mirrors remain opaque extensions
    /// to old readers, preserving the already indexed source text (SC-03/SC-07).
    /// No main-table reconstruction or live-library migration is performed.
    pub fn derive_legacy_snapshot_cancellable(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<LibraryInfo> {
        if !Self::check_owned_file_cancellable(path, cancel)?.is_ok() {
            return Err(Error::Validation(
                "Source owned snapshot failed integrity validation".into(),
            ));
        }
        Self::validate_owned_snapshot_format_cancellable(path, cancel)?;
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        crate::register_library_functions(&conn)?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE;")
            .map_err(db_error)?;
        owned_cancellation::run(&mut conn, cancel, |conn| {
            let version: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .map_err(db_error)?;
            if version != 9 {
                return Err(Error::Validation(
                    "Legacy derivation requires a completed schema-9 owned snapshot".into(),
                ));
            }
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_error)?;
            crate::search_tracking::drop_owned(&tx)?;
            dir_search::drop_tracking(&tx)?;
            let control: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='v9_control')",
                    [],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            if control
                && !schema::definition::matches_sql(
                    &tx,
                    "v9_control",
                    "table",
                    "v9_control",
                    "CREATE TABLE v9_control(singleton INTEGER PRIMARY KEY CHECK(singleton=1),cmd TEXT NOT NULL,requested_at TEXT NOT NULL)",
                )?
            {
                return Err(Error::Validation(
                    "Unknown migration-control object in legacy snapshot".into(),
                ));
            }
            tx.execute_batch("DROP TABLE audio_dir_search; DROP TABLE audio_dir_runs;
                DROP TABLE IF EXISTS v9_control;
                DELETE FROM library_meta WHERE key IN ('search_path_rev','dir_index_gen');
                DELETE FROM audiolib_schema_features WHERE feature IN ('search_path','vorbis_tags','vendor_metadata','audio_sources');")
                .map_err(db_error)?;
            crate::vendor_import::drop_legacy_snapshot_storage(&tx)?;
            // Known references have no cascading delete in old library schemas.
            // Unknown extension foreign keys are checked below, never silently discarded.
            tx.execute_batch(&format!(
                "DELETE FROM playlist_items WHERE audio_file_id IN
                  (SELECT id FROM audio_files WHERE {EXCLUDED_AUDIO});
                DELETE FROM project_references WHERE audio_file_id IN
                  (SELECT id FROM audio_files WHERE {EXCLUDED_AUDIO});
                DELETE FROM waveform_cache WHERE audio_file_id IN
                  (SELECT id FROM audio_files WHERE {EXCLUDED_AUDIO});
                DELETE FROM audio_files WHERE {EXCLUDED_AUDIO};
                INSERT OR REPLACE INTO library_meta(key,value) VALUES
                  ('snapshot_format','ale-s8-cld1-t3-m2-p1-a1'),
                  ('source_search_rev','2'),('source_tags_rev','2');
                PRAGMA user_version=8;"
            ))
            .map_err(db_error)?;
            crate::refresh_folder_statistics(&tx)?;
            // Contentless-delete triggers removed only the excluded IDs above.
            // Never rewrite WAV Metadata or rebuild its FTS projection: old readers
            // must retain the existing rev3 source-text matches (SC-03/SC-07).
            if !schema::readable_format_ready(&tx)? {
                return Err(Error::Validation(
                    "Derived legacy snapshot does not satisfy the schema-8 format".into(),
                ));
            }
            let invalid: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                    [],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            if invalid {
                return Err(Error::Validation(
                    "Derived legacy snapshot contains unresolved extension references".into(),
                ));
            }
            tx.commit().map_err(db_error)
        })?;
        if !Self::check_owned_file_cancellable(path, cancel)?.is_ok() {
            return Err(Error::Validation(
                "Derived legacy snapshot failed integrity validation".into(),
            ));
        }
        let inspection = crate::migration::inspect(
            path,
            audiolib_core::compatibility::LibraryAccess::Read,
            false,
            cancel,
        )?;
        if inspection.format.snapshot_format_id().as_deref() != Some(PLAN_B_FORMAT)
            || !inspection.evidence.search_ready
        {
            return Err(Error::Validation(
                "Derived legacy snapshot format was not positively identified".into(),
            ));
        }
        Self::owned_snapshot_info_cancellable(path, cancel)
    }
}
