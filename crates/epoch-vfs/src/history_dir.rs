//! `.vfs/history` — the time-travel directory: read-only snapshots of the
//! filesystem at any checkpoint.
//!
//! [`HistoryDir`] is a control-plane **feature** (see [`crate::feature`]),
//! mounted via
//! [`ControlPlaneVfs::with_feature`](crate::ControlPlaneVfs::with_feature).
//! Names under `.vfs/history/` resolve to checkpoints on the mount's timeline
//! head:
//!
//! - `.vfs/history/<unix-millis>` — the newest checkpoint at or before that
//!   time on the head's first-parent chain (ZFS-style: *any* millisecond
//!   resolves, not just exact checkpoint times).
//! - `.vfs/history/<N><unit>-ago` — relative form of the same (`500ms-ago`,
//!   `30s-ago`, `5m-ago`, `2h-ago`, `1d-ago`). Served with a zero entry TTL so
//!   the kernel re-resolves it every time.
//! - `.vfs/history/id/<64-hex event id>` — any event, on this head's chain or
//!   not. `id/` itself is intentionally not listable (ids are addressable, not
//!   enumerable). Listing `.vfs/history` shows the newest checkpoints (up to
//!   [`LIST_LIMIT`]) by their millisecond names.
//!
//! Each resolved checkpoint lazily instantiates a [`CasVfs`] over the **far
//! (durable) tier only** — snapshots never see staged, un-checkpointed state.
//!
//! ## Id remapping (the correctness crux)
//!
//! Every snapshot's `WorkingTree` mints its own session-scoped [`InodeId`]s
//! starting at 1 — they collide across snapshots and with the live tree. So
//! this feature never leaks a snapshot-local id out of its slot's id slice:
//! each `(event, local id)` pair gets a stable synthetic id,
//! and every id in every result (entry attrs, readdir rows, handles) is
//! rewritten into that space. Mutating ops answer
//! [`VfsError::ReadOnlyFilesystem`] outright, so the (technically writable)
//! snapshot `CasVfs` is unreachable for writes.
//!
//! Snapshot instances and id mappings are never evicted in v1 — the same
//! deferred bounded-memory problem class as FUSE-`forget` inode eviction.

use std::{
    collections::HashMap,
    ffi::OsStr,
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use epoch_cas::{CASHash, SyncByteBackend};
use epoch_history::History;
use epoch_timeline::EventId;
use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind, OpenMode,
    OpenRequest, OpenedFile, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
};

use crate::cas_vfs::CasVfs;
use crate::feature::FeatureSlot;

/// How many checkpoints a `readdir` of `.vfs/history` lists (newest first).
/// Older checkpoints stay reachable by lookup — listing is a courtesy, not the
/// index.
pub const LIST_LIMIT: usize = 64;

/// The `.vfs/history` sub-service. See the [module docs](self).
pub struct HistoryDir<Near, Far> {
    history: Arc<History<Near, Far>>,
    /// The feature's id slice and `.vfs` parent (see [`crate::feature`]):
    /// `slot.root_id()` is `.vfs/history` itself, `slot.base + 1` the `id/`
    /// subdirectory, and dynamic snapshot ids/handles are minted from
    /// `slot.base + 2` upward.
    slot: FeatureSlot,
    /// The overlay's lower filesystem, when the mount is an overlay. Each
    /// snapshot rehydrates its checkpointed **delta** over this lower, so a
    /// historical view is "the overlay state as of the checkpoint, over the
    /// lower as it is *now*" — faithful for the CAS-owned delta, but subject
    /// to lower drift until `OpaqueTree` ingestion makes checkpoints fully
    /// self-contained.
    lower: Option<Arc<dyn VfsService>>,
    state: RwLock<HistState<Far>>,
    next_id: AtomicU64,
}

struct HistState<Far> {
    /// Lazily-built read-only snapshot filesystems, one per resolved event.
    snapshots: HashMap<EventId, Arc<CasVfs<Far>>>,
    /// Synthetic id → (event, snapshot-local id).
    to_local: HashMap<InodeId, (EventId, InodeId)>,
    /// (event, snapshot-local id) → synthetic id (for minting idempotence).
    from_local: HashMap<(EventId, InodeId), InodeId>,
    /// Synthetic handle → (event, snapshot-local handle).
    handles: HashMap<FileHandle, (EventId, FileHandle)>,
}

impl<Far> Default for HistState<Far> {
    fn default() -> Self {
        Self {
            snapshots: HashMap::new(),
            to_local: HashMap::new(),
            from_local: HashMap::new(),
            handles: HashMap::new(),
        }
    }
}

impl<Near, Far> HistoryDir<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone + 'static,
    Far: SyncByteBackend<CASHash> + Clone + 'static,
{
    /// Build over the mount's [`History`] in the given feature `slot`. Pass
    /// the overlay's `lower` when the mount overlays one — checkpointed roots
    /// are then sparse deltas and snapshots rehydrate over it.
    pub fn new(
        history: Arc<History<Near, Far>>,
        slot: FeatureSlot,
        lower: Option<Arc<dyn VfsService>>,
    ) -> Self {
        Self {
            history,
            slot,
            lower,
            state: RwLock::new(HistState::default()),
            next_id: AtomicU64::new(slot.base + 2),
        }
    }

    /// The `.vfs/history/id` directory node.
    fn id_dir(&self) -> InodeId {
        InodeId(self.slot.base + 1)
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, HistState<Far>>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("history read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, HistState<Far>>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("history write lock poisoned".to_owned()))
    }

    /// Resolve a `.vfs/history/<name>` to an event id, or `None` if the name
    /// is not a recognized time form. The bool is `true` for relative names
    /// (which must not be dentry-cached).
    fn resolve_time_name(&self, name: &OsStr) -> VfsResult<Option<(EventId, bool)>> {
        let Some(text) = name.to_str() else {
            return Ok(None);
        };
        let (ts_ms, relative) = if let Ok(ms) = text.parse::<u64>() {
            (ms, false)
        } else if let Some(ago) = parse_ago(text) {
            (now_ms().saturating_sub(ago), true)
        } else {
            return Ok(None);
        };
        let resolved = self
            .history
            .resolve_at(ts_ms)
            .map_err(history_error)?
            .map(|(id, _)| (id, relative));
        Ok(resolved)
    }

    /// The read-only snapshot filesystem for `event`, built on first use.
    fn snapshot(&self, event: EventId) -> VfsResult<Arc<CasVfs<Far>>> {
        if let Some(vfs) = self.read_state()?.snapshots.get(&event) {
            return Ok(Arc::clone(vfs));
        }
        let record = self
            .history
            .event(&event)
            .map_err(history_error)?
            .ok_or(VfsError::NotFound)?;
        let far = self.history.far().clone();
        let vfs = match &self.lower {
            // Overlay mount: the checkpointed root is a sparse delta —
            // rehydrate it over the (shared) lower.
            Some(lower) => CasVfs::from_overlay(far, Arc::clone(lower), record.root),
            None => CasVfs::from_root(far, record.root),
        }
        .map(Arc::new)
        .map_err(|_| VfsError::Other("snapshot root unreadable".to_owned()))?;
        let mut state = self.write_state()?;
        // Another thread may have raced us here; keep whichever landed first
        // so every caller shares one instance.
        Ok(Arc::clone(
            state.snapshots.entry(event).or_insert(vfs),
        ))
    }

    /// The stable synthetic id for a snapshot-local id (minted on first use).
    fn synthetic_id(&self, event: EventId, local: InodeId) -> VfsResult<InodeId> {
        if let Some(id) = self.read_state()?.from_local.get(&(event, local)) {
            return Ok(*id);
        }
        let mut state = self.write_state()?;
        if let Some(id) = state.from_local.get(&(event, local)) {
            return Ok(*id);
        }
        let id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.from_local.insert((event, local), id);
        state.to_local.insert(id, (event, local));
        Ok(id)
    }

    /// The `(event, snapshot-local id)` a synthetic id stands for.
    fn local_of(&self, id: InodeId) -> VfsResult<(EventId, InodeId)> {
        self.read_state()?
            .to_local
            .get(&id)
            .copied()
            .ok_or(VfsError::NotFound)
    }

    /// Rewrite a snapshot-local attr into the synthetic id space, stripping
    /// write permission bits (this whole subtree is immutable).
    fn remap_attr(&self, event: EventId, mut attr: NodeAttr) -> VfsResult<NodeAttr> {
        attr.inode_id = self.synthetic_id(event, attr.inode_id)?;
        attr.perm &= 0o555;
        Ok(attr)
    }

    /// The root entry of `event`'s snapshot, as seen from `.vfs/history`.
    fn snapshot_root_entry(&self, event: EventId, relative: bool) -> VfsResult<Entry> {
        let snapshot = self.snapshot(event)?;
        let attr = snapshot.getattr(epoch_vfs_api::ROOT_INODE_ID)?;
        let mut entry = Entry::new(self.remap_attr(event, attr)?);
        if relative {
            // "5s-ago" names a different checkpoint five seconds from now —
            // forbid the kernel from caching the resolution.
            entry.ttl = Duration::ZERO;
        }
        Ok(entry)
    }
}

impl<Near, Far> VfsService for HistoryDir<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone + 'static,
    Far: SyncByteBackend<CASHash> + Clone + 'static,
{
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if parent == self.slot.root_id() {
            if name == OsStr::new("id") {
                return Ok(Entry::new(hist_dir_attr(self.id_dir())));
            }
            return match self.resolve_time_name(name)? {
                Some((event, relative)) => self.snapshot_root_entry(event, relative),
                None => Err(VfsError::NotFound),
            };
        }
        if parent == self.id_dir() {
            let text = name.to_str().ok_or(VfsError::NotFound)?;
            let event = EventId::from_str(text).map_err(|_| VfsError::NotFound)?;
            return self.snapshot_root_entry(event, false);
        }
        let (event, local_parent) = self.local_of(parent)?;
        let entry = self.snapshot(event)?.lookup(local_parent, name)?;
        Ok(Entry {
            attr: self.remap_attr(event, entry.attr)?,
            ..entry
        })
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == self.slot.root_id() || inode_id == self.id_dir() {
            return Ok(hist_dir_attr(inode_id));
        }
        let (event, local) = self.local_of(inode_id)?;
        let attr = self.snapshot(event)?.getattr(local)?;
        self.remap_attr(event, attr)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == self.slot.root_id() {
            let mut entries = vec![
                DirEntry::new(self.slot.root_id(), ".", NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, "..", NodeKind::Directory),
                DirEntry::new(self.id_dir(), "id", NodeKind::Directory),
            ];
            for (event, record) in self.history.log(LIST_LIMIT).map_err(history_error)? {
                let id = self.synthetic_id(event, epoch_vfs_api::ROOT_INODE_ID)?;
                entries.push(DirEntry::new(
                    id,
                    record.timestamp_ms.to_string(),
                    NodeKind::Directory,
                ));
            }
            return Ok(entries);
        }
        if inode_id == self.id_dir() {
            // Intentionally not listable: events are addressable by id only.
            return Ok(vec![
                DirEntry::new(self.id_dir(), ".", NodeKind::Directory),
                DirEntry::new(self.slot.root_id(), "..", NodeKind::Directory),
            ]);
        }
        let (event, local) = self.local_of(inode_id)?;
        let mut entries = self.snapshot(event)?.readdir(local)?;
        for entry in &mut entries {
            entry.inode_id = self.synthetic_id(event, entry.inode_id)?;
        }
        Ok(entries)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if request.inode_id == self.slot.root_id() || request.inode_id == self.id_dir() {
            return Err(VfsError::IsDirectory);
        }
        let (event, local) = self.local_of(request.inode_id)?;
        let opened = self.snapshot(event)?.open(OpenRequest {
            inode_id: local,
            ..request
        })?;
        let handle = FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.write_state()?.handles.insert(handle, (event, opened.handle));
        Ok(OpenedFile { handle, ..opened })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        let (event, local) = self.local_of(request.inode_id)?;
        let (handle_event, local_handle) = self
            .read_state()?
            .handles
            .get(&request.handle)
            .copied()
            .ok_or(VfsError::NotFound)?;
        if handle_event != event {
            return Err(VfsError::InvalidInput(
                "handle does not belong to this snapshot".to_owned(),
            ));
        }
        self.snapshot(event)?.read(ReadRequest {
            inode_id: local,
            handle: local_handle,
            ..request
        })
    }

    fn write(&self, _request: WriteRequest<'_>) -> VfsResult<u32> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn create(&self, _request: CreateRequest) -> VfsResult<CreatedNode> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn mkdir(&self, _request: CreateRequest) -> VfsResult<Entry> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn unlink(&self, _parent: InodeId, _name: &OsStr) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn rmdir(&self, _parent: InodeId, _name: &OsStr) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn truncate(&self, _inode_id: InodeId, _size: u64) -> VfsResult<NodeAttr> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, _inode_id: InodeId, _handle: FileHandle, _datasync: bool) -> VfsResult<()> {
        Ok(())
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        let removed = self.write_state()?.handles.remove(&handle);
        match removed {
            Some((event, local_handle)) => self.snapshot(event)?.release(local_handle),
            None => Ok(()),
        }
    }
}

/// Attr for the fixed `.vfs/history` / `.vfs/history/id` directories.
fn hist_dir_attr(inode_id: InodeId) -> NodeAttr {
    let mut attr = NodeAttr::directory(inode_id, SystemTime::now());
    attr.perm = 0o555;
    attr
}

fn history_error(err: epoch_history::HistoryError) -> VfsError {
    VfsError::Other(format!("history: {err}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Parse a relative name like `500ms-ago` / `30s-ago` / `5m-ago` / `2h-ago` /
/// `1d-ago` into the duration it reaches back, in milliseconds.
fn parse_ago(text: &str) -> Option<u64> {
    let body = text.strip_suffix("-ago")?;
    let unit_at = body.find(|c: char| !c.is_ascii_digit())?;
    let (digits, unit) = body.split_at(unit_at);
    let n: u64 = digits.parse().ok()?;
    let scale = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ago_forms() {
        assert_eq!(parse_ago("500ms-ago"), Some(500));
        assert_eq!(parse_ago("30s-ago"), Some(30_000));
        assert_eq!(parse_ago("5m-ago"), Some(300_000));
        assert_eq!(parse_ago("2h-ago"), Some(7_200_000));
        assert_eq!(parse_ago("1d-ago"), Some(86_400_000));
        assert_eq!(parse_ago("5s"), None, "missing -ago");
        assert_eq!(parse_ago("s-ago"), None, "missing digits");
        assert_eq!(parse_ago("5w-ago"), None, "unknown unit");
        assert_eq!(parse_ago("-ago"), None);
        assert_eq!(parse_ago("12345"), None, "absolute, not relative");
    }
}
