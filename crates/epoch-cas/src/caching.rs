//! [`CachingByteBackend`] — a read-through cache in front of an inner byte
//! store.
//!
//! Wraps an `inner` store (the source of truth — e.g. durable storage) with a
//! `cache` store (e.g. a fixed-size moka LRU). Reads are served from the cache
//! and **populate it on a miss**; writes go through to `inner` and warm the
//! cache. Crucially, **eviction is always safe**: the cache is never the only
//! copy of anything, so a fixed-size LRU may drop entries freely and they are
//! simply re-fetched from `inner`.
//!
//! This is the cache that belongs on the **durable** side of a staging tier —
//! the canonical layering is
//!
//! ```text
//! TieredByteBackend< in-memory staging, CachingByteBackend< moka LRU, storage > >
//! ```
//!
//! so that the writable staging tier (un-promoted writes, the only copy) stays
//! non-evictable, while the cache evicts only objects that are already durable.
//! See [`TieredByteBackend`](crate::TieredByteBackend) for the staging half and
//! the storage invariant the pair enforces.
//!
//! The cache is itself just a [`SyncByteBackend`], so the eviction policy lives
//! inside *it* (a moka-backed backend, a bounded map, …); this combinator only
//! orchestrates population and write-through.

use crate::backend::SyncByteBackend;
use crate::error::CASError;
use bytes::Bytes;

/// A read-through cache (`cache`) in front of a source-of-truth store (`inner`).
/// See the [module docs](self) for semantics and the intended layering.
///
/// The key type is whatever the two tiers share (the kernel's `CASHash` in
/// practice); it lives on the [`SyncByteBackend`] impl, not as a struct
/// parameter.
#[derive(Clone)]
pub struct CachingByteBackend<Cache, Inner> {
    cache: Cache,
    inner: Inner,
}

impl<Cache, Inner> CachingByteBackend<Cache, Inner> {
    /// Front the source-of-truth `inner` with a read-through `cache`.
    pub fn new(cache: Cache, inner: Inner) -> Self {
        Self { cache, inner }
    }

    /// The cache tier (e.g. the LRU).
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// The inner (source-of-truth) tier.
    pub fn inner(&self) -> &Inner {
        &self.inner
    }
}

impl<Cache, Inner, K> SyncByteBackend<K> for CachingByteBackend<Cache, Inner>
where
    Cache: SyncByteBackend<K>,
    Inner: SyncByteBackend<K>,
    K: Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<Bytes>, CASError> {
        if let Some(bytes) = self.cache.get(key)? {
            return Ok(Some(bytes));
        }
        // Miss: fetch from the source of truth and populate the cache. Safe to
        // evict later — `inner` remains the source of truth.
        match self.inner.get(key)? {
            Some(bytes) => {
                self.cache.put(key, bytes.clone())?;
                Ok(Some(bytes))
            }
            None => Ok(None),
        }
    }

    fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError> {
        // Write-through: the durable tier is the source of truth, so write it
        // first; then warm the cache (a just-written object is likely hot).
        self.inner.put(key, bytes.clone())?;
        self.cache.put(key, bytes)
    }

    fn has(&self, key: &K) -> Result<bool, CASError> {
        Ok(self.cache.has(key)? || self.inner.has(key)?)
    }

    fn delete(&self, key: &K) -> Result<(), CASError> {
        // Drop from both tiers so a stale cache entry can't outlive a deletion.
        self.cache.delete(key)?;
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryByteBackend;
    use crate::hash::CASHash;
    use crate::TieredByteBackend;

    fn key(n: u8) -> CASHash {
        CASHash::from_bytes([n; 32])
    }

    type Caching = CachingByteBackend<MemoryByteBackend, MemoryByteBackend>;

    #[test]
    fn get_miss_populates_the_cache() {
        let cache = MemoryByteBackend::new();
        let inner = MemoryByteBackend::new();
        inner.put(&key(1), Bytes::from_static(b"v")).unwrap();
        let backend = Caching::new(cache.clone(), inner.clone());

        assert!(!cache.has(&key(1)).unwrap());
        assert_eq!(backend.get(&key(1)).unwrap(), Some(Bytes::from_static(b"v")));
        assert!(cache.has(&key(1)).unwrap(), "a miss populates the cache");

        // Now served from the cache even if `inner` loses it (proving the read
        // came from the cache, not inner).
        inner.delete(&key(1)).unwrap();
        assert_eq!(backend.get(&key(1)).unwrap(), Some(Bytes::from_static(b"v")));
    }

    #[test]
    fn put_writes_through_to_inner_and_warms_cache() {
        let cache = MemoryByteBackend::new();
        let inner = MemoryByteBackend::new();
        let backend = Caching::new(cache.clone(), inner.clone());

        backend.put(&key(2), Bytes::from_static(b"w")).unwrap();
        assert_eq!(inner.get(&key(2)).unwrap(), Some(Bytes::from_static(b"w")), "write-through");
        assert_eq!(cache.get(&key(2)).unwrap(), Some(Bytes::from_static(b"w")), "cache warmed");
    }

    #[test]
    fn cache_serves_reads_without_touching_inner() {
        let cache = MemoryByteBackend::new();
        let inner = MemoryByteBackend::new();
        // Divergent values to prove which tier answered.
        cache.put(&key(3), Bytes::from_static(b"cached")).unwrap();
        inner.put(&key(3), Bytes::from_static(b"stale")).unwrap();
        let backend = Caching::new(cache, inner);
        assert_eq!(backend.get(&key(3)).unwrap(), Some(Bytes::from_static(b"cached")));
    }

    #[test]
    fn has_falls_through_to_inner() {
        let cache = MemoryByteBackend::new();
        let inner = MemoryByteBackend::new();
        inner.put(&key(4), Bytes::from_static(b"v")).unwrap();
        let backend = Caching::new(cache, inner);
        assert!(backend.has(&key(4)).unwrap());
    }

    #[test]
    fn delete_drops_from_both_tiers() {
        let cache = MemoryByteBackend::new();
        let inner = MemoryByteBackend::new();
        let backend = Caching::new(cache.clone(), inner.clone());
        backend.put(&key(5), Bytes::from_static(b"v")).unwrap();

        backend.delete(&key(5)).unwrap();
        assert!(!cache.has(&key(5)).unwrap(), "cache entry dropped");
        assert!(!inner.has(&key(5)).unwrap(), "durable entry dropped");
    }

    /// The canonical stack: staging in front of a cached durable tier. A write
    /// stays in staging (reaching neither cache nor storage); promotion writes
    /// through the cache to storage and warms the cache.
    #[test]
    fn staging_over_cached_storage_layering() {
        let cache = MemoryByteBackend::new();
        let storage = MemoryByteBackend::new();
        let far = CachingByteBackend::new(cache.clone(), storage.clone());
        let staging = MemoryByteBackend::new();
        let stack = TieredByteBackend::new(staging.clone(), far);

        // A write lands in staging only — not durable, not cached.
        stack.put(&key(6), Bytes::from_static(b"x")).unwrap();
        assert!(staging.has(&key(6)).unwrap());
        assert!(!storage.has(&key(6)).unwrap(), "not durable before promotion");
        assert!(!cache.has(&key(6)).unwrap(), "not cached before promotion");

        // Promotion writes through to storage and warms the cache.
        stack.promote(&key(6)).unwrap();
        assert!(storage.has(&key(6)).unwrap(), "promotion makes it durable");
        assert!(cache.has(&key(6)).unwrap(), "promotion warms the cache");

        // A fresh stack with empty staging still reads it — through the cache.
        let fresh = TieredByteBackend::new(
            MemoryByteBackend::new(),
            CachingByteBackend::new(cache, storage),
        );
        assert_eq!(fresh.get(&key(6)).unwrap(), Some(Bytes::from_static(b"x")));
    }
}
