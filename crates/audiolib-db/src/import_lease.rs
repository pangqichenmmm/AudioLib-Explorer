// crates/audiolib-db/src/import_lease.rs
//! Persistent cross-process destination leases covering import and history publication.
use audiolib_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const INTENT_PROTOCOL: u32 = 1;
const MAX_INTENT_BYTES: u64 = 64 * 1024;

/// This receipt precedes final-path publication, so a crash before job
/// registration cannot let a server-style open retire unimported local history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalImportIntent {
    protocol: u32,
    library_id: String,
    data_root: PathBuf,
    limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album_namespace: Option<String>,
}

impl LocalImportIntent {
    pub(crate) fn limit(&self) -> u32 {
        self.limit
    }

    /// Repeated recovery opens must admit exactly one new-library identity. Old
    /// receipts derive it from their already persisted owner without rewriting it.
    pub(crate) fn admission_namespace(&self) -> Result<String> {
        if let Some(namespace) = &self.album_namespace {
            return uuid::Uuid::parse_str(namespace)
                .map(|value| value.to_string())
                .map_err(|_| invalid("Local import album namespace is invalid"));
        }
        let identity = serde_json::to_vec(&(&self.library_id, &self.data_root, self.limit))
            .map_err(|error| invalid(&error.to_string()))?;
        let mut hash = Sha256::new();
        hash.update(b"audiolib-local-import-album-v1\0");
        hash.update(identity);
        let digest = hash.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(uuid::Uuid::from_bytes(bytes).to_string())
    }
}

/// A destination identity exists before the database itself. Both import entry
/// points keep this lease through publication and final provider preparation.
pub(crate) struct ImportDestinationLease {
    file: fs::File,
    destination: PathBuf,
}

/// Public writable opens hold a shared import lease while preparing their
/// provider, including after an importer has published its still-old copy.
pub(crate) struct ExistingImportOpenLease {
    file: fs::File,
    destination: PathBuf,
}

fn invalid(message: &str) -> Error {
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

impl ImportDestinationLease {
    pub(crate) fn acquire(destination: &Path) -> Result<Self> {
        let lease = Self::acquire_for_create(destination)?;
        // Imports never adopt an occupied name, including dangling links.
        match fs::symlink_metadata(lease.destination()) {
            Ok(_) => return Err(invalid("Import destination already exists")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // A previous attempt never published a database under this name. An
        // explicit new import may replace that abandoned intent, including a
        // torn write, but ordinary create/open must still validate it.
        clear_intent(&lease.file)?;
        Ok(lease)
    }

    /// A creator may find a file published between its missing-path probe and
    /// lock acquisition. Keep exclusion while its caller rechecks/opens that file.
    pub(crate) fn acquire_for_create(destination: &Path) -> Result<Self> {
        let (file, destination) = lock_file(destination)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(Error::Busy(
                "Another import or library creation owns this destination; retry when it completes"
                    .into(),
            )),
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if !metadata.is_file() || is_link(&metadata) => {
                return Err(invalid(
                    "Library creation destination became a link or non-file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self { file, destination })
    }

    /// Frozen canonical-parent identity for all staging/history/final publication.
    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    /// Persist the reviewed local owner before publishing the old-format copy.
    /// Only this missing destination may acquire a fresh owner identity.
    pub(crate) fn write_local_intent(&self, data_root: &Path, limit: u32) -> Result<()> {
        match fs::symlink_metadata(&self.destination) {
            Ok(_) => return Err(invalid("Local import intent must precede publication")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let data_root = data_root.canonicalize()?;
        if !data_root.is_dir() {
            return Err(invalid("Local import data root must be a directory"));
        }
        let intent = LocalImportIntent {
            protocol: INTENT_PROTOCOL,
            library_id: crate::local_play_history::local_library_id(&self.destination)?,
            data_root,
            limit,
            album_namespace: Some(uuid::Uuid::new_v4().to_string()),
        };
        let bytes = serde_json::to_vec(&intent).map_err(|error| invalid(&error.to_string()))?;
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn authorize_history(
        &self,
        history: Option<(&Path, u32)>,
    ) -> Result<Option<LocalImportIntent>> {
        authorize_history(&self.file, &self.destination, history)
    }

    /// Caller has completed the actual local open and still owns exclusion.
    pub(crate) fn clear_completed_intent(&self, expected: &LocalImportIntent) -> Result<()> {
        clear_matching_intent(&self.file, &self.destination, expected)
    }
}

fn read_intent(file: &fs::File, destination: &Path) -> Result<Option<LocalImportIntent>> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_INTENT_BYTES {
        return Err(invalid(
            "Local import intent is too large; recovery is required",
        ));
    }
    let mut reader = file;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(length as usize);
    reader.take(MAX_INTENT_BYTES + 1).read_to_end(&mut bytes)?;
    let intent: LocalImportIntent = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Local import intent is damaged; recovery is required"))?;
    if intent.protocol != INTENT_PROTOCOL
        || !intent.data_root.is_absolute()
        || intent.library_id != crate::local_play_history::local_library_id(destination)?
        || intent
            .album_namespace
            .as_deref()
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_err())
    {
        return Err(invalid(
            "Local import intent identity is unsupported or mismatched",
        ));
    }
    Ok(Some(intent))
}

fn authorize_history(
    file: &fs::File,
    destination: &Path,
    history: Option<(&Path, u32)>,
) -> Result<Option<LocalImportIntent>> {
    let Some(intent) = read_intent(file, destination)? else {
        return Ok(None);
    };
    let Some((data_root, _)) = history else {
        return Err(invalid(
            "A local import owns this library; reopen with its local history destination",
        ));
    };
    let actual_root = data_root.canonicalize()?;
    if !actual_root.is_dir()
        || actual_root != intent.data_root
        || intent.data_root.canonicalize()? != intent.data_root
    {
        return Err(invalid(
            "Local import history destination differs from its durable owner",
        ));
    }
    Ok(Some(intent))
}

fn clear_intent(file: &fs::File) -> Result<()> {
    if file.metadata()?.len() != 0 {
        file.set_len(0)?;
        file.sync_all()?;
    }
    Ok(())
}

fn clear_matching_intent(
    file: &fs::File,
    destination: &Path,
    expected: &LocalImportIntent,
) -> Result<()> {
    match read_intent(file, destination)? {
        None => Ok(()),
        Some(actual) if &actual == expected => clear_intent(file),
        Some(_) => Err(invalid("Local import owner changed before completion")),
    }
}

fn lock_file(destination: &Path) -> Result<(fs::File, PathBuf)> {
    existing_or_create_lock_file(destination, true)?
        .ok_or_else(|| invalid("Import lease creation did not produce a file"))
}

fn existing_or_create_lock_file(
    destination: &Path,
    create: bool,
) -> Result<Option<(fs::File, PathBuf)>> {
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("Import destination needs a file name"))?;
    if name
        .to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(".audiolib-import-leases"))
    {
        return Err(invalid(
            "This destination name is reserved for import leases",
        ));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    if !parent.is_dir() {
        return Err(invalid("Import destination parent is not a directory"));
    }
    // Preserve the complete native filename in its own directory. This avoids
    // truncation/hash collisions and follows the filesystem's own case rules,
    // including non-ASCII names and already maximum-length filename components.
    let locks = parent.join(".audiolib-import-leases");
    if create {
        match fs::create_dir(&locks) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = match fs::symlink_metadata(&locks) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir()
        || is_link(&metadata)
        || locks.canonicalize()?.parent() != Some(parent.as_path())
    {
        return Err(invalid(
            "Import lease directory must be a real directory beside the destination",
        ));
    }
    let locks = locks.canonicalize()?;
    let lock_path = locks.join(name);
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if !metadata.is_file() || is_link(&metadata) => {
            return Err(invalid("Import lease must be a real file"));
        }
        Ok(_) => {}
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open a reparse point itself, so the following metadata check can
        // reject it without following it to an unrelated file.
        options.custom_flags(0x0020_0000);
    }
    let file = match options.open(&lock_path) {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file()
        || is_link(&file.metadata()?)
        || lock_path.canonicalize()?.parent() != Some(locks.as_path())
    {
        return Err(invalid("Import lease file escaped its owned directory"));
    }
    let destination = parent.join(name);
    Ok(Some((file, destination)))
}

impl ExistingImportOpenLease {
    pub(crate) fn acquire(destination: &Path) -> Result<Self> {
        let canonical = destination.canonicalize()?;
        if !canonical.is_file() {
            return Err(invalid("Library open destination is not a file"));
        }
        let (file, destination) = lock_file(&canonical)?;
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::Busy(
                    "A library import or creation is still preparing this destination".into(),
                ));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        Ok(Self { file, destination })
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    /// Explicit read-only tools must not create a sidecar merely to ask whether
    /// a pending import owns their source. An existing receipt is locked/read.
    pub(crate) fn acquire_existing(destination: &Path) -> Result<Option<Self>> {
        let canonical = destination.canonicalize()?;
        if !canonical.is_file() {
            return Err(invalid("Library open destination is not a file"));
        }
        let Some((file, destination)) = existing_or_create_lock_file(&canonical, false)? else {
            return Ok(None);
        };
        match file.try_lock_shared() {
            Ok(()) => Ok(Some(Self { file, destination })),
            Err(fs::TryLockError::WouldBlock) => Err(Error::Busy(
                "A library import or creation is still preparing this destination".into(),
            )),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub(crate) fn authorize_migration(
        &self,
        local: Option<&crate::migration::LocalHistoryMigration>,
        recovery: bool,
    ) -> Result<()> {
        let Some(intent) = read_intent(&self.file, &self.destination)? else {
            return Ok(());
        };
        let Some(local) = local else {
            return Err(invalid(
                "A local import owns this library; explicit migration requires its persisted local scope",
            ));
        };
        if local.library_id != intent.library_id || local.limit != intent.limit {
            return Err(invalid(
                "Explicit migration differs from the durable local import owner",
            ));
        }
        if recovery {
            // Verify/rollback do not write client history. Its unavailable root
            // cannot block source recovery, but its saved identity must match.
            if local.data_root != intent.data_root {
                return Err(invalid(
                    "Recovery history destination differs from the durable owner",
                ));
            }
        } else {
            authorize_history(
                &self.file,
                &self.destination,
                Some((&local.data_root, local.limit)),
            )?;
        }
        Ok(())
    }

    pub(crate) fn authorize_history(
        &self,
        history: Option<(&Path, u32)>,
    ) -> Result<Option<LocalImportIntent>> {
        authorize_history(&self.file, &self.destination, history)
    }

    /// Provider preparation has finished. Never wait while upgrading a shared
    /// lock: another local open may still hold one. Retain the durable owner on
    /// contention; a later matching open can clear it once alone.
    pub(crate) fn try_clear_completed_intent(&self, expected: &LocalImportIntent) -> Result<()> {
        self.file.unlock()?;
        match self.file.try_lock() {
            Ok(()) => clear_matching_intent(&self.file, &self.destination, expected),
            Err(fs::TryLockError::WouldBlock) => Ok(()),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for ExistingImportOpenLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Drop for ImportDestinationLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        // Never unlink the persistent lock inode: another process may already
        // have opened it, and replacing it would split the exclusion identity.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_play_history::{LocalPlayHistory, local_library_id};
    use std::{
        sync::{atomic::AtomicBool, mpsc},
        thread,
    };

    fn legacy(path: &Path, label: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        crate::register_library_functions(&conn).unwrap();
        conn.execute_batch(include_str!("../tests/fixtures/schema-v5.sql"))
            .unwrap();
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,user_notes) VALUES(1,?1,?1,?1)",
            [label],
        )
        .unwrap();
        conn.execute("INSERT INTO play_history(id,audio_file_id,played_at,filename,file_path) VALUES(1,1,'2026-10-21T00:00:00.000Z',?1,?1)",[label]).unwrap();
    }

    #[test]
    fn destination_lease_is_persistent_and_checks_occupation_after_acquiring() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("imported.db");
        let first = ImportDestinationLease::acquire(&destination).unwrap();
        assert!(matches!(
            ImportDestinationLease::acquire(&root.path().join(".").join("imported.db")),
            Err(Error::Busy(_))
        ));
        let lock_path = root.path().join(".audiolib-import-leases/imported.db");
        assert!(lock_path.is_file());
        drop(first);
        assert!(lock_path.is_file());
        drop(ImportDestinationLease::acquire(&destination).unwrap());
        fs::write(&destination, b"another published library").unwrap();
        assert!(matches!(
            ImportDestinationLease::acquire(&destination),
            Err(Error::Validation(_))
        ));
        assert_eq!(fs::read(destination).unwrap(), b"another published library");
        assert!(lock_path.is_file());
    }

    #[test]
    fn both_import_entry_points_refuse_a_concurrent_owner_before_touching_history() {
        let root = tempfile::tempdir().unwrap();
        let source_a = root.path().join("source-a.db");
        let source_b = root.path().join("source-b.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        legacy(&source_a, "a.wav");
        legacy(&source_b, "b.wav");
        let a_original = fs::read(&source_a).unwrap();
        let b_original = fs::read(&source_b).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held_destination = destination.clone();
        let holder = thread::spawn(move || {
            let lease = ImportDestinationLease::acquire(&held_destination).unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(lease);
        });
        ready_rx.recv().unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            crate::SqliteProvider::open(&destination),
            Err(Error::Busy(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {}
            ),
            Err(Error::Busy(_))
        ));
        thread::scope(|scope| {
            let ordinary = scope.spawn(|| crate::import_legacy(&source_a, &destination));
            let local =
                scope.spawn(|| crate::import_legacy_local(&source_b, &destination, &data, 5000));
            assert!(matches!(ordinary.join().unwrap(), Err(Error::Busy(_))));
            assert!(matches!(local.join().unwrap(), Err(Error::Busy(_))));
        });
        assert!(!destination.exists());
        assert!(!data.join("play-history").exists());
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        crate::import_legacy_local(&source_b, &destination, &data, 5000).unwrap();
        let id = local_library_id(&destination).unwrap();
        let history = LocalPlayHistory::open(&data, &id, 5000).unwrap();
        let rows = history.page(10, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "b.wav");
        assert!(crate::import_legacy_local(&source_a, &destination, &data, 5000).is_err());
        assert_eq!(history.page(10, 0).unwrap(), rows);
        assert_eq!(fs::read(source_a).unwrap(), a_original);
        assert_eq!(fs::read(source_b).unwrap(), b_original);
    }

    #[test]
    fn public_writable_opens_cannot_adopt_a_published_import_before_its_job() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        legacy(&source, "a.wav");
        let owner = ImportDestinationLease::acquire(&destination).unwrap();
        // This is the publication-to-job window: the old copy exists, while
        // the importer still owns its exclusive destination identity.
        fs::copy(&source, owner.destination()).unwrap();
        let published = fs::read(&destination).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            crate::SqliteProvider::open(&destination),
            Err(Error::Busy(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_existing(&destination),
            Err(Error::Busy(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {}
            ),
            Err(Error::Busy(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {}
            ),
            Err(Error::Busy(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), published);
        assert!(crate::migration::discover(&destination).unwrap().is_none());
        assert!(!data.join("play-history").exists());
        // An explicit importer owner is only valid for its exact destination.
        assert!(matches!(
            crate::SqliteProvider::open_with_history_owner_cancellable(
                &source,
                Some((&data, 5000)),
                &cancel,
                |_| {},
                Some(&owner)
            ),
            Err(Error::Validation(_))
        ));
        drop(owner);
        fs::create_dir_all(&data).unwrap();
        drop(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {},
            )
            .unwrap(),
        );
        let id = local_library_id(&destination).unwrap();
        let rows = LocalPlayHistory::open(&data, &id, 5000)
            .unwrap()
            .page(10, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "a.wav");
    }

    #[test]
    fn rejected_dependency_import_does_not_leave_history_for_another_source() {
        let root = tempfile::tempdir().unwrap();
        let source_a = root.path().join("source-a.db");
        let source_b = root.path().join("source-b.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        legacy(&source_a, "a.wav");
        legacy(&source_b, "b.wav");
        let conn = rusqlite::Connection::open(&source_a).unwrap();
        conn.execute_batch("CREATE VIEW retained_stats AS SELECT play_count FROM audio_files;")
            .unwrap();
        assert!(!crate::schema::legacy_needs_upgrade(&conn).unwrap());
        drop(conn);
        let a_original = fs::read(&source_a).unwrap();
        let b_original = fs::read(&source_b).unwrap();
        assert!(crate::import_legacy_local(&source_a, &destination, &data, 5000).is_err());
        assert!(!destination.exists());
        assert!(!data.join("play-history").exists());
        let report = crate::import_legacy_local(&source_b, &destination, &data, 5000).unwrap();
        assert!(report.source_preserved);
        assert_eq!(report.audio_files, 1);
        let id = local_library_id(&destination).unwrap();
        let rows = LocalPlayHistory::open(&data, &id, 5000)
            .unwrap()
            .page(10, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "b.wav");
        assert_eq!(fs::read(&source_a).unwrap(), a_original);
        assert_eq!(fs::read(&source_b).unwrap(), b_original);
    }

    #[test]
    fn failed_local_import_keeps_owned_final_identity_and_resumes_its_durable_job() {
        let root = tempfile::tempdir().unwrap();
        let source_a = root.path().join("source-a.db");
        let source_b = root.path().join("source-b.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        legacy(&source_a, "a.wav");
        legacy(&source_b, "b.wav");
        let a_original = fs::read(&source_a).unwrap();
        let b_original = fs::read(&source_b).unwrap();
        fs::create_dir(&data).unwrap();
        let obstacle = data.join("play-history");
        fs::write(&obstacle, b"owned blocking file").unwrap();
        assert!(crate::import_legacy_local(&source_a, &destination, &data, 5000).is_err());
        assert!(destination.is_file());
        let job = crate::migration::discover(&destination).unwrap().unwrap();
        assert_eq!(job.phase, crate::migration::MigrationPhase::Backup);
        assert!(job.backup_sha256.is_some());
        assert!(job.directory.join("backup.db").is_file());
        let retained = fs::read(&destination).unwrap();
        assert!(crate::import_legacy_local(&source_b, &destination, &data, 5000).is_err());
        assert_eq!(fs::read(&destination).unwrap(), retained);
        fs::remove_file(obstacle).unwrap();
        let cancel = AtomicBool::new(false);
        drop(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {},
            )
            .unwrap(),
        );
        let complete = crate::migration::discover(&destination).unwrap().unwrap();
        assert_eq!(complete.phase, crate::migration::MigrationPhase::Complete);
        crate::migration::verify(&complete.directory, &cancel).unwrap();
        let id = local_library_id(&destination).unwrap();
        let rows = LocalPlayHistory::open(&data, &id, 5000)
            .unwrap()
            .page(10, 0)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "a.wav");
        assert_eq!(fs::read(&source_a).unwrap(), a_original);
        assert_eq!(fs::read(&source_b).unwrap(), b_original);
    }

    #[test]
    fn local_import_of_current_format_preserves_source_without_creating_history() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        drop(crate::SqliteProvider::open(&source).unwrap());
        let original = fs::read(&source).unwrap();
        let report = crate::import_legacy_local(&source, &destination, &data, 5000).unwrap();
        assert!(report.source_preserved);
        assert_eq!(report.schema_version, crate::SCHEMA_VERSION);
        assert_eq!(fs::read(&source).unwrap(), original);
        assert!(crate::migration::discover(&destination).unwrap().is_none());
        assert!(!data.join("play-history").exists());
    }

    #[test]
    fn durable_local_owner_survives_crash_before_job_and_clears_only_after_completion() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        let wrong_data = root.path().join("other-data");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&wrong_data).unwrap();
        legacy(&source, "a.wav");
        let original = fs::read(&source).unwrap();
        let owner = ImportDestinationLease::acquire(&destination).unwrap();
        owner.write_local_intent(&data, 1).unwrap();
        drop(owner);
        let lock_path = root.path().join(".audiolib-import-leases/imported.db");
        let persisted = fs::read(&lock_path).unwrap();
        let cancel = AtomicBool::new(false);
        // Crash before publication: ordinary creation must not steal the owner.
        assert!(matches!(
            crate::SqliteProvider::open(&destination),
            Err(Error::Validation(_))
        ));
        assert!(!destination.exists());
        assert_eq!(fs::read(&lock_path).unwrap(), persisted);
        // Crash immediately after publication, before any durable job exists.
        let owner = ImportDestinationLease::acquire_for_create(&destination).unwrap();
        fs::copy(&source, owner.destination()).unwrap();
        drop(owner);
        let published = fs::read(&destination).unwrap();
        assert!(crate::migration::discover(&destination).unwrap().is_none());
        assert!(matches!(
            crate::SqliteProvider::open(&destination),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_existing(&destination),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &wrong_data,
                5000,
                &cancel,
                |_| {}
            ),
            Err(Error::Validation(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), published);
        assert_eq!(fs::read(&lock_path).unwrap(), persisted);
        assert!(!data.join("play-history").exists());
        assert!(!wrong_data.join("play-history").exists());
        // Another shared opener prevents cleanup but cannot prevent a correct
        // local recovery. The captured limit wins over a changed caller setting.
        let other_reader = ExistingImportOpenLease::acquire(&destination).unwrap();
        drop(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &data,
                0,
                &cancel,
                |_| {},
            )
            .unwrap(),
        );
        assert_eq!(fs::read(&lock_path).unwrap(), persisted);
        assert!(matches!(
            crate::SqliteProvider::open_existing(&destination),
            Err(Error::Validation(_))
        ));
        let id = local_library_id(&destination).unwrap();
        let history = LocalPlayHistory::open(&data, &id, 5000).unwrap();
        let rows = history.page(10, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "a.wav");
        let job = crate::migration::discover(&destination).unwrap().unwrap();
        assert_eq!(job.phase, crate::migration::MigrationPhase::Complete);
        assert_eq!(job.plan.local_history.as_ref().unwrap().limit, 1);
        crate::migration::verify(&job.directory, &cancel).unwrap();
        drop(other_reader);
        drop(
            crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                &destination,
                &data,
                5000,
                &cancel,
                |_| {},
            )
            .unwrap(),
        );
        assert!(lock_path.is_file());
        assert!(fs::read(&lock_path).unwrap().is_empty());
        drop(crate::SqliteProvider::open_existing(&destination).unwrap());
        assert_eq!(history.page(10, 0).unwrap(), rows);
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn damaged_or_unknown_nonempty_intent_blocks_all_opens_without_touching_library() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        fs::create_dir(&data).unwrap();
        legacy(&source, "a.wav");
        let owner = ImportDestinationLease::acquire(&destination).unwrap();
        owner.write_local_intent(&data, 1).unwrap();
        let lock_path = root.path().join(".audiolib-import-leases/imported.db");
        let mut unknown = serde_json::to_value(
            read_intent(&owner.file, owner.destination())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        unknown["protocol"] = serde_json::json!(99);
        fs::copy(&source, owner.destination()).unwrap();
        drop(owner);
        let original = fs::read(&source).unwrap();
        let published = fs::read(&destination).unwrap();
        for invalid_payload in [
            b"{".to_vec(),
            b" \n".to_vec(),
            serde_json::to_vec(&unknown).unwrap(),
            vec![b'x'; MAX_INTENT_BYTES as usize + 1],
        ] {
            let owner = ImportDestinationLease::acquire_for_create(&destination).unwrap();
            owner.file.set_len(0).unwrap();
            let mut file = &owner.file;
            file.write_all(&invalid_payload).unwrap();
            file.sync_all().unwrap();
            drop(owner);
            let cancel = AtomicBool::new(false);
            assert!(matches!(
                crate::SqliteProvider::open(&destination),
                Err(Error::Validation(_))
            ));
            assert!(matches!(
                crate::SqliteProvider::open_existing(&destination),
                Err(Error::Validation(_))
            ));
            assert!(matches!(
                crate::SqliteProvider::open_existing_local_with_progress_cancellable(
                    &destination,
                    &data,
                    5000,
                    &cancel,
                    |_| {}
                ),
                Err(Error::Validation(_))
            ));
            assert_eq!(fs::read(&destination).unwrap(), published);
            assert_eq!(fs::read(&lock_path).unwrap(), invalid_payload);
            assert!(crate::migration::discover(&destination).unwrap().is_none());
            assert!(!data.join("play-history").exists());
        }
        assert_eq!(fs::read(&source).unwrap(), original);
    }

    #[test]
    fn a_new_explicit_import_can_replace_an_unpublished_local_intent() {
        for local in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("source-b.db");
            let destination = root.path().join("imported.db");
            let abandoned_data = root.path().join("abandoned-data");
            let actual_data = root.path().join("actual-data");
            fs::create_dir(&abandoned_data).unwrap();
            legacy(&source, "b.wav");
            let original = fs::read(&source).unwrap();
            let owner = ImportDestinationLease::acquire(&destination).unwrap();
            owner.write_local_intent(&abandoned_data, 0).unwrap();
            drop(owner);
            let report = if local {
                crate::import_legacy_local(&source, &destination, &actual_data, 5000)
            } else {
                crate::import_legacy(&source, &destination)
            }
            .unwrap();
            assert!(report.source_preserved);
            assert!(!abandoned_data.join("play-history").exists());
            if local {
                let id = local_library_id(&destination).unwrap();
                let rows = LocalPlayHistory::open(&actual_data, &id, 5000)
                    .unwrap()
                    .page(10, 0)
                    .unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].filename, "b.wav");
            } else {
                assert!(!actual_data.exists());
            }
            let lock_path = root.path().join(".audiolib-import-leases/imported.db");
            assert!(lock_path.is_file());
            assert!(fs::read(lock_path).unwrap().is_empty());
            drop(crate::SqliteProvider::open_existing(&destination).unwrap());
            assert_eq!(fs::read(&source).unwrap(), original);
        }
    }

    #[test]
    fn explicit_migration_admission_without_a_receipt_does_not_create_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.db");
        legacy(&path, "a.wav");
        let original = fs::read(&path).unwrap();
        let locks = root.path().join(".audiolib-import-leases");
        let guard = crate::migration::admit_import_owner(&path, None, false).unwrap();
        crate::migration::plan(&path, &AtomicBool::new(false)).unwrap();
        drop(guard);
        assert!(!locks.exists());
        // An existing directory with no receipt must stay empty, too.
        fs::create_dir(&locks).unwrap();
        drop(crate::migration::admit_import_owner(&path, None, false).unwrap());
        assert_eq!(fs::read_dir(&locks).unwrap().count(), 0);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn explicit_tool_admission_blocks_unscoped_plans_and_recovery_binds_saved_owner() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let destination = root.path().join("imported.db");
        let data = root.path().join("client-data");
        fs::create_dir(&data).unwrap();
        legacy(&source, "a.wav");
        let owner = ImportDestinationLease::acquire(&destination).unwrap();
        owner.write_local_intent(&data, 3).unwrap();
        fs::copy(&source, owner.destination()).unwrap();
        drop(owner);
        let published = fs::read(&destination).unwrap();
        let lock_path = root.path().join(".audiolib-import-leases/imported.db");
        let receipt = fs::read(&lock_path).unwrap();
        // The shared engine deliberately has no import-lease reentry. Its raw
        // plan can be server-scoped, so explicit tool adapters must admit first.
        let unscoped = crate::migration::plan(&destination, &AtomicBool::new(false)).unwrap();
        assert!(unscoped.local_history.is_none());
        assert!(matches!(
            crate::migration::admit_import_owner(&destination, None, false),
            Err(Error::Validation(_))
        ));
        assert!(matches!(
            crate::migration::admit_import_owner(&destination, None, true),
            Err(Error::Validation(_))
        ));
        let local = crate::migration::LocalHistoryMigration {
            data_root: data.canonicalize().unwrap(),
            library_id: local_library_id(&destination).unwrap(),
            limit: 3,
        };
        let guard =
            crate::migration::admit_import_owner(&destination, Some(local.clone()), false).unwrap();
        assert!(matches!(
            ImportDestinationLease::acquire_for_create(&destination),
            Err(Error::Busy(_))
        ));
        drop(guard);
        let mut wrong_limit = local.clone();
        wrong_limit.limit = 5000;
        assert!(matches!(
            crate::migration::admit_import_owner(&destination, Some(wrong_limit), false),
            Err(Error::Validation(_))
        ));
        fs::rename(&data, root.path().join("unavailable-data")).unwrap();
        assert!(
            crate::migration::admit_import_owner(&destination, Some(local.clone()), false).is_err()
        );
        drop(
            crate::migration::admit_import_owner(&destination, Some(local.clone()), true).unwrap(),
        );
        let mut wrong_root = local.clone();
        wrong_root.data_root = root.path().join("other-data");
        assert!(matches!(
            crate::migration::admit_import_owner(&destination, Some(wrong_root), true),
            Err(Error::Validation(_))
        ));
        let mut wrong_id = local;
        wrong_id.library_id.push('x');
        assert!(matches!(
            crate::migration::admit_import_owner(&destination, Some(wrong_id), true),
            Err(Error::Validation(_))
        ));
        assert_eq!(fs::read(&destination).unwrap(), published);
        assert_eq!(fs::read(&lock_path).unwrap(), receipt);
        assert!(crate::migration::discover(&destination).unwrap().is_none());
        assert!(!data.exists());
    }
}
