// crates/audiolib-db/src/schema/audio_sources.rs
//! Atomic publication of the required extended-source capability.
use super::*;
use crate::search_storage;
use audiolib_core::compatibility::{
    AUDIO_SOURCES_FEATURE, AUDIO_SOURCES_REVISION, PLAN_B_AUDIO_FORMAT, PLAN_B_V9_AUDIO_FORMAT,
};
#[cfg(feature = "scanner")]
use std::path::Path;

/// A trusted probe may identify a new container behind a historical `.wav` name.
#[cfg(feature = "scanner")]
pub(crate) fn metadata_requires_audio_sources(path: &Path, attributes: &serde_json::Value) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    ["mp3", "aif", "aiff", "bwf"]
        .iter()
        .any(|new| extension.eq_ignore_ascii_case(new))
        || attributes
            .get("container")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|container| {
                ["MP3", "AIFF", "AIFC"]
                    .iter()
                    .any(|new| container.eq_ignore_ascii_case(new))
            })
        || ["mp3_version", "aiff_version"].iter().any(|name| {
            attributes["wav_metadata"][*name]
                .as_i64()
                .is_some_and(|revision| revision > 0)
        })
}

/// Activate only inside the transaction that publishes affected business data.
pub(crate) fn require_audio_sources(conn: &Connection) -> Result<()> {
    search_storage::transaction_required(conn)?;
    match feature_version(conn, AUDIO_SOURCES_FEATURE)? {
        Some(revision) if revision == i64::from(AUDIO_SOURCES_REVISION) => return Ok(()),
        Some(revision) => {
            return Err(Error::NewerDatabase {
                what: format!("feature {AUDIO_SOURCES_FEATURE} revision"),
                found: revision,
                supported: i64::from(AUDIO_SOURCES_REVISION),
            });
        }
        None => {}
    }
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    let format = match version {
        8 => PLAN_B_AUDIO_FORMAT,
        9 => PLAN_B_V9_AUDIO_FORMAT,
        _ => {
            return Err(Error::Validation(
                "Extended audio sources require completed schema 8 or 9".into(),
            ));
        }
    };
    set_feature_version(
        conn,
        AUDIO_SOURCES_FEATURE,
        i64::from(AUDIO_SOURCES_REVISION),
    )?;
    conn.execute(
        "INSERT INTO library_meta(key,value) VALUES('snapshot_format',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [format],
    ).map_err(db_error)?;
    Ok(())
}

/// Existing imported rows are already business data, even without playable source files.
/// Run only in owned migration work, not on each ordinary open or scan.
pub(crate) fn imported_audio_sources_required(conn: &Connection) -> Result<bool> {
    // Both checks are necessary: JSON booleans extract as SQLite integers, while
    // JSON integers outside i64 extract as REAL. Match Rust Value::as_i64 exactly.
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM audio_files WHERE lower(substr(filename,-4)) IN ('.mp3','.aif','.bwf')
         OR lower(substr(filename,-5))='.aiff'
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
             THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,'$.wav_metadata.aiff_version')>0))",
        [], |row| row.get(0),
    ).map_err(db_error)
}

/// Declare the actual source capability while publishing an owned migrated library.
pub(crate) fn require_imported_audio_sources(conn: &Connection) -> Result<()> {
    if imported_audio_sources_required(conn)? {
        require_audio_sources(conn)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imported_source_markers_require_positive_json_i64_values() {
        let temp = tempfile::tempdir().unwrap();
        let provider = crate::SqliteProvider::open(&temp.path().join("library.db")).unwrap();
        let mut writer = provider.writer().unwrap();
        for marker in [
            json!("future"),
            json!("1"),
            json!(true),
            json!(1.0),
            json!(1.5),
            json!(u64::MAX),
            json!(null),
            json!(0),
            json!(-1),
            json!(1),
            json!(i64::MAX),
        ] {
            let attributes = json!({"container":"RIFF", "wav_metadata":{"mp3_version":marker.clone(), "aiff_version":marker.clone()}, "unknown_extension":{"keep":true}});
            let expected = marker.as_i64().is_some_and(|value| value > 0);
            let tx = writer.transaction().unwrap();
            tx.execute("INSERT INTO audio_files(file_path,filename,metadata_json) VALUES('owned.wav','owned.wav',?1)", [attributes.to_string()]).unwrap();
            require_imported_audio_sources(&tx).unwrap();
            assert_eq!(
                feature_version(&tx, AUDIO_SOURCES_FEATURE).unwrap(),
                expected.then_some(1),
                "{marker}"
            );
            #[cfg(feature = "scanner")]
            assert_eq!(
                metadata_requires_audio_sources(Path::new("owned.wav"), &attributes),
                expected,
                "{marker}"
            );
            tx.rollback().unwrap();
        }
        for container in ["MP3", "AIFF", "AIFC"] {
            let attributes = json!({"container":container, "wav_metadata":{"mp3_version":"future", "aiff_version":false}});
            let tx = writer.transaction().unwrap();
            tx.execute("INSERT INTO audio_files(file_path,filename,metadata_json) VALUES('owned.wav','owned.wav',?1)", [attributes.to_string()]).unwrap();
            require_imported_audio_sources(&tx).unwrap();
            assert_eq!(
                feature_version(&tx, AUDIO_SOURCES_FEATURE).unwrap(),
                Some(1),
                "{container}"
            );
            #[cfg(feature = "scanner")]
            assert!(metadata_requires_audio_sources(
                Path::new("owned.wav"),
                &attributes
            ));
            tx.rollback().unwrap();
        }
    }
}
