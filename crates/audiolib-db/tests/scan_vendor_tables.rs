// crates/audiolib-db/tests/scan_vendor_tables.rs
//! Automatic sidecars use successfully seen, same-directory WAVs and leave source bytes alone.
use audiolib_core::{DataProvider, Error, MetadataPatch, ScanProgress, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

fn tiny_wav() -> Vec<u8> {
    [
        b"RIFF".as_slice(),
        &38u32.to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &48000u32.to_le_bytes(),
        &96000u32.to_le_bytes(),
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &2u32.to_le_bytes(),
        &[0, 0],
    ]
    .concat()
}

fn wav_with_metadata(description: &str, keywords: &str) -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            bytes.push(0);
        }
    }
    let mut bytes = tiny_wav();
    let mut bext = [0_u8; 602];
    assert!(description.len() < 256);
    bext[..description.len()].copy_from_slice(description.as_bytes());
    chunk(&mut bytes, b"bext", &bext);
    let mut info = b"INFO".to_vec();
    chunk(&mut info, b"ICMT", description.as_bytes());
    chunk(&mut info, b"IKEY", keywords.as_bytes());
    chunk(&mut bytes, b"LIST", &info);
    let riff_size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    bytes
}

fn options() -> ScanOptions {
    ScanOptions {
        workers: 2,
        batch_size: 8,
        ..Default::default()
    }
}

fn scan(provider: &Arc<SqliteProvider>, root: &Path) -> ScanProgress {
    ScanService::new(provider.clone())
        .scan(root, &options(), &ScanCancellation::default(), |_| {})
        .unwrap()
}

fn ids(provider: &SqliteProvider, query: &str) -> Vec<i64> {
    let mut ids: Vec<_> = provider
        .search(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .into_iter()
        .map(|file| file.id)
        .collect();
    ids.sort_unstable();
    ids
}

fn audio_id(provider: &SqliteProvider, path: &Path) -> i64 {
    provider.find_audio_by_path(path).unwrap().unwrap().id
}

fn fixture() -> (tempfile::TempDir, PathBuf, Arc<SqliteProvider>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    (temp, root, provider)
}

#[test]
fn incremental_sidecar_is_same_directory_only_and_preserves_annotations_extensions_and_sources() {
    let (_temp, root, provider) = fixture();
    let designed = root.join("Designed");
    let other = root.join("Other");
    fs::create_dir(&designed).unwrap();
    fs::create_dir(&other).unwrap();
    let sound = designed.join("strike.wav");
    let foreign = other.join("foreign.wav");
    let wav = tiny_wav();
    fs::write(&sound, &wav).unwrap();
    fs::write(&foreign, &wav).unwrap();
    assert_eq!(scan(&provider, &root).added, 2);
    let id = audio_id(&provider, &sound);
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("protectedtag".into()),
                notes: Some("protected note".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let connection = rusqlite::Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection
        .execute(
            "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),
         '$.private_extension',json('{\"nested\":[1,{\"preserve\":true}]}')) WHERE id=?1",
            [id],
        )
        .unwrap();
    drop(connection);
    let table = designed.join("catalog.csv");
    let table_bytes = b"Filename,Description,Keywords\nstrike.wav,autovendorwind,windtag\nforeign.wav,shouldnotcrossdir,foreign\n";
    fs::write(&table, table_bytes).unwrap();
    let result = scan(&provider, &root);
    assert_eq!(result.state, ScanState::Completed);
    assert_eq!((result.updated, result.unchanged, result.errors), (0, 2, 0));
    assert_eq!(
        (result.vendor_metadata.tables, result.vendor_metadata.rows),
        (1, 2)
    );
    assert_eq!(
        (
            result.vendor_metadata.matched,
            result.vendor_metadata.updated,
            result.vendor_metadata.unmatched
        ),
        (1, 1, 1)
    );
    assert_eq!(ids(&provider, "autovendorwind"), [id]);
    assert!(ids(&provider, "shouldnotcrossdir").is_empty());
    let file = provider.get_audio(id).unwrap();
    assert_eq!(
        (&*file.tags, &*file.notes, file.favorite),
        ("protectedtag", "protected note", true)
    );
    assert_eq!(
        file.attributes["private_extension"],
        serde_json::json!({"nested":[1,{"preserve":true}]})
    );
    assert!(
        provider
            .get_audio(audio_id(&provider, &foreign))
            .unwrap()
            .attributes
            .get("vendor_metadata")
            .is_none()
    );
    assert_eq!(fs::read(&sound).unwrap(), wav);
    assert_eq!(fs::read(&foreign).unwrap(), wav);
    assert_eq!(fs::read(&table).unwrap(), table_bytes);
}

#[test]
fn unchanged_table_is_reconciled_for_new_wavs_and_table_only_changes_refresh_metadata() {
    let (_temp, root, provider) = fixture();
    let first = root.join("first.wav");
    let future = root.join("future.wav");
    fs::write(&first, tiny_wav()).unwrap();
    let table = root.join("catalog.CSV");
    let initial = b"Filename,Description\nfirst.wav,firstautoterm\nfuture.wav,futureautoterm\n";
    fs::write(&table, initial).unwrap();
    let result = scan(&provider, &root);
    let first_id = audio_id(&provider, &first);
    assert_eq!(
        (
            result.vendor_metadata.updated,
            result.vendor_metadata.unmatched
        ),
        (1, 1)
    );
    let original_attributes = provider.get_audio(first_id).unwrap().attributes.clone();
    let again = scan(&provider, &root);
    assert_eq!((again.added, again.updated, again.unchanged), (0, 0, 1));
    assert_eq!(
        (
            again.vendor_metadata.updated,
            again.vendor_metadata.unchanged
        ),
        (0, 1)
    );
    assert_eq!(
        provider.get_audio(first_id).unwrap().attributes,
        original_attributes
    );
    fs::write(&future, tiny_wav()).unwrap();
    let expanded = scan(&provider, &root);
    assert_eq!((expanded.added, expanded.unchanged), (1, 1));
    assert_eq!(
        (
            expanded.vendor_metadata.matched,
            expanded.vendor_metadata.updated,
            expanded.vendor_metadata.unchanged
        ),
        (2, 1, 1)
    );
    assert_eq!(
        ids(&provider, "futureautoterm"),
        [audio_id(&provider, &future)]
    );
    assert_eq!(
        provider.get_audio(first_id).unwrap().attributes,
        original_attributes
    );
    assert_eq!(fs::read(&table).unwrap(), initial);
    fs::write(
        &table,
        b"Filename,Description\nfirst.wav,revisedautoterm\nfuture.wav,futureautoterm\n",
    )
    .unwrap();
    let revised = scan(&provider, &root);
    assert_eq!(
        (revised.added, revised.updated, revised.unchanged),
        (0, 0, 2)
    );
    assert_eq!(revised.vendor_metadata.updated, 1);
    assert_eq!(ids(&provider, "revisedautoterm"), [first_id]);
}

#[test]
fn reextracted_real_bwf_and_info_restore_deduplicated_sidecar_when_table_is_missing_or_corrupt() {
    const OLD_DESCRIPTION: &str = "equalembeddedvendorword";
    const OLD_KEYWORDS: &str = "equalembeddedkeyword";
    const NEW_DESCRIPTION: &str = "currentreextractedbwfdescription";
    const NEW_KEYWORDS: &str = "currentreextractedinfokeyword";
    for corrupt_table in [false, true] {
        let (temp, root, provider) = fixture();
        let sound = root.join("one.wav");
        fs::write(&sound, wav_with_metadata(OLD_DESCRIPTION, OLD_KEYWORDS)).unwrap();
        let table = root.join("catalog.csv");
        fs::write(
            &table,
            format!("Filename,Description,Keywords\none.wav,{OLD_DESCRIPTION},{OLD_KEYWORDS}\n"),
        )
        .unwrap();
        let initial = scan(&provider, &root);
        assert_eq!(initial.errors, 0);
        assert_eq!(initial.vendor_metadata.matched, 1);
        assert_eq!(
            initial.vendor_metadata.updated, 0,
            "equal source text must not duplicate the projection"
        );
        let id = audio_id(&provider, &sound);
        let before = provider.get_audio(id).unwrap();
        assert_eq!(before.bext_description, OLD_DESCRIPTION);
        assert_eq!(
            before.attributes["wav_metadata"]["fields"]["keywords"],
            OLD_KEYWORDS
        );
        assert!(before.attributes["vendor_metadata"]["fields"]["description"].is_null());
        assert!(before.attributes["vendor_metadata"]["fields"]["keywords"].is_null());
        provider
            .update_metadata(
                id,
                &MetadataPatch {
                    tags: Some("protectedreprojecttag".into()),
                    notes: Some("protected reproject note".into()),
                    favorite: Some(true),
                    color: Some("#123456".into()),
                },
            )
            .unwrap();
        if corrupt_table {
            fs::write(&table, []).unwrap();
        } else {
            fs::rename(&table, temp.path().join("removed-table.csv")).unwrap();
        }
        let changed_wav = wav_with_metadata(NEW_DESCRIPTION, NEW_KEYWORDS);
        fs::write(&sound, &changed_wav).unwrap();
        let result = ScanService::new(provider.clone())
            .scan(
                &root,
                &ScanOptions {
                    incremental: false,
                    ..options()
                },
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        assert_eq!(result.state, ScanState::Completed);
        assert_eq!(
            result.updated, 1,
            "the new WAV header must really be extracted"
        );
        assert_eq!(result.vendor_metadata.tables, u64::from(corrupt_table));
        assert_eq!(result.errors > 0, corrupt_table);
        assert_eq!(
            result.vendor_metadata.updated, 0,
            "no table was successfully parsed in this scan"
        );
        let after = provider.get_audio(id).unwrap();
        assert_eq!(after.bext_description, NEW_DESCRIPTION);
        assert_eq!(
            after.attributes["wav_metadata"]["fields"]["keywords"],
            NEW_KEYWORDS
        );
        assert_eq!(
            after.attributes["vendor_metadata"]["fields"]["description"],
            serde_json::json!([OLD_DESCRIPTION])
        );
        assert_eq!(
            after.attributes["vendor_metadata"]["fields"]["keywords"],
            serde_json::json!([OLD_KEYWORDS])
        );
        for word in [OLD_DESCRIPTION, OLD_KEYWORDS, NEW_DESCRIPTION, NEW_KEYWORDS] {
            assert_eq!(
                ids(&provider, word),
                [id],
                "retained/current source: {word}"
            );
        }
        assert_eq!(after.tags, "protectedreprojecttag");
        assert_eq!(after.notes, "protected reproject note");
        assert_eq!(after.color.as_deref(), Some("#123456"));
        assert!(after.favorite);
        assert_eq!(fs::read(&sound).unwrap(), changed_wav);
        if corrupt_table {
            assert!(fs::read(&table).unwrap().is_empty());
        } else {
            assert!(!table.exists());
        }
    }
}

#[test]
fn failed_probe_and_disappeared_source_cannot_import_into_retained_database_rows() {
    let (_temp, root, provider) = fixture();
    let bad = root.join("bad.wav");
    let gone = root.join("gone.wav");
    fs::write(&bad, tiny_wav()).unwrap();
    fs::write(&gone, tiny_wav()).unwrap();
    scan(&provider, &root);
    let bad_id = audio_id(&provider, &bad);
    let gone_id = audio_id(&provider, &gone);
    fs::write(&bad, b"invalid header").unwrap();
    fs::remove_file(&gone).unwrap();
    fs::write(
        root.join("catalog.csv"),
        b"Filename,Description\nbad.wav,unsafeprobevendor\ngone.wav,unsafegonevendor\n",
    )
    .unwrap();
    let result = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..options()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.state, ScanState::Completed);
    assert!(result.errors > 0);
    assert_eq!(
        (
            result.vendor_metadata.matched,
            result.vendor_metadata.updated,
            result.vendor_metadata.unmatched
        ),
        (0, 0, 2)
    );
    assert_eq!(
        result.removed, 0,
        "probe failure must disable missing reconciliation"
    );
    assert!(!provider.get_audio(bad_id).unwrap().missing);
    assert!(!provider.get_audio(gone_id).unwrap().missing);
    assert!(ids(&provider, "unsafeprobevendor").is_empty());
    assert!(ids(&provider, "unsafegonevendor").is_empty());
}

#[test]
fn malformed_table_is_reported_without_losing_other_imports_or_reconciling_missing_files() {
    let (_temp, root, provider) = fixture();
    let gone = root.join("gone.wav");
    fs::write(&gone, tiny_wav()).unwrap();
    fs::write(root.join("present.wav"), tiny_wav()).unwrap();
    scan(&provider, &root);
    let gone_id = audio_id(&provider, &gone);
    fs::remove_file(&gone).unwrap();
    let bad_table = root.join("bad.csv");
    fs::write(&bad_table, []).unwrap();
    fs::write(
        root.join("good.csv"),
        b"Filename,Description\npresent.wav,validbesidebadtable\n",
    )
    .unwrap();
    let result = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..options()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.state, ScanState::Completed);
    assert!(
        result.errors > 0,
        "an unreadable/empty table must not silently disappear"
    );
    assert!(result.error_details.iter().any(|issue| {
        Path::new(&issue.path)
            .file_name()
            .is_some_and(|name| name == "bad.csv")
    }));
    assert_eq!(result.vendor_metadata.updated, 1);
    assert_eq!(
        ids(&provider, "validbesidebadtable"),
        [audio_id(&provider, &root.join("present.wav"))]
    );
    assert_eq!(result.removed, 0);
    assert!(!provider.get_audio(gone_id).unwrap().missing);
    assert!(fs::read(bad_table).unwrap().is_empty());
}

#[test]
fn real_xls_xlsx_and_tsv_are_discovered_but_office_and_appledouble_files_are_skipped() {
    for name in ["basic.xls", "basic.xlsx"] {
        let (_temp, root, provider) = fixture();
        for sound in ["door.wav", "slam.wav"] {
            fs::write(root.join(sound), tiny_wav()).unwrap();
        }
        let original = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vendor_import")
            .join(name);
        let bytes = fs::read(original).unwrap();
        fs::write(root.join(name), &bytes).unwrap();
        fs::write(root.join("~$lock.xlsx"), b"not a spreadsheet").unwrap();
        fs::write(root.join("._resource.csv"), b"not a spreadsheet").unwrap();
        fs::write(
            root.join("unrequested.txt"),
            b"Filename,Description\ndoor.wav,notautotxt\n",
        )
        .unwrap();
        let result = scan(&provider, &root);
        assert_eq!(result.errors, 0, "{result:?}");
        assert_eq!(
            (
                result.vendor_metadata.tables,
                result.vendor_metadata.rows,
                result.vendor_metadata.matched,
                result.vendor_metadata.updated
            ),
            (1, 3, 3, 2)
        );
        for sound in ["door.wav", "slam.wav"] {
            let file = provider
                .get_audio(audio_id(&provider, &root.join(sound)))
                .unwrap();
            assert!(
                !file.attributes["vendor_metadata"]["fields"]["description"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(ids(&provider, "notautotxt").is_empty());
        assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
    }
    let (_temp, root, provider) = fixture();
    fs::write(root.join("one.wav"), tiny_wav()).unwrap();
    fs::write(
        root.join("catalog.tsv"),
        "Filename\tDescription\none.wav\t自动表格中文独特词\n",
    )
    .unwrap();
    let result = scan(&provider, &root);
    assert_eq!(
        (
            result.vendor_metadata.tables,
            result.vendor_metadata.updated
        ),
        (1, 1)
    );
    assert_eq!(
        ids(&provider, "自动表格中文独特词"),
        [audio_id(&provider, &root.join("one.wav"))]
    );
}

fn wait_vendor_pause(receiver: &mpsc::Receiver<ScanProgress>) -> ScanProgress {
    loop {
        let progress = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("vendor phase must acknowledge pause");
        if progress.paused && progress.vendor_metadata.updated > 0 {
            return progress;
        }
    }
}

#[test]
fn vendor_pause_keeps_scan_lease_releases_writer_and_cancel_retains_durable_metadata() {
    for cancel in [false, true] {
        let (_temp, root, provider) = fixture();
        fs::write(root.join("one.wav"), tiny_wav()).unwrap();
        fs::write(
            root.join("a.csv"),
            b"Filename,Description\none.wav,firstdurablevendor\n",
        )
        .unwrap();
        fs::write(
            root.join("b.csv"),
            b"Filename,Description\none.wav,seconddurablevendor\n",
        )
        .unwrap();
        let control = ScanCancellation::default();
        let worker_control = control.clone();
        let worker_provider = provider.clone();
        let worker_root = root.clone();
        let (updates, receiver) = mpsc::channel();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut requested = false;
            let result = ScanService::new(worker_provider).scan(
                &worker_root,
                &options(),
                &worker_control,
                |progress| {
                    if !requested && progress.vendor_metadata.updated > 0 && !progress.finalizing {
                        requested = true;
                        assert!(worker_control.pause());
                    }
                    updates.send(progress).unwrap();
                },
            );
            done.send(result).unwrap();
        });
        let paused = wait_vendor_pause(&receiver);
        assert!(!paused.finalizing);
        assert_eq!(paused.vendor_metadata.updated, 1);
        assert_eq!(ids(&provider, "firstdurablevendor").len(), 1);
        assert!(ids(&provider, "seconddurablevendor").is_empty());
        assert!(matches!(
            ScanService::new(provider.clone()).scan(
                &root,
                &options(),
                &ScanCancellation::default(),
                |_| {}
            ),
            Err(Error::Busy(_))
        ));
        let id = audio_id(&provider, &root.join("one.wav"));
        provider
            .update_metadata(
                id,
                &MetadataPatch {
                    notes: Some("annotation during table pause".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if cancel {
            control.cancel();
        } else {
            control.resume();
        }
        let result = completion
            .recv_timeout(Duration::from_secs(15))
            .expect("table phase must drain")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(result.session_id, paused.session_id);
        assert_eq!(
            result.state,
            if cancel {
                ScanState::Cancelled
            } else {
                ScanState::Completed
            }
        );
        assert_eq!(result.errors, 0);
        assert_eq!(result.removed, 0);
        assert_eq!(
            provider.get_audio(id).unwrap().notes,
            "annotation during table pause"
        );
        assert_eq!(ids(&provider, "firstdurablevendor"), [id]);
        assert_eq!(
            ids(&provider, "seconddurablevendor").len(),
            usize::from(!cancel)
        );
        // The token is terminal, but its OS lease has been released for a fresh scan.
        assert_eq!(scan(&provider, &root).state, ScanState::Completed);
        assert_eq!(fs::read(root.join("one.wav")).unwrap(), tiny_wav());
    }
}

#[test]
fn unowned_reserved_basename_is_not_skipped_or_adopted_as_import_storage() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    let reserved = root.join("catalog.db.vendor-import");
    fs::create_dir_all(&reserved).unwrap();
    let wav = tiny_wav();
    fs::write(reserved.join("user.wav"), &wav).unwrap();
    let csv = b"Filename,Description\nuser.wav,mustnotadoptuserdirectory\n";
    fs::write(reserved.join("catalog.csv"), csv).unwrap();
    let provider = Arc::new(SqliteProvider::open(&root.join("catalog.db")).unwrap());
    for _ in 0..2 {
        let result = scan(&provider, &root);
        assert_eq!(
            result.discovered, 1,
            "a basename cannot establish ownership"
        );
        assert!(
            result.errors > 0,
            "automatic staging must refuse to adopt a foreign directory"
        );
        assert!(
            provider
                .find_audio_by_path(&reserved.join("user.wav"))
                .unwrap()
                .is_some()
        );
        let mut names: Vec<_> = fs::read_dir(&reserved)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["catalog.csv", "user.wav"]);
        assert_eq!(fs::read(reserved.join("user.wav")).unwrap(), wav);
        assert_eq!(fs::read(reserved.join("catalog.csv")).unwrap(), csv);
    }
}

#[test]
fn provider_owned_store_is_not_rediscovered_when_scanning_database_directory_or_ancestor() {
    for scan_ancestor in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let library_dir = temp.path().join("library");
        let designed = library_dir.join("Designed");
        fs::create_dir_all(&designed).unwrap();
        let database = library_dir.join("catalog.db");
        let provider = Arc::new(SqliteProvider::open(&database).unwrap());
        fs::write(designed.join("one.wav"), tiny_wav()).unwrap();
        fs::write(
            designed.join("catalog.csv"),
            b"Filename,Description\none.wav,ownedstorevendor\n",
        )
        .unwrap();
        let root = if scan_ancestor {
            temp.path()
        } else {
            library_dir.as_path()
        };
        assert_eq!(scan(&provider, root).vendor_metadata.tables, 1);
        let store = library_dir.join("catalog.db.vendor-import");
        assert!(
            store.is_dir(),
            "the importer must have created its verified owned store"
        );
        // A future staging format may retain an original extension. Even then,
        // an exact provider-owned directory must not become an input producer.
        fs::write(
            store.join("would-recurse.csv"),
            b"Filename,Description\none.wav,neverrediscovered\n",
        )
        .unwrap();
        let again = scan(&provider, root);
        assert_eq!(again.errors, 0, "{again:?}");
        assert_eq!(
            (
                again.vendor_metadata.tables,
                again.vendor_metadata.updated,
                again.vendor_metadata.unchanged
            ),
            (1, 0, 1)
        );
        assert!(ids(&provider, "neverrediscovered").is_empty());
    }
}
