// crates/audiolib-db/src/search_snapshot.rs
//! Bounded first-page SQL on the caller's snapshot, without shared cache admission.
use crate::{
    SqliteProvider, audio_from_row, audio_projection, db_error, scoped_search, search_sql,
};
use audiolib_core::{Error, Result, SearchPage, SearchRequest, SearchScope, SortOrder};
use rusqlite::{Connection, params_from_iter, types::Value};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

/// Eligibility is independent of query frequency or benchmark-specific text.
/// Callers normalize page aliases first; page 1 itself is equivalent to offset 0.
pub(super) fn eligible(request: &SearchRequest) -> bool {
    request.cursor.is_none()
        && request.offset == 0
        && (1..=200).contains(&request.limit)
        && request.page.is_none_or(|page| page == 1)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// An unpublished response has no sendable cursor. Its owner binds continuation
/// identity only after proving the successful commit and its current observer.
pub(super) struct SnapshotPage {
    pub(super) page: SearchPage,
    pub(super) has_more: bool,
}

/// Build an owned response candidate without opening a provider reader, ranking
/// admission, directory-cache publication, or interrupting the business transaction.
/// Cancellation is checked between statements/rows; it never installs a SQLite
/// interrupt/progress hook which could abort the writer's durable batch.
/// Scoped continuation is represented solely by `has_more`; the owned page's
/// `next_cursor` is always None. The response owner encodes a fresh continuation
/// with the accepted committed observer before returning a cache hit.
pub(super) fn first_page(
    provider: &SqliteProvider,
    tx: &Connection,
    request: &SearchRequest,
    cancel: &AtomicBool,
) -> Result<Option<SnapshotPage>> {
    check_cancel(cancel)?;
    request.validate()?;
    if !eligible(request) {
        return Ok(None);
    }
    if tx.is_autocommit() {
        return Err(Error::Database(
            "First-page snapshot requires an active transaction".into(),
        ));
    }
    // A response may authorize later lookup-only readers, so prove readiness
    // in this same writer snapshot before any candidate SQL or owned DTO exists.
    crate::schema::ensure_ready(tx)?;
    check_cancel(cancel)?;
    let start = Instant::now();
    let mut page = if matches!(request.scope, SearchScope::All | SearchScope::Path)
        && !request.query.trim().is_empty()
    {
        if crate::dir_search::ready(tx)? {
            scoped_search::page_in_snapshot(provider, tx, request, 0, start)?
        } else if request.scope == SearchScope::Path {
            SearchPage {
                total_count_exact: true,
                query: request.query.clone(),
                execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                ..Default::default()
            }
        } else {
            let mut fallback = request.clone();
            fallback.scope = SearchScope::Main;
            main_page(provider, tx, &fallback, cancel, start)?
        }
    } else {
        let mut canonical = request.clone();
        if matches!(canonical.scope, SearchScope::All | SearchScope::Path) {
            canonical.scope = SearchScope::Main;
        }
        main_page(provider, tx, &canonical, cancel, start)?
    };
    check_cancel(cancel)?;
    let has_more = page.next_cursor.take().is_some();
    Ok(Some(SnapshotPage { page, has_more }))
}

fn main_page(
    provider: &SqliteProvider,
    tx: &Connection,
    request: &SearchRequest,
    cancel: &AtomicBool,
    start: Instant,
) -> Result<SearchPage> {
    let sql = search_sql(
        tx,
        &provider.tokenizer,
        request,
        true,
        provider.derived_tags.load(Ordering::Acquire),
    )?;
    let conditions = sql.conditions.join(" AND ");
    check_cancel(cancel)?;
    let total_count = tx
        .query_row(
            &format!("SELECT COUNT(*) FROM {} WHERE {conditions}", sql.from),
            params_from_iter(&sql.values),
            |row| crate::unsigned(row, 0),
        )
        .map_err(db_error)?;
    check_cancel(cancel)?;
    if total_count == 0 {
        return Ok(SearchPage {
            total_count_exact: true,
            query: request.query.clone(),
            execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
            ..Default::default()
        });
    }
    // Exactly the public canonical Main/Filename SQL order, including raw NULL
    // ordering and ascending ID ties. Filename search_sql compiles its strict
    // FTS column restriction; it must not be replaced by the unscoped compiler.
    let order = match request.order_by {
        SortOrder::Relevance if !request.query.trim().is_empty() => "bm25(audio_search)",
        SortOrder::Relevance | SortOrder::Filename => "af.filename COLLATE NOCASE",
        SortOrder::Duration => "af.duration",
        SortOrder::SampleRate => "af.sample_rate",
        SortOrder::Modified => "af.modified_at",
        SortOrder::Channels => "af.channels",
        SortOrder::Tags if provider.derived_tags.load(Ordering::Acquire) => {
            "COALESCE(audiolib_user_tags_key(af.user_tags),af.source_tags_text) COLLATE NOCASE"
        }
        SortOrder::Tags => {
            "COALESCE(audiolib_user_tags_key(af.user_tags),audiolib_source_tags_key(af.metadata_json)) COLLATE NOCASE"
        }
    };
    let direction = if request.descending && request.order_by != SortOrder::Relevance {
        "DESC"
    } else {
        "ASC"
    };
    let projection = audio_projection(tx)?;
    let mut values = sql.values;
    values.push(Value::Integer(request.limit as i64));
    let mut statement = tx
        .prepare(&format!(
            "WITH selected AS MATERIALIZED (
               SELECT af.id,{order} AS sort_value FROM {} WHERE {conditions}
               ORDER BY sort_value {direction},af.id ASC LIMIT ?
             ) SELECT {projection} FROM selected JOIN audio_files af ON af.id=selected.id
             ORDER BY selected.sort_value {direction},af.id ASC",
            sql.from
        ))
        .map_err(db_error)?;
    let mut rows = statement
        .query(params_from_iter(&values))
        .map_err(db_error)?;
    let mut results = Vec::with_capacity(request.limit);
    while let Some(row) = rows.next().map_err(db_error)? {
        check_cancel(cancel)?;
        results.push(audio_from_row(row).map_err(db_error)?);
    }
    check_cancel(cancel)?;
    Ok(SearchPage {
        total_count_exact: true,
        returned_count: results.len(),
        results,
        total_count,
        query: request.query.clone(),
        execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
        cache_hit: false,
        next_cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::DataProvider;
    use rusqlite::{TransactionBehavior, params};

    fn fixture(indexed: bool) -> (tempfile::TempDir, SqliteProvider) {
        let directory = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&directory.path().join("snapshot.db")).unwrap();
        let writer = provider.writer().unwrap();
        writer
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
             (1,'/library-root','library-root',NULL),
             (2,'/library-root/door','door',1),(3,'/library-root/forest','forest',1);
             INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);",
            )
            .unwrap();
        for id in 1..=12 {
            let (filename, notes) = match id {
                1 => ("door.wav".to_owned(), "old-note"),
                2 => ("quiet.wav".to_owned(), "door"),
                _ => (format!("neutral-{id}.wav"), ""),
            };
            let folder = if id <= 3 { 2 } else { 3 };
            writer
                .execute(
                    "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,
                 sample_rate,bit_depth,channels,duration,metadata_json)
                 VALUES(?1,?2,?2,?3,?4,?5,48000,16,1,?6,'{}')",
                    params![
                        id,
                        format!("/library-root/{folder}/{filename}"),
                        filename,
                        folder,
                        notes,
                        id as f64
                    ],
                )
                .unwrap();
        }
        drop(writer);
        if indexed {
            provider.rebuild_directory_search().unwrap();
        }
        (directory, provider)
    }

    fn request(scope: SearchScope, query: &str) -> SearchRequest {
        SearchRequest {
            scope,
            query: query.into(),
            ..Default::default()
        }
    }

    fn ids(page: &SearchPage) -> Vec<i64> {
        page.results.iter().map(|row| row.id).collect()
    }

    #[test]
    fn writer_page_reads_uncommitted_metadata_without_admitting_shared_search_state() {
        let (_directory, provider) = fixture(true);
        provider
            .search(&request(SearchScope::Main, "door"))
            .unwrap();
        let rankings = provider.ranking_cache.lock().unwrap().maintenance_queries();
        let length_keys = provider
            .document_lengths
            .lock()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let observation = provider.search_cache_observation.lock().unwrap().clone();
        let mut writer = provider.writer().unwrap();
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE audio_files SET user_notes='freshmarker',metadata_json=json_set(metadata_json,'$.independent','current') WHERE id=1", []).unwrap();
        let cancel = AtomicBool::new(false);
        let candidate = first_page(
            &provider,
            &tx,
            &request(SearchScope::Main, "freshmarker"),
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert_eq!(ids(&candidate.page), vec![1]);
        assert_eq!(candidate.page.results[0].notes, "freshmarker");
        assert_eq!(
            candidate.page.results[0].attributes["independent"],
            "current"
        );
        assert!(candidate.page.next_cursor.is_none());
        assert!(!candidate.has_more);
        let filename = first_page(
            &provider,
            &tx,
            &request(SearchScope::Filename, "freshmarker"),
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert!(filename.page.results.is_empty());
        assert_eq!(filename.page.total_count, 0);
        assert_eq!(
            provider.ranking_cache.lock().unwrap().maintenance_queries(),
            rankings
        );
        assert_eq!(
            provider
                .document_lengths
                .lock()
                .unwrap()
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>(),
            length_keys
        );
        assert_eq!(
            *provider.search_cache_observation.lock().unwrap(),
            observation
        );
        tx.rollback().unwrap();
        drop(writer);
        assert!(
            provider
                .search(&request(SearchScope::Main, "freshmarker"))
                .unwrap()
                .results
                .is_empty()
        );
        assert_eq!(
            candidate.page.results[0].notes, "freshmarker",
            "candidate is independently owned after abort"
        );
    }

    #[test]
    fn aborted_scoped_candidate_cannot_poison_a_later_same_generation_directory_index() {
        let (_directory, provider) = fixture(true);
        let selection = request(SearchScope::Path, "door");
        assert_eq!(ids(&provider.search(&selection).unwrap()), vec![1, 2, 3]);
        let cancel = AtomicBool::new(false);
        let mut writer = provider.writer().unwrap();
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE audio_files SET folder_id=3 WHERE id=3", [])
            .unwrap();
        crate::dir_search::maintain(&tx, &[], &[2, 3]).unwrap();
        let aborted = first_page(&provider, &tx, &selection, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(ids(&aborted.page), vec![1, 2]);
        assert!(aborted.page.next_cursor.is_none());
        tx.rollback().unwrap();
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE audio_files SET folder_id=3 WHERE id=2", [])
            .unwrap();
        crate::dir_search::maintain(&tx, &[], &[2, 3]).unwrap();
        tx.commit().unwrap();
        drop(writer);
        assert_eq!(ids(&provider.search(&selection).unwrap()), vec![1, 3]);
        assert_eq!(ids(&aborted.page), vec![1, 2]);
    }

    #[test]
    fn native_and_writer_first_pages_rebind_to_current_scoped_cursor_without_order_changes() {
        let (_directory, provider) = fixture(true);
        let cancel = AtomicBool::new(false);
        for scope in [SearchScope::All, SearchScope::Path] {
            let complete = provider.search(&request(scope, "door")).unwrap();
            let mut selection = request(scope, "door");
            selection.limit = 1;
            // Exercise the native producer itself; an outer response-cache hit
            // is allowed to have already rebound its continuation to v2.
            let native = provider.search_scoped(&selection, None).unwrap();
            assert!(native.next_cursor.as_deref().unwrap().starts_with("v3."));
            let revision = provider.observe_search_revision().unwrap();
            selection.cursor = scoped_search::encode_committed_first_page_cursor(
                &provider,
                &selection,
                revision,
                native.results.last().map(|row| row.id),
                true,
            )
            .unwrap();
            let mut visited = ids(&native);
            while selection.cursor.is_some() {
                selection.offset = 999;
                let page = provider.search(&selection).unwrap();
                visited.extend(ids(&page));
                selection.cursor = page.next_cursor;
            }
            assert_eq!(visited, ids(&complete), "native -> v2: {scope:?}");

            let mut selection = request(scope, "door");
            selection.limit = 1;
            let mut writer = provider.writer().unwrap();
            let tx = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute(
                "UPDATE audio_files SET user_notes=user_notes||' extra-current-text' WHERE id=1",
                [],
            )
            .unwrap();
            let prepared = first_page(&provider, &tx, &selection, &cancel)
                .unwrap()
                .unwrap();
            assert!(prepared.page.next_cursor.is_none());
            assert!(prepared.has_more);
            tx.commit().unwrap();
            drop(writer);
            let revision = provider.observe_search_revision().unwrap();
            selection.cursor = scoped_search::encode_committed_first_page_cursor(
                &provider,
                &selection,
                revision,
                prepared.page.results.last().map(|row| row.id),
                prepared.has_more,
            )
            .unwrap();
            let mut visited = ids(&prepared.page);
            while selection.cursor.is_some() {
                let page = provider.search(&selection).unwrap();
                assert!(page.total_count_exact);
                assert_eq!(page.total_count, prepared.page.total_count);
                visited.extend(ids(&page));
                selection.cursor = page.next_cursor;
            }
            assert_eq!(
                visited,
                ids(&provider.search(&request(scope, "door")).unwrap()),
                "writer -> v2: {scope:?}"
            );
        }
    }

    #[test]
    fn unsupported_first_pages_fallbacks_and_cancellation_leave_the_writer_usable() {
        let (_directory, provider) = fixture(false);
        let mut writer = provider.writer().unwrap();
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let cancel = AtomicBool::new(false);
        let path = first_page(&provider, &tx, &request(SearchScope::Path, "door"), &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(path.page.total_count, 0);
        let all = first_page(&provider, &tx, &request(SearchScope::All, "door"), &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(all.page.total_count, 2);
        assert!(all.page.next_cursor.is_none());
        assert!(!all.has_more);
        let browse = first_page(&provider, &tx, &request(SearchScope::Path, ""), &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(browse.page.total_count, 12);
        for changed in [
            SearchRequest {
                offset: 1,
                ..request(SearchScope::Main, "door")
            },
            SearchRequest {
                limit: 201,
                ..request(SearchScope::Main, "door")
            },
            SearchRequest {
                cursor: Some("must-bypass".into()),
                ..request(SearchScope::All, "door")
            },
        ] {
            assert!(
                first_page(&provider, &tx, &changed, &cancel)
                    .unwrap()
                    .is_none()
            );
        }
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            first_page(&provider, &tx, &request(SearchScope::Main, "door"), &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(
            tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        tx.execute(
            "UPDATE audio_files SET user_notes='durable-after-cancel' WHERE id=1",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn pending_migration_commit_cannot_consume_a_previously_ready_cached_page() {
        for marker in ["audiolib_search_upgrade", "audiolib_storage_migration"] {
            let (_directory, provider) = fixture(true);
            let selection = request(SearchScope::Main, "door");
            provider.search(&selection).unwrap();
            assert!(provider.search(&selection).unwrap().cache_hit);
            let external = Connection::open(provider.database_path()).unwrap();
            external
                .execute_batch(&format!("CREATE TABLE {marker}(singleton INTEGER)"))
                .unwrap();
            assert!(
                matches!(provider.search(&selection), Err(Error::Busy(_))),
                "{marker}"
            );
            external
                .execute_batch(&format!("DROP TABLE {marker}"))
                .unwrap();
            assert_eq!(provider.search(&selection).unwrap().total_count, 2);
            assert!(provider.search(&selection).unwrap().cache_hit);
        }
    }

    #[test]
    fn exclusive_upgrade_still_blocks_cached_lookup_and_cancel_is_checked_first() {
        let (_directory, provider) = fixture(true);
        let selection = request(SearchScope::Main, "door");
        provider.search(&selection).unwrap();
        assert!(provider.search(&selection).unwrap().cache_hit);
        let exclusive =
            crate::management::SearchUpgradeLease::acquire(provider.database_path(), true).unwrap();
        assert!(matches!(provider.search(&selection), Err(Error::Busy(_))));
        let cancelled = std::sync::Arc::new(AtomicBool::new(true));
        assert!(matches!(
            provider.search_cancellable(&selection, cancelled),
            Err(Error::Cancelled)
        ));
        drop(exclusive);
        assert!(provider.search(&selection).unwrap().cache_hit);
    }

    #[test]
    fn unready_writer_snapshot_cannot_produce_a_ready_response_candidate() {
        for marker in ["audiolib_search_upgrade", "audiolib_storage_migration"] {
            let (_directory, provider) = fixture(true);
            let mut writer = provider.writer().unwrap();
            let tx = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute_batch(&format!("CREATE TABLE {marker}(singleton INTEGER)"))
                .unwrap();
            let candidate = first_page(
                &provider,
                &tx,
                &request(SearchScope::Main, "door"),
                &AtomicBool::new(false),
            );
            assert!(matches!(candidate, Err(Error::Busy(_))), "{marker}");
            assert_eq!(
                tx.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            tx.rollback().unwrap();
        }
    }

    #[test]
    fn cached_lookup_still_requires_an_authorized_main_snapshot_pin() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let (_directory, provider) = fixture(true);
        let selection = request(SearchScope::Main, "door");
        provider.search(&selection).unwrap();
        assert!(provider.search(&selection).unwrap().cache_hit);
        let reader = provider.reader().unwrap();
        reader.authorizer(Some(|context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read {table_name,..} if table_name=="audio_files") {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        })).unwrap();
        drop(reader);
        assert!(
            provider.search(&selection).is_err(),
            "a ready cache does not bypass the pinned reader's authorizer"
        );
        let reader = provider.reader().unwrap();
        reader
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        drop(reader);
        assert!(provider.search(&selection).unwrap().cache_hit);
    }
}
