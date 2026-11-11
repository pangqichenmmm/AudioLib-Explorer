// crates/audiolib-db/src/path_identity.rs
//! Stored path identity is independent of the machine reading a library snapshot.
use crate::{Error, Result, db_error, path_key};
use rusqlite::Connection;

pub(crate) fn windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with("\\\\")
        || value.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// Normalize Windows syntax while retaining directory-entry case on every host.
pub(crate) fn comparison_key(value: &str) -> String {
    if windows_path(value) {
        let native = value.replace('\\', "/");
        let native = native
            .strip_prefix("//?/UNC/")
            .map(|rest| format!("//{rest}"))
            .or_else(|| native.strip_prefix("//?/").map(str::to_owned))
            .unwrap_or(native);
        let native = native.trim_end_matches('/');
        if let Some(unc) = native.strip_prefix("//") {
            let mut parts = unc.splitn(3, '/');
            let host = parts.next().unwrap_or_default().to_lowercase();
            let share = parts.next().map(str::to_lowercase);
            let rest = parts.next();
            let mut key = format!("//{host}");
            if let Some(share) = share {
                key.push('/');
                key.push_str(&share);
            }
            if let Some(rest) = rest {
                key.push('/');
                key.push_str(rest);
            }
            key
        } else if native.as_bytes().get(1) == Some(&b':') {
            format!("{}{}", native[..1].to_ascii_lowercase(), &native[1..])
        } else {
            native.into()
        }
    } else {
        let trimmed = value.trim_end_matches('/');
        if trimmed.is_empty() && value.starts_with('/') {
            "/".into()
        } else {
            trimmed.into()
        }
    }
}

pub(crate) fn matches(left: &str, right: &str, native_alias: bool) -> bool {
    if comparison_key(left) == comparison_key(right) {
        return true;
    }
    if windows_path(left) && windows_path(right) && path_key(left) == path_key(right) {
        // Offline snapshots carry no volume case policy. Exact-first selection
        // disambiguates distinct entries; a unique historical Windows alias remains
        // usable without probing source paths on the reader's machine.
        if !native_alias {
            return true;
        }
        #[cfg(windows)]
        return native_case_alias(std::path::Path::new(left), std::path::Path::new(right));
        #[cfg(not(windows))]
        return true;
    }
    // A native case-insensitive volume may expose two spellings of one entry.
    // Never infer that identity for an offline POSIX path or foreign snapshot.
    #[cfg(unix)]
    if native_alias
        && !windows_path(left)
        && !windows_path(right)
        && path_key(left) == path_key(right)
    {
        return native_case_alias(std::path::Path::new(left), std::path::Path::new(right));
    }
    #[cfg(not(any(unix, windows)))]
    let _ = native_alias;
    false
}

#[cfg(unix)]
fn native_case_alias(left: &std::path::Path, right: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let same_object = |left: &std::path::Path, right: &std::path::Path| {
        matches!((std::fs::metadata(left), std::fs::metadata(right)),
            (Ok(left), Ok(right)) if left.dev() == right.dev() && left.ino() == right.ino())
    };
    if !same_object(left, right) {
        return false;
    }
    let (Some(left_parent), Some(right_parent), Some(left_name), Some(right_name)) = (
        left.parent(),
        right.parent(),
        left.file_name(),
        right.file_name(),
    ) else {
        return false;
    };
    if !same_object(left_parent, right_parent) {
        return false;
    }
    one_directory_entry(left_parent, left_name, right_name)
}

#[cfg(windows)]
fn native_case_alias(left: &std::path::Path, right: &std::path::Path) -> bool {
    // Windows canonicalization uses the opened object's normalized final path.
    // Distinct entries in an NTFS case-sensitive directory retain distinct names.
    let same_object = |left: &std::path::Path, right: &std::path::Path| {
        matches!((left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if comparison_key(&left.to_string_lossy()) == comparison_key(&right.to_string_lossy()))
    };
    if !same_object(left, right) {
        return false;
    }
    let (Some(left_parent), Some(right_parent), Some(left_name), Some(right_name)) = (
        left.parent(),
        right.parent(),
        left.file_name(),
        right.file_name(),
    ) else {
        return false;
    };
    same_object(left_parent, right_parent)
        && one_directory_entry(left_parent, left_name, right_name)
}

#[cfg(any(unix, windows))]
fn one_directory_entry(
    parent: &std::path::Path,
    left_name: &std::ffi::OsStr,
    right_name: &std::ffi::OsStr,
) -> bool {
    if left_name == right_name {
        return true;
    }
    // Equal inodes alone also describe hardlinks. Two exact directory entries
    // remain distinct paths, including Foo.wav/foo.wav on a sensitive volume.
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    let (mut found_left, mut found_right) = (false, false);
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        found_left |= entry.file_name() == left_name;
        found_right |= entry.file_name() == right_name;
        if found_left && found_right {
            return false;
        }
    }
    found_left || found_right
}

/// Prefer an exact stored spelling; reject ambiguous legacy duplicate identities.
pub(crate) fn choose<T>(
    requested: &str,
    candidates: impl IntoIterator<Item = (String, T)>,
    native_alias: bool,
) -> Result<Option<T>> {
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    let literal = comparison_key(requested);
    let exact = candidates
        .iter()
        .any(|(path, _)| comparison_key(path) == literal);
    let mut selected = None;
    for (path, value) in candidates {
        if (exact && comparison_key(&path) == literal)
            || (!exact && matches(&path, requested, native_alias))
        {
            if selected.is_some() {
                return Err(Error::Validation(format!(
                    "Path matches multiple library records: {requested}"
                )));
            }
            selected = Some(value);
        }
    }
    Ok(selected)
}

pub(crate) fn audio_id(conn: &Connection, path: &str, native_alias: bool) -> Result<Option<i64>> {
    let mut statement = conn
        .prepare_cached("SELECT file_path,id FROM audio_files WHERE path_key=?1 ORDER BY id")
        .map_err(db_error)?;
    let rows = statement
        .query_map([path_key(path)], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?))
        })
        .map_err(db_error)?;
    choose(
        path,
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?,
        native_alias,
    )
}

#[cfg(feature = "scanner")]
pub(crate) fn folder_id(conn: &Connection, path: &str) -> Result<Option<i64>> {
    use rusqlite::OptionalExtension;
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM folders WHERE folder_path=?1 COLLATE BINARY",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?
    {
        return Ok(Some(id));
    }
    // The exact UNIQUE index handles the common case. Only an absent spelling
    // needs the legacy Unicode bucket; SQLite NOCASE handles ASCII alone.
    folder_id_with_alias(conn, path, true)
}

/// Snapshot lookups preserve exact source spellings and reject ambiguous aliases;
/// scanner callers retain their existing native-filesystem alias policy above.
pub(crate) fn folder_id_with_alias(
    conn: &Connection,
    path: &str,
    native_alias: bool,
) -> Result<Option<i64>> {
    // Read-only snapshot readers do not register write-side scalar functions.
    // The historical SQL function also scanned this table; retain its shared
    // folded-key bucket in Rust, then apply exact-first identity selection.
    let mut statement = conn
        .prepare_cached("SELECT folder_path,id FROM folders ORDER BY id")
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))
        .map_err(db_error)?;
    let bucket = path_key(path);
    let mut candidates = Vec::new();
    for row in rows {
        let (stored_path, id) = row.map_err(db_error)?;
        if path_key(&stored_path) == bucket {
            candidates.push((stored_path, id));
        }
    }
    choose(path, candidates, native_alias)
}

#[cfg(all(test, feature = "scanner"))]
mod tests {
    use super::*;
    use crate::SqliteProvider;
    use audiolib_core::DataProvider;
    use std::path::Path;

    #[test]
    fn posix_volume_root_reconciliation_and_seen_ids_do_not_merge_folded_keys() {
        let root = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&root.path().join("library.db")).unwrap();
        provider
            .ensure_scan_folder(Path::new("/"), Path::new("/"))
            .unwrap();
        let folder = provider.folders(None).unwrap()[0].id;
        let conn = Connection::open(provider.database_path()).unwrap();
        crate::register_library_functions(&conn).unwrap();
        conn.execute("INSERT INTO audio_files(id,file_path,path_key,filename,folder_id) VALUES(1,'/Case.wav','/case.wav','Case.wav',?1),(2,'/case.wav','/case.wav','case.wav',?1),(3,'/orphan.wav','/orphan.wav','orphan.wav',NULL)", [folder]).unwrap();
        drop(conn);
        provider.mark_seen(&[2], 1).unwrap();
        let mut progress = audiolib_core::ScanProgress {
            session_id: 1,
            ..Default::default()
        };
        provider
            .finish_scan(
                Path::new("/"),
                &mut progress,
                true,
                true,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
        assert!(provider.get_audio(1).unwrap().missing);
        assert!(!provider.get_audio(2).unwrap().missing);
        assert!(provider.get_audio(3).unwrap().missing);
        assert_eq!(progress.removed, 2);
    }

    #[test]
    fn recursive_reconciliation_uses_folder_identity_for_native_alias_spellings() {
        let root = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&root.path().join("library.db")).unwrap();
        provider
            .ensure_scan_folder(Path::new("/offline/Äudio"), Path::new("/offline/Äudio"))
            .unwrap();
        let folder = provider.folders(None).unwrap()[0].id;
        let conn = Connection::open(provider.database_path()).unwrap();
        crate::register_library_functions(&conn).unwrap();
        conn.execute("INSERT INTO audio_files(id,file_path,path_key,filename,folder_id) VALUES(1,'/offline/äudio/new.wav','/offline/äudio/new.wav','new.wav',?1)", [folder]).unwrap();
        drop(conn);
        let mut progress = audiolib_core::ScanProgress {
            session_id: 1,
            ..Default::default()
        };
        provider
            .finish_scan(
                Path::new("/offline/Äudio"),
                &mut progress,
                true,
                true,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(progress.removed, 1);
        assert!(provider.get_audio(1).unwrap().missing);
    }

    #[test]
    fn windows_orphan_reconciliation_preserves_case_distinct_source_roots() {
        let root = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&root.path().join("library.db")).unwrap();
        let source = Path::new("C:/Sensitive/Audio");
        provider.ensure_scan_folder(source, source).unwrap();
        let conn = Connection::open(provider.database_path()).unwrap();
        crate::register_library_functions(&conn).unwrap();
        for (id, path) in [
            (1, r"\\?\C:\Sensitive\Audio\Foo.wav"),
            (2, "c:/Sensitive/audio/foo.wav"),
        ] {
            conn.execute("INSERT INTO audio_files(id,file_path,path_key,filename) VALUES(?1,?2,?3,'foo.wav')", rusqlite::params![id, path, path_key(path)]).unwrap();
        }
        drop(conn);
        let mut progress = audiolib_core::ScanProgress {
            session_id: 1,
            ..Default::default()
        };
        provider
            .finish_scan(
                source,
                &mut progress,
                true,
                true,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(progress.removed, 1);
        assert!(provider.get_audio(1).unwrap().missing);
        assert!(!provider.get_audio(2).unwrap().missing);
    }
}
