// crates/audiolib-db/src/search_page_control/rank_deprioritization_tests.rs
//! Owned-provider regressions for skipping duplicate optional full-rank work.
use super::*;
use crate::{search_cache::CacheRevision, search_tracking};
use audiolib_core::DataProvider;
use rusqlite::{TransactionBehavior, params};
use std::collections::BTreeMap;

fn fixture() -> (tempfile::TempDir, SqliteProvider) {
    let directory = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&directory.path().join("rank-pages.db")).unwrap();
    let writer = provider.writer().unwrap();
    writer
        .execute_batch(
            "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
             (1,'/owned','owned',NULL),(2,'/owned/First','First',1),(3,'/owned/Other','Other',1);
             INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);",
        )
        .unwrap();
    for id in 1..=10 {
        let filename = if id <= 6 {
            format!("wind-{id}.wav")
        } else {
            format!("neutral-{id}.wav")
        };
        let folder = if id == 1 { 2 } else { 3 };
        writer
            .execute(
                "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes)
                 VALUES(?1,?2,?2,?3,?4,'wind')",
                params![id, format!("/owned/{folder}/{filename}"), filename, folder],
            )
            .unwrap();
    }
    drop(writer);
    (directory, provider)
}

fn request(scope: SearchScope, limit: usize) -> SearchRequest {
    SearchRequest {
        query: "wind".into(),
        scope,
        limit,
        order_by: SortOrder::Relevance,
        ..Default::default()
    }
}

fn commit(provider: &SqliteProvider, change: impl FnOnce(&Connection)) {
    let mut writer = provider.writer().unwrap();
    let tx = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let baseline = provider.begin_search_cache_batch(&tx).unwrap();
    change(&tx);
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
}

fn rank_key(provider: &SqliteProvider) -> CacheRevision {
    CacheRevision::Tracked(
        search_tracking::epoch(&provider.reader().unwrap())
            .unwrap()
            .unwrap(),
    )
}

fn native_ids(provider: &SqliteProvider, request: &SearchRequest) -> Vec<i64> {
    let query = crate::compile_request_query(&provider.tokenizer, request).unwrap();
    let reader = provider.reader().unwrap();
    let mut statement = reader
        .prepare(
            "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1
             ORDER BY bm25(audio_search),rowid",
        )
        .unwrap();
    statement
        .query_map([query], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn ids(page: &SearchPage) -> Vec<i64> {
    page.results.iter().map(|file| file.id).collect()
}

#[test]
fn accepted_main_page_skips_only_its_rank_and_cold_deep_page_and_facets_remain_complete() {
    let (_directory, provider) = fixture();
    let main = request(SearchScope::Main, 2);
    let filename = request(SearchScope::Filename, 201); // Deliberately not page-cache eligible.
    provider.search(&main).unwrap();
    assert!(provider.search(&main).unwrap().cache_hit);
    assert_eq!(provider.search(&filename).unwrap().total_count, 6);
    let main_query = crate::compile_request_query(&provider.tokenizer, &main).unwrap();
    let filename_query = crate::compile_request_query(&provider.tokenizer, &filename).unwrap();
    assert_ne!(main_query, filename_query);
    commit(&provider, |tx| {
        tx.execute("UPDATE audio_files SET user_notes='quiet' WHERE id=10", [])
            .unwrap();
        tx.execute(
            "UPDATE audio_files SET filename='wind-added.wav' WHERE id=8",
            [],
        )
        .unwrap();
    });
    let revision = rank_key(&provider);
    {
        let cache = provider.ranking_cache.lock().unwrap();
        assert!(cache.peek(&revision, &main_query).is_none());
        assert_eq!(cache.peek(&revision, &filename_query).unwrap().len(), 7);
    }
    let oracle = native_ids(&provider, &main);
    assert_eq!(oracle.len(), 9);
    let first = provider.search(&main).unwrap();
    assert!(first.cache_hit);
    assert_eq!(first.total_count, 9);
    assert_eq!(ids(&first), oracle[..2]);
    assert!(
        provider
            .ranking_cache
            .lock()
            .unwrap()
            .peek(&revision, &main_query)
            .is_none(),
        "a two-row cached page must not masquerade as the nine-row full ranking"
    );
    let deep = provider
        .search(&SearchRequest {
            offset: 2,
            ..main.clone()
        })
        .unwrap();
    assert_eq!(deep.total_count, 9);
    assert_eq!(ids(&deep), oracle[2..4]);
    assert_eq!(
        provider
            .ranking_cache
            .lock()
            .unwrap()
            .peek(&revision, &main_query)
            .unwrap()
            .len(),
        9,
        "the ordinary cold deep-page path reconstructs the complete current ranking"
    );
    let actual = provider
        .search_folders(&main)
        .unwrap()
        .into_iter()
        .map(|folder| (folder.id, folder.file_count))
        .collect::<BTreeMap<_, _>>();
    let reader = provider.reader().unwrap();
    let mut statement = reader
        .prepare(
            "SELECT fc.ancestor_id,COUNT(*) FROM folder_closure fc
             JOIN audio_files af ON af.folder_id=fc.descendant_id
             JOIN audio_search ON audio_search.rowid=af.id
             WHERE af.missing=0 AND audio_search MATCH ?1 GROUP BY fc.ancestor_id",
        )
        .unwrap();
    let expected = statement
        .query_map([&main_query], |row| {
            Ok((row.get::<_, i64>(0)?, crate::unsigned(row, 1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .unwrap();
    drop(statement);
    drop(reader);
    assert_eq!(actual, expected);
    let strict = provider.search(&filename).unwrap();
    assert!(strict.cache_hit);
    assert_eq!(strict.total_count, 7);
    assert_eq!(ids(&strict), native_ids(&provider, &filename));
    assert!(
        !strict
            .results
            .iter()
            .any(|file| file.id == 7 || file.id == 9)
    );
}

#[test]
fn a_rejected_oversize_page_keeps_full_rank_maintenance_and_returns_current_metadata() {
    let (_directory, provider) = fixture();
    let request = SearchRequest {
        folder_id: Some(2),
        ..request(SearchScope::Main, 2)
    };
    assert_eq!(provider.search(&request).unwrap().total_count, 1);
    assert!(provider.search(&request).unwrap().cache_hit);
    let large = "x".repeat(crate::search_page_cache::MAX_ENTRY_ENCODED_BYTES);
    commit(&provider, |tx| {
        tx.execute(
            "UPDATE audio_files SET metadata_json=json_set(metadata_json,'$.unrelated_extension',?1)
             WHERE id=1",
            [&large],
        )
        .unwrap();
    });
    let revision = rank_key(&provider);
    let query = crate::compile_request_query(&provider.tokenizer, &request).unwrap();
    assert_eq!(
        provider
            .ranking_cache
            .lock()
            .unwrap()
            .peek(&revision, &query)
            .unwrap()
            .len(),
        10
    );
    let page = provider.search(&request).unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results[0].id, 1);
    assert_eq!(page.results[0].attributes["unrelated_extension"], large);
    let observer = provider.observe_search_revision().unwrap();
    let key = PageRequest::new(&request).unwrap();
    assert!(
        provider
            .first_page_cache
            .lock()
            .unwrap()
            .get(observer, key.bytes())
            .is_none()
    );
}
