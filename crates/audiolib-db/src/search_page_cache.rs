// crates/audiolib-db/src/search_page_cache.rs
//! Optional owned first-page responses for one provider's dedicated observer.
//! SQL, serialization, response cloning, and cursor encoding belong outside the
//! owner's cache mutex. Budgets measure encoded key/payload bytes, not heap/RSS.
use audiolib_core::SearchRequest;
use serde::Serialize;
use std::{
    collections::{HashSet, VecDeque},
    io::{self, Write},
    sync::Arc,
};

mod lookup_request;

pub(crate) use lookup_request::LookupRequest;

pub(crate) const MAX_PAGE_ENTRIES: usize = 8;
pub(crate) const MAX_PAGE_GENERATIONS: usize = 2;
pub(crate) const MAX_PAGE_ENCODED_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ENTRY_ENCODED_BYTES: usize = 1024 * 1024;

/// Exact serialized identity of an already normalized, valid first-page request.
/// No query/scope/filter equivalence is inferred from the human-readable text.
pub(crate) struct PageRequest {
    request: SearchRequest,
    key: Vec<u8>,
}

impl PageRequest {
    /// Test convenience for owned requests; production admits borrowed lookups
    /// only after a stable, size-checked miss. Uses the same validator and bytes.
    #[cfg(test)]
    pub(crate) fn new(request: &SearchRequest) -> Option<Arc<Self>> {
        LookupRequest::new(request)?.into_owned()
    }

    /// Exact serialized identity; lookups borrow the same bytes without cloning.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.key
    }

    /// An immutable request; hot-list callers may clone it after releasing locks.
    pub(crate) fn request(&self) -> &SearchRequest {
        &self.request
    }

    /// Encoded bytes reserved for a payload after charging this request's key.
    pub(crate) fn payload_budget(&self) -> usize {
        MAX_ENTRY_ENCODED_BYTES - self.key.len()
    }
}

struct CountWriter {
    bytes: usize,
    limit: usize,
}

impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| io::Error::other("First-page payload exceeds encoded budget"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Check borrowed data before cloning a potentially large owned response.
/// This writes into a capped counter only; call it outside the cache mutex.
pub(crate) fn encoded_fits<T: Serialize + ?Sized>(value: &T, max_bytes: usize) -> bool {
    serde_json::to_writer(
        CountWriter {
            bytes: 0,
            limit: max_bytes,
        },
        value,
    )
    .is_ok()
}

/// An immutable owned response whose exact encoded charge was checked off-lock.
/// P may contain a SearchPage plus internal continuation information. The owner
/// must not retain a precommit opaque cursor or reuse old response timing.
pub(crate) struct PreparedCacheEntry<P> {
    request: Arc<PageRequest>,
    page: Arc<P>,
    encoded_bytes: usize,
}

impl<P: Serialize> PreparedCacheEntry<P> {
    /// Serialize only into a bounded counter, without allocating a payload copy.
    /// Any serialization/size failure declines this optional cache candidate.
    pub(crate) fn new(request: Arc<PageRequest>, page: Arc<P>) -> Option<Self> {
        let mut counter = CountWriter {
            bytes: 0,
            limit: request.payload_budget(),
        };
        serde_json::to_writer(&mut counter, page.as_ref()).ok()?;
        let encoded_bytes = request.bytes().len().checked_add(counter.bytes)?;
        Some(Self {
            request,
            page,
            encoded_bytes,
        })
    }
}

struct Entry<P> {
    revision: u64,
    prepared: PreparedCacheEntry<P>,
}

/// Bounded storage only: this module never proves a database image or does SQL.
/// Revisions belong to one dedicated observer for this provider. Their distances
/// have no meaning. The owner clears before replacing/resetting that observer.
pub(crate) struct PageCache<P> {
    generations: VecDeque<u64>,
    entries: VecDeque<Entry<P>>,
    encoded_bytes: usize,
}

impl<P> Default for PageCache<P> {
    fn default() -> Self {
        Self {
            generations: VecDeque::new(),
            entries: VecDeque::new(),
            encoded_bytes: 0,
        }
    }
}

impl<P> PageCache<P> {
    /// Reserve a trusted current observer revision without inserting a fake page.
    /// Late older readers/candidates cannot retire a higher admitted revision.
    /// Drop retired payloads only after releasing the owner's cache mutex.
    #[must_use = "Drop retired page payloads after releasing the cache mutex"]
    pub(crate) fn register_revision(&mut self, revision: u64) -> Vec<Arc<P>> {
        let mut retired = Vec::new();
        let _ = self.admit_revision(revision, &mut retired);
        retired
    }

    fn admit_revision(&mut self, revision: u64, retired: &mut Vec<Arc<P>>) -> bool {
        if let Some(current) = self.generations.back() {
            if revision < *current {
                return false;
            }
            if revision == *current {
                return true;
            }
        }
        self.generations.push_back(revision);
        while self.generations.len() > MAX_PAGE_GENERATIONS {
            let oldest = self.generations.pop_front().unwrap();
            let mut index = 0;
            while index < self.entries.len() {
                if self.entries[index].revision == oldest
                    && let Some(page) = self.remove(index)
                {
                    retired.push(page);
                } else {
                    index += 1;
                }
            }
        }
        true
    }

    /// Return only an Arc. Clone owned DTOs and encode current cursors off-lock.
    /// An old pinned lookup does not change the current generation or LRU order.
    pub(crate) fn get(&mut self, revision: u64, key: &[u8]) -> Option<Arc<P>> {
        let index = self.entries.iter().position(|entry| {
            entry.revision == revision && entry.prepared.request.bytes() == key
        })?;
        if self.generations.back() != Some(&revision) {
            return Some(self.entries[index].prepared.page.clone());
        }
        let entry = self.entries.remove(index)?;
        let page = entry.prepared.page.clone();
        self.entries.push_back(entry);
        Some(page)
    }

    /// Admit a complete proven response. Its request and payload were prepared
    /// before locking. Both retained generations share all entry/byte budgets.
    /// Replaced, evicted, and refused payloads are returned for off-lock drop.
    #[must_use = "Drop retired page payloads after releasing the cache mutex"]
    pub(crate) fn insert(&mut self, revision: u64, prepared: PreparedCacheEntry<P>) -> Vec<Arc<P>> {
        let mut retired = Vec::new();
        if prepared.encoded_bytes > MAX_ENTRY_ENCODED_BYTES
            || !self.admit_revision(revision, &mut retired)
        {
            retired.push(prepared.page);
            return retired;
        }
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.revision == revision && entry.prepared.request.key == prepared.request.key
        }) && let Some(page) = self.remove(index)
        {
            retired.push(page);
        }
        while self.entries.len() >= MAX_PAGE_ENTRIES
            || self.encoded_bytes + prepared.encoded_bytes > MAX_PAGE_ENCODED_BYTES
        {
            if let Some(page) = self.remove(0) {
                retired.push(page);
            }
        }
        self.encoded_bytes += prepared.encoded_bytes;
        self.entries.push_back(Entry { revision, prepared });
        retired
    }

    fn remove(&mut self, index: usize) -> Option<Arc<P>> {
        let entry = self.entries.remove(index)?;
        self.encoded_bytes -= entry.prepared.encoded_bytes;
        Some(entry.prepared.page)
    }

    /// Snapshot distinct recent requests across retained generations, newest/LRU
    /// first. Only Arcs are cloned here; expensive preparation occurs off-lock.
    pub(crate) fn maintenance_requests(&self) -> Vec<Arc<PageRequest>> {
        let mut seen = HashSet::new();
        self.entries
            .iter()
            .rev()
            .filter(|entry| seen.insert(entry.prepared.request.key.as_slice()))
            .take(MAX_PAGE_ENTRIES)
            .map(|entry| entry.prepared.request.clone())
            .collect()
    }

    /// Retire all generations after administration/reset. In-flight Arcs remain
    /// independently owned and may finish their already pinned read snapshots.
    /// Drop the returned old storage only after releasing the owner's mutex.
    #[must_use = "Drop old page storage after releasing the cache mutex"]
    pub(crate) fn clear(&mut self) -> Self {
        std::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::{AudioFile, SearchPage, SearchScope, SortOrder};

    fn request(query: &str) -> Arc<PageRequest> {
        PageRequest::new(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
    }

    fn text_entry(request: Arc<PageRequest>, text: &str) -> PreparedCacheEntry<String> {
        PreparedCacheEntry::new(request, Arc::new(text.to_owned())).unwrap()
    }

    fn insert<P>(cache: &mut PageCache<P>, revision: u64, entry: PreparedCacheEntry<P>) {
        drop(cache.insert(revision, entry));
    }

    #[test]
    fn only_valid_normalized_first_pages_are_eligible() {
        let valid = SearchRequest::default();
        assert!(PageRequest::new(&valid).is_some());
        for invalid in [
            SearchRequest {
                limit: 0,
                ..valid.clone()
            },
            SearchRequest {
                limit: 201,
                ..valid.clone()
            },
            SearchRequest {
                offset: 1,
                ..valid.clone()
            },
            SearchRequest {
                cursor: Some("opaque".into()),
                ..valid.clone()
            },
            SearchRequest {
                page: Some(1),
                ..valid.clone()
            },
            SearchRequest {
                query: "a\0b".into(),
                ..valid.clone()
            },
        ] {
            assert!(PageRequest::new(&invalid).is_none());
        }
        assert!(PageRequest::new(&SearchRequest { limit: 1, ..valid }).is_some());
    }

    #[test]
    fn key_keeps_scope_filters_order_and_limit_isolated() {
        let base = SearchRequest {
            query: "vehicle".into(),
            ..Default::default()
        };
        let original = PageRequest::new(&base).unwrap();
        let mut cache = PageCache::default();
        insert(&mut cache, 3, text_entry(original.clone(), "main"));
        assert_eq!(
            cache
                .get(3, PageRequest::new(&base).unwrap().bytes())
                .unwrap()
                .as_str(),
            "main"
        );
        let mut filtered = base.clone();
        filtered.filters.favorites_only = true;
        for different in [
            SearchRequest {
                scope: SearchScope::Filename,
                ..base.clone()
            },
            SearchRequest {
                scope: SearchScope::Path,
                ..base.clone()
            },
            SearchRequest {
                scope: SearchScope::All,
                ..base.clone()
            },
            SearchRequest {
                folder_id: Some(7),
                ..base.clone()
            },
            SearchRequest {
                include_subfolders: false,
                ..base.clone()
            },
            SearchRequest {
                order_by: SortOrder::Filename,
                ..base.clone()
            },
            SearchRequest {
                descending: true,
                ..base.clone()
            },
            SearchRequest {
                limit: 100,
                ..base.clone()
            },
            filtered,
        ] {
            assert!(
                cache
                    .get(3, PageRequest::new(&different).unwrap().bytes())
                    .is_none()
            );
        }
        assert_eq!(original.request().query, "vehicle");
    }

    #[test]
    fn pinned_old_responses_survive_but_late_inserts_never_retire_newer_revisions() {
        let key = request("vehicle");
        let mut cache = PageCache::default();
        insert(&mut cache, 4, text_entry(key.clone(), "old"));
        let pinned = cache.get(4, key.bytes()).unwrap();
        insert(&mut cache, 19, text_entry(key.clone(), "new"));
        assert_eq!(cache.get(4, key.bytes()).unwrap().as_str(), "old");
        insert(&mut cache, 4, text_entry(key.clone(), "late old"));
        insert(&mut cache, 7, text_entry(request("late"), "late unknown"));
        assert_eq!(cache.generations, VecDeque::from([4, 19]));
        assert_eq!(cache.get(19, key.bytes()).unwrap().as_str(), "new");
        drop(cache.register_revision(42));
        assert!(cache.get(4, key.bytes()).is_none());
        assert_eq!(pinned.as_str(), "old");
        insert(&mut cache, 19, text_entry(key.clone(), "late retained"));
        assert_eq!(cache.get(19, key.bytes()).unwrap().as_str(), "new");
        insert(&mut cache, 42, text_entry(key.clone(), "current"));
        assert_eq!(cache.get(42, key.bytes()).unwrap().as_str(), "current");
        drop(cache.clear());
        insert(&mut cache, 2, text_entry(key.clone(), "new observer"));
        assert_eq!(cache.get(2, key.bytes()).unwrap().as_str(), "new observer");
    }

    #[test]
    fn maintenance_is_newest_lru_first_deduplicated_and_old_get_does_not_reorder() {
        let a = request("alpha");
        let b = request("beta");
        let c = request("gamma");
        let mut cache = PageCache::default();
        insert(&mut cache, 10, text_entry(c.clone(), "old c"));
        insert(&mut cache, 10, text_entry(a.clone(), "old a"));
        insert(&mut cache, 20, text_entry(a.clone(), "new a"));
        insert(&mut cache, 20, text_entry(b.clone(), "new b"));
        cache.get(20, a.bytes()).unwrap();
        cache.get(10, c.bytes()).unwrap();
        let hot = cache.maintenance_requests();
        assert_eq!(
            hot.iter()
                .map(|key| key.request().query.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"]
        );
        assert!(Arc::ptr_eq(&hot[0], &a));
    }

    #[test]
    fn encoded_key_and_payload_share_entry_and_global_budgets() {
        let mut cache = PageCache::default();
        for index in 0..MAX_PAGE_ENTRIES {
            let key = request(&format!("word{index}"));
            let text = "x".repeat(MAX_ENTRY_ENCODED_BYTES - key.key.len() - 2);
            let prepared = PreparedCacheEntry::new(key.clone(), Arc::new(text)).unwrap();
            assert_eq!(prepared.encoded_bytes, MAX_ENTRY_ENCODED_BYTES);
            insert(&mut cache, 11, prepared);
        }
        assert_eq!(cache.entries.len(), MAX_PAGE_ENTRIES);
        assert_eq!(cache.encoded_bytes, MAX_PAGE_ENCODED_BYTES);
        let first = request("word0");
        let last = request("word7");
        cache.get(11, first.bytes()).unwrap();
        insert(&mut cache, 11, text_entry(request("replacement"), "small"));
        assert!(cache.get(11, request("word1").bytes()).is_none());
        assert!(cache.get(11, first.bytes()).is_some());
        let replacement = text_entry(last.clone(), "replaced last");
        let exact_charge = last.key.len() + serde_json::to_vec("replaced last").unwrap().len();
        assert_eq!(replacement.encoded_bytes, exact_charge);
        insert(&mut cache, 11, replacement);
        assert_eq!(
            cache.encoded_bytes,
            cache
                .entries
                .iter()
                .map(|entry| entry.prepared.encoded_bytes)
                .sum::<usize>()
        );
        assert!(cache.encoded_bytes <= MAX_PAGE_ENCODED_BYTES);
        assert!(cache.entries.len() <= MAX_PAGE_ENTRIES);
    }

    #[test]
    fn large_metadata_or_key_declines_optional_admission_without_replacing_data() {
        let key = request("vehicle");
        let mut cache = PageCache::default();
        insert(&mut cache, 1, text_entry(key.clone(), "kept"));
        let large = SearchPage {
            results: vec![AudioFile::from(audiolib_core::AudioFileData {
                attributes: serde_json::json!({"preserved": "x".repeat(MAX_ENTRY_ENCODED_BYTES)}),
                ..Default::default()
            })],
            ..Default::default()
        };
        assert!(PreparedCacheEntry::new(key.clone(), Arc::new(large)).is_none());
        let mut huge_key = SearchRequest::default();
        huge_key.filters.album = Some("x".repeat(MAX_ENTRY_ENCODED_BYTES));
        assert!(huge_key.validate().is_ok());
        assert!(PageRequest::new(&huge_key).is_none());
        assert_eq!(cache.get(1, key.bytes()).unwrap().as_str(), "kept");
        let mut counter = CountWriter {
            bytes: usize::MAX,
            limit: usize::MAX,
        };
        assert!(counter.write(b"x").is_err());
        assert!(encoded_fits("small", key.payload_budget()));
        assert!(!encoded_fits(
            &"x".repeat(MAX_ENTRY_ENCODED_BYTES),
            key.payload_budget()
        ));
        assert!(!encoded_fits("small", 0));
    }

    #[test]
    fn get_does_not_clone_or_serialize_payload_and_provider_stores_are_isolated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counted(Arc<AtomicUsize>);
        impl Serialize for Counted {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.fetch_add(1, Ordering::Relaxed);
                serializer.serialize_str("owned payload")
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let payload = Arc::new(Counted(calls.clone()));
        let key = request("vehicle");
        let prepared = PreparedCacheEntry::new(key.clone(), payload.clone()).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let mut left = PageCache::default();
        let mut right: PageCache<Counted> = PageCache::default();
        insert(&mut left, 31, prepared);
        let found = left.get(31, key.bytes()).unwrap();
        assert!(Arc::ptr_eq(&found, &payload));
        assert!(right.get(31, key.bytes()).is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        drop(left.clear());
        assert!(Arc::ptr_eq(&found, &payload));
    }

    #[test]
    fn replacement_generation_retirement_refusal_and_clear_defer_last_payload_drop() {
        use std::sync::{
            Weak,
            atomic::{AtomicUsize, Ordering},
        };
        #[derive(Serialize)]
        struct Payload {
            value: &'static str,
            #[serde(skip)]
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn prepare(
            key: &Arc<PageRequest>,
            drops: &Arc<AtomicUsize>,
        ) -> (PreparedCacheEntry<Payload>, Weak<Payload>) {
            let page = Arc::new(Payload {
                value: "owned",
                drops: drops.clone(),
            });
            let weak = Arc::downgrade(&page);
            (PreparedCacheEntry::new(key.clone(), page).unwrap(), weak)
        }
        let key = request("vehicle");
        let drops = Arc::new(AtomicUsize::new(0));
        let mut cache = PageCache::default();
        let (first, first_weak) = prepare(&key, &drops);
        assert!(cache.insert(3, first).is_empty());
        let (second, second_weak) = prepare(&key, &drops);
        let replaced = cache.insert(3, second);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(first_weak.upgrade().is_some());
        drop(replaced);
        assert!(first_weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(cache.register_revision(15).is_empty());
        let retired = cache.register_revision(30);
        assert_eq!(cache.encoded_bytes, 0);
        assert!(second_weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(retired);
        assert!(second_weak.upgrade().is_none());
        let (late, late_weak) = prepare(&key, &drops);
        let refused = cache.insert(15, late);
        assert!(late_weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        drop(refused);
        assert!(late_weak.upgrade().is_none());
        let (last, last_weak) = prepare(&key, &drops);
        assert!(cache.insert(30, last).is_empty());
        let old_storage = cache.clear();
        assert_eq!(cache.encoded_bytes, 0);
        assert!(cache.entries.is_empty());
        assert!(last_weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::Relaxed), 3);
        drop(old_storage);
        assert!(last_weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 4);
    }
}
