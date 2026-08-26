//! [`CheckpointedFs`] — the shared object the checkpoint scheduler drives.
//!
//! Pairs the live [`CasVfs`] with its [`History`] and exposes the one-shot
//! [`checkpoint`](CheckpointedFs::checkpoint) the scheduler's closure calls:
//! **seal** (the only step under the `WorkingTree` lock) then
//! [`commit_root`](History::commit_root) (closure walk, batched promotion,
//! event append, head advance — all off-lock, per the TASKS.md concurrency
//! contract). Mutations racing the promotion simply land in staging for the
//! next checkpoint.

use std::sync::Arc;

use epoch_cas::{CASHash, SyncByteBackend, TieredByteBackend};
use epoch_history::{History, HistoryError};
use epoch_timeline::EventId;

use crate::cas_vfs::CasVfs;

/// The live filesystem plus its checkpoint layer. Cheap to clone into the
/// scheduler closure (both halves are `Arc`s).
pub struct CheckpointedFs<Near, Far> {
    vfs: Arc<CasVfs<TieredByteBackend<Near, Far>>>,
    history: Arc<History<Near, Far>>,
}

impl<Near, Far> Clone for CheckpointedFs<Near, Far> {
    fn clone(&self) -> Self {
        Self {
            vfs: Arc::clone(&self.vfs),
            history: Arc::clone(&self.history),
        }
    }
}

impl<Near, Far> CheckpointedFs<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone,
    Far: SyncByteBackend<CASHash> + Clone,
{
    /// Pair a live filesystem with its history. Both must share the same tier
    /// (the backends are `Arc`-backed clones of one another).
    pub fn new(
        vfs: Arc<CasVfs<TieredByteBackend<Near, Far>>>,
        history: Arc<History<Near, Far>>,
    ) -> Self {
        Self { vfs, history }
    }

    /// Run one checkpoint: seal under the tree lock, commit off-lock.
    /// Returns `None` when nothing changed since the last checkpoint.
    pub fn checkpoint(&self) -> Result<Option<EventId>, HistoryError> {
        let root = self
            .vfs
            .seal()
            .map_err(|err| HistoryError::Seal(err.to_string()))?;
        self.history.commit_root(root, None)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Duration;

    use epoch_cas::MemoryByteBackend;
    use epoch_history::{CheckpointPolicy, Scheduler};
    use epoch_timeline::MemoryTimelineStore;
    use epoch_vfs_api::{
        CreateRequest, OpenMode, OpenRequest, ROOT_INODE_ID, VfsService, WriteRequest,
    };

    use super::*;
    use crate::signal::SignalVfs;

    type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

    /// The full trigger loop: a write + flush through `SignalVfs` debounces
    /// into a background checkpoint that advances the head.
    #[test]
    fn write_flush_debounces_into_a_checkpoint() {
        let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
        let live = Arc::new(CasVfs::new(tier.clone()));
        let history = Arc::new(History::new(
            tier,
            Arc::new(MemoryTimelineStore::new()),
            "main",
        ));
        let fs = CheckpointedFs::new(Arc::clone(&live), Arc::clone(&history));
        let scheduler = Scheduler::spawn(
            CheckpointPolicy::new(
                Duration::from_millis(5),
                Duration::from_millis(100),
                u64::MAX,
            ),
            Box::new(move || fs.checkpoint()),
        );
        let vfs = SignalVfs::new(live, scheduler.handle(), false);

        let created = vfs
            .create(CreateRequest::file(ROOT_INODE_ID, "hello.txt", 0o644))
            .unwrap();
        let node = created.entry.attr.inode_id;
        let opened = vfs
            .open(OpenRequest {
                inode_id: node,
                mode: OpenMode::ReadWrite,
                truncate: false,
                append: false,
            })
            .unwrap();
        vfs.write(WriteRequest {
            inode_id: node,
            handle: opened.handle,
            offset: 0,
            data: b"world",
        })
        .unwrap();
        vfs.flush(opened.handle).unwrap();
        vfs.release(opened.handle).unwrap();

        // The debounce window elapses and the background thread checkpoints.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let head = loop {
            if let Some(head) = history.head().unwrap() {
                break head;
            }
            assert!(std::time::Instant::now() < deadline, "no checkpoint landed");
            std::thread::sleep(Duration::from_millis(2));
        };
        let event = history.event(&head).unwrap().unwrap();
        assert!(event.parents.is_empty());

        // A lookup-only op emits nothing; the head stays put.
        vfs.lookup(ROOT_INODE_ID, OsStr::new("hello.txt")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(history.head().unwrap(), Some(head));
    }

    /// Hard fsync blocks until the checkpoint is durable.
    #[test]
    fn hard_fsync_is_a_synchronous_barrier() {
        let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
        let live = Arc::new(CasVfs::new(tier.clone()));
        let history = Arc::new(History::new(
            tier,
            Arc::new(MemoryTimelineStore::new()),
            "main",
        ));
        let fs = CheckpointedFs::new(Arc::clone(&live), Arc::clone(&history));
        // A policy that would never fire on its own: only the barrier commits.
        let scheduler = Scheduler::spawn(
            CheckpointPolicy::new(Duration::from_secs(60), Duration::from_secs(60), u64::MAX),
            Box::new(move || fs.checkpoint()),
        );
        let vfs = SignalVfs::new(live, scheduler.handle(), true);

        let created = vfs
            .create(CreateRequest::file(ROOT_INODE_ID, "f", 0o644))
            .unwrap();
        let node = created.entry.attr.inode_id;
        let opened = vfs
            .open(OpenRequest {
                inode_id: node,
                mode: OpenMode::ReadWrite,
                truncate: false,
                append: false,
            })
            .unwrap();
        vfs.write(WriteRequest {
            inode_id: node,
            handle: opened.handle,
            offset: 0,
            data: b"x",
        })
        .unwrap();

        assert_eq!(history.head().unwrap(), None, "nothing durable yet");
        vfs.fsync(node, opened.handle, false).unwrap();
        assert!(
            history.head().unwrap().is_some(),
            "fsync returned ⇒ checkpoint landed"
        );
    }
}
