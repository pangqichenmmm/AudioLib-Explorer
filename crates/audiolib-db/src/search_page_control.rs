// crates/audiolib-db/src/search_page_control.rs
//! Bounded first pages tied to the dedicated observer's complete database image.
//! Writer candidates stay private until the committing connection proves that
//! no other connection committed between its admitted transaction and publication.
use crate::{
    SqliteProvider, db_error,
    search_page_cache::{LookupRequest, PageRequest, PreparedCacheEntry, encoded_fits},
    search_snapshot,
};
use audiolib_core::{Error, Result, SearchPage, SearchRequest, SearchScope, SortOrder};
use rusqlite::Connection;
use serde::Serialize;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
mod cow_isolation_tests;
#[cfg(test)]
mod implicit_pin_tests;
#[cfg(test)]
mod rank_deprioritization_tests;
#[cfg(test)]
mod tests;

const PREPARATION_BUDGET: Duration = Duration::from_millis(1500);

/// Owned data only; continuation bytes are always created for the reader's
/// committed observer, never copied from a provisional writer response.
#[derive(Serialize)]
pub(super) struct PresentationPage {
    page: SearchPage,
    has_more: bool,
}

#[derive(Serialize)]
struct BorrowedPage<'a> {
    page: &'a SearchPage,
    has_more: bool,
}

pub(super) struct PageBatchBaseline {
    writer_revision: u64,
    observer_revision: u64,
    requests: Vec<Arc<PageRequest>>,
}

pub(super) struct PreparedPages {
    writer_revision: u64,
    observer_revision: u64,
    entries: Vec<PreparedCacheEntry<PresentationPage>>,
    prepared_relevance_queries: Vec<String>,
}

impl PreparedPages {
    /// An accepted first page makes duplicate full-rank maintenance optional.
    /// This is only a work hint: page IDs never stand in for a complete ranking.
    pub(super) fn has_prepared_relevance_query(&self, query: &str) -> bool {
        self.prepared_relevance_queries
            .iter()
            .any(|prepared| prepared == query)
    }
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn writer_revision(conn: &Connection) -> Result<u64> {
    conn.query_row("PRAGMA data_version", [], |row| crate::unsigned(row, 0))
        .map_err(db_error)
}

impl SqliteProvider {
    /// Validate before using any cached page. A hit is bound to a pinned image,
    /// including folders, albums and all other tables used by the projection.
    pub(super) fn search_impl(
        &self,
        request: &SearchRequest,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<SearchPage> {
        request.validate()?;
        let normalized = crate::normalize_search_page(request)?;
        let request = normalized.as_ref();
        let start = Instant::now();
        let Some(key) = LookupRequest::new(request) else {
            return self.search_impl_uncached(request, cancel);
        };
        check_cancel(cancel.as_deref())?;
        {
            let reader = self.search_page_reader(cancel.clone())?;
            let before = self.observe_search_revision()?;
            {
                // This single statement pins an implicit SQLite read snapshot.
                // Rows drop/reset before the cached statement returns, and both
                // end before the second observer read; no read transaction is
                // retained during owned DTO cloning/cursor encoding.
                let mut statement = reader
                    .prepare_cached("SELECT id FROM audio_files LIMIT 1")
                    .map_err(db_error)?;
                let mut rows = statement.query([]).map_err(db_error)?;
                if let Some(row) = rows.next().map_err(db_error)? {
                    row.get::<_, i64>(0).map_err(db_error)?;
                }
            }
            let revision = self.observe_search_revision()?;
            check_cancel(cancel.as_deref())?;
            if before == revision {
                // The cached DTO is already a complete canonical image of R.
                // Equal observations prove the implicit pin saw that same R.
                // A later commit may leave this owned old image usable, but the
                // next lookup and its continuation must observe the newer R.
                let cached = self
                    .first_page_cache
                    .lock()
                    .map_err(|_| Error::Database("First-page cache lock was poisoned".into()))?
                    .get(revision, key.bytes());
                if let Some(cached) = cached {
                    // Response cloning and cursor encoding never hold the cache lock.
                    let mut page = cached.page.clone();
                    page.next_cursor = crate::scoped_search::encode_committed_first_page_cursor(
                        self,
                        request,
                        revision,
                        page.results.last().map(|file| file.id),
                        cached.has_more,
                    )?;
                    check_cancel(cancel.as_deref())?;
                    page.cache_hit = true;
                    page.execution_time_ms = start.elapsed().as_secs_f64() * 1000.0;
                    return Ok(page);
                }
            }
        }
        // This fence spans the actual uncached reader, not just the lookup above.
        // A concurrent commit leaves the owned result usable, but not cacheable.
        let before = self.observe_search_revision()?;
        let mut page = self.search_impl_uncached(request, cancel.clone())?;
        let after = self.observe_search_revision()?;
        check_cancel(cancel.as_deref())?;
        if before == after {
            let has_more = page.next_cursor.is_some();
            let borrowed = BorrowedPage {
                page: &page,
                has_more,
            };
            if encoded_fits(&borrowed, key.payload_budget())
                && let Some(key) = key.into_owned()
            {
                let mut owned = page.clone();
                owned.next_cursor = None;
                if let Some(entry) = PreparedCacheEntry::new(
                    key,
                    Arc::new(PresentationPage {
                        page: owned,
                        has_more,
                    }),
                ) {
                    let retired = {
                        let mut cache = self.first_page_cache.lock().map_err(|_| {
                            Error::Database("First-page cache lock was poisoned".into())
                        })?;
                        cache.insert(after, entry)
                    };
                    drop(retired);
                }
            }
        }
        page.execution_time_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok(page)
    }

    /// Pure local retirement under the caller's administration/read gate. The
    /// dedicated observer remains open, so its opaque identities never restart.
    pub(super) fn clear_first_pages(&self) -> Result<()> {
        let retired = {
            let mut cache = self
                .first_page_cache
                .lock()
                .map_err(|_| Error::Database("First-page cache lock was poisoned".into()))?;
            cache.clear()
        };
        drop(retired);
        Ok(())
    }

    /// Called inside the admitted writer transaction, before business mutation.
    /// Only hot request names are reused; no old Metadata is relabelled.
    pub(super) fn begin_first_pages(&self, conn: &Connection) -> Result<Option<PageBatchBaseline>> {
        let requests = self
            .first_page_cache
            .lock()
            .map_err(|_| Error::Database("First-page cache lock was poisoned".into()))?
            .maintenance_requests();
        if requests.is_empty() {
            return Ok(None);
        }
        Ok(Some(PageBatchBaseline {
            writer_revision: writer_revision(conn)?,
            observer_revision: self.observe_search_revision()?,
            requests,
        }))
    }

    /// Run pure canonical SQL in the actual writer snapshot. The time budget is
    /// checked between requests; it is not a hard SQL interrupt or transaction
    /// deadline. Large/unsupported pages retain the ordinary reader fallback.
    pub(super) fn prepare_first_pages(
        &self,
        conn: &Connection,
        baseline: Option<PageBatchBaseline>,
        cancel: &AtomicBool,
    ) -> Result<Option<PreparedPages>> {
        let Some(baseline) = baseline else {
            return Ok(None);
        };
        let start = Instant::now();
        let mut entries = Vec::new();
        let mut prepared_relevance_queries = Vec::new();
        for key in baseline.requests {
            check_cancel(Some(cancel))?;
            if start.elapsed() >= PREPARATION_BUDGET {
                break;
            }
            match search_snapshot::first_page(self, conn, key.request(), cancel) {
                Ok(Some(snapshot)) => {
                    let request = key.request();
                    let relevance_query = if request.order_by == SortOrder::Relevance
                        && matches!(request.scope, SearchScope::Main | SearchScope::Filename)
                        && !request.query.trim().is_empty()
                    {
                        crate::compile_request_query(&self.tokenizer, request).ok()
                    } else {
                        None
                    };
                    if let Some(entry) = PreparedCacheEntry::new(
                        key,
                        Arc::new(PresentationPage {
                            page: snapshot.page,
                            has_more: snapshot.has_more,
                        }),
                    ) {
                        entries.push(entry);
                        if let Some(query) = relevance_query
                            && !prepared_relevance_queries.contains(&query)
                        {
                            prepared_relevance_queries.push(query);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if conn.is_autocommit() {
                        return Err(Error::Database(format!(
                            "First-page preparation lost the business transaction: {error}"
                        )));
                    }
                    check_cancel(Some(cancel))?;
                    // Optional derived work never changes a valid business batch.
                    tracing::debug!(%error, "Optional first-page preparation declined");
                }
            }
        }
        check_cancel(Some(cancel))?;
        Ok(Some(PreparedPages {
            writer_revision: baseline.writer_revision,
            observer_revision: baseline.observer_revision,
            entries,
            prepared_relevance_queries,
        }))
    }

    /// Call after COMMIT on the SAME writer connection. Own commits do not
    /// change its data_version; any external commit changes it. Read the global
    /// observer first, then this fence. A later external commit receives a newer
    /// cache key and cannot consume the old candidate.
    pub(super) fn publish_first_pages(
        &self,
        conn: &Connection,
        prepared: Option<PreparedPages>,
    ) -> Result<()> {
        let Some(prepared) = prepared else {
            return Ok(());
        };
        if prepared.entries.is_empty() {
            return Ok(());
        }
        if !conn.is_autocommit() {
            return Err(Error::Database(
                "First pages may only publish after COMMIT".into(),
            ));
        }
        let observer = self.observe_search_revision()?;
        if observer == prepared.observer_revision
            || writer_revision(conn)? != prepared.writer_revision
        {
            return Ok(());
        }
        let retired = {
            let mut cache = self
                .first_page_cache
                .lock()
                .map_err(|_| Error::Database("First-page cache lock was poisoned".into()))?;
            let mut retired = cache.register_revision(observer);
            for entry in prepared.entries {
                retired.extend(cache.insert(observer, entry));
            }
            retired
        };
        drop(retired);
        Ok(())
    }
}
