//! `epoch-cas-redb` — a durable [`SyncByteBackend`] over an embedded
//! [`redb`](https://docs.rs/redb) database.
//!
//! This is the **storage / `far`** tier of the epoch CAS: content-addressed,
//! immutable objects keyed by [`CASHash`]. It is written only by promotion from
//! a staging tier (see [`epoch_cas::TieredByteBackend`]), so its contents are
//! exactly the transitive closure of checkpointed roots.
//!
//! The canonical mount-time stack is
//!
//! ```text
//! TieredByteBackend< MemoryByteBackend,
//!                    CachingByteBackend< MokaByteBackend, RedbByteBackend > >
//! ```
//!
//! i.e. an in-memory staging tier in front of a moka-cached, redb-durable tier.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use epoch_cas::{Bytes, CASError, CASHash, SyncByteBackend};
use redb::{Database, TableDefinition};

/// Objects table: content hash bytes → stored (compressed) object bytes.
const OBJECTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("epoch_cas_objects");

/// A durable [`SyncByteBackend`] backed by a redb database.
///
/// Keyed by [`CASHash`] (its 32 raw bytes are the table key). Cheaply clonable —
/// clones share the same database handle (`Arc<Database>`), so a clone handed to
/// a `CachingByteBackend`/`TieredByteBackend` and one kept elsewhere operate on
/// the same store.
pub struct RedbByteBackend {
    db: Arc<Database>,
}

impl Clone for RedbByteBackend {
    fn clone(&self) -> Self {
        Self { db: Arc::clone(&self.db) }
    }
}

impl RedbByteBackend {
    /// Open (or create) a file-backed database at `path`.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, CASError> {
        let db = Database::create(path).map_err(CASError::backend)?;
        Self::with_database(Arc::new(db))
    }

    /// A non-persistent, in-memory database — handy for tests and ephemeral use.
    pub fn in_memory() -> Result<Self, CASError> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(CASError::backend)?;
        Self::with_database(Arc::new(db))
    }

    /// Build over an already-open, possibly shared database.
    ///
    /// This is how one `.redb` file hosts more than the object store: the caller
    /// opens the [`Database`] once and hands clones of the `Arc` to this backend
    /// and to any sibling stores keeping their own tables (e.g. a timeline
    /// store). Ensures the objects table exists so a read before any write
    /// doesn't fault.
    pub fn with_database(db: Arc<Database>) -> Result<Self, CASError> {
        let txn = db.begin_write().map_err(CASError::backend)?;
        txn.open_table(OBJECTS).map_err(CASError::backend)?;
        txn.commit().map_err(CASError::backend)?;
        Ok(Self { db })
    }
}

impl SyncByteBackend<CASHash> for RedbByteBackend {
    fn get(&self, key: &CASHash) -> Result<Option<Bytes>, CASError> {
        let txn = self.db.begin_read().map_err(CASError::backend)?;
        let table = txn.open_table(OBJECTS).map_err(CASError::backend)?;
        let value = table
            .get(key.as_bytes().as_slice())
            .map_err(CASError::backend)?;
        Ok(value.map(|guard| Bytes::copy_from_slice(guard.value())))
    }

    fn put(&self, key: &CASHash, bytes: Bytes) -> Result<(), CASError> {
        let txn = self.db.begin_write().map_err(CASError::backend)?;
        {
            let mut table = txn.open_table(OBJECTS).map_err(CASError::backend)?;
            table
                .insert(key.as_bytes().as_slice(), bytes.as_ref())
                .map_err(CASError::backend)?;
        }
        txn.commit().map_err(CASError::backend)?;
        Ok(())
    }

    fn has(&self, key: &CASHash) -> Result<bool, CASError> {
        let txn = self.db.begin_read().map_err(CASError::backend)?;
        let table = txn.open_table(OBJECTS).map_err(CASError::backend)?;
        Ok(table
            .get(key.as_bytes().as_slice())
            .map_err(CASError::backend)?
            .is_some())
    }

    fn delete(&self, key: &CASHash) -> Result<(), CASError> {
        let txn = self.db.begin_write().map_err(CASError::backend)?;
        {
            let mut table = txn.open_table(OBJECTS).map_err(CASError::backend)?;
            table
                .remove(key.as_bytes().as_slice())
                .map_err(CASError::backend)?;
        }
        txn.commit().map_err(CASError::backend)?;
        Ok(())
    }

    fn put_many(&self, items: &[(CASHash, Bytes)]) -> Result<(), CASError> {
        // The whole batch lands in one write transaction: one durable commit
        // (one fsync) instead of one per object.
        let txn = self.db.begin_write().map_err(CASError::backend)?;
        {
            let mut table = txn.open_table(OBJECTS).map_err(CASError::backend)?;
            for (key, bytes) in items {
                table
                    .insert(key.as_bytes().as_slice(), bytes.as_ref())
                    .map_err(CASError::backend)?;
            }
        }
        txn.commit().map_err(CASError::backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_cas::{CachingByteBackend, MemoryByteBackend, TieredByteBackend};

    fn key(n: u8) -> CASHash {
        CASHash::from_bytes([n; 32])
    }

    #[test]
    fn put_get_has_delete_roundtrip() {
        let b = RedbByteBackend::in_memory().unwrap();
        assert!(!b.has(&key(1)).unwrap());
        b.put(&key(1), Bytes::from_static(b"durable")).unwrap();
        assert!(b.has(&key(1)).unwrap());
        assert_eq!(b.get(&key(1)).unwrap(), Some(Bytes::from_static(b"durable")));
        b.delete(&key(1)).unwrap();
        assert!(!b.has(&key(1)).unwrap());
        assert_eq!(b.get(&key(1)).unwrap(), None);
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.redb");
        {
            let b = RedbByteBackend::create(&path).unwrap();
            b.put(&key(2), Bytes::from_static(b"persisted")).unwrap();
        } // drop closes the database
        let b = RedbByteBackend::create(&path).unwrap();
        assert_eq!(b.get(&key(2)).unwrap(), Some(Bytes::from_static(b"persisted")));
    }

    #[test]
    fn put_many_lands_the_whole_batch_durably() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.redb");
        {
            let b = RedbByteBackend::create(&path).unwrap();
            let items: Vec<_> = (10u8..14)
                .map(|n| (key(n), Bytes::copy_from_slice(&[n])))
                .collect();
            b.put_many(&items).unwrap();
        }
        let reopened = RedbByteBackend::create(&path).unwrap();
        for n in 10u8..14 {
            assert_eq!(
                reopened.get(&key(n)).unwrap(),
                Some(Bytes::copy_from_slice(&[n]))
            );
        }
    }

    #[test]
    fn with_database_shares_one_database_across_stores() {
        let db = Arc::new(
            Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let a = RedbByteBackend::with_database(Arc::clone(&db)).unwrap();
        let b = RedbByteBackend::with_database(db).unwrap();
        a.put(&key(20), Bytes::from_static(b"shared")).unwrap();
        assert_eq!(b.get(&key(20)).unwrap(), Some(Bytes::from_static(b"shared")));
    }

    /// The full mount-time stack: staging in front of a moka-cached, redb-durable
    /// tier. A write stays in staging (not durable); promotion makes it durable
    /// in redb and it survives reopening the database.
    #[test]
    fn staging_promotes_into_durable_redb() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cas.redb");

        {
            let storage = RedbByteBackend::create(&path).unwrap();
            // A plain in-memory cache here keeps this crate independent of any
            // particular cache backend; the moka LRU is just one drop-in choice.
            let durable = CachingByteBackend::new(MemoryByteBackend::new(), storage.clone());
            let stack = TieredByteBackend::new(MemoryByteBackend::new(), durable);

            stack.put(&key(3), Bytes::from_static(b"x")).unwrap();
            assert!(!storage.has(&key(3)).unwrap(), "a write is not durable before promotion");

            stack.promote(&key(3)).unwrap();
            assert!(storage.has(&key(3)).unwrap(), "promotion writes through to redb");
        } // drop everything

        // Reopen the on-disk database: the promoted object is still there.
        let reopened = RedbByteBackend::create(&path).unwrap();
        assert_eq!(reopened.get(&key(3)).unwrap(), Some(Bytes::from_static(b"x")));
    }
}
