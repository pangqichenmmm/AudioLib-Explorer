// crates/audiolib-db/src/search_page_control/tests.rs
//! Full DTO publication uses real owned WAL fixtures and the provider's cache gates.
use super::*;
use crate::{audio_from_row, audio_projection, register_library_functions, search_snapshot};
use audiolib_core::{DataProvider, SearchScope, SortOrder};
use rusqlite::{OpenFlags, TransactionBehavior, params};

fn fixture() -> (tempfile::TempDir, SqliteProvider) {
    let directory = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&directory.path().join("page-control.db")).unwrap();
    {
        let writer = provider.writer().unwrap();
        writer
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
             (1,'/owned','owned',NULL),(2,'/owned/Wind','Wind',1),(3,'/owned/Other','Other',1);
             INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);",
            )
            .unwrap();
        let metadata = serde_json::json!({
            "wav_metadata":{"fields":{"library":"Shared","keywords":"embeddedtag"}},
            "private_extension":{"retain":"unknown 🦉"}
        })
        .to_string();
        for (id, filename) in [
            (1, "cover.wav"),
            (2, "wind-a.wav"),
            (3, "wind-b.wav"),
            (4, "wind-c.wav"),
        ] {
            writer.execute(
                "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,metadata_json)
                 VALUES(?1,?2,?2,?3,2,'original',?4)",
                params![id, format!("/owned/Wind/{filename}"), filename, metadata],
            ).unwrap();
        }
        for id in 10..=15 {
            writer
                .execute(
                    "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id)
                 VALUES(?1,?2,?2,?3,3)",
                    params![
                        id,
                        format!("/owned/Other/neutral-{id}.wav"),
                        format!("neutral-{id}.wav")
                    ],
                )
                .unwrap();
        }
        writer.execute_batch(
            "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes)
             VALUES(20,'/owned/Other/folder-only.wav','/owned/Other/folder-only.wav','folder-only.wav',3,'original');",
        ).unwrap();
    }
    provider.rebuild_directory_search().unwrap();
    (directory, provider)
}

fn request(scope: SearchScope, query: &str, limit: usize) -> SearchRequest {
    SearchRequest {
        scope,
        query: query.into(),
        limit,
        order_by: SortOrder::Filename,
        ..Default::default()
    }
}

fn warm(provider: &SqliteProvider, request: &SearchRequest) -> SearchPage {
    provider.search(request).unwrap();
    let page = provider.search(request).unwrap();
    assert!(page.cache_hit);
    page
}

fn change_business_rows(conn: &Connection) {
    conn.execute("UPDATE audio_files SET missing=1 WHERE id=1", [])
        .unwrap();
    let vendor = serde_json::json!({
        "fields":{"description":["aftervendor"]},
        "private_source_key":42
    })
    .to_string();
    conn.execute(
        "UPDATE audio_files SET user_notes='requested batch',
         metadata_json=json_set(metadata_json,'$.vendor_metadata',json(?1)) WHERE id=2",
        [vendor],
    )
    .unwrap();
}

fn projected(conn: &Connection, id: i64) -> audiolib_core::AudioFile {
    let projection = audio_projection(conn).unwrap();
    conn.query_row(
        &format!("SELECT {projection} FROM audio_files af WHERE af.id=?1"),
        [id],
        audio_from_row,
    )
    .unwrap()
}

#[test]
fn committed_full_pages_refresh_representatives_of_unchanged_result_rows() {
    let (_directory, provider) = fixture();
    let request = request(SearchScope::Main, "wind", 2);
    let before = warm(&provider, &request);
    assert_eq!(
        before
            .results
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(
        before.results.iter().all(|file| file
            .album_entity
            .as_ref()
            .unwrap()
            .representative_audio_id
            == Some(1))
    );
    let mut writer = provider.writer().unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let baseline = provider.begin_search_cache_batch(&tx).unwrap();
    change_business_rows(&tx);
    let prepared = provider
        .prepare_search_cache_batch(&tx, baseline, &AtomicBool::new(false))
        .unwrap();
    assert!(prepared.is_some());
    let committed = provider
        .commit_search_cache_batch(tx, prepared)
        .unwrap()
        .unwrap();
    provider
        .publish_search_cache_batch(&writer, committed)
        .unwrap();
    drop(writer);
    let current = provider.search(&request).unwrap();
    assert!(
        current.cache_hit,
        "the successful writer published the actual hot request"
    );
    assert_eq!(current.total_count, 3);
    assert_eq!(current.results[0].notes, "requested batch");
    assert_eq!(
        current.results[0].attributes["vendor_metadata"]["fields"]["description"][0],
        "aftervendor"
    );
    assert_eq!(
        current.results[0].attributes["vendor_metadata"]["private_source_key"],
        42
    );
    assert_eq!(
        current.results[0].attributes["private_extension"],
        before.results[0].attributes["private_extension"]
    );
    let unchanged = current.results.iter().find(|file| file.id == 3).unwrap();
    assert_eq!(unchanged.notes, "original");
    assert_eq!(unchanged.album, "Shared");
    assert_eq!(
        unchanged
            .album_entity
            .as_ref()
            .unwrap()
            .representative_audio_id,
        Some(2)
    );
}

#[test]
fn external_album_or_folder_commit_rejects_precomputed_full_pages() {
    for folder_only in [false, true] {
        let (_directory, provider) = fixture();
        let (query, id) = if folder_only {
            ("folder", 20)
        } else {
            ("wind", 2)
        };
        let request = request(SearchScope::Main, query, 2);
        warm(&provider, &request);
        let foreign = Connection::open(provider.database_path()).unwrap();
        register_library_functions(&foreign).unwrap();
        let mut writer = provider.writer().unwrap();
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let baseline = provider.begin_search_cache_batch(&tx).unwrap();
        tx.execute(
            "UPDATE audio_files SET user_notes='requested batch' WHERE id=?1",
            [id],
        )
        .unwrap();
        let prepared = provider
            .prepare_search_cache_batch(&tx, baseline, &AtomicBool::new(false))
            .unwrap();
        let committed = provider
            .commit_search_cache_batch(tx, prepared)
            .unwrap()
            .unwrap();
        let before_external = projected(&writer, id);
        let audio_epoch = crate::search_tracking::epoch(&writer).unwrap();
        assert!(audio_epoch.is_some());
        if folder_only {
            foreign
                .execute(
                    "UPDATE folders SET folder_path='/relocated/Other' WHERE id=3",
                    [],
                )
                .unwrap();
        } else {
            foreign
                .execute(
                    "UPDATE albums SET name='Foreign album' WHERE metadata_key='Shared'",
                    [],
                )
                .unwrap();
        }
        let after_external = projected(&foreign, id);
        assert_eq!(
            crate::search_tracking::epoch(&foreign).unwrap(),
            audio_epoch
        );
        assert_eq!(before_external.notes, after_external.notes);
        assert_ne!(before_external.album_entity, after_external.album_entity);
        provider
            .publish_search_cache_batch(&writer, committed)
            .unwrap();
        let revision = provider.observe_search_revision().unwrap();
        let key = PageRequest::new(&request).unwrap();
        assert!(
            provider
                .first_page_cache
                .lock()
                .unwrap()
                .get(revision, key.bytes())
                .is_none()
        );
        drop(writer);
        let current = provider.search(&request).unwrap();
        assert!(
            !current.cache_hit,
            "rejected precommit DTOs must use fresh SQL"
        );
        assert_eq!(current.results[0].album_entity, after_external.album_entity);
        assert_eq!(current.results[0].notes, "requested batch");
    }
}

#[test]
fn rolled_back_page_candidate_cannot_poison_the_unchanged_observer_revision() {
    let (_directory, provider) = fixture();
    let request = request(SearchScope::Main, "wind", 2);
    let original = warm(&provider, &request);
    let original_revision = provider.observe_search_revision().unwrap();
    let mut writer = provider.writer().unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let baseline = provider.begin_first_pages(&tx).unwrap();
    change_business_rows(&tx);
    let prepared = provider
        .prepare_first_pages(&tx, baseline, &AtomicBool::new(false))
        .unwrap();
    assert!(!prepared.as_ref().unwrap().entries.is_empty());
    tx.rollback().unwrap();
    assert_eq!(
        provider.observe_search_revision().unwrap(),
        original_revision
    );
    // Deliberately exercise the lower-level guard. Runtime publication instead
    // requires the successful-commit capability, which a rollback cannot return.
    provider.publish_first_pages(&writer, prepared).unwrap();
    drop(writer);
    let current = provider.search(&request).unwrap();
    assert!(current.cache_hit);
    assert_eq!(current.total_count, original.total_count);
    assert_eq!(current.results[0].notes, "original");
    assert_eq!(
        current.results[0]
            .album_entity
            .as_ref()
            .unwrap()
            .representative_audio_id,
        Some(1)
    );
    assert!(
        current.results[0]
            .attributes
            .get("vendor_metadata")
            .is_none()
    );
}

#[test]
fn old_pinned_dtos_keep_their_generation_and_new_hits_emit_current_v2_cursors() {
    let (_directory, provider) = fixture();
    let request = request(SearchScope::All, "wind", 1);
    let original = warm(&provider, &request);
    let old_cursor = original.next_cursor.clone().unwrap();
    assert!(old_cursor.starts_with("v2."));
    let old_revision = provider.observe_search_revision().unwrap();
    let key = PageRequest::new(&request).unwrap();
    let retained = provider
        .first_page_cache
        .lock()
        .unwrap()
        .get(old_revision, key.bytes())
        .unwrap();
    let mut reader =
        Connection::open_with_flags(provider.database_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    register_library_functions(&reader).unwrap();
    let old_tx = reader.transaction().unwrap();
    let pinned = search_snapshot::first_page(&provider, &old_tx, &request, &AtomicBool::new(false))
        .unwrap()
        .unwrap();
    let mut writer = provider.writer().unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let baseline = provider.begin_search_cache_batch(&tx).unwrap();
    change_business_rows(&tx);
    let prepared = provider
        .prepare_search_cache_batch(&tx, baseline, &AtomicBool::new(false))
        .unwrap();
    let committed = provider
        .commit_search_cache_batch(tx, prepared)
        .unwrap()
        .unwrap();
    provider
        .publish_search_cache_batch(&writer, committed)
        .unwrap();
    drop(writer);
    let current_revision = provider.observe_search_revision().unwrap();
    assert_ne!(current_revision, old_revision);
    let old_cached = provider
        .first_page_cache
        .lock()
        .unwrap()
        .get(old_revision, key.bytes())
        .unwrap();
    assert!(Arc::ptr_eq(&retained, &old_cached));
    assert_eq!(old_cached.page.results[0].notes, "original");
    let pinned_again =
        search_snapshot::first_page(&provider, &old_tx, &request, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
    assert_eq!(pinned_again.page.total_count, pinned.page.total_count);
    assert_eq!(
        pinned_again.page.results[0].album_entity,
        pinned.page.results[0].album_entity
    );
    let current = provider.search(&request).unwrap();
    assert!(current.cache_hit);
    assert_eq!(current.results[0].id, 2);
    assert_eq!(current.results[0].notes, "requested batch");
    let cursor = current.next_cursor.clone().unwrap();
    assert!(cursor.starts_with("v2."));
    assert_ne!(cursor, old_cursor);
    let next = provider
        .search(&SearchRequest {
            cursor: Some(cursor),
            ..request.clone()
        })
        .unwrap();
    assert_eq!(next.results[0].id, 3);
    assert!(matches!(
        provider.search(&SearchRequest {
            cursor: Some(old_cursor),
            ..request
        }),
        Err(Error::Validation(_))
    ));
}

#[test]
fn cancelled_optional_page_preparation_keeps_requested_business_writes() {
    let (_directory, provider) = fixture();
    let request = request(SearchScope::Main, "wind", 2);
    warm(&provider, &request);
    let mut writer = provider.writer().unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let baseline = provider.begin_first_pages(&tx).unwrap();
    tx.execute(
        "UPDATE audio_files SET user_notes='requested despite cache cancellation' WHERE id=2",
        [],
    )
    .unwrap();
    assert!(matches!(
        provider.prepare_first_pages(&tx, baseline, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(!tx.is_autocommit());
    assert!(
        provider
            .commit_search_cache_batch(tx, None)
            .unwrap()
            .is_none()
    );
    drop(writer);
    assert_eq!(
        provider.get_audio(2).unwrap().notes,
        "requested despite cache cancellation"
    );
    assert_eq!(
        provider.search(&request).unwrap().results[0].notes,
        "requested despite cache cancellation"
    );
}
