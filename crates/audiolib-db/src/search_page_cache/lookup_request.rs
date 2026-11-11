// crates/audiolib-db/src/search_page_cache/lookup_request.rs
//! Borrowed first-page lookup identity; ownership is acquired only on admission.
//! Exact bytes and encoded limits match the owned cache identity; no SQL occurs here.
use super::{MAX_ENTRY_ENCODED_BYTES, PageRequest};
use audiolib_core::SearchRequest;
use std::{
    io::{self, Write},
    sync::Arc,
};

const INLINE_BYTES: usize = 2048;

/// A caller-owned normalized request and its exact, bounded serde lookup bytes.
/// Common lookups allocate no key Vec, request clone, or owned-key Arc.
pub(crate) struct LookupRequest<'a> {
    request: &'a SearchRequest,
    key: KeyBytes,
}

impl<'a> LookupRequest<'a> {
    /// Same eligibility and validation as the current owned PageRequest builder.
    /// Serialization occurs before any cache mutex is acquired.
    pub(crate) fn new(request: &'a SearchRequest) -> Option<Self> {
        if request.cursor.is_some()
            || request.page.is_some()
            || request.offset != 0
            || !(1..=200).contains(&request.limit)
            || request.validate().is_err()
        {
            return None;
        }
        let mut key = KeyBytes::default();
        serde_json::to_writer(&mut key, request).ok()?;
        Some(Self { request, key })
    }

    /// Exact serialized bytes; PageCache's equality/LRU lookup uses this slice.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.key.bytes()
    }

    /// Existing per-entry encoded budget after charging these request bytes.
    pub(crate) fn payload_budget(&self) -> usize {
        MAX_ENTRY_ENCODED_BYTES - self.bytes().len()
    }

    /// Call only after a stable, cacheable miss passes borrowed payload sizing.
    /// Spilled storage is moved intact; inline bytes are copied once to an owned
    /// Vec. No second serialization is performed. Request cloning stays off-lock.
    pub(crate) fn into_owned(self) -> Option<Arc<PageRequest>> {
        let key = match self.key {
            KeyBytes::Inline { bytes, used } => {
                let mut key = Vec::new();
                key.try_reserve_exact(used).ok()?;
                key.extend_from_slice(&bytes[..used]);
                key
            }
            KeyBytes::Heap(key) => key,
        };
        Some(Arc::new(PageRequest {
            request: self.request.clone(),
            key,
        }))
    }
}

// Deliberate bounded stack storage keeps the common lookup allocation-free.
#[allow(clippy::large_enum_variant)]
enum KeyBytes {
    Inline {
        bytes: [u8; INLINE_BYTES],
        used: usize,
    },
    Heap(Vec<u8>),
}

impl Default for KeyBytes {
    fn default() -> Self {
        Self::Inline {
            bytes: [0; INLINE_BYTES],
            used: 0,
        }
    }
}

impl KeyBytes {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Inline { bytes, used } => &bytes[..*used],
            Self::Heap(bytes) => bytes,
        }
    }
}

impl Write for KeyBytes {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes()
            .len()
            .checked_add(input.len())
            .filter(|length| *length <= MAX_ENTRY_ENCODED_BYTES)
            .ok_or_else(|| io::Error::other("First-page key exceeds encoded budget"))?;
        match self {
            Self::Inline { bytes, used } if next <= INLINE_BYTES => {
                bytes[*used..next].copy_from_slice(input);
                *used = next;
            }
            Self::Inline { bytes, used } => {
                let mut heap = Vec::new();
                let capacity = next.clamp(INLINE_BYTES * 2, MAX_ENTRY_ENCODED_BYTES);
                heap.try_reserve_exact(capacity)
                    .map_err(|_| io::Error::other("First-page spill allocation unavailable"))?;
                heap.extend_from_slice(&bytes[..*used]);
                heap.extend_from_slice(input);
                *self = Self::Heap(heap);
            }
            Self::Heap(bytes) => {
                // Escaped strings can arrive in tiny fragments. Grow within
                // the cap geometrically rather than reallocating per fragment.
                if next > bytes.capacity() {
                    let capacity = bytes
                        .capacity()
                        .saturating_mul(2)
                        .max(next)
                        .min(MAX_ENTRY_ENCODED_BYTES);
                    bytes
                        .try_reserve_exact(capacity - bytes.len())
                        .map_err(|_| io::Error::other("First-page key allocation unavailable"))?;
                }
                bytes.extend_from_slice(input);
            }
        }
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PageCache, PreparedCacheEntry};
    use super::*;
    use audiolib_core::SearchFilters;

    fn request_at_bytes(target: usize) -> SearchRequest {
        let mut request = SearchRequest {
            query: "风 \"quoted\" \\ folder".into(),
            filters: SearchFilters {
                album: Some("中文\"\\".into()),
                tag_contains: Some("tag\"\\".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let prefix = serde_json::to_vec(&request).unwrap().len();
        assert!(target >= prefix);
        request
            .filters
            .album
            .as_mut()
            .unwrap()
            .push_str(&"x".repeat(target - prefix));
        assert_eq!(serde_json::to_vec(&request).unwrap().len(), target);
        request
    }

    #[test]
    fn exact_serde_unicode_and_escaping_match_at_2047_2048_2049_bytes() {
        for size in [2047, 2048, 2049] {
            let request = request_at_bytes(size);
            let expected = serde_json::to_vec(&request).unwrap();
            let lookup = LookupRequest::new(&request).unwrap();
            assert_eq!(lookup.bytes(), expected);
            assert_eq!(matches!(&lookup.key, KeyBytes::Inline { .. }), size <= 2048);
            assert_eq!(lookup.payload_budget(), MAX_ENTRY_ENCODED_BYTES - size);
            let owned = lookup.into_owned().unwrap();
            assert_eq!(owned.key, expected);
            assert_eq!(
                serde_json::to_value(owned.request()).unwrap(),
                serde_json::to_value(&request).unwrap()
            );
        }
    }

    #[test]
    fn spill_and_cap_are_bounded_and_failed_writes_do_not_change_existing_bytes() {
        let mut writer = KeyBytes::default();
        writer.write_all(&[b'x'; 2048]).unwrap();
        assert!(matches!(&writer, KeyBytes::Inline { .. }));
        writer.write_all(b"y").unwrap();
        assert!(matches!(&writer, KeyBytes::Heap(_)));
        assert_eq!(writer.bytes()[2047..], [b'x', b'y']);
        let mut unicode_boundary = KeyBytes::default();
        unicode_boundary.write_all(&[b'x'; 2047]).unwrap();
        unicode_boundary.write_all("风".as_bytes()).unwrap();
        assert_eq!(&unicode_boundary.bytes()[2047..], "风".as_bytes());
        let before = writer.bytes().to_vec();
        assert!(
            writer
                .write_all(&vec![b'z'; MAX_ENTRY_ENCODED_BYTES])
                .is_err()
        );
        assert_eq!(writer.bytes(), before);
        let boundary = request_at_bytes(MAX_ENTRY_ENCODED_BYTES);
        assert_eq!(
            LookupRequest::new(&boundary).unwrap().bytes().len(),
            MAX_ENTRY_ENCODED_BYTES
        );
        let too_large = request_at_bytes(MAX_ENTRY_ENCODED_BYTES + 1);
        assert!(too_large.validate().is_ok());
        assert!(LookupRequest::new(&too_large).is_none());
    }

    #[test]
    fn accepted_owned_binding_survives_borrow_end_and_reuses_the_spilled_allocation() {
        let (owned, expected) = {
            let request = request_at_bytes(2049);
            let expected = serde_json::to_vec(&request).unwrap();
            let lookup = LookupRequest::new(&request).unwrap();
            let spill_pointer = lookup.bytes().as_ptr();
            let owned = lookup.into_owned().unwrap();
            assert_eq!(spill_pointer, owned.key.as_ptr());
            (owned, expected)
        };
        assert_eq!(owned.key, expected);
        assert_eq!(serde_json::to_vec(owned.request()).unwrap(), expected);
    }

    #[test]
    fn byte_lookup_preserves_filter_isolation_retained_generation_and_late_insert_rules() {
        let request = request_at_bytes(2047);
        let lookup = LookupRequest::new(&request).unwrap();
        let owned = LookupRequest::new(&request).unwrap().into_owned().unwrap();
        let mut changed = request.clone();
        changed.filters.favorites_only = true;
        let changed_lookup = LookupRequest::new(&changed).unwrap();
        assert_ne!(lookup.bytes(), changed_lookup.bytes());
        let mut cache = PageCache::default();
        drop(cache.insert(
            5,
            PreparedCacheEntry::new(owned.clone(), Arc::new("old".to_string())).unwrap(),
        ));
        drop(cache.insert(
            23,
            PreparedCacheEntry::new(owned.clone(), Arc::new("new".to_string())).unwrap(),
        ));
        assert_eq!(cache.get(5, lookup.bytes()).unwrap().as_str(), "old");
        assert_eq!(cache.get(23, lookup.bytes()).unwrap().as_str(), "new");
        assert!(cache.get(23, changed_lookup.bytes()).is_none());
        drop(cache.insert(
            5,
            PreparedCacheEntry::new(owned, Arc::new("late".to_string())).unwrap(),
        ));
        assert_eq!(cache.get(23, lookup.bytes()).unwrap().as_str(), "new");
    }
}
