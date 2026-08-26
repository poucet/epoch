//! [`MemoryTimelineStore`] — the in-memory reference [`TimelineStore`], for
//! tests and ephemeral use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::event::TimelineEvent;
use crate::id::EventId;
use crate::store::{TimelineError, TimelineStore};

/// An in-memory [`TimelineStore`] backed by two maps behind one mutex.
///
/// Cheaply clonable — clones share the same underlying state (mirroring
/// `MemoryByteBackend`), so a clone handed to a checkpoint layer and one kept
/// by a test observe the same timeline.
#[derive(Clone, Default)]
pub struct MemoryTimelineStore {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    events: HashMap<EventId, TimelineEvent>,
    heads: HashMap<String, EventId>,
}

impl MemoryTimelineStore {
    /// A fresh, empty timeline.
    pub fn new() -> MemoryTimelineStore {
        MemoryTimelineStore::default()
    }
}

impl MemoryTimelineStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock only means another thread panicked mid-op; the maps
        // themselves are always in a consistent state after any single insert.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TimelineStore for MemoryTimelineStore {
    fn append(&self, event: &TimelineEvent) -> Result<EventId, TimelineError> {
        let id = event.id();
        let mut state = self.lock();
        for parent in &event.parents {
            if !state.events.contains_key(parent) {
                return Err(TimelineError::MissingParent(*parent));
            }
        }
        state.events.entry(id).or_insert_with(|| event.clone());
        Ok(id)
    }

    fn get(&self, id: &EventId) -> Result<Option<TimelineEvent>, TimelineError> {
        Ok(self.lock().events.get(id).cloned())
    }

    fn set_head(&self, name: &str, id: EventId) -> Result<(), TimelineError> {
        let mut state = self.lock();
        if !state.events.contains_key(&id) {
            return Err(TimelineError::Missing(id));
        }
        state.heads.insert(name.to_string(), id);
        Ok(())
    }

    fn get_head(&self, name: &str) -> Result<Option<EventId>, TimelineError> {
        Ok(self.lock().heads.get(name).copied())
    }

    fn list_heads(&self) -> Result<Vec<(String, EventId)>, TimelineError> {
        Ok(self
            .lock()
            .heads
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_cas::CASHash;
    use epoch_tree::TreeId;

    fn event(parents: Vec<EventId>, seed: u8, ts: u64) -> TimelineEvent {
        TimelineEvent {
            parents,
            root: TreeId::from_cas_hash(CASHash::from_bytes([seed; 32])),
            timestamp_ms: ts,
            meta: None,
        }
    }

    #[test]
    fn append_get_and_idempotence() {
        let store = MemoryTimelineStore::new();
        let e = event(vec![], 1, 100);
        let id = store.append(&e).unwrap();
        assert_eq!(id, e.id());
        assert_eq!(store.get(&id).unwrap(), Some(e.clone()));
        assert_eq!(store.append(&e).unwrap(), id, "idempotent re-append");
    }

    #[test]
    fn append_rejects_dangling_parent() {
        let store = MemoryTimelineStore::new();
        let ghost = event(vec![], 9, 1).id();
        let e = event(vec![ghost], 1, 100);
        assert!(matches!(
            store.append(&e),
            Err(TimelineError::MissingParent(p)) if p == ghost
        ));
    }

    #[test]
    fn heads_point_at_present_events_only() {
        let store = MemoryTimelineStore::new();
        let ghost = event(vec![], 9, 1).id();
        assert!(matches!(
            store.set_head("main", ghost),
            Err(TimelineError::Missing(_))
        ));

        let id = store.append(&event(vec![], 1, 100)).unwrap();
        store.set_head("main", id).unwrap();
        assert_eq!(store.get_head("main").unwrap(), Some(id));
        assert_eq!(store.get_head("nope").unwrap(), None);
        assert_eq!(store.list_heads().unwrap(), vec![("main".to_string(), id)]);
    }

    #[test]
    fn fork_two_heads_off_one_base_diverge_independently() {
        let store = MemoryTimelineStore::new();
        // A shared base lineage: base(100) <- shared(200).
        let base = store.append(&event(vec![], 1, 100)).unwrap();
        let shared = store.append(&event(vec![base], 2, 200)).unwrap();
        store.set_head("main", shared).unwrap();

        // Fork: agent-b starts from the same event...
        store.set_head("agent-b", shared).unwrap();
        // ...then both lineages advance independently.
        let main_tip = store.append(&event(vec![shared], 3, 300)).unwrap();
        store.set_head("main", main_tip).unwrap();
        let fork_tip = store.append(&event(vec![shared], 4, 350)).unwrap();
        store.set_head("agent-b", fork_tip).unwrap();

        // Each head logs its own line back through the shared base.
        let main_log: Vec<EventId> = store
            .log("main", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(main_log, vec![main_tip, shared, base]);
        let fork_log: Vec<EventId> = store
            .log("agent-b", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(fork_log, vec![fork_tip, shared, base]);

        // Time queries resolve per lineage.
        assert_eq!(store.at("main", 299).unwrap(), Some(shared));
        assert_eq!(store.at("main", 300).unwrap(), Some(main_tip));
        assert_eq!(store.at("agent-b", 340).unwrap(), Some(shared));
        assert_eq!(store.at("agent-b", 350).unwrap(), Some(fork_tip));
        assert_eq!(store.at("main", 50).unwrap(), None, "before all history");
        assert_eq!(store.at("ghost-head", 999).unwrap(), None);
    }

    #[test]
    fn log_respects_limit() {
        let store = MemoryTimelineStore::new();
        let a = store.append(&event(vec![], 1, 100)).unwrap();
        let b = store.append(&event(vec![a], 2, 200)).unwrap();
        let c = store.append(&event(vec![b], 3, 300)).unwrap();
        store.set_head("main", c).unwrap();

        let ids: Vec<EventId> = store
            .log("main", 2)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![c, b]);
        assert!(store.log("main", 0).unwrap().is_empty());
    }

    #[test]
    fn merge_event_walks_first_parent_only() {
        let store = MemoryTimelineStore::new();
        let a = store.append(&event(vec![], 1, 100)).unwrap();
        let side = store.append(&event(vec![a], 2, 150)).unwrap();
        let mainline = store.append(&event(vec![a], 3, 200)).unwrap();
        let merge = store.append(&event(vec![mainline, side], 4, 300)).unwrap();
        store.set_head("main", merge).unwrap();

        let ids: Vec<EventId> = store
            .log("main", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![merge, mainline, a], "second parent not walked");
    }
}
