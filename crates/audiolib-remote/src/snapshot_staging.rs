// crates/audiolib-remote/src/snapshot_staging.rs
//! Optional per-machine workspace for preparing remote snapshots on another volume.
//!
//! The active cache and resumable transfer remain under the data root. A configured
//! workspace contains only an owned disposable preparation; publication is copied,
//! verified and atomically installed on the cache volume before its state changes.
use crate::error::{RemoteError, Result};
use crate::sync::RemoteCache;
use ring::digest::{Context, Digest, SHA256};
use serde::Deserialize;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::{Builder, TempDir};

const CONFIG_NAME: &str = "snapshot-workdir.json";
const CONFIG_LIMIT: u64 = 16 * 1024;
const COPY_BUFFER: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkdirConfig {
    directory: PathBuf,
}

fn plain_file(metadata: &Metadata) -> bool {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

/// A missing configuration keeps the existing cache-local staging behavior.
/// An invalid or unavailable configured directory is an error, not a silent
/// fallback to a potentially full system disk.
fn configured_workdir(cache: &RemoteCache) -> Result<Option<PathBuf>> {
    let path = cache.root().join(CONFIG_NAME);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !plain_file(&metadata)
        || metadata.len() > CONFIG_LIMIT
        || !crate::cache_cleanup::plain_directory_tree(cache.root())
    {
        return Err(RemoteError::Validation(
            "Invalid local snapshot workspace configuration".into(),
        ));
    }
    let config: WorkdirConfig = serde_json::from_slice(&std::fs::read(&path)?).map_err(|_| {
        RemoteError::Validation("Invalid local snapshot workspace configuration".into())
    })?;
    if !config.directory.is_absolute()
        || config.directory.parent().is_none()
        || !crate::cache_cleanup::plain_directory_tree(&config.directory)
    {
        return Err(RemoteError::Validation(
            "The configured snapshot workspace is unavailable or is not a plain absolute directory"
                .into(),
        ));
    }
    Ok(Some(config.directory.canonicalize()?))
}

/// Create an isolated job in the configured workspace or the original cache directory.
pub(crate) fn snapshot_job(cache: &RemoteCache, cache_dir: &Path) -> Result<(TempDir, bool)> {
    let configured = configured_workdir(cache)?;
    let external = configured.is_some();
    let root = configured.as_deref().unwrap_or(cache_dir);
    let job = Builder::new().prefix(".snapshot-job-").tempdir_in(root)?;
    Ok((job, external))
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(RemoteError::Cancelled)
    } else {
        Ok(())
    }
}

fn copy_with_digest(
    source: &mut impl Read,
    destination: &mut impl Write,
    cancel: &AtomicBool,
) -> Result<Digest> {
    let mut digest = Context::new(&SHA256);
    let mut buffer = vec![0; COPY_BUFFER];
    loop {
        cancelled(cancel)?;
        let read = source.read(&mut buffer)?;
        cancelled(cancel)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
    }
    Ok(digest.finish())
}

/// Publish a completed job. Cross-volume preparation is copied to a unique
/// temporary file on the cache volume, SHA-256 verified and installed without
/// replacing an existing file. Cancellation leaves the active cache unchanged.
pub(crate) fn publish_snapshot(
    staging: &Path,
    target: &Path,
    external: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    cancelled(cancel)?;
    if !external {
        std::fs::rename(staging, target)?;
        return Ok(());
    }
    let directory = target.parent().ok_or_else(|| {
        RemoteError::Validation("The snapshot cache has no parent directory".into())
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RemoteError::Validation("The snapshot cache filename is invalid".into()))?;
    let size = std::fs::metadata(staging)?.len();
    audiolib_db::ensure_free_space(directory, size)?;
    // This name is recognized by normal stale-staging housekeeping. The unique
    // target already includes a nonce; create_new must not replace a prior file.
    let prefix = format!("{name}.download.part-{}", std::process::id());
    let mut local = Builder::new()
        .prefix(&prefix)
        .rand_bytes(0)
        .tempfile_in(directory)?;
    let mut source = File::open(staging)?;
    let source_digest = copy_with_digest(&mut source, &mut local, cancel)?;
    let mut buffer = vec![0; COPY_BUFFER];
    local.as_file().sync_all()?;
    local.as_file_mut().seek(SeekFrom::Start(0))?;
    let mut copied_digest = Context::new(&SHA256);
    loop {
        cancelled(cancel)?;
        let read = local.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied_digest.update(&buffer[..read]);
    }
    if source_digest.as_ref() != copied_digest.finish().as_ref() {
        return Err(RemoteError::Snapshot(
            "The copied snapshot failed its integrity check".into(),
        ));
    }
    cancelled(cancel)?;
    local
        .persist_noclobber(target)
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_workspace_is_per_profile_and_requires_a_real_directory() {
        let root = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(root.path());
        std::fs::create_dir_all(cache.root()).unwrap();
        let fallback = root.path().join("fallback");
        std::fs::create_dir(&fallback).unwrap();
        let (job, external) = snapshot_job(&cache, &fallback).unwrap();
        assert!(!external && job.path().starts_with(&fallback));
        drop(job);

        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            cache.root().join(CONFIG_NAME),
            serde_json::to_vec(&serde_json::json!({"directory": workspace})).unwrap(),
        )
        .unwrap();
        let (job, external) = snapshot_job(&cache, &fallback).unwrap();
        assert!(external && job.path().starts_with(workspace.canonicalize().unwrap()));
        drop(job);
        std::fs::remove_dir(&workspace).unwrap();
        assert!(snapshot_job(&cache, &fallback).is_err());
        std::fs::write(
            cache.root().join(CONFIG_NAME),
            br#"{"directory":"relative"}"#,
        )
        .unwrap();
        assert!(snapshot_job(&cache, &fallback).is_err());
    }

    #[test]
    fn cross_volume_publish_verifies_bytes_and_never_overwrites_a_target() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging.db");
        let cache = root.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let bytes = vec![42; COPY_BUFFER + 37];
        std::fs::write(&staging, &bytes).unwrap();
        let target = cache.join("library-r1-1.db");
        let cancel = AtomicBool::new(false);
        publish_snapshot(&staging, &target, true, &cancel).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(std::fs::read(&staging).unwrap(), bytes);
        std::fs::write(&staging, b"different").unwrap();
        assert!(publish_snapshot(&staging, &target, true, &cancel).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[test]
    fn cancelled_publication_keeps_existing_files_untouched() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging.db");
        let target = root.path().join("library-r1-1.db");
        std::fs::write(&staging, b"candidate").unwrap();
        std::fs::write(&target, b"existing").unwrap();
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            publish_snapshot(&staging, &target, true, &cancel),
            Err(RemoteError::Cancelled)
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        assert_eq!(std::fs::read(&staging).unwrap(), b"candidate");
    }

    #[test]
    fn cancellation_is_observed_during_copy_without_writing_more() {
        struct CancellingReader<'a> {
            cancel: &'a AtomicBool,
            reads: usize,
        }
        impl Read for CancellingReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads == 2 {
                    self.cancel.store(true, Ordering::Release);
                }
                buffer[0] = self.reads as u8;
                Ok(1)
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = CancellingReader {
            cancel: &cancel,
            reads: 0,
        };
        let mut output = Vec::new();
        assert!(matches!(
            copy_with_digest(&mut reader, &mut output, &cancel),
            Err(RemoteError::Cancelled)
        ));
        assert_eq!(output, [1]);
    }
}
