//! The opaque byte backend (D4, D14): a dumb content-keyed KV with **zero** CAS
//! semantics. It never hashes, compresses, or parses — it just stores and
//! returns bytes keyed by an opaque key `K` (the kernel uses [`CASHash`]).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;

use crate::error::CASError;
use crate::hash::CASHash;

/// A synchronous opaque content-keyed KV, generic over the key type `K`
/// (defaulting to the kernel's [`CASHash`]). This is what `redb` implements.
///
/// Semantics-free: the key is just an opaque token to it; the engine
/// ([`crate::generic`]) is the only thing that knows the key is a content hash.
pub trait SyncByteBackend<K = CASHash>: Send + Sync + 'static {
    /// Fetch the bytes stored under `key`, if any.
    fn get(&self, key: &K) -> Result<Option<Bytes>, CASError>;
    /// Store `bytes` under `key` (idempotent for content addressing).
    fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError>;
    /// Whether `key` is present.
    fn has(&self, key: &K) -> Result<bool, CASError>;
    /// Remove `key` (no error if absent).
    fn delete(&self, key: &K) -> Result<(), CASError>;
}

/// The async twin of [`SyncByteBackend`] for network backends (S3/R2/Redis),
/// likewise generic over the key type `K` (defaulting to [`CASHash`]).
#[async_trait]
pub trait AsyncByteBackend<K = CASHash>: Send + Sync + 'static {
    /// Fetch the bytes stored under `key`, if any.
    async fn get(&self, key: &K) -> Result<Option<Bytes>, CASError>;
    /// Store `bytes` under `key`.
    async fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError>;
    /// Whether `key` is present.
    async fn has(&self, key: &K) -> Result<bool, CASError>;
    /// Remove `key`.
    async fn delete(&self, key: &K) -> Result<(), CASError>;
}

/// An in-memory [`SyncByteBackend`] backed by a `Mutex<HashMap<K, Bytes>>`,
/// generic over the key type `K` (defaulting to the kernel's [`CASHash`]).
///
/// The kernel's reference backend: used for the kernel's own tests and as a
/// drop-in for anything that needs a real CAS without persistence.
///
/// Cheaply clonable — clones share the same underlying map (handy for tests
/// that want to inspect or corrupt entries after handing the backend to a CAS).
pub struct MemoryByteBackend<K = CASHash> {
    map: Arc<Mutex<HashMap<K, Bytes>>>,
}

// `Clone`/`Default` are written by hand rather than derived: deriving would
// over-constrain `K` to `Clone`/`Default`, but the shared `Arc` clones and the
// empty map default-constructs regardless of the key type.
impl<K> Clone for MemoryByteBackend<K> {
    fn clone(&self) -> Self {
        Self {
            map: Arc::clone(&self.map),
        }
    }
}

impl<K> Default for MemoryByteBackend<K> {
    fn default() -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K> MemoryByteBackend<K> {
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
}

impl<K> MemoryByteBackend<K>
where
    K: Eq + Hash + Clone,
{
    /// Overwrite the raw bytes under `key` without any integrity check. Used in
    /// tests to corrupt a stored entry and exercise the read-side verifier.
    pub fn corrupt(&self, key: &K, bytes: Bytes) {
        self.map.lock().insert(key.clone(), bytes);
    }
}

impl<K> SyncByteBackend<K> for MemoryByteBackend<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<Bytes>, CASError> {
        Ok(self.map.lock().get(key).cloned())
    }

    fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError> {
        self.map.lock().insert(key.clone(), bytes);
        Ok(())
    }

    fn has(&self, key: &K) -> Result<bool, CASError> {
        Ok(self.map.lock().contains_key(key))
    }

    fn delete(&self, key: &K) -> Result<(), CASError> {
        self.map.lock().remove(key);
        Ok(())
    }
}

/// An async adapter over any [`MemoryByteBackend`], driving it inline (the map
/// is non-blocking) so the async surface is available without a runtime.
///
/// This is primarily here so async-side tests and the [`crate::batching`]
/// decorator can talk to the in-memory backend.
pub struct AsyncMemoryByteBackend<K = CASHash> {
    inner: MemoryByteBackend<K>,
}

impl<K> Clone for AsyncMemoryByteBackend<K> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K> Default for AsyncMemoryByteBackend<K> {
    fn default() -> Self {
        Self {
            inner: MemoryByteBackend::default(),
        }
    }
}

impl<K> AsyncMemoryByteBackend<K> {
    /// A fresh, empty async-facing backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// The underlying sync backend (shares storage).
    pub fn sync(&self) -> &MemoryByteBackend<K> {
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
impl<K> AsyncByteBackend<K> for AsyncMemoryByteBackend<K>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    async fn get(&self, key: &K) -> Result<Option<Bytes>, CASError> {
        self.inner.get(key)
    }

    async fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError> {
        self.inner.put(key, bytes)
    }

    async fn has(&self, key: &K) -> Result<bool, CASError> {
        self.inner.has(key)
    }

    async fn delete(&self, key: &K) -> Result<(), CASError> {
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete() {
        let b = MemoryByteBackend::new();
        let k = CASHash::from_bytes([1u8; 32]);
        assert!(!b.has(&k).unwrap());
        b.put(&k, Bytes::from_static(b"v")).unwrap();
        assert!(b.has(&k).unwrap());
        assert_eq!(b.get(&k).unwrap(), Some(Bytes::from_static(b"v")));
        b.delete(&k).unwrap();
        assert!(!b.has(&k).unwrap());
        assert_eq!(b.get(&k).unwrap(), None);
    }

    // The key is a free type parameter: a backend keyed by something other than
    // the kernel's `CASHash` works just the same.
    #[test]
    fn key_type_is_generic() {
        let b: MemoryByteBackend<String> = MemoryByteBackend::new();
        let k = "key".to_string();
        b.put(&k, Bytes::from_static(b"v")).unwrap();
        assert!(b.has(&k).unwrap());
        assert_eq!(b.get(&k).unwrap(), Some(Bytes::from_static(b"v")));
    }
}
