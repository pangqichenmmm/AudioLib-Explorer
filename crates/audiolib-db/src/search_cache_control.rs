// crates/audiolib-db/src/search_cache_control.rs
//! Provider-owned snapshot cache admission and unpublished writer candidates.
//! AF journaling covers supported AudioLib writers, including older writers.
//! Managed FTS-only rebuilds/restore retire the journal through their reset path;
//! arbitrary direct FTS DML bypassing both contracts is not a tracked writer.
use crate::{
    SqliteProvider, db_error, search_bm25, search_cache::CacheRevision, search_rank_parallel,
    search_tracking,
};
use audiolib_core::{Error, Result};
use rusqlite::{Connection, Transaction, params};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const LENGTH_GENERATIONS: usize = 2;
const MAINTENANCE_IDS: usize = 262_144;
const NONEMPTY_QUERIES: usize = 2;

/// Latest accepted live epoch/observer pair. Quarantine prevents a same-epoch
/// untracked change from reusing previously trusted identities or lengths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Observation {
    epoch: Option<search_tracking::Epoch>,
    observer: u64,
    quarantined: bool,
}

pub(super) struct BatchBaseline {
    epoch: Option<search_tracking::Epoch>,
    observer: u64,
    lengths: Vec<(CacheRevision, Arc<search_bm25::Lengths>)>,
    queries: Vec<(String, usize)>,
    first_pages: Option<crate::search_page_control::PageBatchBaseline>,
}

/// Entirely owned candidates, usable only after the exact writer epoch commits.
pub(super) struct PreparedBatch {
    epoch: search_tracking::Epoch,
    lengths: Option<Arc<search_bm25::Lengths>>,
    rankings: Vec<(String, Arc<Vec<i64>>)>,
    first_pages: Option<crate::search_page_control::PreparedPages>,
}

/// Only a successful COMMIT can construct this publication capability.
pub(super) struct CommittedBatch {
    prepared: PreparedBatch,
}

fn cancelled(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn newer(candidate: &CacheRevision, current: &CacheRevision) -> bool {
    match (candidate, current) {
        (CacheRevision::Tracked(next), CacheRevision::Tracked(previous)) => {
            next.cookie == previous.cookie && next.sequence > previous.sequence
        }
        (CacheRevision::Untracked(next), CacheRevision::Untracked(previous)) => next > previous,
        _ => false,
    }
}

fn retain_lengths(
    cache: &mut VecDeque<(CacheRevision, Arc<search_bm25::Lengths>)>,
    revision: CacheRevision,
    lengths: Arc<search_bm25::Lengths>,
) {
    if let Some((current, _)) = cache.back() {
        if current == &revision {
            return;
        }
        if !newer(&revision, current) {
            return;
        }
    }
    cache.push_back((revision, lengths));
    while cache.len() > LENGTH_GENERATIONS {
        cache.pop_front();
    }
}

impl SqliteProvider {
    /// Reset under the caller's administration/read gate. No SQL or observer
    /// replacement: its existing PRAGMA identity must also invalidate cursors.
    pub(super) fn reset_search_caches(&self) -> Result<()> {
        self.clear_first_pages()?;
        let mut observation = self
            .search_cache_observation
            .lock()
            .map_err(|_| Error::Database("Search cache observation lock was poisoned".into()))?;
        self.clear_search_cache_payloads(None)?;
        *observation = None;
        Ok(())
    }

    fn clear_search_cache_payloads(&self, revision: Option<&CacheRevision>) -> Result<()> {
        {
            let mut cache = self
                .ranking_cache
                .lock()
                .map_err(|_| Error::Database("Ranking cache lock was poisoned".into()))?;
            cache.clear();
            if let Some(revision) = revision {
                cache.register_revision(revision.clone());
            }
        }
        self.document_lengths
            .lock()
            .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?
            .clear();
        Ok(())
    }

    fn register_cache_revision(&self, revision: &CacheRevision) -> Result<()> {
        self.ranking_cache
            .lock()
            .map_err(|_| Error::Database("Ranking cache lock was poisoned".into()))?
            .register_revision(revision.clone());
        Ok(())
    }

    /// Probe the existing dedicated observer only when a live-current proof is
    /// needed. Its version is an opaque identity, never a count of commits.
    fn live_search_cache_state(&self) -> Result<Option<(Option<search_tracking::Epoch>, u64)>> {
        self.observe_search_revision()?;
        let observer = self
            .revision_reader
            .lock()
            .map_err(|_| Error::Database("Search revision observer lock was poisoned".into()))?;
        let conn = observer
            .as_ref()
            .ok_or_else(|| Error::Database("Search revision observer is unavailable".into()))?;
        let before = conn
            .query_row("PRAGMA data_version", [], |row| crate::unsigned(row, 0))
            .map_err(db_error)?;
        let epoch = search_tracking::epoch(conn)?;
        let after = conn
            .query_row("PRAGMA data_version", [], |row| crate::unsigned(row, 0))
            .map_err(db_error)?;
        Ok((before == after).then_some((epoch, after)))
    }

    /// Bind reusable state to the caller's pinned database image. Older readers
    /// can use their own retained generation, but never retire a newer one.
    pub(super) fn cache_revision(
        &self,
        tx: &Connection,
        fallback_observer: u64,
    ) -> Result<CacheRevision> {
        let pinned = search_tracking::epoch(tx)?;
        for _ in 0..3 {
            let previous = self
                .search_cache_observation
                .lock()
                .map_err(|_| Error::Database("Search cache observation lock was poisoned".into()))?
                .clone();
            let needs_live = match (&pinned, &previous) {
                (None, Some(old)) => old.epoch.is_some(),
                (Some(epoch), Some(old)) => match &old.epoch {
                    Some(known) if known.cookie == epoch.cookie => {
                        epoch.sequence == known.sequence
                            && !old.quarantined
                            && fallback_observer != old.observer
                    }
                    _ => true,
                },
                _ => false,
            };
            let live = if needs_live {
                self.live_search_cache_state()?
            } else {
                None
            };
            let mut observation = self.search_cache_observation.lock().map_err(|_| {
                Error::Database("Search cache observation lock was poisoned".into())
            })?;
            if *observation != previous {
                continue;
            }
            let Some(epoch) = &pinned else {
                let key = CacheRevision::Untracked(fallback_observer);
                if let Some(old) = &previous {
                    if old.epoch.is_some() {
                        if !matches!(&live, Some((None, value)) if *value == fallback_observer) {
                            return Ok(key);
                        }
                        self.clear_search_cache_payloads(Some(&key))?;
                    } else if fallback_observer < old.observer {
                        return Ok(key);
                    }
                }
                self.register_cache_revision(&key)?;
                *observation = Some(Observation {
                    epoch: None,
                    observer: fallback_observer,
                    quarantined: false,
                });
                return Ok(key);
            };
            let key = CacheRevision::Tracked(epoch.clone());
            if let Some(old) = &previous {
                match &old.epoch {
                    Some(known) if known.cookie == epoch.cookie => {
                        if epoch.sequence < known.sequence {
                            return Ok(key);
                        }
                        if epoch.sequence == known.sequence {
                            if old.quarantined {
                                return Ok(CacheRevision::Untracked(fallback_observer));
                            }
                            if fallback_observer == old.observer {
                                return Ok(key);
                            }
                            if !matches!(&live, Some((Some(current), value)) if current == epoch && *value == fallback_observer)
                            {
                                // A concurrent newer committed image makes this
                                // pinned image old/ambiguous, not an invalidator.
                                return Ok(CacheRevision::Untracked(fallback_observer));
                            }
                            let fallback = CacheRevision::Untracked(fallback_observer);
                            self.clear_search_cache_payloads(Some(&fallback))?;
                            *observation = Some(Observation {
                                epoch: Some(epoch.clone()),
                                observer: fallback_observer,
                                quarantined: true,
                            });
                            return Ok(fallback);
                        }
                        if old.quarantined {
                            self.clear_search_cache_payloads(Some(&key))?;
                        }
                    }
                    _ => {
                        if !matches!(&live, Some((Some(current), value)) if current == epoch && *value == fallback_observer)
                        {
                            return Ok(CacheRevision::Untracked(fallback_observer));
                        }
                        self.clear_search_cache_payloads(Some(&key))?;
                    }
                }
            } else {
                self.clear_search_cache_payloads(Some(&key))?;
            }
            self.register_cache_revision(&key)?;
            *observation = Some(Observation {
                epoch: Some(epoch.clone()),
                observer: fallback_observer,
                quarantined: false,
            });
            return Ok(key);
        }
        Ok(CacheRevision::Untracked(fallback_observer))
    }

    /// Probe only; no SQL length scan and no mutex retained after returning Arc.
    pub(super) fn cached_document_lengths(
        &self,
        tx: &Connection,
        revision: u64,
    ) -> Result<Option<Arc<search_bm25::Lengths>>> {
        let key = self.cache_revision(tx, revision)?;
        let cache = self
            .document_lengths
            .lock()
            .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?;
        Ok(cache
            .iter()
            .find(|(cached, _)| cached == &key)
            .map(|(_, lengths)| lengths.clone()))
    }

    fn length_candidate(
        &self,
        tx: &Connection,
        key: &CacheRevision,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<Arc<search_bm25::Lengths>>> {
        let cached = {
            let cache = self
                .document_lengths
                .lock()
                .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?;
            if let Some((_, lengths)) = cache.iter().find(|(revision, _)| revision == key) {
                return Ok(Some(lengths.clone()));
            }
            cache
                .iter()
                .rev()
                .find_map(|(revision, lengths)| match (revision, key) {
                    (CacheRevision::Tracked(from), CacheRevision::Tracked(to))
                        if from.cookie == to.cookie && from.sequence <= to.sequence =>
                    {
                        Some((from.clone(), lengths.clone()))
                    }
                    _ => None,
                })
        };
        if let (Some((from, lengths)), CacheRevision::Tracked(to)) = (cached, key)
            && let Some(changed) = search_tracking::delta(tx, &from, to)?
            && let Some(candidate) =
                search_bm25::apply_length_delta(tx, &lengths, &changed, cancel)?
        {
            return Ok(Some(candidate));
        }
        search_bm25::load_lengths(tx, cancel)
    }

    fn admit_document_lengths(
        &self,
        key: CacheRevision,
        lengths: Arc<search_bm25::Lengths>,
    ) -> Result<()> {
        let observation = self
            .search_cache_observation
            .lock()
            .map_err(|_| Error::Database("Search cache observation lock was poisoned".into()))?;
        let current = observation.as_ref().is_some_and(|known| match &key {
            CacheRevision::Tracked(epoch) => {
                !known.quarantined && known.epoch.as_ref() == Some(epoch)
            }
            CacheRevision::Untracked(value) => {
                (known.quarantined || known.epoch.is_none()) && *value == known.observer
            }
        });
        if current {
            let mut cache = self
                .document_lengths
                .lock()
                .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?;
            retain_lengths(&mut cache, key, lengths);
        }
        Ok(())
    }

    pub(super) fn rank_document_lengths(
        &self,
        tx: &Connection,
        stable_revision: Option<u64>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<Arc<search_bm25::Lengths>>> {
        cancelled(cancel)?;
        let fallback = match stable_revision {
            Some(value) => value,
            None => self.observe_search_revision()?,
        };
        let key = self.cache_revision(tx, fallback)?;
        if stable_revision.is_none() && matches!(key, CacheRevision::Untracked(_)) {
            return search_bm25::load_lengths(tx, cancel);
        }
        let lengths = self.length_candidate(tx, &key, cancel)?;
        if let Some(lengths) = &lengths {
            // No cache mutex spans SQL. The observation guard only admits the
            // current generation, so a late old load cannot evict a newer Arc.
            if matches!(key, CacheRevision::Tracked(_))
                || self.observe_search_revision()? == fallback
            {
                self.admit_document_lengths(key, lengths.clone())?;
            }
        }
        Ok(lengths)
    }

    /// Called before AF changes inside the writer's transaction. Capture only
    /// query text/count hints before optional journal installation changes lineage.
    pub(super) fn begin_search_cache_batch(&self, tx: &Connection) -> Result<BatchBaseline> {
        let first_pages = self.begin_first_pages(tx)?;
        let queries = self
            .ranking_cache
            .lock()
            .map_err(|_| Error::Database("Ranking cache lock was poisoned".into()))?
            .maintenance_queries();
        let lengths = self
            .document_lengths
            .lock()
            .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?
            .iter()
            .cloned()
            .collect();
        let mut epoch = search_tracking::epoch(tx)?;
        if epoch.is_none() {
            search_tracking::install(tx)?;
            epoch = search_tracking::epoch(tx)?;
        }
        Ok(BatchBaseline {
            epoch,
            observer: self.observe_search_revision()?,
            lengths,
            queries,
            first_pages,
        })
    }

    /// Serial full-MATCH scoring uses current transaction DF/N/avgdl. Everything
    /// stays unpublished until the caller reports a successful SQL commit.
    pub(super) fn prepare_search_cache_batch(
        &self,
        tx: &Connection,
        baseline: BatchBaseline,
        cancel: &AtomicBool,
    ) -> Result<Option<PreparedBatch>> {
        cancelled(Some(cancel))?;
        let (Some(from), Some(epoch)) = (&baseline.epoch, search_tracking::epoch(tx)?) else {
            return Ok(None);
        };
        if from.cookie != epoch.cookie {
            return Ok(None);
        }
        let Some(changed) = search_tracking::delta(tx, from, &epoch)? else {
            return Ok(None);
        };
        // Prefer complete owned first pages. Their accepted exact compiled keys
        // may suppress duplicate optional scoring, never supply partial rank IDs.
        let first_pages = self.prepare_first_pages(tx, baseline.first_pages, cancel)?;
        if changed.is_empty() && from == &epoch {
            return Ok(Some(PreparedBatch {
                epoch,
                lengths: None,
                rankings: Vec::new(),
                first_pages,
            }));
        }
        let mut selected = Vec::new();
        let mut nonempty = 0usize;
        let mut old_ids = 0usize;
        let mut current_ids = 0usize;
        for (query, old_count) in baseline.queries {
            cancelled(Some(cancel))?;
            if query.trim().is_empty()
                || old_count
                    .saturating_add(old_ids)
                    .saturating_add(changed.len())
                    > MAINTENANCE_IDS
            {
                continue;
            }
            let remaining = MAINTENANCE_IDS - current_ids;
            let count: i64 = tx.prepare_cached("SELECT count(*) FROM (SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 LIMIT ?2)").map_err(db_error)?.query_row(params![query, (remaining + 1) as i64], |row| row.get(0)).map_err(db_error)?;
            let count = usize::try_from(count)
                .map_err(|_| Error::Database("Invalid search maintenance row count".into()))?;
            if count > remaining || (count > 0 && nonempty == NONEMPTY_QUERIES) {
                continue;
            }
            nonempty += usize::from(count > 0);
            old_ids += old_count;
            current_ids += count;
            selected.push((query, count));
        }
        let base = baseline
            .lengths
            .iter()
            .rev()
            .find(|(revision, _)| match revision {
                CacheRevision::Tracked(old) => {
                    old.cookie == epoch.cookie && old.sequence <= epoch.sequence
                }
                CacheRevision::Untracked(value) => *value == baseline.observer,
            });
        let needs_lengths = base.is_some() || selected.iter().any(|(_, count)| *count > 0);
        let lengths = if !needs_lengths {
            None
        } else if let Some((revision, lengths)) = base {
            let delta = match revision {
                CacheRevision::Tracked(previous) => search_tracking::delta(tx, previous, &epoch)?,
                CacheRevision::Untracked(_) => Some(changed.clone()),
            };
            match delta {
                Some(delta) => {
                    match search_bm25::apply_length_delta(tx, lengths, &delta, Some(cancel))? {
                        Some(value) => Some(value),
                        None => search_bm25::load_lengths(tx, Some(cancel))?,
                    }
                }
                None => search_bm25::load_lengths(tx, Some(cancel))?,
            }
        } else {
            search_bm25::load_lengths(tx, Some(cancel))?
        };
        let mut rankings = Vec::new();
        for (query, count) in selected {
            cancelled(Some(cancel))?;
            // Selection/count/length budgets above stay unchanged. Do not fill
            // skipped slots with new queries and erase the saved writer work.
            if first_pages
                .as_ref()
                .is_some_and(|pages| pages.has_prepared_relevance_query(&query))
            {
                continue;
            }
            let ids = if count == 0 {
                Some(Arc::new(Vec::new()))
            } else {
                search_rank_parallel::collect_serial(
                    tx,
                    query.clone(),
                    lengths.clone(),
                    Some(cancel),
                )?
            };
            if let Some(ids) = ids
                && ids.len() == count
            {
                rankings.push((query, ids));
            }
        }
        cancelled(Some(cancel))?;
        Ok(Some(PreparedBatch {
            epoch,
            lengths,
            rankings,
            first_pages,
        }))
    }

    /// Consume the actual business transaction before granting publication.
    /// A rollback/error cannot produce a committed-page capability.
    /// The caller must pass the same lexical transaction that built `prepared`.
    pub(super) fn commit_search_cache_batch(
        &self,
        tx: Transaction<'_>,
        prepared: Option<PreparedBatch>,
    ) -> Result<Option<CommittedBatch>> {
        tx.commit().map_err(db_error)?;
        Ok(prepared.map(|prepared| CommittedBatch { prepared }))
    }

    /// Publish only the candidate consumed by a successful business COMMIT.
    /// The caller retains its same writer guard through the observer/Dw fence.
    pub(super) fn publish_search_cache_batch(
        &self,
        conn: &Connection,
        committed: CommittedBatch,
    ) -> Result<()> {
        let mut prepared = committed.prepared;
        self.publish_first_pages(conn, prepared.first_pages.take())?;
        self.publish_rankings(prepared)
    }

    /// Rank-only identities retain their AF epoch proof and never publish pages.
    fn publish_rankings(&self, prepared: PreparedBatch) -> Result<()> {
        let Some((Some(current), observer)) = self.live_search_cache_state()? else {
            return Ok(());
        };
        if current != prepared.epoch {
            return Ok(());
        }
        let mut observation = self
            .search_cache_observation
            .lock()
            .map_err(|_| Error::Database("Search cache observation lock was poisoned".into()))?;
        if observation.as_ref().is_some_and(|old| {
            old.epoch.as_ref().is_some_and(|known| {
                known.cookie == current.cookie && known.sequence > current.sequence
            })
        }) {
            return Ok(());
        }
        let key = CacheRevision::Tracked(current.clone());
        if observation.as_ref().is_none_or(|old| {
            old.quarantined
                || old
                    .epoch
                    .as_ref()
                    .is_none_or(|epoch| epoch.cookie != current.cookie)
        }) {
            self.clear_search_cache_payloads(Some(&key))?;
        }
        self.register_cache_revision(&key)?;
        *observation = Some(Observation {
            epoch: Some(current),
            observer,
            quarantined: false,
        });
        if let Some(lengths) = prepared.lengths {
            let mut cache = self
                .document_lengths
                .lock()
                .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?;
            retain_lengths(&mut cache, key.clone(), lengths);
        }
        self.ranking_cache
            .lock()
            .map_err(|_| Error::Database("Ranking cache lock was poisoned".into()))?
            .publish(key, prepared.rankings);
        Ok(())
    }

    /// Register only an explicitly known provider bookkeeping commit which did
    /// not modify AF or FTS. Quarantined/untracked payloads are retired by the
    /// same publisher; this never relabels their old IDs with a tracked epoch.
    pub(super) fn search_cache_bookkeeping_committed(&self, conn: &Connection) -> Result<()> {
        if !conn.is_autocommit() {
            return Err(Error::Database(
                "Search cache bookkeeping registration requires a committed transaction".into(),
            ));
        }
        let Some(epoch) = search_tracking::epoch(conn)? else {
            return Ok(());
        };
        self.publish_rankings(PreparedBatch {
            epoch,
            lengths: None,
            rankings: Vec::new(),
            first_pages: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, SqliteProvider) {
        let directory = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&directory.path().join("cache-control.db")).unwrap();
        provider
            .writer()
            .unwrap()
            .execute_batch(
                "CREATE TABLE cache_control_bookkeeping(value INTEGER);
                 INSERT INTO cache_control_bookkeeping VALUES(0);
                 INSERT INTO audio_files(id,file_path,filename) VALUES
                 (1,'one.wav','wind metal'),(2,'two.wav','wind'),
                 (3,'three.wav','water'),(4,'four.wav','silence'),
                 (5,'five.wav','air'),(6,'six.wav','rain');",
            )
            .unwrap();
        (directory, provider)
    }

    fn native_ids(conn: &Connection, query: &str) -> Vec<i64> {
        conn.prepare(
            "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 ORDER BY bm25(audio_search),rowid",
        )
        .unwrap()
        .query_map([query], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn warm(provider: &SqliteProvider, query: &str) -> CacheRevision {
        let mut reader = provider.reader().unwrap();
        let tx = reader.transaction().unwrap();
        let observer = provider.observe_search_revision().unwrap();
        let revision = provider.cache_revision(&tx, observer).unwrap();
        let lengths = provider
            .rank_document_lengths(&tx, Some(observer), None)
            .unwrap();
        let ids = search_rank_parallel::collect_serial(&tx, query.into(), lengths, None)
            .unwrap()
            .unwrap();
        assert_eq!(ids.as_ref(), &native_ids(&tx, query));
        provider
            .ranking_cache
            .lock()
            .unwrap()
            .insert(&revision, query.into(), ids);
        revision
    }

    #[test]
    fn aborted_batch_never_publishes_and_commit_uses_current_native_ranking() {
        let (_directory, provider) = fixture();
        let before = warm(&provider, "wind");
        let original = provider
            .ranking_cache
            .lock()
            .unwrap()
            .peek(&before, "wind")
            .unwrap();
        let cancel = AtomicBool::new(false);
        let mut writer = provider.writer().unwrap();
        let tx = writer.transaction().unwrap();
        let baseline = provider.begin_search_cache_batch(&tx).unwrap();
        tx.execute(
            "UPDATE audio_files SET filename='wind wind wind' WHERE id=3",
            [],
        )
        .unwrap();
        let aborted = provider
            .prepare_search_cache_batch(&tx, baseline, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(aborted.rankings[0].1.as_ref(), &native_ids(&tx, "wind"));
        tx.rollback().unwrap();
        provider.publish_rankings(aborted).unwrap();
        assert!(Arc::ptr_eq(
            &original,
            &provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&before, "wind")
                .unwrap()
        ));

        let tx = writer.transaction().unwrap();
        let baseline = provider.begin_search_cache_batch(&tx).unwrap();
        tx.execute(
            "UPDATE audio_files SET filename='wind wind wind' WHERE id=3",
            [],
        )
        .unwrap();
        let expected = native_ids(&tx, "wind");
        let prepared = provider
            .prepare_search_cache_batch(&tx, baseline, &cancel)
            .unwrap()
            .unwrap();
        let committed = CacheRevision::Tracked(prepared.epoch.clone());
        assert!(
            provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&committed, "wind")
                .is_none()
        );
        tx.commit().unwrap();
        provider.publish_rankings(prepared).unwrap();
        assert_eq!(
            provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&committed, "wind")
                .unwrap()
                .as_ref(),
            &expected
        );
        assert_eq!(expected.len(), 3);
        assert_eq!(original.len(), 2);
    }

    #[test]
    fn foreign_audio_write_updates_lengths_without_mutating_pinned_old_arc() {
        let (_directory, provider) = fixture();
        {
            let writer = provider.writer().unwrap();
            search_tracking::install(&writer).unwrap();
        }
        let before = warm(&provider, "wind");
        let mut reader = provider.reader().unwrap();
        let old = reader.transaction().unwrap();
        let observer = provider.observe_search_revision().unwrap();
        let old_lengths = provider
            .cached_document_lengths(&old, observer)
            .unwrap()
            .unwrap();
        let old_ids = native_ids(&old, "wind");
        // A real separate connection exercises the persistent built-in capture,
        // without any provider cache hook or trusted publication.
        let foreign = Connection::open(&provider.path).unwrap();
        crate::schema::register_tokenizer(&foreign, crate::load_tokenizer()).unwrap();
        foreign
            .execute(
                "UPDATE audio_files SET filename='wind wind wind wind' WHERE id=1",
                [],
            )
            .unwrap();
        let after = warm(&provider, "wind");
        assert_ne!(before, after);
        let cached = provider.document_lengths.lock().unwrap();
        let new_lengths = cached
            .iter()
            .find(|(key, _)| key == &after)
            .unwrap()
            .1
            .clone();
        assert!(!Arc::ptr_eq(&old_lengths, &new_lengths));
        drop(cached);
        assert_eq!(
            search_rank_parallel::collect_serial(&old, "wind".into(), Some(old_lengths), None)
                .unwrap()
                .unwrap()
                .as_ref(),
            &old_ids
        );
        // A late old snapshot must not rewind the accepted live observation or
        // evict the already computed new generation.
        assert_eq!(provider.cache_revision(&old, observer).unwrap(), before);
        assert_eq!(
            provider.document_lengths.lock().unwrap().back().unwrap().0,
            after
        );
        assert!(
            provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&after, "wind")
                .is_some()
        );
    }

    #[test]
    fn unknown_same_epoch_commit_retires_tracked_payloads_and_reset_keeps_observer() {
        let (_directory, provider) = fixture();
        {
            let writer = provider.writer().unwrap();
            search_tracking::install(&writer).unwrap();
        }
        let before = warm(&provider, "wind");
        let previous_observer = provider.observe_search_revision().unwrap();
        let foreign = Connection::open(&provider.path).unwrap();
        // This is deliberately outside the supported audio_files writer contract.
        // Its unchanged journal plus changed observer must conservatively miss.
        foreign
            .execute("DELETE FROM audio_search WHERE rowid=1", [])
            .unwrap();
        let mut reader = provider.reader().unwrap();
        let tx = reader.transaction().unwrap();
        let current_observer = provider.observe_search_revision().unwrap();
        assert_ne!(previous_observer, current_observer);
        assert_eq!(
            Some(before.clone()),
            search_tracking::epoch(&tx)
                .unwrap()
                .map(CacheRevision::Tracked)
        );
        let unknown = provider.cache_revision(&tx, current_observer).unwrap();
        assert_eq!(unknown, CacheRevision::Untracked(current_observer));
        assert!(
            provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&before, "wind")
                .is_none()
        );
        assert!(
            provider
                .cached_document_lengths(&tx, current_observer)
                .unwrap()
                .is_none()
        );
        tx.commit().unwrap();
        let quarantined = warm(&provider, "wind");
        assert!(matches!(quarantined, CacheRevision::Untracked(_)));
        {
            let writer = provider.writer().unwrap();
            writer
                .execute("UPDATE cache_control_bookkeeping SET value=value+1", [])
                .unwrap();
            provider
                .search_cache_bookkeeping_committed(&writer)
                .unwrap();
        }
        assert!(
            provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&quarantined, "wind")
                .is_none()
        );
        assert!(provider.document_lengths.lock().unwrap().is_empty());
        provider.reset_search_caches().unwrap();
        assert!(provider.revision_reader.lock().unwrap().is_some());
        let retained_observer = provider.observe_search_revision().unwrap();
        assert!(retained_observer > current_observer);
        provider.reset_search_caches().unwrap();
        assert_eq!(
            provider.observe_search_revision().unwrap(),
            retained_observer
        );
        assert!(provider.search_cache_observation.lock().unwrap().is_none());
    }

    #[test]
    fn known_bookkeeping_keeps_tracked_payloads_without_relabeling() {
        let (_directory, provider) = fixture();
        {
            let writer = provider.writer().unwrap();
            search_tracking::install(&writer).unwrap();
        }
        let before = warm(&provider, "wind");
        let original = provider
            .ranking_cache
            .lock()
            .unwrap()
            .peek(&before, "wind")
            .unwrap();
        let writer = provider.writer().unwrap();
        writer
            .execute("UPDATE cache_control_bookkeeping SET value=value+1", [])
            .unwrap();
        provider
            .search_cache_bookkeeping_committed(&writer)
            .unwrap();
        let mut reader = provider.reader().unwrap();
        let tx = reader.transaction().unwrap();
        let current = provider
            .cache_revision(&tx, provider.observe_search_revision().unwrap())
            .unwrap();
        assert_eq!(current, before);
        assert!(Arc::ptr_eq(
            &original,
            &provider
                .ranking_cache
                .lock()
                .unwrap()
                .peek(&current, "wind")
                .unwrap()
        ));
        assert!(
            provider
                .cached_document_lengths(&tx, provider.observe_search_revision().unwrap())
                .unwrap()
                .is_some()
        );
    }
}
