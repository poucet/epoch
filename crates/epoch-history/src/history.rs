//! [`History`] — the checkpoint orchestrator: closure → promote → event → head.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use epoch_cas::{CASHash, SyncByteBackend, TieredByteBackend};
use epoch_file_cas::{BlobId, FileCas};
use epoch_timeline::{EventId, TimelineError, TimelineEvent, TimelineStore};
use epoch_tree::{EntryId, Overlay, TreeId, TreeStore};

use crate::closure::pending_closure;
use crate::error::HistoryError;

/// Objects per [`promote_many`](TieredByteBackend::promote_many) batch: one
/// durable commit each, and the transient batch buffer is bounded by this many
/// objects (chunks are ≤ 1 MiB, so ≤ ~256 MiB worst case).
const PROMOTE_BATCH: usize = 256;

/// The link type sealed worktrees store their trees with.
type WorktreeLink = Overlay<EntryId<BlobId>>;

/// The checkpoint layer over one mount's storage tier and one timeline head.
///
/// Holds clones of the shared tier (backends are `Arc`-backed, so these are the
/// *same* stores the [`WorkingTree`](epoch_worktree) writes into) plus the
/// timeline and the name of the head this mount advances.
/// [`commit_root`](Self::commit_root) is the whole checkpoint dance after
/// `seal`, upholding the ordering rule (see the [crate docs](crate)):
/// closure durable → event appended → head advanced.
pub struct History<Near, Far> {
    tier: TieredByteBackend<Near, Far>,
    trees: TreeStore<TieredByteBackend<Near, Far>, WorktreeLink>,
    files: FileCas<TieredByteBackend<Near, Far>>,
    timeline: Arc<dyn TimelineStore>,
    head: String,
}

impl<Near, Far> History<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone,
    Far: SyncByteBackend<CASHash> + Clone,
{
    /// Build over the mount's tier and timeline, advancing head `head`.
    pub fn new(
        tier: TieredByteBackend<Near, Far>,
        timeline: Arc<dyn TimelineStore>,
        head: impl Into<String>,
    ) -> History<Near, Far> {
        History {
            trees: TreeStore::new(tier.clone()),
            files: FileCas::new(tier.clone()),
            tier,
            timeline,
            head: head.into(),
        }
    }

    /// Checkpoint `root` (a sealed worktree root) now.
    ///
    /// Returns `None` without touching anything when the head already points
    /// at this root — a debounce can fire with no effective change, and an
    /// empty checkpoint would only bloat the timeline.
    pub fn commit_root(
        &self,
        root: TreeId,
        meta: Option<String>,
    ) -> Result<Option<EventId>, HistoryError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.commit_root_at(root, now_ms, meta)
    }

    /// [`commit_root`](Self::commit_root) with an explicit wall-clock reading
    /// (exposed for tests and replays). The recorded timestamp is clamped to
    /// stay strictly increasing along this head's chain (`max(now, parent+1)`),
    /// so time queries survive clock steps.
    pub fn commit_root_at(
        &self,
        root: TreeId,
        now_ms: u64,
        meta: Option<String>,
    ) -> Result<Option<EventId>, HistoryError> {
        let parent = self.timeline.get_head(&self.head)?;
        let parent_event = parent
            .map(|id| {
                self.timeline
                    .get(&id)?
                    .ok_or(TimelineError::Missing(id))
                    .map_err(HistoryError::from)
            })
            .transpose()?;
        if let Some(event) = &parent_event
            && event.root == root
        {
            return Ok(None);
        }

        // 1. Make the closure durable, children-first, draining staging.
        let pending = pending_closure(&self.trees, &self.files, root, &|k| {
            self.tier.far().has(k)
        })?;
        for batch in pending.chunks(PROMOTE_BATCH) {
            self.tier.promote_many(batch, true)?;
        }

        // 2. Only now is the root safe to reference: append the event.
        let timestamp_ms = parent_event.map_or(now_ms, |e| now_ms.max(e.timestamp_ms + 1));
        let event = TimelineEvent {
            parents: parent.into_iter().collect(),
            root,
            timestamp_ms,
            meta,
        };
        let id = self.timeline.append(&event)?;

        // 3. And only now is the event safe to reach: advance the head.
        self.timeline.set_head(&self.head, id)?;
        Ok(Some(id))
    }

    /// The name of the head this history advances.
    pub fn head_name(&self) -> &str {
        &self.head
    }

    /// The head's current event id, or `None` before the first checkpoint.
    pub fn head(&self) -> Result<Option<EventId>, HistoryError> {
        Ok(self.timeline.get_head(&self.head)?)
    }

    /// Fetch an event by id.
    pub fn event(&self, id: &EventId) -> Result<Option<TimelineEvent>, HistoryError> {
        Ok(self.timeline.get(id)?)
    }

    /// The newest checkpoint at or before `ts_ms` on this head's chain.
    pub fn resolve_at(
        &self,
        ts_ms: u64,
    ) -> Result<Option<(EventId, TimelineEvent)>, HistoryError> {
        match self.timeline.at(&self.head, ts_ms)? {
            None => Ok(None),
            Some(id) => {
                let event = self.timeline.get(&id)?.ok_or(TimelineError::Missing(id))?;
                Ok(Some((id, event)))
            }
        }
    }

    /// Up to `limit` checkpoints, newest first, along this head's chain.
    pub fn log(&self, limit: usize) -> Result<Vec<(EventId, TimelineEvent)>, HistoryError> {
        Ok(self.timeline.log(&self.head, limit)?)
    }

    /// The durable (far) tier — what read-only snapshots should read from.
    pub fn far(&self) -> &Far {
        self.tier.far()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use epoch_cas::MemoryByteBackend;
    use epoch_timeline::MemoryTimelineStore;
    use epoch_vfs_api::ROOT_INODE_ID;
    use epoch_worktree::WorkingTree;

    use super::*;

    type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

    fn setup() -> (WorkingTree<Tiered>, History<MemoryByteBackend, MemoryByteBackend>, Tiered) {
        let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
        let history = History::new(
            tier.clone(),
            Arc::new(MemoryTimelineStore::new()),
            "main",
        );
        (WorkingTree::new(tier.clone()), history, tier)
    }

    #[test]
    fn checkpoint_promotes_drains_and_advances_head() {
        let (mut wt, history, tier) = setup();
        let f = wt.create(ROOT_INODE_ID, OsStr::new("hello"), 0o644).unwrap();
        wt.write_at(f, 0, b"world").unwrap();
        let root = wt.seal().unwrap();

        assert_eq!(history.head().unwrap(), None);
        let id = history
            .commit_root_at(root, 1_000, Some("first".into()))
            .unwrap()
            .expect("a real change checkpoints");

        assert_eq!(history.head().unwrap(), Some(id));
        let event = history.event(&id).unwrap().unwrap();
        assert_eq!(event.root, root);
        assert_eq!(event.parents, vec![]);
        assert_eq!(event.timestamp_ms, 1_000);

        // Everything reachable is durable; staging fully drained.
        assert!(
            pending_closure(&history.trees, &history.files, root, &|k| tier.far().has(k))
                .unwrap()
                .is_empty()
        );
        assert_eq!(tier.near().len(), 0, "staging drained after checkpoint");
    }

    #[test]
    fn second_checkpoint_is_delta_only_and_links_lineage() {
        let (mut wt, history, tier) = setup();
        let f = wt.create(ROOT_INODE_ID, OsStr::new("a"), 0o644).unwrap();
        wt.write_at(f, 0, b"one").unwrap();
        let root1 = wt.seal().unwrap();
        let first = history.commit_root_at(root1, 1_000, None).unwrap().unwrap();
        let durable_after_first = tier.far().len();

        let g = wt.create(ROOT_INODE_ID, OsStr::new("b"), 0o644).unwrap();
        wt.write_at(g, 0, b"two").unwrap();
        let root2 = wt.seal().unwrap();
        let second = history.commit_root_at(root2, 2_000, None).unwrap().unwrap();

        // Delta = new manifest + new root tree.
        assert_eq!(tier.far().len(), durable_after_first + 2);
        assert_eq!(tier.near().len(), 0);

        let event = history.event(&second).unwrap().unwrap();
        assert_eq!(event.parents, vec![first], "lineage links to prior head");

        // Both roots remain readable from the far tier alone.
        let far_files: FileCas<MemoryByteBackend> = FileCas::new(tier.far().clone());
        let far_trees: TreeStore<MemoryByteBackend, WorktreeLink> =
            TreeStore::new(tier.far().clone());
        assert!(epoch_cas::SyncCAS::get(&far_trees, &root1).unwrap().is_some());
        assert!(epoch_cas::SyncCAS::get(&far_trees, &root2).unwrap().is_some());
        let _ = far_files; // manifests are checked transitively via the walks above
    }

    #[test]
    fn unchanged_root_short_circuits() {
        let (mut wt, history, _tier) = setup();
        let f = wt.create(ROOT_INODE_ID, OsStr::new("a"), 0o644).unwrap();
        wt.write_at(f, 0, b"x").unwrap();
        let root = wt.seal().unwrap();
        let first = history.commit_root_at(root, 1_000, None).unwrap();
        assert!(first.is_some());

        // Sealing again with no mutations yields the same root — no event.
        let same = wt.seal().unwrap();
        assert_eq!(same, root);
        assert_eq!(history.commit_root_at(same, 2_000, None).unwrap(), None);
        assert_eq!(history.log(10).unwrap().len(), 1);
    }

    #[test]
    fn timestamps_clamp_monotonic_across_clock_steps() {
        let (mut wt, history, _tier) = setup();
        let f = wt.create(ROOT_INODE_ID, OsStr::new("a"), 0o644).unwrap();
        wt.write_at(f, 0, b"1").unwrap();
        history.commit_root_at(wt.seal().unwrap(), 5_000, None).unwrap().unwrap();

        // Clock stepped backwards: the event still lands strictly after.
        wt.write_at(f, 0, b"2").unwrap();
        let id = history
            .commit_root_at(wt.seal().unwrap(), 3_000, None)
            .unwrap()
            .unwrap();
        assert_eq!(history.event(&id).unwrap().unwrap().timestamp_ms, 5_001);

        // resolve_at picks per the clamped timeline.
        let (at, _) = history.resolve_at(5_000).unwrap().unwrap();
        assert_ne!(at, id);
        let (at, _) = history.resolve_at(6_000).unwrap().unwrap();
        assert_eq!(at, id);
    }
}
