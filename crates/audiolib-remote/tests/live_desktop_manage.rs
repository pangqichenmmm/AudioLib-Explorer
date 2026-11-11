// crates/audiolib-remote/tests/live_desktop_manage.rs
//! Opt-in, DESTRUCTIVE end-to-end test of the desktop's server management path (the
//! 服务器音效库 page, design `server-mgmt-gui.md` B2) against a disposable test server:
//! a subpath scan started like 「开始扫描…」 (workers = server default) and polled every
//! 500 ms, 暂停 / 继续 / 取消扫描, then 移除所选记录 and 删除目录记录 through the
//! desktop's `DataProvider` (`RemoteProvider`, which asks for a re-sync on the revision
//! bump), and finally a full scan that re-adds everything with new ids.
//!
//! Off by default: `#[ignore]`d, needs `AUDIOLIB_LIVE_MANAGE_URL`, refuses port 8790.
//!
//! ```text
//! $env:AUDIOLIB_LIVE_MANAGE_URL = "http://server.example.lan:8799"
//! cargo test -p audiolib-remote --test live_desktop_manage -- --ignored --nocapture
//! ```
use audiolib_core::{DataProvider, ScanState};
use audiolib_db::SqliteProvider;
use audiolib_remote::{
    CacheState, RemoteCache, RemoteClient, RemoteError, RemoteProvider, RemoteWriter, ScanControl,
    ScanMode, ScanRequest, ServerScan, WriteEvent, WriteListener, sync_library,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The desktop's poll interval for an active server scan.
const POLL: Duration = Duration::from_millis(500);

fn wait_inactive(client: &RemoteClient, id: i64) -> ServerScan {
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let scan = client.scan_status(id).expect("GET /scans/{id}");
        if !scan.is_active() {
            return scan;
        }
        assert!(
            Instant::now() < deadline,
            "scan {id} still active after 240 s"
        );
        std::thread::sleep(POLL);
    }
}

fn wait_idle(client: &RemoteClient) {
    while let Some(scan) = client.current_scan().expect("GET /scans/current") {
        eprintln!("waiting for running scan {}", scan.progress.session_id);
        wait_inactive(client, scan.progress.session_id);
    }
}

fn sync(client: &RemoteClient, cache: &RemoteCache) -> CacheState {
    let cancel = AtomicBool::new(false);
    sync_library(client, cache, &mut |_| {}, &cancel, false)
        .expect("snapshot sync")
        .state
}

fn snapshot(cache: &RemoteCache, state: &CacheState) -> Connection {
    Connection::open_with_flags(cache.database_path(state), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open snapshot")
}

fn audio_id(db: &Connection, path: &str) -> Option<i64> {
    db.query_row(
        "SELECT id FROM audio_files WHERE file_path = ?1",
        [path],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

fn count(db: &Connection) -> i64 {
    db.query_row("SELECT COUNT(*) FROM audio_files", [], |row| row.get(0))
        .unwrap()
}

/// The page's provider: snapshot reads, server writes, write events recorded.
fn provider(
    client: &RemoteClient,
    cache: &RemoteCache,
    state: &CacheState,
    events: &Arc<Mutex<Vec<String>>>,
) -> RemoteProvider {
    let snapshot = Arc::new(SqliteProvider::open_read_only(&cache.database_path(state)).unwrap());
    let writer = RemoteWriter::new(client.clone(), cache.clone(), state.library_id.clone());
    let log = events.clone();
    let listener: WriteListener = Arc::new(move |event| {
        log.lock().unwrap().push(match event {
            WriteEvent::Applied { revision } => format!("applied {revision}"),
            WriteEvent::Resync { .. } => "resync".into(),
            WriteEvent::LibraryChanged { .. } => "library-changed".into(),
            WriteEvent::MigrationWaiting { retry_after, .. } => {
                format!("migration-wait {retry_after}")
            }
        });
    });
    RemoteProvider::new(snapshot, writer, true, Some(listener))
}

/// 「开始扫描」 as the desktop sends it: workers left to the server.
fn start(
    client: &RemoteClient,
    root: &str,
    subpath: Option<&str>,
    mode: ScanMode,
    workers: Option<u32>,
) -> i64 {
    let request = ScanRequest {
        root: root.into(),
        subpath: subpath.map(str::to_owned),
        mode,
        workers,
    }
    .normalized()
    .expect("valid request");
    let started = client.start_scan(&request).expect("POST /scans");
    assert_eq!(started.root.as_deref(), Some(root));
    let mut id = started.progress.session_id;
    while id == 0 {
        std::thread::sleep(Duration::from_millis(100));
        id = client
            .current_scan()
            .unwrap()
            .map(|scan| scan.progress.session_id)
            .unwrap_or(-1);
    }
    id
}

/// 暂停 → (409 BUSY for a second scan and for removals) → 继续 → 取消扫描. False when
/// the scan ended before the pause landed.
fn pause_resume_cancel(
    client: &RemoteClient,
    page: &RemoteProvider,
    root: &str,
    id: i64,
    probe_id: i64,
) -> bool {
    match client.control_scan(id, ScanControl::Pause) {
        Ok(_) => {}
        Err(RemoteError::ScanState(_)) => {
            wait_inactive(client, id);
            return false;
        }
        Err(error) => panic!("pause: {error:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let scan = client.scan_status(id).unwrap();
        if !scan.is_active() {
            return false;
        }
        if scan.progress.paused {
            break;
        }
        assert!(Instant::now() < deadline, "pause not confirmed within 30 s");
        std::thread::sleep(POLL);
    }
    eprintln!("scan {id} paused");
    // The page refuses nothing itself: the server answers 409 BUSY.
    let second = client
        .start_scan(&ScanRequest {
            root: root.into(),
            ..ScanRequest::default()
        })
        .unwrap_err();
    assert!(matches!(second, RemoteError::Busy { .. }), "{second:?}");
    let refused = page.remove_audio(&[probe_id]);
    assert!(refused.is_err(), "removal while paused: {refused:?}");
    eprintln!("removal while paused refused: {}", refused.unwrap_err());
    let resumed = client
        .control_scan(id, ScanControl::Resume)
        .expect("resume");
    assert!(resumed.active);
    match client.control_scan(id, ScanControl::Cancel) {
        Ok(_) | Err(RemoteError::ScanState(_)) => {}
        Err(error) => panic!("cancel: {error:?}"),
    }
    let ended = wait_inactive(client, id);
    eprintln!("scan {id} ended as {:?}", ended.progress.state);
    true
}

#[test]
#[ignore = "destructive: needs AUDIOLIB_LIVE_MANAGE_URL pointing at a disposable test server"]
fn live_desktop_management_page_round_trip() {
    let Ok(url) = std::env::var("AUDIOLIB_LIVE_MANAGE_URL") else {
        eprintln!("AUDIOLIB_LIVE_MANAGE_URL not set; skipping");
        return;
    };
    assert!(
        !url.contains(":8790"),
        "refusing to run destructive tests against the production port 8790"
    );
    let token = std::env::var("AUDIOLIB_LIVE_MANAGE_TOKEN").unwrap_or_default();
    let client = RemoteClient::new(&url, &token).expect("client");
    let directory = tempfile::tempdir().unwrap();
    let cache = RemoteCache::new(directory.path());

    // The page enables management only with can_manage.
    let capabilities = client.capabilities().expect("GET /capabilities");
    eprintln!(
        "capabilities: mode {:?}, external {:?}, can_manage {:?}",
        capabilities.manage_mode, capabilities.external, capabilities.can_manage
    );
    assert!(capabilities.may_manage());
    wait_idle(&client);
    let roots = client.roots().expect("GET /roots");
    let root = roots.first().expect("a root").clone();
    let state = sync(&client, &cache);
    let db = snapshot(&cache, &state);
    let before = count(&db);
    let root_folder: i64 = db
        .query_row(
            "SELECT id FROM folders WHERE folder_path = ?1",
            [&root.server_path],
            |row| row.get(0),
        )
        .expect("the root's top-level folder");
    // A small first-level subfolder as the 子目录 of the scan.
    let (sub_path, sub_files): (String, i64) = db
        .query_row(
            "SELECT f.folder_path, COUNT(a.id) FROM folders f
             JOIN folder_closure c ON c.ancestor_id = f.id
             JOIN audio_files a ON a.folder_id = c.descendant_id
             WHERE f.parent_id = ?1
             GROUP BY f.id ORDER BY COUNT(a.id), f.id LIMIT 1",
            [root_folder],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("a first-level subfolder with files");
    let subpath = sub_path
        .strip_prefix(&format!("{}/", root.server_path.trim_end_matches('/')))
        .expect("subfolder inside the root")
        .to_owned();
    let probe_id: i64 = db
        .query_row(
            "SELECT id FROM audio_files ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    eprintln!(
        "root {} · {before} records · revision {} · subpath {subpath:?} ({sub_files} files)",
        root.name, state.revision
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let page = provider(&client, &cache, &state, &events);

    // 开始扫描 (subpath, 增量) and poll until it ends.
    // As the page sends it: workers left to the server.
    let id = start(
        &client,
        &root.name,
        Some(&subpath),
        ScanMode::Incremental,
        None,
    );
    let scan = wait_inactive(&client, id);
    eprintln!(
        "subpath scan {id}: {:?} · processed {} · revision {:?}",
        scan.progress.state, scan.progress.processed, scan.revision
    );
    assert_eq!(scan.progress.state, ScanState::Completed);
    assert_eq!(scan.subpath.as_deref(), Some(subpath.as_str()));
    let _ = sub_files;

    // 暂停 / 继续 / 取消扫描: a full subpath scan, else the whole root.
    let mut controlled = false;
    for (attempt, target) in [Some(subpath.as_str()), None, None].into_iter().enumerate() {
        // One worker so a pause can land before a small tree finishes.
        let id = start(&client, &root.name, target, ScanMode::Full, Some(1));
        if pause_resume_cancel(&client, &page, &root.name, id, probe_id) {
            controlled = true;
            break;
        }
        eprintln!(
            "attempt {}: scan {id} ended before the pause landed",
            attempt + 1
        );
    }
    assert!(controlled, "could not pause a scan in 3 attempts");
    wait_idle(&client);

    // 移除所选记录… through the page's DataProvider.
    let state = sync(&client, &cache);
    let db = snapshot(&cache, &state);
    let picked: Vec<(i64, String)> = db
        .prepare(
            "SELECT id, file_path FROM audio_files WHERE missing = 0 AND file_path NOT LIKE ?1
             ORDER BY id LIMIT 3 OFFSET 7",
        )
        .unwrap()
        .query_map([format!("{sub_path}/%")], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(picked.len(), 3);
    // 删除目录记录… on a non-root leaf folder.
    let (leaf, leaf_path, leaf_files): (i64, String, i64) = db
        .query_row(
            "SELECT f.id, f.folder_path, COUNT(a.id) FROM folders f
             JOIN audio_files a ON a.folder_id = f.id
             WHERE f.id != ?1 AND f.parent_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM folders c WHERE c.parent_id = f.id)
               AND f.id NOT IN (SELECT folder_id FROM audio_files WHERE id IN (?2, ?3, ?4))
             GROUP BY f.id ORDER BY COUNT(a.id), f.id LIMIT 1",
            [root_folder, picked[0].0, picked[1].0, picked[2].0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("a leaf folder with files");
    let leaf_sample: String = db
        .query_row(
            "SELECT file_path FROM audio_files WHERE folder_id = ?1 LIMIT 1",
            [leaf],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);
    let page = provider(&client, &cache, &state, &events);
    events.lock().unwrap().clear();
    let ids: Vec<i64> = picked.iter().map(|(id, _)| *id).collect();
    let removed = page.remove_audio(&ids).expect("remove records");
    assert_eq!(removed, 3);
    assert_eq!(
        &*events.lock().unwrap(),
        &["resync"],
        "the revision bump re-syncs"
    );
    let synced = sync(&client, &cache);
    assert_eq!(synced.revision, state.revision + 1);
    let db = snapshot(&cache, &synced);
    for (id, path) in &picked {
        assert!(audio_id(&db, path).is_none(), "{id} removed");
    }
    drop(db);
    eprintln!("removed records {ids:?} · revision {}", synced.revision);

    let page = provider(&client, &cache, &synced, &events);
    events.lock().unwrap().clear();
    let removed = page.remove_folder(leaf).expect("remove folder");
    assert_eq!(removed, leaf_files as u64);
    assert_eq!(&*events.lock().unwrap(), &["resync"]);
    let refused = page.remove_folder(root_folder);
    assert!(refused.is_err(), "a root's top-level folder is refused");
    eprintln!("root folder refused: {}", refused.unwrap_err());
    let synced = sync(&client, &cache);
    let db = snapshot(&cache, &synced);
    assert!(audio_id(&db, &leaf_sample).is_none());
    assert_eq!(count(&db), before - 3 - leaf_files);
    drop(db);
    eprintln!(
        "removed folder {leaf_path} ({leaf_files} records) · revision {}",
        synced.revision
    );

    // A full scan re-adds everything with new ids (cached ids must be dropped).
    let id = start(&client, &root.name, None, ScanMode::Full, None);
    let scan = wait_inactive(&client, id);
    assert_eq!(scan.progress.state, ScanState::Completed);
    assert!(
        scan.needs_sync(synced.revision),
        "the scan re-added records"
    );
    let restored = sync(&client, &cache);
    let db = snapshot(&cache, &restored);
    for (old, path) in &picked {
        let new = audio_id(&db, path).expect("re-added by the scan");
        assert_ne!(new, *old, "{path} came back with a new id");
    }
    assert!(audio_id(&db, &leaf_sample).is_some());
    let after = count(&db);
    eprintln!(
        "restored: revision {} · {after} records (before {before})",
        restored.revision
    );
    assert_eq!(after, before, "the test server holds the same files again");
}
