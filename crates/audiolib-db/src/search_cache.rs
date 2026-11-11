// crates/audiolib-db/src/search_cache.rs
//! Bounded FTS ranking identities; Metadata is always hydrated from the current snapshot.
use std::{collections::VecDeque, sync::Arc};

pub(crate) const MAX_RANKED_IDS: usize = 2_097_152;
pub(crate) const MAX_CACHED_IDS: usize = 4_194_304;
const MAX_ENTRIES: usize = 16;
const MAX_GENERATIONS: usize = 2;

/// Full tracked lineage/watermark, or the legacy observer scoped to this provider.
/// An observer counter alone never escapes its provider's private cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CacheRevision {
    Tracked(crate::search_tracking::Epoch),
    Untracked(u64),
}

impl From<u64> for CacheRevision {
    fn from(value: u64) -> Self {
        Self::Untracked(value)
    }
}

impl From<&CacheRevision> for CacheRevision {
    fn from(value: &CacheRevision) -> Self {
        value.clone()
    }
}

struct Entry {
    revision: CacheRevision,
    query: String,
    ids: Arc<Vec<i64>>,
    fingerprint: u64,
}

// A deterministic, order-sensitive prefilter only. Full sequence equality below
// remains authoritative, including when two different rankings hash alike.
fn ranking_fingerprint(ids: &[i64]) -> u64 {
    ids.iter().fold(0xcbf2_9ce4_8422_2325, |hash, id| {
        (hash ^ (*id as u64)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Default)]
pub(crate) struct RankingCache {
    generations: VecDeque<CacheRevision>,
    entries: VecDeque<Entry>,
    ids: usize,
}

impl RankingCache {
    /// Borrow a complete ranking for a proven pinned snapshot without changing
    /// its revision, LRU order or admission state. An older reader must not clear
    /// a newer cache while opportunistically checking a concurrent page result.
    pub(crate) fn peek(
        &self,
        revision: impl Into<CacheRevision>,
        query: &str,
    ) -> Option<Arc<Vec<i64>>> {
        let revision = revision.into();
        self.entries
            .iter()
            .find(|entry| entry.revision == revision && entry.query == query)
            .map(|entry| entry.ids.clone())
    }

    pub(crate) fn get(
        &mut self,
        revision: impl Into<CacheRevision>,
        query: &str,
    ) -> Option<Arc<Vec<i64>>> {
        let revision = revision.into();
        if self.generations.back() != Some(&revision) {
            return self.peek(&revision, query);
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.revision == revision && entry.query == query)?;
        let entry = self.entries.remove(index)?;
        let ids = entry.ids.clone();
        self.entries.push_back(entry);
        Some(ids)
    }

    pub(crate) fn insert(
        &mut self,
        revision: impl Into<CacheRevision>,
        query: String,
        ids: Arc<Vec<i64>>,
    ) {
        let revision = revision.into();
        if ids.len() > MAX_RANKED_IDS {
            return;
        }
        let Some(new_generation) = self.admit_revision(&revision) else {
            return;
        };
        self.insert_admitted(revision, query, ids, new_generation);
    }

    fn admit_revision(&mut self, revision: &CacheRevision) -> Option<bool> {
        let new_generation = !self.generations.contains(revision);
        if !new_generation && self.generations.back() != Some(revision) {
            // An old request may finish after publication. Its owned result is
            // valid, but admission must not evict the new generation's entries.
            return None;
        }
        if new_generation
            && let Some(current) = self.generations.back()
            && !match (revision, current) {
                (CacheRevision::Tracked(next), CacheRevision::Tracked(previous)) => {
                    next.cookie == previous.cookie && next.sequence > previous.sequence
                }
                (CacheRevision::Untracked(next), CacheRevision::Untracked(previous)) => {
                    next > previous
                }
                _ => false,
            }
        {
            // A retired snapshot/unknown lineage cannot supersede the current
            // generation. The owner explicitly clears on a proven restore/reset.
            return None;
        }
        // Only admitting a new generation may retire the oldest one. A lookup
        // from an older pinned reader must never rewind/clear a newer generation.
        if new_generation {
            self.generations.push_back(revision.clone());
            while self.generations.len() > MAX_GENERATIONS {
                if let Some(oldest) = self.generations.pop_front() {
                    let mut index = 0;
                    while index < self.entries.len() {
                        if self.entries[index].revision == oldest {
                            if let Some(previous) = self.entries.remove(index) {
                                self.release_payload(&previous.ids);
                            }
                        } else {
                            index += 1;
                        }
                    }
                }
            }
        }
        Some(new_generation)
    }

    /// Reserve a proven current lineage under the same owner mutex as clear().
    /// No fake query or identity payload is needed. Late readers cannot claim
    /// an old cookie/observer while the first new ranking is still being built.
    pub(crate) fn register_revision(&mut self, revision: CacheRevision) {
        let _ = self.admit_revision(&revision);
    }

    fn insert_admitted(
        &mut self,
        revision: CacheRevision,
        query: String,
        ids: Arc<Vec<i64>>,
        new_generation: bool,
    ) {
        let fingerprint = ranking_fingerprint(&ids);
        let canonical = self
            .entries
            .iter()
            .find(|entry| {
                entry.ids.len() == ids.len()
                    && entry.fingerprint == fingerprint
                    && (Arc::ptr_eq(&entry.ids, &ids) || entry.ids.as_slice() == ids.as_slice())
            })
            .map(|entry| entry.ids.clone());
        let ids = canonical.unwrap_or(ids);
        let replacement = self
            .entries
            .iter()
            .position(|entry| entry.revision == revision && entry.query == query);
        let replaced_payload = replacement.map_or(0, |index| {
            let previous = &self.entries[index];
            if self
                .entries
                .iter()
                .enumerate()
                .any(|(other, entry)| other != index && Arc::ptr_eq(&entry.ids, &previous.ids))
            {
                0
            } else {
                previous.ids.len()
            }
        });
        let added_payload = if self
            .entries
            .iter()
            .enumerate()
            .any(|(index, entry)| Some(index) != replacement && Arc::ptr_eq(&entry.ids, &ids))
        {
            0
        } else {
            ids.len()
        };
        // A broad new query must not flush a working set of smaller rankings.
        // Admission can be refused without changing the complete in-flight
        // result or the per-query/global memory limits. Count only unique payloads
        // that remain after replacement; small queries keep LRU admission.
        if !new_generation
            && ids.len() > MAX_CACHED_IDS / 8
            && self.ids - replaced_payload + added_payload > MAX_CACHED_IDS
        {
            return;
        }
        if let Some(index) = replacement
            && let Some(previous) = self.entries.remove(index)
        {
            self.release_payload(&previous.ids);
        }
        loop {
            // Eviction may remove the last cached reference to this candidate's
            // canonical Arc. Recompute its added payload after every removal.
            let added_payload = if self.retains_payload(&ids) {
                0
            } else {
                ids.len()
            };
            if self.ids + added_payload <= MAX_CACHED_IDS && self.entries.len() < MAX_ENTRIES {
                self.ids += added_payload;
                self.entries.push_back(Entry {
                    revision,
                    query,
                    ids,
                    fingerprint,
                });
                return;
            }
            let Some(previous) = self.entries.pop_front() else {
                return;
            };
            self.release_payload(&previous.ids);
        }
    }

    /// Snapshot hot compiled queries without holding a lock during SQL work.
    /// The caller additionally limits actual candidate counts/work before commit.
    #[cfg(test)]
    pub(crate) fn hot_queries(&self, revision: impl Into<CacheRevision>) -> Vec<String> {
        let revision = revision.into();
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.revision == revision)
            .take(MAX_ENTRIES)
            .map(|entry| entry.query.clone())
            .collect()
    }

    /// Recent compiled query names/counts across retained generations. Counts
    /// bound maintenance work only: IDs and scores must be recomputed in the
    /// current writer transaction, including when tracking was just installed.
    pub(crate) fn maintenance_queries(&self) -> Vec<(String, usize)> {
        let mut seen = std::collections::HashSet::new();
        self.entries
            .iter()
            .rev()
            .filter(|entry| seen.insert(entry.query.as_str()))
            .take(MAX_ENTRIES)
            .map(|entry| (entry.query.clone(), entry.ids.len()))
            .collect()
    }

    /// Retire identities after the provider proves an incarnation/restore change.
    /// Existing readers retain their independently owned immutable Arc payloads.
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Admit immutable candidates only for the exact committed epoch they were
    /// built from. Aborted transactions simply drop their unpublished candidates.
    /// Admission preserves the same per-query/global identity and entry budgets.
    pub(crate) fn publish(
        &mut self,
        revision: impl Into<CacheRevision>,
        candidates: Vec<(String, Arc<Vec<i64>>)>,
    ) {
        let revision = revision.into();
        let mut total = 0usize;
        for (query, ids) in candidates.into_iter().take(MAX_ENTRIES) {
            let Some(next) = total.checked_add(ids.len()) else {
                break;
            };
            if next > MAX_CACHED_IDS {
                break;
            }
            total = next;
            self.insert(&revision, query, ids);
        }
    }

    fn retains_payload(&self, ids: &Arc<Vec<i64>>) -> bool {
        self.entries
            .iter()
            .any(|entry| Arc::ptr_eq(&entry.ids, ids))
    }

    fn release_payload(&mut self, ids: &Arc<Vec<i64>>) {
        // In-flight readers may still hold this Arc. Only references retained by
        // cache entries contribute to the cache's 32 MiB identity-payload budget.
        if !self.retains_payload(ids) {
            self.ids -= ids.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_peeks_do_not_rewind_revision_or_change_lru_payloads() {
        let mut cache = RankingCache::default();
        let older = Arc::new(vec![9, 1]);
        let newer = Arc::new(vec![8, 3]);
        cache.insert(11_u64, "old".into(), older.clone());
        cache.insert(11_u64, "other".into(), Arc::new(vec![4]));
        assert!(Arc::ptr_eq(&cache.peek(11_u64, "old").unwrap(), &older));
        assert_eq!(cache.entries.front().unwrap().query, "old");
        cache.insert(12_u64, "new".into(), newer.clone());
        let payloads = cache.ids;
        assert!(Arc::ptr_eq(&cache.peek(11_u64, "old").unwrap(), &older));
        assert!(cache.peek(13_u64, "new").is_none());
        assert!(cache.peek(12_u64, "different-query").is_none());
        assert_eq!(
            cache.generations.back(),
            Some(&CacheRevision::Untracked(12))
        );
        assert_eq!(cache.ids, payloads);
        assert!(Arc::ptr_eq(&cache.peek(12_u64, "new").unwrap(), &newer));
        // A reader holding the old Arc can finish its older pinned transaction.
        assert_eq!(*older, vec![9, 1]);
    }

    #[test]
    fn retained_rankings_are_bounded_and_never_cross_commit_keys() {
        let mut cache = RankingCache::default();
        for index in 0..20 {
            cache.insert(
                1_u64,
                index.to_string(),
                Arc::new(vec![index; MAX_RANKED_IDS / 4]),
            );
            assert!(cache.ids <= MAX_CACHED_IDS);
            assert!(cache.entries.len() <= MAX_ENTRIES);
        }
        assert!(cache.get(1_u64, "0").is_none());
        assert!(cache.get(1_u64, "19").is_some());
        assert!(cache.get(2_u64, "19").is_none());
        assert!(cache.ids <= MAX_CACHED_IDS);
        assert!(
            cache.get(1_u64, "19").is_some(),
            "a miss cannot discard an old pinned generation"
        );
    }

    #[test]
    fn broad_queries_cannot_turn_a_slightly_oversized_working_set_into_all_misses() {
        let mut cache = RankingCache::default();
        let sizes = [
            100_000, 1_000_000, 100_000, 200_000, 1_000_000, 100_000, 0, 100_000, 1_000, 100_000,
            100_000, 100_000, 100_000, 1_000_000, 100_000, 100_000,
        ];
        let mut hits = 0;
        for round in 0..3 {
            hits = 0;
            for (query, size) in sizes.into_iter().enumerate() {
                let query = query.to_string();
                if cache.get(1_u64, &query).is_some() {
                    hits += 1;
                } else {
                    let identity = query.parse::<i64>().unwrap();
                    cache.insert(1_u64, query, Arc::new(vec![identity; size]));
                }
                assert!(cache.ids <= MAX_CACHED_IDS, "round {round}");
                assert!(cache.entries.len() <= MAX_ENTRIES);
            }
        }
        assert!(
            hits >= 15,
            "the retained working set must stabilize: {hits}"
        );
    }

    #[test]
    fn refused_large_admission_preserves_entries_and_new_generations_stay_bounded() {
        let mut cache = RankingCache::default();
        let original = Arc::new(vec![1; 200_000]);
        cache.insert(1_u64, "small".into(), original.clone());
        for (index, query) in ["large-a", "large-b", "large-c"].into_iter().enumerate() {
            cache.insert(
                1_u64,
                query.into(),
                Arc::new(vec![index as i64 + 10; 1_000_000]),
            );
        }
        let before = cache.ids;
        cache.insert(1_u64, "small".into(), Arc::new(vec![2; MAX_RANKED_IDS]));
        assert_eq!(cache.ids, before);
        assert!(Arc::ptr_eq(&cache.get(1_u64, "small").unwrap(), &original));
        cache.insert(1_u64, "new-large".into(), Arc::new(vec![0; MAX_RANKED_IDS]));
        assert!(cache.get(1_u64, "new-large").is_none());
        assert_eq!(cache.ids, before);
        cache.insert(2_u64, "new-large".into(), Arc::new(vec![0; MAX_RANKED_IDS]));
        assert!(cache.ids <= MAX_CACHED_IDS);
        assert!(cache.get(2_u64, "new-large").is_some());
        assert!(cache.get(2_u64, "small").is_none());
    }

    #[test]
    fn different_queries_with_identical_ordered_ids_share_one_payload() {
        let mut cache = RankingCache::default();
        let original = Arc::new(vec![8, 3, 5]);
        let duplicate = Arc::new(vec![8, 3, 5]);
        assert!(!Arc::ptr_eq(&original, &duplicate));
        cache.insert(1_u64, "metal".into(), original.clone());
        cache.insert(1_u64, "metal".into(), duplicate.clone());
        assert_eq!(cache.ids, 3, "same-key replacement retains one payload");
        cache.insert(1_u64, "et".into(), duplicate.clone());
        let metal = cache.get(1_u64, "metal").unwrap();
        let fragment = cache.get(1_u64, "et").unwrap();
        assert!(Arc::ptr_eq(&metal, &original));
        assert!(Arc::ptr_eq(&metal, &fragment));
        assert!(!Arc::ptr_eq(&fragment, &duplicate));
        assert_eq!(cache.ids, 3);
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn a_fingerprint_collision_cannot_share_differently_ordered_ids() {
        let mut cache = RankingCache::default();
        let original = Arc::new(vec![8, 3, 5]);
        let different = Arc::new(vec![5, 3, 8]);
        cache.insert(1_u64, "first".into(), original.clone());
        // Force the inexpensive prefilter to collide. Exact sequence equality
        // must still protect both the result order and payload accounting.
        cache.entries.back_mut().unwrap().fingerprint = ranking_fingerprint(&different);
        cache.insert(1_u64, "second".into(), different.clone());
        let first = cache.get(1_u64, "first").unwrap();
        let second = cache.get(1_u64, "second").unwrap();
        assert!(Arc::ptr_eq(&first, &original));
        assert!(Arc::ptr_eq(&second, &different));
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(cache.ids, 6);
    }

    #[test]
    fn replacements_release_only_the_last_cached_payload_reference() {
        let mut cache = RankingCache::default();
        let in_flight = Arc::new(vec![1; 10]);
        cache.insert(1_u64, "first".into(), in_flight.clone());
        cache.insert(1_u64, "second".into(), Arc::new(vec![1; 10]));
        assert_eq!(cache.ids, 10);
        cache.insert(1_u64, "first".into(), Arc::new(vec![2; 5]));
        assert_eq!(cache.ids, 15);
        cache.insert(1_u64, "second".into(), Arc::new(vec![3; 7]));
        assert_eq!(
            cache.ids, 12,
            "an in-flight Arc is outside the cache budget"
        );
        assert_eq!(in_flight.len(), 10);
        cache.insert(1_u64, "first".into(), Arc::new(vec![3; 7]));
        assert_eq!(cache.ids, 7);
        assert!(Arc::ptr_eq(
            &cache.get(1_u64, "first").unwrap(),
            &cache.get(1_u64, "second").unwrap()
        ));
    }

    #[test]
    fn entry_eviction_preserves_shared_payload_until_its_last_reference() {
        let mut cache = RankingCache::default();
        let in_flight = Arc::new(vec![1; MAX_RANKED_IDS]);
        cache.insert(1_u64, "shared-first".into(), in_flight.clone());
        cache.insert(
            1_u64,
            "shared-second".into(),
            Arc::new(vec![1; MAX_RANKED_IDS]),
        );
        cache.insert(1_u64, "other".into(), Arc::new(vec![2; MAX_RANKED_IDS]));
        assert_eq!(cache.ids, MAX_CACHED_IDS);
        for index in 0..14 {
            cache.insert(1_u64, format!("empty-{index}"), Arc::new(Vec::new()));
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert_eq!(cache.ids, MAX_CACHED_IDS);
        assert!(
            cache
                .entries
                .iter()
                .all(|entry| entry.query != "shared-first")
        );
        cache.insert(1_u64, "empty-last".into(), Arc::new(Vec::new()));
        assert_eq!(cache.ids, MAX_RANKED_IDS);
        assert!(
            cache
                .entries
                .iter()
                .all(|entry| entry.query != "shared-second")
        );
        assert_eq!(in_flight.len(), MAX_RANKED_IDS);
    }

    #[test]
    fn evicting_the_candidates_canonical_entry_recharges_its_payload() {
        let mut cache = RankingCache::default();
        let canonical = Arc::new(vec![1; MAX_RANKED_IDS]);
        cache.insert(1_u64, "canonical".into(), canonical.clone());
        cache.insert(1_u64, "other".into(), Arc::new(vec![2; MAX_RANKED_IDS]));
        for index in 0..14 {
            cache.insert(1_u64, format!("empty-{index}"), Arc::new(Vec::new()));
        }
        cache.insert(
            1_u64,
            "equivalent".into(),
            Arc::new(vec![1; MAX_RANKED_IDS]),
        );
        assert!(cache.get(1_u64, "canonical").is_none());
        assert!(Arc::ptr_eq(
            &cache.get(1_u64, "equivalent").unwrap(),
            &canonical
        ));
        assert_eq!(cache.ids, MAX_CACHED_IDS);
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn refused_replacement_cannot_discount_a_payload_shared_by_another_key() {
        let mut cache = RankingCache::default();
        let original = Arc::new(vec![1; 200_000]);
        cache.insert(1_u64, "original".into(), original.clone());
        cache.insert(1_u64, "alias".into(), Arc::new(vec![1; 200_000]));
        for (index, query) in ["large-a", "large-b", "large-c"].into_iter().enumerate() {
            cache.insert(
                1_u64,
                query.into(),
                Arc::new(vec![index as i64 + 10; 1_000_000]),
            );
        }
        let before = cache.ids;
        cache.insert(1_u64, "original".into(), Arc::new(vec![2; MAX_RANKED_IDS]));
        assert_eq!(cache.ids, before);
        assert!(Arc::ptr_eq(
            &cache.get(1_u64, "original").unwrap(),
            &original
        ));
        assert!(Arc::ptr_eq(&cache.get(1_u64, "alias").unwrap(), &original));
        assert!(cache.get(2_u64, "original").is_none());
        assert_eq!(
            cache.ids, before,
            "a newer lookup cannot destroy still-pinned identities"
        );
        assert!(Arc::ptr_eq(
            &cache.get(1_u64, "original").unwrap(),
            &original
        ));
        cache.insert(
            2_u64,
            "too-large".into(),
            Arc::new(vec![0; MAX_RANKED_IDS + 1]),
        );
        assert_eq!(cache.ids, before);
        assert!(cache.get(2_u64, "too-large").is_none());
    }

    #[test]
    fn full_epoch_cookie_and_sequence_bind_two_generations_and_late_readers() {
        let epoch = |cookie: &str, sequence| {
            CacheRevision::Tracked(crate::search_tracking::Epoch {
                cookie: cookie.into(),
                sequence,
            })
        };
        let mut cache = RankingCache::default();
        let old = Arc::new(vec![9, 1]);
        let new = Arc::new(vec![1, 9]);
        cache.insert(epoch("a", 10), "word".into(), old.clone());
        cache.publish(epoch("a", 11), vec![("word".into(), new.clone())]);
        assert!(Arc::ptr_eq(
            &cache.get(epoch("a", 10), "word").unwrap(),
            &old
        ));
        assert!(Arc::ptr_eq(
            &cache.get(epoch("a", 11), "word").unwrap(),
            &new
        ));
        assert!(cache.get(epoch("foreign", 11), "word").is_none());
        assert_eq!(cache.hot_queries(epoch("a", 11)), vec!["word"]);
        assert_eq!(cache.maintenance_queries(), vec![("word".into(), 2)]);
        cache.insert(epoch("a", 10), "late".into(), Arc::new(vec![2]));
        assert!(cache.peek(epoch("a", 10), "late").is_none());
        cache.insert(epoch("a", 12), "newest".into(), Arc::new(vec![3]));
        assert!(cache.peek(epoch("a", 10), "word").is_none());
        cache.insert(epoch("a", 10), "retired".into(), old.clone());
        cache.insert(epoch("foreign", 100), "foreign".into(), old.clone());
        assert!(cache.peek(epoch("a", 12), "newest").is_some());
        assert_eq!(cache.generations.back(), Some(&epoch("a", 12)));
        assert_eq!(
            cache.maintenance_queries(),
            vec![("newest".into(), 1), ("word".into(), 2)]
        );
        cache.clear();
        cache.insert(epoch("foreign", 1), "foreign".into(), Arc::new(vec![8]));
        assert!(cache.peek(epoch("foreign", 1), "foreign").is_some());
        assert_eq!(*old, vec![9, 1]);
    }

    #[test]
    fn trusted_empty_reservation_rejects_late_readers_after_cookie_and_mode_reset() {
        let epoch = |cookie: &str, sequence| {
            CacheRevision::Tracked(crate::search_tracking::Epoch {
                cookie: cookie.into(),
                sequence,
            })
        };
        let mut cache = RankingCache::default();
        cache.insert(epoch("old", 10), "word".into(), Arc::new(vec![1]));
        cache.clear();
        cache.register_revision(epoch("new", 1));
        cache.insert(epoch("old", 100), "late".into(), Arc::new(vec![2]));
        assert!(cache.maintenance_queries().is_empty());
        assert_eq!(cache.generations.back(), Some(&epoch("new", 1)));
        cache.insert(epoch("new", 1), "word".into(), Arc::new(vec![3]));
        assert_eq!(*cache.peek(epoch("new", 1), "word").unwrap(), vec![3]);
        cache.clear();
        cache.register_revision(CacheRevision::Untracked(20));
        cache.insert(epoch("new", 1), "late-tracked".into(), Arc::new(vec![4]));
        cache.insert(19_u64, "late-observer".into(), Arc::new(vec![5]));
        assert!(cache.maintenance_queries().is_empty());
        cache.insert(20_u64, "word".into(), Arc::new(vec![6]));
        assert_eq!(*cache.peek(20_u64, "word").unwrap(), vec![6]);
    }
}
