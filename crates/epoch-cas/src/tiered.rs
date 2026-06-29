//! [`TieredByteBackend`] — a two-tier byte store with read-through reads and
//! **promote-only** writes to the durable tier.
//!
//! Given a writable `near` tier (e.g. an in-memory [`MemoryByteBackend`]) and a
//! `far` tier (e.g. durable storage), it presents a single [`SyncByteBackend`]
//! whose:
//!
//! - `get` / `has` **fall through** near → far, and
//! - `put` lands in `near` **only** — never write-through.
//!
//! The sole path by which an object reaches the durable tier is an explicit
//! [`promote`](TieredByteBackend::promote). This is the mechanism behind the
//! storage invariant *"the durable CAS holds exactly the transitive closure of
//! checkpointed roots"*: the working tree writes freely into `near`, and a
//! checkpoint promotes the reachable closure into `far`. Nothing else is durable.
//!
//! The combinator is content-agnostic — it speaks raw keys and opaque bytes, so
//! it serves tree, blob, and chunk objects uniformly. Deciding *which* keys make
//! up a checkpoint (walking a root's closure, decoding trees and chunked blobs)
//! is the job of a layer above; this type only moves one key on request.
//!
//! It composes: `Tiered<Mem, Tiered<Ssd, Remote>>` is a three-tier read
//! fall-through, and `promote` moves one tier down at a time.

use crate::backend::SyncByteBackend;
use crate::error::CASError;
use bytes::Bytes;

/// A two-tier content-keyed byte store: a writable `near` tier in front of a
/// promote-only `far` tier. Reads fall through; writes never do. See the
/// [module docs](self) for the semantics and the storage invariant it enforces.
///
/// The key type is whatever the two tiers share (the kernel's `CASHash` in
/// practice); it lives on the [`SyncByteBackend`] impl and on
/// [`promote`](Self::promote), not as a struct parameter.
///
/// `Clone` shares the same underlying tiers (both backends are cheap,
/// `Arc`-backed clones), so a clone handed to a `FileCas`/`TreeStore` and a clone
/// kept by the checkpoint layer to call `promote` operate on the same storage.
#[derive(Clone)]
pub struct TieredByteBackend<Near, Far> {
    near: Near,
    far: Far,
}

impl<Near, Far> TieredByteBackend<Near, Far> {
    /// Pair a writable `near` tier with a promote-only `far` tier.
    pub fn new(near: Near, far: Far) -> Self {
        Self { near, far }
    }

    /// The near (writable / staging) tier.
    pub fn near(&self) -> &Near {
        &self.near
    }

    /// The far (durable / promote-only) tier.
    pub fn far(&self) -> &Far {
        &self.far
    }

    /// Copy the object under `key` from `near` to `far` — the **only** path by
    /// which anything becomes durable.
    ///
    /// Idempotent and incremental: a no-op if `far` already holds `key` (so a
    /// closure walk prunes at already-promoted subtrees), and a no-op if `near`
    /// does not hold it (already promoted, or never staged here).
    pub fn promote<K>(&self, key: &K) -> Result<(), CASError>
    where
        Near: SyncByteBackend<K>,
        Far: SyncByteBackend<K>,
    {
        if self.far.has(key)? {
            return Ok(());
        }
        if let Some(bytes) = self.near.get(key)? {
            self.far.put(key, bytes)?;
        }
        Ok(())
    }
}

impl<Near, Far, K> SyncByteBackend<K> for TieredByteBackend<Near, Far>
where
    Near: SyncByteBackend<K>,
    Far: SyncByteBackend<K>,
    K: Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<Bytes>, CASError> {
        match self.near.get(key)? {
            Some(bytes) => Ok(Some(bytes)),
            None => self.far.get(key), // read-through
        }
    }

    fn put(&self, key: &K, bytes: Bytes) -> Result<(), CASError> {
        // Near only — never write-through. `promote` is the path to `far`.
        self.near.put(key, bytes)
    }

    fn has(&self, key: &K) -> Result<bool, CASError> {
        Ok(self.near.has(key)? || self.far.has(key)?)
    }

    fn delete(&self, key: &K) -> Result<(), CASError> {
        // Un-stage from `near`. Dropping a durable object is a retention/GC
        // operation that belongs to a higher layer, not this fall-through.
        self.near.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryByteBackend;
    use crate::hash::CASHash;

    fn key(n: u8) -> CASHash {
        CASHash::from_bytes([n; 32])
    }

    type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

    #[test]
    fn put_writes_near_only_never_far() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        let tiered = Tiered::new(near.clone(), far.clone());

        tiered.put(&key(1), Bytes::from_static(b"v")).unwrap();

        assert!(near.has(&key(1)).unwrap(), "put lands in near");
        assert!(!far.has(&key(1)).unwrap(), "put must NOT write through to far");
        // Visible through the tier all the same.
        assert!(tiered.has(&key(1)).unwrap());
        assert_eq!(tiered.get(&key(1)).unwrap(), Some(Bytes::from_static(b"v")));
    }

    #[test]
    fn get_and_has_fall_through_to_far() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        // Seed `far` directly (as if a prior checkpoint had promoted it).
        far.put(&key(2), Bytes::from_static(b"durable")).unwrap();
        let tiered = Tiered::new(near.clone(), far);

        assert!(tiered.has(&key(2)).unwrap(), "has falls through to far");
        assert_eq!(
            tiered.get(&key(2)).unwrap(),
            Some(Bytes::from_static(b"durable")),
            "get falls through to far"
        );
        // Read-through does not populate near (the tier stays a pure overlay).
        assert!(!near.has(&key(2)).unwrap(), "read-through must not copy far -> near");
    }

    #[test]
    fn near_shadows_far() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        far.put(&key(3), Bytes::from_static(b"old")).unwrap();
        near.put(&key(3), Bytes::from_static(b"new")).unwrap();
        let tiered = Tiered::new(near, far);
        assert_eq!(tiered.get(&key(3)).unwrap(), Some(Bytes::from_static(b"new")));
    }

    #[test]
    fn promote_is_the_only_path_to_far() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        let tiered = Tiered::new(near, far.clone());

        tiered.put(&key(4), Bytes::from_static(b"x")).unwrap();
        assert!(!far.has(&key(4)).unwrap());

        tiered.promote(&key(4)).unwrap();
        assert_eq!(
            far.get(&key(4)).unwrap(),
            Some(Bytes::from_static(b"x")),
            "promote copies near -> far"
        );
    }

    #[test]
    fn promote_is_idempotent_and_prunes_already_durable() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        // Already durable with a particular value; near has nothing for this key.
        far.put(&key(5), Bytes::from_static(b"durable")).unwrap();
        let tiered = Tiered::new(near, far.clone());

        // No-op: far already has it (the closure-walk prune case).
        tiered.promote(&key(5)).unwrap();
        assert_eq!(far.get(&key(5)).unwrap(), Some(Bytes::from_static(b"durable")));

        // Promoting a key absent from both tiers is a harmless no-op.
        tiered.promote(&key(6)).unwrap();
        assert!(!far.has(&key(6)).unwrap());
    }

    #[test]
    fn delete_unstages_from_near_only() {
        let near = MemoryByteBackend::new();
        let far = MemoryByteBackend::new();
        far.put(&key(7), Bytes::from_static(b"durable")).unwrap();
        near.put(&key(7), Bytes::from_static(b"staged")).unwrap();
        let tiered = Tiered::new(near.clone(), far.clone());

        tiered.delete(&key(7)).unwrap();
        assert!(!near.has(&key(7)).unwrap(), "delete removes the staged copy");
        assert!(far.has(&key(7)).unwrap(), "delete must not touch the durable tier");
        // The durable value now shows through.
        assert_eq!(tiered.get(&key(7)).unwrap(), Some(Bytes::from_static(b"durable")));
    }
}
