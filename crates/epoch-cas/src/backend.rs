//! The opaque byte backend (D4, D14): a dumb content-keyed KV with **zero** CAS
//! semantics. It never hashes, compresses, or parses — it just stores and
//! returns bytes keyed by [`Hash`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::error::CASError;
use crate::hash::Hash;

/// A synchronous opaque content-keyed KV. This is what `redb` implements.
///
/// Semantics-free: the key is just bytes to it; the engine ([`crate::generic`])
/// is the only thing that knows the key is a content hash.
pub trait SyncByteBackend: Send + Sync + 'static {
    /// Fetch the bytes stored under `key`, if any.
    fn get(&self, key: &Hash) -> Result<Option<Bytes>, CASError>;
    /// Store `bytes` under `key` (idempotent for content addressing).
    fn put(&self, key: &Hash, bytes: Bytes) -> Result<(), CASError>;
    /// Whether `key` is present.
    fn has(&self, key: &Hash) -> Result<bool, CASError>;
    /// Remove `key` (no error if absent).
    fn delete(&self, key: &Hash) -> Result<(), CASError>;
}

/// The async twin of [`SyncByteBackend`] for network backends (S3/R2/Redis).
#[async_trait]
pub trait AsyncByteBackend: Send + Sync + 'static {
    /// Fetch the bytes stored under `key`, if any.
    async fn get(&self, key: &Hash) -> Result<Option<Bytes>, CASError>;
    /// Store `bytes` under `key`.
    async fn put(&self, key: &Hash, bytes: Bytes) -> Result<(), CASError>;
    /// Whether `key` is present.
    async fn has(&self, key: &Hash) -> Result<bool, CASError>;
    /// Remove `key`.
    async fn delete(&self, key: &Hash) -> Result<(), CASError>;
}

/// An in-memory [`SyncByteBackend`] backed by a `Mutex<HashMap<Hash, Bytes>>`.
///
/// The kernel's reference backend: used for the kernel's own tests and as a
/// drop-in for anything that needs a real CAS without persistence.
///
/// Cheaply clonable — clones share the same underlying map (handy for tests
/// that want to inspect or corrupt entries after handing the backend to a CAS).
#[derive(Clone, Default)]
pub struct MemoryByteBackend {
    map: Arc<Mutex<HashMap<Hash, Bytes>>>,
}

impl MemoryByteBackend {
    /// A fresh, empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries (test helper).
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    /// Whether the backend holds no entries.
    pub fn is_empty(&self) -> bool {
        self.map.lock().is_empty()
    }

    /// Overwrite the raw bytes under `key` without any integrity check. Used in
    /// tests to corrupt a stored entry and exercise the read-side verifier.
    pub fn corrupt(&self, key: &Hash, bytes: Bytes) {
        self.map.lock().insert(*key, bytes);
    }
}

impl SyncByteBackend for MemoryByteBackend {
    fn get(&self, key: &Hash) -> Result<Option<Bytes>, CASError> {
        Ok(self.map.lock().get(key).cloned())
    }

    fn put(&self, key: &Hash, bytes: Bytes) -> Result<(), CASError> {
        self.map.lock().insert(*key, bytes);
        Ok(())
    }

    fn has(&self, key: &Hash) -> Result<bool, CASError> {
        Ok(self.map.lock().contains_key(key))
    }

    fn delete(&self, key: &Hash) -> Result<(), CASError> {
        self.map.lock().remove(key);
        Ok(())
    }
}

/// An async adapter over any [`SyncByteBackend`], driving it inline (the map is
/// non-blocking) so the async surface is available without a runtime.
///
/// This is primarily here so async-side tests and the [`crate::batching`]
/// decorator can talk to the in-memory backend.
#[derive(Clone, Default)]
pub struct AsyncMemoryByteBackend {
    inner: MemoryByteBackend,
}

impl AsyncMemoryByteBackend {
    /// A fresh, empty async-facing backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// The underlying sync backend (shares storage).
    pub fn sync(&self) -> &MemoryByteBackend {
        &self.inner
    }

    /// Number of stored entries (test helper).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the backend holds no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[async_trait]
impl AsyncByteBackend for AsyncMemoryByteBackend {
    async fn get(&self, key: &Hash) -> Result<Option<Bytes>, CASError> {
        self.inner.get(key)
    }

    async fn put(&self, key: &Hash, bytes: Bytes) -> Result<(), CASError> {
        self.inner.put(key, bytes)
    }

    async fn has(&self, key: &Hash) -> Result<bool, CASError> {
        self.inner.has(key)
    }

    async fn delete(&self, key: &Hash) -> Result<(), CASError> {
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let b = MemoryByteBackend::new();
        let k = Hash::of(b"k");
        assert!(!b.has(&k).unwrap());
        b.put(&k, Bytes::from_static(b"v")).unwrap();
        assert!(b.has(&k).unwrap());
        assert_eq!(b.get(&k).unwrap(), Some(Bytes::from_static(b"v")));
        b.delete(&k).unwrap();
        assert!(!b.has(&k).unwrap());
        assert_eq!(b.get(&k).unwrap(), None);
    }
}
