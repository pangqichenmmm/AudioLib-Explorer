// server/audiolib-server/tests/migration_snapshot.rs
//! Native HTTP migration, snapshot publication, cache updates and offline recovery.
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest, SearchScope};
use audiolib_db::SqliteProvider;
use audiolib_remote::{CacheUpdate, RemoteCache, RemoteClient, RemoteWriter, sync_library};
use audiolib_server::{AppState, Security, ServeOptions, ServerConfig, router, serve};
use std::{sync::atomic::AtomicBool, time::Duration};

fn assert_search(provider: &SqliteProvider, query: &str, scope: SearchScope) {
    let page = provider
        .search(&SearchRequest {
            query: query.into(),
            scope,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
        vec![1]
    );
}

fn migrate(client: &RemoteClient, action: &str, expected: &str) {
    let mut report = client.migration_action(action, true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while report.state != expected {
        assert!(report.error_code.is_none(), "{report:?}");
        assert!(std::time::Instant::now() < deadline, "{report:?}");
        std::thread::sleep(Duration::from_millis(20));
        report = client.migration_status().unwrap();
    }
    assert!(report.error_code.is_none(), "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema9_migration_sync_metadata_offline_and_rollback_preserve_the_library() {
    use audiolib_core::compatibility::{PLAN_B_FORMAT, PLAN_B_V9_FORMAT};
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    drop(SqliteProvider::open(&database).unwrap());
    let conn = rusqlite::Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
         (1,'owned','OwnedRoot',NULL),(2,'owned/Metalfolder','Metalfolder',1),
         (3,'owned/Otherfolder','Otherfolder',1);
         INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(1,2,1),(3,3,0),(1,3,1);
         INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,metadata_json)
         VALUES(1,'owned/Metalfolder/hit.wav','owned/Metalfolder/hit.wav','impact.wav',2,
         'originalannotation','{\"unknown_extension\":{\"retained\":true}}');
         INSERT INTO audio_files(id,file_path,path_key,filename,folder_id) VALUES
         (2,'owned/Otherfolder/rain.wav','owned/Otherfolder/rain.wav','rain.wav',3),
         (3,'owned/Otherfolder/wind.wav','owned/Otherfolder/wind.wav','wind.wav',3);",
    )
    .unwrap();
    drop(conn);
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nmanage_mode='all'\nlist_waveform_workers=1\n",
        root.path(),
    ).unwrap();
    let state =
        AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(serve(
        listener,
        router(state.clone()),
        async {
            let _ = stopped.await;
        },
        ServeOptions {
            shutdown_grace: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    let data = root.path().join("client");
    // Keep cleanup outside the blocking assertion task, including on failure.
    let result =
        tokio::task::spawn_blocking(move || {
            let client = RemoteClient::new(&url, "").unwrap();
            let cache = RemoteCache::new(&data);
            let cancel = AtomicBool::new(false);
            let before = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
            let identity = before.state.library_id.clone();
            assert_eq!(before.state.schema_version, 8);
            let original_path = cache.database_path(&before.state);
            let original_bytes = std::fs::read(&original_path).unwrap();
            migrate(&client, "start", "done");
            assert_eq!(
                client.library().unwrap().snapshot_format.unwrap().id,
                PLAN_B_V9_FORMAT
            );
            let migrated = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
            assert!(migrated.downloaded);
            assert_eq!(migrated.state.schema_version, 9);
            assert_eq!(migrated.state.library_id, identity);
            assert_eq!(std::fs::read(&original_path).unwrap(), original_bytes);
            let snapshot =
                SqliteProvider::open_read_only(&cache.database_path(&migrated.state)).unwrap();
            assert_search(&snapshot, "impact", SearchScope::Main);
            assert_search(&snapshot, "Metalfolder", SearchScope::Path);
            drop(snapshot);
            let writer = RemoteWriter::new(client.clone(), cache.clone(), &identity);
            let written = writer
                .update_metadata(
                    1,
                    &MetadataPatch {
                        notes: Some("postmigrationannotation".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                matches!(written.cache, CacheUpdate::Applied { .. }),
                "{:?}",
                written.cache
            );
            let current = cache.active().unwrap();
            let prepared = cache.prepare_cached_state(&current).unwrap();
            assert_eq!(prepared.schema_version, 9);
            let offline = SqliteProvider::open_read_only(&cache.database_path(&prepared)).unwrap();
            assert_search(&offline, "postmigrationannotation", SearchScope::Main);
            assert_search(&offline, "Metalfolder", SearchScope::Path);
            assert_eq!(
                offline.get_audio(1).unwrap().attributes["unknown_extension"]["retained"],
                true
            );
            drop(offline);
            migrate(&client, "rollback", "rolled_back");
            assert_eq!(
                client.library().unwrap().snapshot_format.unwrap().id,
                PLAN_B_FORMAT
            );
            let rollback = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
            assert!(rollback.downloaded);
            assert_eq!(rollback.state.schema_version, 8);
            assert_eq!(rollback.state.library_id, identity);
            let restored =
                SqliteProvider::open_read_only(&cache.database_path(&rollback.state)).unwrap();
            assert_search(&restored, "postmigrationannotation", SearchScope::Main);
            let conn = rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let metadata: String = conn
                .query_row(
                    "SELECT metadata_json FROM audio_files WHERE id=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&metadata).unwrap()["unknown_extension"]
                    ["retained"],
                true
            );
        })
        .await;
    let _ = stop.send(());
    serving.await.unwrap();
    state.shutdown(Duration::from_secs(5)).unwrap();
    result.unwrap();
}
