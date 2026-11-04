// crates/audiolib-db/src/search_workspace.rs
//! Versioned personal search state, never stored in a library or server snapshot.
use crate::local_play_history::safe_library_id;
use audiolib_core::{Error, Result, SearchWorkspace, SearchWorkspaceTarget};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_WORKSPACE_BYTES: usize = 16 * 1024 * 1024;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn invalid(message: impl Into<String>) -> Error {
    Error::Validation(message.into())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn child_directory(parent: &Path, name: &str, create: bool) -> Result<Option<PathBuf>> {
    let path = parent.join(name);
    if create {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(invalid(
            "Search workspace directory must not be a link or non-directory",
        ));
    }
    let canonical = path.canonicalize()?;
    if canonical.parent() != Some(parent) {
        return Err(invalid(
            "Search workspace directory escaped its user-data root",
        ));
    }
    Ok(Some(canonical))
}

fn workspace_path(target: &SearchWorkspaceTarget, create: bool) -> Result<Option<PathBuf>> {
    target.validate()?;
    let encoded = safe_library_id(&target.library_scope)?;
    if create {
        fs::create_dir_all(&target.data_root)?;
    }
    let root = match target.data_root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !root.is_dir() {
        return Err(invalid(
            "Search workspace user-data root is not a directory",
        ));
    }
    let Some(workspaces) = child_directory(&root, "search-workspaces", create)? else {
        return Ok(None);
    };
    let Some(directory) = child_directory(&workspaces, &encoded, create)? else {
        return Ok(None);
    };
    Ok(Some(directory.join("workspace.json")))
}

fn check_file(path: &Path) -> Result<Option<fs::Metadata>> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Search workspace needs a parent directory"))?;
    let directory = fs::symlink_metadata(parent)?;
    if !directory.is_dir() || is_link(&directory) || parent.canonicalize()? != parent {
        return Err(invalid("Search workspace directory identity changed"));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file()
                || is_link(&metadata)
                || path.canonicalize()?.parent() != Some(parent)
            {
                return Err(invalid("Search workspace must be a regular unlinked file"));
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_existing(path: &Path, target: &SearchWorkspaceTarget) -> Result<Option<SearchWorkspace>> {
    let Some(metadata) = check_file(path)? else {
        return Ok(None);
    };
    if metadata.len() > MAX_WORKSPACE_BYTES as u64 {
        return Err(invalid("Search workspace exceeds 16 MiB"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take((MAX_WORKSPACE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_WORKSPACE_BYTES {
        return Err(invalid("Search workspace exceeds 16 MiB"));
    }
    let original: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("Cannot read search workspace: {error}")))?;
    let mut workspace: SearchWorkspace = serde_json::from_value(original.clone())
        .map_err(|error| invalid(format!("Cannot read search workspace: {error}")))?;
    let decoded = serde_json::to_value(&workspace)
        .map_err(|error| invalid(format!("Cannot inspect search workspace: {error}")))?;
    reject_unknown_fields(&original, &decoded, false)?;
    workspace.upgrade_legacy_version()?;
    if workspace.library_scope != target.library_scope {
        return Err(invalid("Search workspace belongs to another library"));
    }
    Ok(Some(workspace))
}

// Refuse state this reader cannot preserve. Missing defaults are allowed, but
// silently discarding an unknown stored condition could broaden a saved search.
fn reject_unknown_fields(
    original: &serde_json::Value,
    decoded: &serde_json::Value,
    filters: bool,
) -> Result<()> {
    match (original, decoded) {
        (serde_json::Value::Object(original), serde_json::Value::Object(decoded)) => {
            for (key, value) in original {
                if let Some(known) = decoded.get(key) {
                    reject_unknown_fields(value, known, key == "filters")?;
                } else if !(filters
                    && matches!(
                        key.as_str(),
                        "include_tags" | "exclude_tags" | "album" | "album_id"
                    ))
                {
                    return Err(invalid(format!(
                        "Unknown search workspace field {key}; original state was preserved"
                    )));
                }
            }
        }
        (serde_json::Value::Array(original), serde_json::Value::Array(decoded)) => {
            for (value, known) in original.iter().zip(decoded) {
                reject_unknown_fields(value, known, false)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn load(target: &SearchWorkspaceTarget) -> Result<SearchWorkspace> {
    let Some(path) = workspace_path(target, false)? else {
        return Ok(SearchWorkspace::for_scope(&target.library_scope));
    };
    Ok(read_existing(&path, target)?
        .unwrap_or_else(|| SearchWorkspace::for_scope(&target.library_scope)))
}

pub(crate) fn save(target: &SearchWorkspaceTarget, workspace: &SearchWorkspace) -> Result<()> {
    target.validate()?;
    workspace.validate()?;
    if workspace.library_scope != target.library_scope {
        return Err(invalid(
            "Cannot save a search workspace into another library scope",
        ));
    }
    let data = serde_json::to_vec_pretty(workspace)
        .map_err(|error| invalid(format!("Cannot encode search workspace: {error}")))?;
    if data.len() > MAX_WORKSPACE_BYTES {
        return Err(invalid("Search workspace exceeds 16 MiB"));
    }
    let path = workspace_path(target, true)?
        .ok_or_else(|| invalid("Search workspace directory is unavailable"))?;
    // Never replace corrupt, foreign-scope or newer personal state with an empty fallback.
    read_existing(&path, target)?;
    write_atomic(&path, &data)
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}-{sequence}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        check_file(path)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
