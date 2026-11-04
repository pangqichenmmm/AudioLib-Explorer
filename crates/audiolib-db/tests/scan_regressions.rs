// crates/audiolib-db/tests/scan_regressions.rs
use audiolib_core::{DataProvider, Error, MetadataPatch, ScanState, SearchFilters, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::{Connection, params};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

fn wav(path: &Path, metadata: bool) {
    let mut chunks = Vec::new();
    let mut append = |name: &[u8; 4], bytes: &[u8]| {
        chunks.extend_from_slice(name);
        chunks.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        chunks.extend_from_slice(bytes);
        if !bytes.len().is_multiple_of(2) {
            chunks.push(0);
        }
    };
    let mut fmt = Vec::new();
    for value in [1u16, 1] {
        fmt.extend_from_slice(&value.to_le_bytes());
    }
    for value in [48000u32, 96000] {
        fmt.extend_from_slice(&value.to_le_bytes());
    }
    for value in [2u16, 16] {
        fmt.extend_from_slice(&value.to_le_bytes());
    }
    append(b"fmt ", &fmt);
    if metadata {
        let mut bext = vec![0u8; 602];
        bext[..6].copy_from_slice(b"oldbwf");
        bext[256..265].copy_from_slice(b"oldorigin");
        bext.extend_from_slice(b"oldhistory");
        append(b"bext", &bext);
        append(b"iXML",b"<BWFXML><SCENE>oldscene</SCENE><TAKE>oldtake</TAKE><TAPE>oldtape</TAPE><CIRCLED>TRUE</CIRCLED></BWFXML>");
    }
    append(b"data", &[0; 200]);
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend(chunks);
    fs::write(path, bytes).unwrap();
}
fn scan(service: &ScanService, root: &Path, incremental: bool) {
    let result = service
        .scan(
            root,
            &ScanOptions {
                incremental,
                batch_size: 1,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.state, ScanState::Completed);
    assert_eq!(result.errors, 0);
}

#[test]
fn parallel_probes_match_serial_metadata_and_incremental_counts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    for folder in 0..4 {
        let directory = root.join(format!("folder-{folder}"));
        fs::create_dir_all(&directory).unwrap();
        for index in 0..24 {
            wav(&directory.join(format!("sound-{index}.wav")), true);
        }
    }
    fs::write(root.join("broken.wav"), b"bad header").unwrap();
    let mut reference = None;
    for workers in [1, 4, 16] {
        let provider =
            Arc::new(SqliteProvider::open(&temp.path().join(format!("{workers}.db"))).unwrap());
        let service = ScanService::new(provider.clone());
        let options = ScanOptions {
            workers,
            batch_size: 7,
            ..Default::default()
        };
        let first = service
            .scan(&root, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(
            (first.discovered, first.processed, first.added, first.errors),
            (97, 97, 96, 1)
        );
        let request = SearchRequest {
            query: "oldscene".into(),
            ..Default::default()
        };
        let page = provider.search(&request).unwrap();
        assert_eq!(page.total_count, 96);
        let records = page
            .results
            .into_iter()
            .map(|audio| (audio.file_path, audio.attributes))
            .collect::<std::collections::BTreeMap<_, _>>();
        // Runtime timestamps/encoding are hydrated as attributes; compare source fields only.
        let source: std::collections::BTreeMap<_, _> = records
            .into_iter()
            .map(|(path, attrs)| {
                (
                    path,
                    (
                        attrs["wav_metadata"].clone(),
                        attrs["ixml_scene"].clone(),
                        attrs["bwf_originator"].clone(),
                    ),
                )
            })
            .collect();
        if let Some(expected) = &reference {
            assert_eq!(&source, expected);
        } else {
            reference = Some(source);
        }
        let second = service
            .scan(&root, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(
            (second.updated, second.unchanged, second.errors),
            (0, 96, 1)
        );
    }
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("invalid.db")).unwrap());
    assert!(matches!(
        ScanService::new(provider).scan(
            &root,
            &ScanOptions {
                workers: 17,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {}
        ),
        Err(Error::Validation(_))
    ));
}

#[test]
fn child_then_parent_incremental_scan_repairs_descendants_without_detaching_child_roots() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("parent");
    let child = root.join("child");
    let grandchild = child.join("grandchild");
    fs::create_dir_all(&grandchild).unwrap();
    wav(&child.join("direct.wav"), false);
    wav(&grandchild.join("nested.wav"), false);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    scan(&service, &child, false);
    let child_id = provider.folders(None).unwrap()[0].id;
    let ids: Vec<_> = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .iter()
        .map(|audio| audio.id)
        .collect();
    scan(&service, &root, true); // Metadata is unchanged, so folder repair cannot depend on probing.
    let roots = provider.folders(None).unwrap();
    assert_eq!(roots.len(), 1);
    let root_id = roots[0].id;
    assert_eq!(roots[0].file_count, 2);
    assert_eq!(
        provider
            .search(&SearchRequest {
                folder_id: Some(root_id),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    scan(&service, &child, false); // A narrow scan must preserve the wider tree's parent.
    assert_eq!(provider.folders(None).unwrap()[0].id, root_id);
    assert_eq!(provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(
        provider
            .search(&SearchRequest {
                folder_id: Some(root_id),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .results
            .iter()
            .map(|audio| audio.id)
            .collect::<Vec<_>>(),
        ids
    );
    let conn = Connection::open(provider.database_path()).unwrap();
    assert_eq!(
        conn.query_row("SELECT depth FROM folders WHERE id=?1", [child_id], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT MAX(depth) FROM folder_closure WHERE ancestor_id=?1",
            [root_id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    // Imported Python trees use an absolute base depth. A narrow scan preserves
    // higher parents and keeps child.depth == parent.depth + 1 with that base.
    conn.execute("UPDATE folders SET depth=depth+3", [])
        .unwrap();
    conn.execute(
        "DELETE FROM folder_closure WHERE descendant_id=?1 AND ancestor_id<>?1",
        [child_id],
    )
    .unwrap();
    scan(&service, &child, true);
    assert_eq!(
        conn.query_row("SELECT depth FROM folders WHERE id=?1", [root_id], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row("SELECT depth FROM folders WHERE id=?1", [child_id], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        4
    );
    assert_eq!(provider.folders(Some(root_id)).unwrap()[0].id, child_id);
    assert_eq!(
        provider
            .search(&SearchRequest {
                folder_id: Some(root_id),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
}

#[test]
fn successful_probe_removes_absent_bwf_ixml_and_fts_but_preserves_user_data() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(&root).unwrap();
    let file = root.join("one.wav");
    wav(&file, true);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    scan(&service, &root, true);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    assert_eq!(provider.get_audio(id).unwrap().bext_description, "oldbwf");
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("user tag".into()),
                notes: Some("private notes".into()),
                color: Some("#123456".into()),
                favorite: Some(true),
            },
        )
        .unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    // Legacy rating data lives only in the column now; a rescan must leave it alone.
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute("UPDATE audio_files SET user_rating=5 WHERE id=?1", [id])
        .unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.plugin_private','keep','$.bwf_plugin_private','independent') WHERE id=?1",[id]).unwrap();
    fs::write(&file, b"damaged file").unwrap();
    let failed = service
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(failed.errors, 1);
    assert_eq!(
        provider.get_audio(id).unwrap().bext_description,
        "oldbwf",
        "Failed probing cannot discard Metadata"
    );
    wav(&file, false);
    scan(&service, &root, true);
    let audio = provider.get_audio(id).unwrap();
    assert!(audio.bext_description.is_empty());
    for key in [
        "bwf_description",
        "bwf_originator",
        "bwf_coding_history",
        "ixml_scene",
        "ixml_take",
        "ixml_tape",
        "ixml_circled",
        "ixml_raw",
    ] {
        assert!(audio.attributes.get(key).is_none(), "stale {key}");
    }
    assert_eq!(audio.attributes["plugin_private"], "keep");
    assert_eq!(audio.attributes["bwf_plugin_private"], "independent");
    assert_eq!(
        (
            audio.tags.as_str(),
            audio.notes.as_str(),
            audio.color.as_deref(),
            audio.favorite
        ),
        ("user tag", "private notes", Some("#123456"), true)
    );
    let stored: i64 = conn
        .query_row(
            "SELECT user_rating FROM audio_files WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 5, "rescan keeps stored legacy rating data");
    for query in ["oldbwf", "oldscene", "oldtake"] {
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: query.into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            0
        );
    }
    assert!(
        conn.query_row(
            "SELECT bwf_originator FROM audio_files WHERE id=?1",
            [id],
            |row| row.get::<_, Option<String>>(0)
        )
        .unwrap()
        .is_none()
    );
    assert!(
        conn.query_row(
            "SELECT ixml_tape FROM audio_files WHERE id=?1",
            [id],
            |row| row.get::<_, Option<String>>(0)
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn unicode_tag_folding_is_literal_and_shared_by_counts_pages_and_readonly_connections() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for (id, tags) in [
        (1, "ÉCHO_100% Straße Σ"),
        (2, "écho_100% STRASSE ς"),
        (3, "echoX100more"),
    ] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,user_tags) VALUES(?1,?2,'one.wav',?3)",
            params![id, format!("{id}.wav"), tags],
        )
        .unwrap();
    }
    let request = SearchRequest {
        query: "one".into(),
        filters: SearchFilters {
            tag_contains: Some("éChO_100%".into()),
            ..Default::default()
        },
        limit: 1,
        offset: 1,
        ..Default::default()
    };
    let page = provider.search(&request).unwrap();
    assert_eq!(page.total_count, 2);
    assert_eq!(page.results[0].id, 2);
    for tag in ["STRASSE", "σ"] {
        assert_eq!(
            provider
                .search(&SearchRequest {
                    filters: SearchFilters {
                        tag_contains: Some(tag.into()),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            2
        );
    }
    assert_eq!(
        SqliteProvider::open_read_only(&path)
            .unwrap()
            .search(&request)
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(
        provider
            .search(&SearchRequest {
                filters: SearchFilters {
                    tag_contains: Some("%'; OR 1=1 --".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .total_count,
        0
    );
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn held_scan_process(database: &Path, root: &Path, ready: &Path) -> ChildGuard {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "process_exclusion_and_crash_release",
            "--exact",
            "--nocapture",
        ])
        .env("AUDIOLIB_SCAN_CHILD_DATABASE", database)
        .env("AUDIOLIB_SCAN_CHILD_ROOT", root)
        .env("AUDIOLIB_SCAN_CHILD_READY", ready)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "Child exited before acquiring the scan lease"
        );
        assert!(
            Instant::now() < deadline,
            "Child scan lease acquisition timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

#[test]
fn process_exclusion_and_crash_release() {
    if let Some(database) = std::env::var_os("AUDIOLIB_SCAN_CHILD_DATABASE") {
        let root = std::env::var_os("AUDIOLIB_SCAN_CHILD_ROOT").unwrap();
        let ready = std::env::var_os("AUDIOLIB_SCAN_CHILD_READY").unwrap();
        let provider = Arc::new(SqliteProvider::open(Path::new(&database)).unwrap());
        ScanService::new(provider)
            .scan(
                Path::new(&root),
                &ScanOptions {
                    incremental: false,
                    ..Default::default()
                },
                &ScanCancellation::default(),
                |progress| {
                    if progress.state == ScanState::Running && progress.processed == 0 {
                        fs::write(&ready, b"locked").unwrap();
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line).unwrap();
                    }
                },
            )
            .unwrap();
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(&root).unwrap();
    wav(&root.join("one.wav"), false);
    let database = temp.path().join("db.sqlite");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let service = ScanService::new(provider.clone());
    scan(&service, &root, false);
    let ready = temp.path().join("ready-success");
    let mut child = held_scan_process(&database, &root, &ready);
    assert!(matches!(
        service.scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {}
        ),
        Err(Error::Busy(_))
    ));
    assert_eq!(provider.stats().unwrap().audio_files, 1);
    child
        .0
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"continue\n")
        .unwrap();
    assert!(child.0.wait().unwrap().success());
    scan(&service, &root, true);
    let mut child = held_scan_process(&database, &root, &temp.path().join("ready-crash"));
    assert!(matches!(
        service.scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {}
        ),
        Err(Error::Busy(_))
    ));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    scan(&service, &root, true);
    assert_eq!(provider.stats().unwrap().audio_files, 1);
    let conn = Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM scan_sessions WHERE status='running'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM scan_sessions WHERE status='failed'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let cancel = ScanCancellation::default();
    cancel.cancel();
    assert_eq!(
        service
            .scan(&root, &ScanOptions::default(), &cancel, |_| {})
            .unwrap()
            .state,
        ScanState::Cancelled
    );
    scan(&service, &root, true);
}
