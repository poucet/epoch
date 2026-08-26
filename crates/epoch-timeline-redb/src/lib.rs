//! `epoch-timeline-redb` — a durable [`TimelineStore`] over an embedded
//! [`redb`](https://docs.rs/redb) database.
//!
//! The first storage engine behind the engine-agnostic `epoch-timeline`
//! contract. Events are stored by their content-derived id (the canonical
//! encoding is what's on disk, re-decoded — and thereby re-validated — on
//! read); heads are a tiny mutable `name → id` table. Every mutation is one
//! durable redb commit, which is cheap here: timeline writes happen once per
//! *checkpoint*, not once per filesystem write.
//!
//! The [`Database`] is taken as an `Arc`, so a timeline can share one `.redb`
//! file with the object store (`RedbByteBackend::with_database`) or live in
//! its own — the trait contract doesn't care, and the checkpoint layer's
//! ordering rule (closure durable → event appended → head advanced) needs no
//! cross-table atomicity.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use epoch_timeline::{EventId, TimelineError, TimelineEvent, TimelineStore};
use redb::{Database, ReadableTable, TableDefinition};

/// Events table: raw `EventId` hash bytes → canonical event encoding.
const EVENTS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("timeline_events");

/// Heads table: head name → raw `EventId` hash bytes.
const HEADS: TableDefinition<&str, &[u8; 32]> = TableDefinition::new("timeline_heads");

/// A durable [`TimelineStore`] backed by a redb database.
///
/// Cheaply clonable — clones share the same database handle.
#[derive(Clone)]
pub struct RedbTimelineStore {
    db: Arc<Database>,
}

impl RedbTimelineStore {
    /// Build over an already-open, possibly shared database, ensuring both
    /// tables exist so a read before any write doesn't fault.
    pub fn new(db: Arc<Database>) -> Result<RedbTimelineStore, TimelineError> {
        let txn = db.begin_write().map_err(TimelineError::storage)?;
        txn.open_table(EVENTS).map_err(TimelineError::storage)?;
        txn.open_table(HEADS).map_err(TimelineError::storage)?;
        txn.commit().map_err(TimelineError::storage)?;
        Ok(RedbTimelineStore { db })
    }

    /// Open (or create) a file-backed timeline of its own at `path`.
    pub fn create(path: impl AsRef<std::path::Path>) -> Result<RedbTimelineStore, TimelineError> {
        let db = Database::create(path).map_err(TimelineError::storage)?;
        RedbTimelineStore::new(Arc::new(db))
    }
}

impl TimelineStore for RedbTimelineStore {
    fn append(&self, event: &TimelineEvent) -> Result<EventId, TimelineError> {
        let id = event.id();
        let txn = self.db.begin_write().map_err(TimelineError::storage)?;
        {
            let mut table = txn.open_table(EVENTS).map_err(TimelineError::storage)?;
            for parent in &event.parents {
                if table
                    .get(parent.as_cas_hash().as_bytes())
                    .map_err(TimelineError::storage)?
                    .is_none()
                {
                    return Err(TimelineError::MissingParent(*parent));
                }
            }
            table
                .insert(id.as_cas_hash().as_bytes(), event.encode().as_slice())
                .map_err(TimelineError::storage)?;
        }
        txn.commit().map_err(TimelineError::storage)?;
        Ok(id)
    }

    fn get(&self, id: &EventId) -> Result<Option<TimelineEvent>, TimelineError> {
        let txn = self.db.begin_read().map_err(TimelineError::storage)?;
        let table = txn.open_table(EVENTS).map_err(TimelineError::storage)?;
        let value = table
            .get(id.as_cas_hash().as_bytes())
            .map_err(TimelineError::storage)?;
        value
            .map(|guard| TimelineEvent::decode(guard.value()))
            .transpose()
    }

    fn set_head(&self, name: &str, id: EventId) -> Result<(), TimelineError> {
        let txn = self.db.begin_write().map_err(TimelineError::storage)?;
        {
            let events = txn.open_table(EVENTS).map_err(TimelineError::storage)?;
            if events
                .get(id.as_cas_hash().as_bytes())
                .map_err(TimelineError::storage)?
                .is_none()
            {
                return Err(TimelineError::Missing(id));
            }
            let mut heads = txn.open_table(HEADS).map_err(TimelineError::storage)?;
            heads
                .insert(name, id.as_cas_hash().as_bytes())
                .map_err(TimelineError::storage)?;
        }
        txn.commit().map_err(TimelineError::storage)?;
        Ok(())
    }

    fn get_head(&self, name: &str) -> Result<Option<EventId>, TimelineError> {
        let txn = self.db.begin_read().map_err(TimelineError::storage)?;
        let table = txn.open_table(HEADS).map_err(TimelineError::storage)?;
        let value = table.get(name).map_err(TimelineError::storage)?;
        Ok(value.map(|guard| EventId::from_cas_hash(epoch_cas_hash(*guard.value()))))
    }

    fn list_heads(&self) -> Result<Vec<(String, EventId)>, TimelineError> {
        let txn = self.db.begin_read().map_err(TimelineError::storage)?;
        let table = txn.open_table(HEADS).map_err(TimelineError::storage)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(TimelineError::storage)? {
            let (name, id) = entry.map_err(TimelineError::storage)?;
            out.push((
                name.value().to_string(),
                EventId::from_cas_hash(epoch_cas_hash(*id.value())),
            ));
        }
        Ok(out)
    }
}

/// Rebuild a `CASHash` from raw table bytes.
fn epoch_cas_hash(bytes: [u8; 32]) -> epoch_cas::CASHash {
    epoch_cas::CASHash::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_cas::CASHash;
    use epoch_tree::TreeId;

    fn store() -> RedbTimelineStore {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap();
        RedbTimelineStore::new(Arc::new(db)).unwrap()
    }

    fn event(parents: Vec<EventId>, seed: u8, ts: u64) -> TimelineEvent {
        TimelineEvent {
            parents,
            root: TreeId::from_cas_hash(CASHash::from_bytes([seed; 32])),
            timestamp_ms: ts,
            meta: Some(format!("event-{seed}")),
        }
    }

    #[test]
    fn append_get_heads_roundtrip() {
        let s = store();
        let a = s.append(&event(vec![], 1, 100)).unwrap();
        let b = s.append(&event(vec![a], 2, 200)).unwrap();
        assert_eq!(s.get(&a).unwrap(), Some(event(vec![], 1, 100)));
        assert_eq!(s.get(&b).unwrap().unwrap().parents, vec![a]);

        s.set_head("main", b).unwrap();
        assert_eq!(s.get_head("main").unwrap(), Some(b));
        assert_eq!(s.list_heads().unwrap(), vec![("main".to_string(), b)]);
        assert_eq!(s.get_head("nope").unwrap(), None);
    }

    #[test]
    fn append_rejects_dangling_parent_and_set_head_rejects_missing() {
        let s = store();
        let ghost = event(vec![], 9, 1).id();
        assert!(matches!(
            s.append(&event(vec![ghost], 1, 100)),
            Err(TimelineError::MissingParent(_))
        ));
        assert!(matches!(
            s.set_head("main", ghost),
            Err(TimelineError::Missing(_))
        ));
    }

    #[test]
    fn chain_walk_defaults_work_over_redb() {
        let s = store();
        let a = s.append(&event(vec![], 1, 100)).unwrap();
        let b = s.append(&event(vec![a], 2, 200)).unwrap();
        let c = s.append(&event(vec![b], 3, 300)).unwrap();
        s.set_head("main", c).unwrap();

        assert_eq!(s.at("main", 250).unwrap(), Some(b));
        assert_eq!(s.at("main", 99).unwrap(), None);
        let ids: Vec<EventId> = s.log("main", 10).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![c, b, a]);
    }

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timeline.redb");
        let (a, b) = {
            let s = RedbTimelineStore::create(&path).unwrap();
            let a = s.append(&event(vec![], 1, 100)).unwrap();
            let b = s.append(&event(vec![a], 2, 200)).unwrap();
            s.set_head("main", b).unwrap();
            (a, b)
        };
        let s = RedbTimelineStore::create(&path).unwrap();
        assert_eq!(s.get_head("main").unwrap(), Some(b));
        assert_eq!(s.get(&a).unwrap(), Some(event(vec![], 1, 100)));
        assert_eq!(s.at("main", 150).unwrap(), Some(a));
    }
}
