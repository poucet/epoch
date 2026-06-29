//! `epoch-cas-moka` — a fixed-size, byte-weighted in-memory cache backend.
//!
//! [`MokaByteBackend`] is a [`SyncByteBackend`](epoch_cas::SyncByteBackend)
//! backed by a [`moka`] cache with a total-size budget: each entry weighs its
//! byte length, and the cache evicts (W-TinyLFU) once the budget is exceeded. It
//! is the eviction-policy half of a read-through cache — drop it into the cache
//! slot of an [`epoch_cas::CachingByteBackend`]:
//!
//! ```text
//! TieredByteBackend< staging, CachingByteBackend< MokaByteBackend, storage > >
//! ```
//!
//! Eviction here is always safe: this backend only ever sits in front of a
//! durable source of truth, so a dropped entry is simply re-fetched. It must
//! **never** be used as the writable staging tier, where an evicted un-promoted
//! object would be lost.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::hash::Hash;

use epoch_cas::{Bytes, CASError, CASHash, SyncByteBackend};
use moka::sync::Cache;

/// A byte-weighted, fixed-budget in-memory cache
/// [`SyncByteBackend`](epoch_cas::SyncByteBackend), generic over the key type
/// `K` (defaulting to the kernel's [`CASHash`]).
///
/// Cheaply clonable — clones share the same underlying cache (the moka cache is
/// itself `Arc`-backed), matching the other backends.
pub struct MokaByteBackend<K = CASHash>
where
    K: Hash + Eq + Send + Sync + 'static,
{
    cache: Cache<K, Bytes>,
}

impl<K> Clone for MokaByteBackend<K>
where
    K: Hash + Eq + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self { cache: self.cache.clone() }
    }
}

impl<K> MokaByteBackend<K>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
{
    /// A cache that holds up to `max_bytes` of stored value bytes before it
    /// starts evicting.
    pub fn new(max_bytes: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|_k: &K, v: &Bytes| u32::try_from(v.len()).unwrap_or(u32::MAX))
            .build();
        Self { cache }
    }

    /// The number of cached entries (after pending maintenance — see
    /// [`run_pending_tasks`](Self::run_pending_tasks)).
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// The total weight (byte length) of cached entries.
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Force any pending cache maintenance (insertions/evictions) to run now.
    /// moka applies eviction lazily; this makes [`entry_count`](Self::entry_count)
    /// and [`weighted_size`](Self::weighted_size) reflect the latest state — used
    /// in tests, rarely needed otherwise.
    pub fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks();
    }
}

impl<K> SyncByteBackend<K> for MokaByteBackend<K>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<Bytes>, CASError> {
        Ok(self.cache.get(key))
    }

    fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError> {
        self.cache.insert(key.clone(), bytes);
        Ok(())
    }

    fn has(&self, key: &K) -> Result<bool, CASError> {
        Ok(self.cache.contains_key(key))
    }

    fn delete(&self, key: &K) -> Result<(), CASError> {
        self.cache.invalidate(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> CASHash {
        CASHash::from_bytes([n; 32])
    }

    #[test]
    fn put_get_has_delete_roundtrip() {
        let b: MokaByteBackend = MokaByteBackend::new(1 << 20);
        assert!(!b.has(&key(1)).unwrap());
        b.put(&key(1), Bytes::from_static(b"v")).unwrap();
        assert!(b.has(&key(1)).unwrap());
        assert_eq!(b.get(&key(1)).unwrap(), Some(Bytes::from_static(b"v")));
        b.delete(&key(1)).unwrap();
        b.run_pending_tasks();
        assert!(!b.has(&key(1)).unwrap());
        assert_eq!(b.get(&key(1)).unwrap(), None);
    }

    #[test]
    fn evicts_once_the_byte_budget_is_exceeded() {
        // Budget for ~3 entries of 30 bytes each; insert 10.
        let b: MokaByteBackend = MokaByteBackend::new(100);
        let blob = Bytes::from(vec![0u8; 30]);
        for n in 0..10u8 {
            b.put(&key(n), blob.clone()).unwrap();
        }
        b.run_pending_tasks();

        assert!(b.weighted_size() <= 100, "stays within the byte budget");
        assert!(b.entry_count() < 10, "older entries were evicted");
    }
}
