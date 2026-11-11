// app/audiolib-desktop/src/slice_staging.rs
//! Owned, locked slice staging and conservative recovery of abandoned exports.
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    time::{Duration, SystemTime},
};
use tempfile::TempDir;

const PREFIX: &str = ".audiolib-slice-";
const OWNER: &[u8] = b"AudioLib Explorer WAV slice staging v1\n";
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const GENERATED_FILES: &[&str] = &["owner", "lease", "slice.wav", "original.audio"];

/// Field order closes the lease before TempDir attempts its Windows cleanup.
pub(super) struct SliceStaging {
    _lease: File,
    pub directory: TempDir,
}

fn plain(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject every reparse-point kind, including junctions that do not
        // present as ordinary symbolic links through FileType.
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

fn old(metadata: &Metadata, now: SystemTime) -> bool {
    metadata.modified().ok().is_some_and(|modified| {
        now.duration_since(modified)
            .is_ok_and(|age| age > STALE_AFTER)
    })
}

fn owner_matches(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !plain(&metadata) || !metadata.is_file() || metadata.len() != OWNER.len() as u64 {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut bytes = [0; OWNER.len()];
    file.read_exact(&mut bytes).is_ok() && bytes.as_slice() == OWNER
}

fn generated_files_only(directory: &Path, now: SystemTime) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if !GENERATED_FILES.contains(&name) {
            return false;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if !plain(&metadata) || !metadata.is_file() || !old(&metadata, now) {
            return false;
        }
    }
    true
}

fn cleanup_candidate(export_dir: &Path, candidate: &Path, now: SystemTime) {
    let Ok(metadata) = fs::symlink_metadata(candidate) else {
        return;
    };
    if !plain(&metadata) || !metadata.is_dir() || !old(&metadata, now) {
        return;
    }
    let Ok(resolved) = candidate.canonicalize() else {
        return;
    };
    if resolved.parent() != Some(export_dir) || resolved.file_name() != candidate.file_name() {
        return;
    }
    if !owner_matches(&resolved.join("owner")) || !generated_files_only(&resolved, now) {
        return;
    }
    let lease_path = resolved.join("lease");
    let Ok(lease_metadata) = fs::symlink_metadata(&lease_path) else {
        return;
    };
    if !plain(&lease_metadata) || !lease_metadata.is_file() {
        return;
    }
    let Ok(lease) = OpenOptions::new().read(true).write(true).open(&lease_path) else {
        return;
    };
    if lease.try_lock().is_err() {
        return;
    }
    // A second check after admission protects another cleanup or an application
    // that had not yet finished creating/locking its new directory on the first pass.
    if !owner_matches(&resolved.join("owner")) || !generated_files_only(&resolved, now) {
        return;
    }
    for name in ["slice.wav", "original.audio"] {
        let path = resolved.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if plain(&metadata) && metadata.is_file() => {
                if fs::remove_file(&path).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return,
        }
    }
    // There is no recursive removal. Any unknown object appearing concurrently
    // survives and prevents removal of the directory itself.
    drop(lease);
    for name in ["owner", "lease"] {
        let path = resolved.join(name);
        if !fs::symlink_metadata(&path).is_ok_and(|metadata| plain(&metadata) && metadata.is_file())
        {
            return;
        }
        if fs::remove_file(path).is_err() {
            return;
        }
    }
    let _ = fs::remove_dir(resolved);
}

fn cleanup_stale(export_dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(export_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(PREFIX))
        {
            cleanup_candidate(export_dir, &entry.path(), now);
        }
    }
}

/// Create and exclusively lease a new staging directory under the selected export
/// folder. Before creation, remove only fully recognized, unlocked crash leftovers
/// older than 24 hours; preserve unknown files, links, reparse points and live work.
pub(super) fn prepare_staging(export_dir: &Path) -> io::Result<SliceStaging> {
    fs::create_dir_all(export_dir)?;
    let export_dir = export_dir.canonicalize()?;
    cleanup_stale(&export_dir, SystemTime::now());
    let directory = tempfile::Builder::new()
        .prefix(PREFIX)
        .tempdir_in(&export_dir)?;
    let mut owner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.path().join("owner"))?;
    owner.write_all(OWNER)?;
    drop(owner);
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.path().join("lease"))?;
    lease.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "New slice staging lease is already held",
        ),
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(SliceStaging {
        _lease: lease,
        directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_time(path: &Path, time: SystemTime) {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES + FILE_FLAG_BACKUP_SEMANTICS also opens
            // the owned directory handles needed by the stale-age fixtures.
            options.access_mode(0x0100).custom_flags(0x0200_0000);
        }
        options.open(path).unwrap().set_modified(time).unwrap();
    }

    fn age(path: &Path) {
        set_time(
            path,
            SystemTime::now() - STALE_AFTER - Duration::from_secs(60),
        );
    }

    fn abandoned(export_dir: &Path, name: &str) -> std::path::PathBuf {
        let path = export_dir.join(format!("{PREFIX}{name}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("owner"), OWNER).unwrap();
        fs::write(path.join("lease"), b"").unwrap();
        fs::write(path.join("slice.wav"), b"owned output").unwrap();
        fs::write(path.join("original.audio"), b"owned original").unwrap();
        for name in GENERATED_FILES {
            age(&path.join(name));
        }
        age(&path);
        path
    }

    #[test]
    fn stale_owned_files_are_removed_and_recent_work_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let stale = abandoned(&root, "stale");
        let recent = abandoned(&root, "recent");
        set_time(&recent.join("owner"), SystemTime::now());
        let stage = prepare_staging(&root).unwrap();
        assert!(!stale.exists());
        assert!(recent.join("slice.wav").exists());
        assert!(owner_matches(&stage.directory.path().join("owner")));
        let stage_path = stage.directory.path().to_owned();
        drop(stage);
        assert!(
            !stage_path.exists(),
            "Lease closes before owned TempDir cleanup"
        );
    }

    #[test]
    fn active_unknown_unowned_and_nested_work_is_never_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let active = abandoned(&root, "active");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(active.join("lease"))
            .unwrap();
        lease.try_lock().unwrap();
        let unknown = abandoned(&root, "unknown");
        fs::write(unknown.join("user-note.txt"), b"preserve").unwrap();
        age(&unknown);
        let unowned = abandoned(&root, "unowned");
        fs::write(unowned.join("owner"), b"different owner").unwrap();
        age(&unowned.join("owner"));
        age(&unowned);
        let nested = abandoned(&root, "nested");
        fs::remove_file(nested.join("slice.wav")).unwrap();
        fs::create_dir(nested.join("slice.wav")).unwrap();
        fs::write(nested.join("slice.wav/keep"), b"do not recurse").unwrap();
        age(&nested);
        let _new_stage = prepare_staging(&root).unwrap();
        for path in [&active, &unknown, &unowned, &nested] {
            assert!(path.exists(), "{} was not owned/idle", path.display());
            assert!(path.join("original.audio").exists());
        }
        assert_eq!(
            fs::read(unknown.join("user-note.txt")).unwrap(),
            b"preserve"
        );
        assert_eq!(
            fs::read(nested.join("slice.wav/keep")).unwrap(),
            b"do not recurse"
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_generated_file_symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = abandoned(outside.path(), "target");
        symlink(&target, root.join(format!("{PREFIX}link"))).unwrap();
        let linked_file = abandoned(&root, "linked-file");
        fs::remove_file(linked_file.join("slice.wav")).unwrap();
        symlink(target.join("slice.wav"), linked_file.join("slice.wav")).unwrap();
        age(&linked_file);
        let _stage = prepare_staging(&root).unwrap();
        assert_eq!(fs::read(target.join("slice.wav")).unwrap(), b"owned output");
        assert!(linked_file.join("original.audio").exists());
    }

    #[cfg(windows)]
    #[test]
    fn directory_junctions_are_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = abandoned(outside.path(), "target");
        let link = root.join(format!("{PREFIX}junction"));
        let output = std::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _stage = prepare_staging(&root).unwrap();
        assert_eq!(fs::read(target.join("slice.wav")).unwrap(), b"owned output");
        assert!(link.exists());
        // Remove only the verified direct-child junction itself before TempDir
        // cleanup; never pass the external target to a recursive remover.
        assert_eq!(link.parent(), Some(root.as_path()));
        fs::remove_dir(link).unwrap();
    }
}
