// crates/audiolib-db/src/vendor_import/automatic/tests.rs
//! Automatic-source preservation, normalization and confined source capabilities.
use super::*;
use crate::{ScanCancellation, ScanOptions, ScanService};
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest};
use std::sync::Arc;

fn wav() -> Vec<u8> {
    [
        b"RIFF".as_slice(),
        &38_u32.to_le_bytes(),
        b"WAVEfmt ",
        &16_u32.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &48000_u32.to_le_bytes(),
        &96000_u32.to_le_bytes(),
        &2_u16.to_le_bytes(),
        &16_u16.to_le_bytes(),
        b"data",
        &2_u32.to_le_bytes(),
        &[0, 0],
    ]
    .concat()
}
fn fixture() -> (tempfile::TempDir, PathBuf, Arc<SqliteProvider>, i64, i64) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("door.wav"), wav()).unwrap();
    let root = root.canonicalize().unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let scan = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let id = provider
        .find_audio_by_path(&root.join("door.wav"))
        .unwrap()
        .unwrap()
        .id;
    (temp, root, provider, id, scan.session_id)
}
fn refresh(provider: &SqliteProvider, root: &Path, name: &str, session: i64) -> VendorScanSummary {
    provider
        .scan_vendor_table(&root.join(name), root, session, &mut |_| true)
        .unwrap()
}
fn count(provider: &SqliteProvider, query: &str) -> usize {
    provider
        .search(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .len()
}
fn metadata(provider: &SqliteProvider, id: i64) -> serde_json::Value {
    let conn = provider.reader().unwrap();
    let raw: String = conn
        .query_row(
            "SELECT metadata_json FROM audio_files WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn automatic_sources_deduplicate_normalized_text_but_keep_raw_events_and_user_tag_independence() {
    let (_temp, root, provider, id, session) = fixture();
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("wood".into()),
                notes: Some("keep notes".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fs::write(
        root.join("a.csv"),
        "Filename,Description,Keywords\ndoor.wav,Café  wind,wood\n",
    )
    .unwrap();
    fs::write(
        root.join("b.csv"),
        "Filename,Description,Keywords\ndoor.wav,CAFE\u{301} wind,WOOD\n",
    )
    .unwrap();
    assert_eq!(refresh(&provider, &root, "a.csv", session).updated, 1);
    assert_eq!(refresh(&provider, &root, "b.csv", session).updated, 0);
    let value = metadata(&provider, id);
    assert_eq!(
        value["vendor_metadata"]["fields"]["description"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        value["vendor_metadata"]["fields"]["keywords"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let conn = provider.reader().unwrap();
    let events: i64 = conn
        .query_row(
            "SELECT count(*) FROM audiolib_vendor_events WHERE audio_file_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(events, 4);
    drop(conn);
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(count(&provider, "wood"), 1);
    assert!(
        audiolib_core::source_tags(&metadata(&provider, id))
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case("wood"))
    );
    assert_eq!(
        provider
            .find_audio_by_path(&root.join("door.wav"))
            .unwrap()
            .unwrap()
            .notes,
        "keep notes"
    );
}

#[test]
fn identical_refresh_has_no_business_commit_and_successful_staging_does_not_accumulate() {
    let (_temp, root, provider, _id, session) = fixture();
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,sidecarwind\n",
    )
    .unwrap();
    assert_eq!(refresh(&provider, &root, "table.csv", session).updated, 1);
    let observer = Connection::open(provider.database_path()).unwrap();
    let before: i64 = observer
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    let repeat = refresh(&provider, &root, "table.csv", session);
    assert_eq!((repeat.updated, repeat.unchanged), (0, 1));
    let after: i64 = observer
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        after, before,
        "stable automatic source must not rewrite jobs/events/AF"
    );
    let owned = provider.vendor_import_owned_root().unwrap().unwrap();
    for child in fs::read_dir(owned).unwrap() {
        let child = child.unwrap();
        if child.file_type().unwrap().is_dir() {
            assert!(!fs::read_dir(child.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("automatic-")
            }));
        }
    }
}

#[test]
fn unsafe_explicit_paths_never_fall_back_and_partial_refresh_keeps_old_only_contributions() {
    let (_temp, root, provider, _id, session) = fixture();
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,lastgoodwind\n",
    )
    .unwrap();
    refresh(&provider, &root, "table.csv", session);
    for path in [
        "../door.wav",
        "elsewhere/door.wav",
        "/outside/door.wav",
        "C:\\outside\\door.wav",
    ] {
        fs::write(
            root.join("table.csv"),
            format!("Filename,Path,Description\ndoor.wav,{path},mustnotreplace\n"),
        )
        .unwrap();
        let result = refresh(&provider, &root, "table.csv", session);
        assert_eq!(
            (result.matched, result.unmatched, result.updated),
            (0, 1, 0)
        );
        assert_eq!(count(&provider, "lastgoodwind"), 1);
        assert_eq!(count(&provider, "mustnotreplace"), 0);
    }
    fs::write(
        root.join("table.csv"),
        "Filename,Description\n./door.wav,replacementwind\n",
    )
    .unwrap();
    assert_eq!(refresh(&provider, &root, "table.csv", session).updated, 1);
    assert_eq!(count(&provider, "lastgoodwind"), 0);
    assert_eq!(count(&provider, "replacementwind"), 1);
}

#[test]
fn partial_reordered_table_keeps_all_earlier_contributions_until_fully_associated() {
    let (_temp, root, provider, _id, session) = fixture();
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,oldfirstwind\ndoor.wav,oldsecondwind\n",
    )
    .unwrap();
    refresh(&provider, &root, "table.csv", session);
    fs::write(
        root.join("table.csv"),
        // An inserted unassociated row and reordering cannot prove which old
        // contribution was intentionally removed. Even the old safe value stays.
        "Filename,Description\n../door.wav,mustnotapply\ndoor.wav,newfirstwind\ndoor.wav,newsecondwind\n",
    )
    .unwrap();
    let result = refresh(&provider, &root, "table.csv", session);
    assert_eq!(
        (result.matched, result.unmatched, result.updated),
        (2, 1, 1)
    );
    assert_eq!(count(&provider, "oldfirstwind"), 1);
    assert_eq!(count(&provider, "newfirstwind"), 1);
    assert_eq!(count(&provider, "newsecondwind"), 1);
    assert_eq!(count(&provider, "oldsecondwind"), 1);
    assert_eq!(count(&provider, "mustnotapply"), 0);
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,finalwind\n",
    )
    .unwrap();
    refresh(&provider, &root, "table.csv", session);
    assert_eq!(count(&provider, "oldfirstwind"), 0);
    assert_eq!(count(&provider, "oldsecondwind"), 0);
    assert_eq!(count(&provider, "newfirstwind"), 0);
    assert_eq!(count(&provider, "newsecondwind"), 0);
    assert_eq!(count(&provider, "finalwind"), 1);
}

#[test]
fn foreign_same_named_store_and_oversized_markers_remain_unowned() {
    let (_temp, root, provider, _id, session) = fixture();
    let store = provider.path.with_file_name("library.db.vendor-import");
    fs::create_dir(&store).unwrap();
    fs::write(store.join("user.wav"), wav()).unwrap();
    fs::write(store.join(".audiolib-owner"), vec![b'x'; 1024 * 1024]).unwrap();
    assert!(provider.vendor_import_owned_root().unwrap().is_none());
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,refusedwind\n",
    )
    .unwrap();
    assert!(
        provider
            .scan_vendor_table(&root.join("table.csv"), &root, session, &mut |_| true)
            .is_err()
    );
    assert!(store.join("user.wav").is_file());
    assert_eq!(count(&provider, "refusedwind"), 0);
}

#[test]
fn missing_or_malformed_tables_report_examined_before_failure_and_keep_last_good() {
    let (_temp, root, provider, _id, session) = fixture();
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,lastgoodwind\n",
    )
    .unwrap();
    refresh(&provider, &root, "table.csv", session);
    for contents in ["", "Filename,Description\n"] {
        fs::write(root.join("table.csv"), contents).unwrap();
        let mut observed = Vec::new();
        assert!(
            provider
                .scan_vendor_table(&root.join("table.csv"), &root, session, &mut |summary| {
                    observed.push(*summary);
                    true
                })
                .is_err()
        );
        assert_eq!(observed.first().unwrap().tables, 1);
        assert_eq!(count(&provider, "lastgoodwind"), 1);
    }
}

#[cfg(unix)]
#[test]
fn a_replaced_preview_database_cannot_redirect_staging_writes() {
    use std::os::unix::fs::symlink;
    let (temp, root, provider, _id, session) = fixture();
    fs::write(
        root.join("table.csv"),
        "Filename,Description\ndoor.wav,lastgoodwind\n",
    )
    .unwrap();
    refresh(&provider, &root, "table.csv", session);
    let outside = temp.path().join("unrelated.db");
    let conn = Connection::open(&outside).unwrap();
    conn.execute_batch(
        "CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('untouched');",
    )
    .unwrap();
    drop(conn);
    let before = fs::read(&outside).unwrap();
    let store = provider.vendor_import_owned_root().unwrap().unwrap();
    let job = fs::read_dir(store)
        .unwrap()
        .map(|entry| entry.unwrap())
        .find(|entry| entry.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    fs::remove_file(job.join("preview.db")).unwrap();
    symlink(&outside, job.join("preview.db")).unwrap();
    assert!(
        provider
            .scan_vendor_table(&root.join("table.csv"), &root, session, &mut |_| true)
            .is_err()
    );
    assert_eq!(fs::read(outside).unwrap(), before);
    assert_eq!(count(&provider, "lastgoodwind"), 1);
}

#[cfg(unix)]
#[test]
fn approved_root_parent_and_child_symlink_replacements_are_not_authorized() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    fs::create_dir_all(root.join("parent")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("parent/table.csv"), b"approved").unwrap();
    fs::write(outside.join("table.csv"), b"outside").unwrap();
    let root = root.canonicalize().unwrap();
    let mut pinned = rooted::open(&root, &root.join("parent/table.csv")).unwrap();
    fs::rename(root.join("parent"), root.join("old-parent")).unwrap();
    symlink(&outside, root.join("parent")).unwrap();
    assert!(rooted::open(&root, &root.join("parent/table.csv")).is_err());
    let mut value = String::new();
    pinned.file.read_to_string(&mut value).unwrap();
    assert_eq!(value, "approved");
    fs::remove_file(root.join("parent")).unwrap();
    fs::rename(root.join("old-parent"), root.join("parent")).unwrap();
    fs::remove_file(root.join("parent/table.csv")).unwrap();
    symlink(outside.join("table.csv"), root.join("parent/table.csv")).unwrap();
    assert!(rooted::open(&root, &root.join("parent/table.csv")).is_err());
    fs::rename(&root, temp.path().join("old-root")).unwrap();
    symlink(&outside, &root).unwrap();
    assert!(rooted::open(&root, &root.join("table.csv")).is_err());
}

#[cfg(windows)]
#[test]
fn opened_file_and_parent_are_pinned_until_source_copy_finishes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let parent = root.join("parent");
    fs::create_dir(&parent).unwrap();
    let path = parent.join("table.csv");
    fs::write(&path, b"approved").unwrap();
    let mut source = rooted::open(&root, &path).unwrap();
    assert!(fs::rename(&path, parent.join("moved.csv")).is_err());
    assert!(fs::rename(&parent, root.join("moved-parent")).is_err());
    assert!(OpenOptions::new().write(true).open(&path).is_err());
    let mut value = String::new();
    source.file.read_to_string(&mut value).unwrap();
    assert_eq!(value, "approved");
    drop(source);
    fs::rename(&path, parent.join("moved.csv")).unwrap();
}

fn description(provider: &SqliteProvider, root: &Path, relative: &str) -> Option<String> {
    let id = provider
        .find_audio_by_path(&root.join(relative))
        .unwrap()
        .unwrap()
        .id;
    metadata(provider, id)["vendor_metadata"]["fields"]["description"]
        .as_array()
        .and_then(|values| values.first())
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value["value"].as_str().map(str::to_owned))
        })
}

#[test]
fn tables_match_their_subtree_and_docs_sounds_siblings_but_not_the_library_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    for directory in [
        "Lib/Sub",
        "Other",
        "Pack/Pack--Docs",
        "Pack/Pack--Sounds 48k",
        "Pack/Extras",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    for file in [
        "Lib/same.wav",
        "Lib/Sub/child.wav",
        "Other/elsewhere.wav",
        "Pack/Pack--Sounds 48k/sibling.wav",
        "Pack/Extras/extra.wav",
    ] {
        fs::write(root.join(file), wav()).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let session = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
        .session_id;
    fs::write(
        root.join("Lib/table.csv"),
        "Filename,Description\nsame.wav,alpha\nchild.wav,beta\nelsewhere.wav,delta\nsame,nosuffix\n",
    )
    .unwrap();
    fs::write(
        root.join("Pack/Pack--Docs/table.csv"),
        "Filename,Description\nsibling.wav,gamma\nextra.wav,epsilon\n",
    )
    .unwrap();

    let lib = refresh(&provider, &root, "Lib/table.csv", session);
    assert_eq!((lib.matched, lib.unmatched, lib.ambiguous), (2, 2, 0));
    let pack = refresh(&provider, &root, "Pack/Pack--Docs/table.csv", session);
    assert_eq!((pack.matched, pack.unmatched, pack.ambiguous), (1, 1, 0));

    assert_eq!(
        description(&provider, &root, "Lib/same.wav").as_deref(),
        Some("alpha")
    );
    assert_eq!(
        description(&provider, &root, "Lib/Sub/child.wav").as_deref(),
        Some("beta")
    );
    assert_eq!(
        description(&provider, &root, "Pack/Pack--Sounds 48k/sibling.wav").as_deref(),
        Some("gamma")
    );
    assert_eq!(description(&provider, &root, "Other/elsewhere.wav"), None);
    assert_eq!(description(&provider, &root, "Pack/Extras/extra.wav"), None);
}

#[test]
fn duplicate_basenames_inside_the_table_subtree_remain_ambiguous() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(root.join("Lib/A")).unwrap();
    fs::create_dir_all(root.join("Lib/B")).unwrap();
    fs::write(root.join("Lib/A/hit.wav"), wav()).unwrap();
    fs::write(root.join("Lib/B/hit.wav"), wav()).unwrap();
    let root = root.canonicalize().unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let session = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
        .session_id;
    fs::write(
        root.join("Lib/table.csv"),
        "Filename,Description\nhit.wav,boom\n",
    )
    .unwrap();
    let summary = refresh(&provider, &root, "Lib/table.csv", session);
    assert_eq!((summary.matched, summary.ambiguous), (0, 1));
    assert_eq!(description(&provider, &root, "Lib/A/hit.wav"), None);
}

#[test]
fn side_by_side_packs_only_pair_docs_and_sounds_with_the_same_stem() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    for directory in [
        "Foley/PackA--Docs",
        "Foley/PackA--Sounds",
        "Foley/PackB--Docs",
        "Foley/PackB--Sounds 48k",
    ] {
        fs::create_dir_all(root.join(directory)).unwrap();
    }
    fs::write(root.join("Foley/PackA--Sounds/a1.wav"), wav()).unwrap();
    fs::write(root.join("Foley/PackB--Sounds 48k/b1.wav"), wav()).unwrap();
    let root = root.canonicalize().unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("library.db")).unwrap());
    let session = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
        .session_id;
    let table = "Filename,Description\na1.wav,alpha\nb1.wav,beta\n";
    fs::write(root.join("Foley/PackA--Docs/table.csv"), table).unwrap();
    let a = refresh(&provider, &root, "Foley/PackA--Docs/table.csv", session);
    assert_eq!((a.matched, a.unmatched), (1, 1));
    assert_eq!(
        description(&provider, &root, "Foley/PackA--Sounds/a1.wav").as_deref(),
        Some("alpha")
    );
    assert_eq!(
        description(&provider, &root, "Foley/PackB--Sounds 48k/b1.wav"),
        None
    );

    // A table in a Sounds folder sees its own pack's siblings, not the other pack.
    fs::write(
        root.join("Foley/PackB--Sounds 48k/table.csv"),
        "Filename,Description\na1.wav,wrong\n",
    )
    .unwrap();
    let b = refresh(
        &provider,
        &root,
        "Foley/PackB--Sounds 48k/table.csv",
        session,
    );
    assert_eq!((b.matched, b.unmatched), (0, 1));

    // A table in an ordinary folder does not widen to sibling packs at all.
    fs::create_dir_all(root.join("Foley/Notes")).unwrap();
    fs::write(root.join("Foley/Notes/table.csv"), table).unwrap();
    let n = refresh(&provider, &root, "Foley/Notes/table.csv", session);
    assert_eq!((n.matched, n.unmatched), (0, 2));
}

#[test]
fn pack_stems_are_case_insensitive_and_require_a_marker() {
    assert_eq!(
        pack_stem("Rustle_Tones--Docs").as_deref(),
        Some("rustle_tones")
    );
    assert_eq!(
        pack_stem("Rustle_Tones--Sounds 48k").as_deref(),
        Some("rustle_tones")
    );
    assert_eq!(
        pack_stem("RUSTLE_TONES--SOUNDS").as_deref(),
        Some("rustle_tones")
    );
    assert_eq!(pack_stem("Rustle_Tones"), None);
    assert_eq!(pack_stem("--Docs"), None);
}
