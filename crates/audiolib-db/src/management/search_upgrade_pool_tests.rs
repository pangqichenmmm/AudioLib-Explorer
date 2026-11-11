// crates/audiolib-db/src/management/search_upgrade_pool_tests.rs
//! Native search-upgrade locks use only owned temporary files, never source libraries.
use super::*;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("owned.db");
    fs::write(&database, b"owned synthetic lock identity").unwrap();
    (directory, database)
}

fn idle(pool: &SearchUpgradePool) -> usize {
    pool.state.lock().unwrap().idle.len()
}

#[cfg(windows)]
fn native_handle(file: &fs::File) -> usize {
    use std::os::windows::io::AsRawHandle;
    file.as_raw_handle() as usize
}

#[cfg(unix)]
fn native_handle(file: &fs::File) -> usize {
    use std::os::unix::io::AsRawFd;
    file.as_raw_fd() as usize
}

#[cfg(any(windows, unix))]
#[test]
fn shared_handles_are_reused_without_retaining_an_idle_lock() {
    let (_directory, database) = fixture();
    let sidecar = search_upgrade_lock_path(&database).unwrap();
    fs::write(&sidecar, b"preserve sidecar bytes").unwrap();
    let pool = SearchUpgradePool::default();
    let lease = pool.acquire_shared(&database).unwrap();
    let handle = native_handle(lease.file.as_ref().unwrap());
    assert!(matches!(
        SearchUpgradeLease::acquire(&database, true),
        Err(Error::Busy(_))
    ));
    drop(lease);
    assert_eq!(idle(&pool), 1);
    {
        let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
    }
    let lease = pool.acquire_shared(&database).unwrap();
    assert_eq!(native_handle(lease.file.as_ref().unwrap()), handle);
    assert_eq!(idle(&pool), 0);
    drop(lease);
    assert_eq!(idle(&pool), 1);
    assert_eq!(fs::read(sidecar).unwrap(), b"preserve sidecar bytes");
}

#[test]
fn every_active_shared_handle_has_an_independent_os_lock_lifetime() {
    let (_directory, database) = fixture();
    let pool = SearchUpgradePool::default();
    let first = pool.acquire_shared(&database).unwrap();
    let second = pool.acquire_shared(&database).unwrap();
    assert!(matches!(
        SearchUpgradeLease::acquire(&database, true),
        Err(Error::Busy(_))
    ));
    drop(first);
    assert_eq!(idle(&pool), 1);
    // Reusing a cloned open-file description would unlock the second reader too.
    assert!(matches!(
        SearchUpgradeLease::acquire(&database, true),
        Err(Error::Busy(_))
    ));
    drop(second);
    assert_eq!(idle(&pool), 2);
    let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
}

#[test]
fn failed_shared_acquisition_closes_the_handle_and_does_not_poison_the_pool() {
    let (_directory, database) = fixture();
    let pool = SearchUpgradePool::default();
    drop(pool.acquire_shared(&database).unwrap());
    assert_eq!(idle(&pool), 1);
    let exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
    assert!(matches!(
        pool.acquire_shared(&database),
        Err(Error::Busy(_))
    ));
    assert_eq!(
        idle(&pool),
        0,
        "a failed handle must not return to the idle list"
    );
    drop(exclusive);
    let lease = pool.acquire_shared(&database).unwrap();
    drop(lease);
    assert_eq!(idle(&pool), 1);
    let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
}

#[test]
fn an_open_error_is_recoverable_without_adopting_another_database() {
    let (directory, database) = fixture();
    let sidecar = search_upgrade_lock_path(&database).unwrap();
    assert!(sidecar.starts_with(directory.path().canonicalize().unwrap()));
    fs::create_dir(&sidecar).unwrap();
    let pool = SearchUpgradePool::default();
    assert!(matches!(pool.acquire_shared(&database), Err(Error::Io(_))));
    assert_eq!(idle(&pool), 0);
    // The directory is an owned, empty test obstruction, never an actual lock file.
    fs::remove_dir(&sidecar).unwrap();
    drop(pool.acquire_shared(&database).unwrap());
    assert_eq!(idle(&pool), 1);
}

#[test]
fn the_original_provider_path_cannot_be_rebound_to_another_path_or_alias() {
    let (directory, database) = fixture();
    let other = directory.path().join("other.db");
    fs::write(&other, b"other owned identity").unwrap();
    let alias = directory.path().join(".").join("owned.db");
    let pool = SearchUpgradePool::default();
    drop(pool.acquire_shared(&database).unwrap());
    assert!(matches!(
        pool.acquire_shared(&other),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        pool.acquire_shared(&alias),
        Err(Error::Validation(_))
    ));
    assert_eq!(idle(&pool), 1);
    drop(pool.acquire_shared(&database).unwrap());
    assert_eq!(idle(&pool), 1);
}

#[test]
fn retained_handles_are_bounded_and_all_are_unlocked() {
    let (_directory, database) = fixture();
    let pool = SearchUpgradePool::default();
    let leases: Vec<_> = (0..MAX_IDLE_SEARCH_UPGRADE_HANDLES + 5)
        .map(|_| pool.acquire_shared(&database).unwrap())
        .collect();
    assert_eq!(idle(&pool), 0);
    assert!(matches!(
        SearchUpgradeLease::acquire(&database, true),
        Err(Error::Busy(_))
    ));
    drop(leases);
    assert_eq!(idle(&pool), MAX_IDLE_SEARCH_UPGRADE_HANDLES);
    let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
}

#[test]
fn a_lease_outliving_its_pool_still_unlocks_and_closes_normally() {
    let (_directory, database) = fixture();
    let pool = SearchUpgradePool::default();
    let lease = pool.acquire_shared(&database).unwrap();
    drop(pool);
    assert!(matches!(
        SearchUpgradeLease::acquire(&database, true),
        Err(Error::Busy(_))
    ));
    drop(lease);
    let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
}

#[test]
fn cancellation_after_acquisition_returns_an_unlocked_handle() {
    fn cancelled(pool: &SearchUpgradePool, database: &Path) -> Result<()> {
        let _lease = pool.acquire_shared(database)?;
        Err(Error::Cancelled)
    }
    let (_directory, database) = fixture();
    let pool = SearchUpgradePool::default();
    assert!(matches!(cancelled(&pool, &database), Err(Error::Cancelled)));
    assert_eq!(idle(&pool), 1);
    let _exclusive = SearchUpgradeLease::acquire(&database, true).unwrap();
}
