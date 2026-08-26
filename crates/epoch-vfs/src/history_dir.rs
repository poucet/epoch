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
//! - `.vfs/history/head` — a small text file naming the current checkpoint:
//!   its event id, its **root tree hash** (feed it to `.vfs/cas/<hash>` to
//!   browse), and its timestamp.
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
    ffi::OsStr,
    str::FromStr,
    sync::{
        Arc,
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
use crate::feature::{FeatureSlot, cp_file_attr, slice_bytes};
use crate::submount::SubMounts;

/// How many checkpoints a `readdir` of `.vfs/history` lists (newest first).
/// Older checkpoints stay reachable by lookup — listing is a courtesy, not the
/// index.
pub const LIST_LIMIT: usize = 64;

/// The `.vfs/history` sub-service. See the [module docs](self).
pub struct HistoryDir<Near, Far> {
    history: Arc<History<Near, Far>>,
    /// The feature's id slice and `.vfs` parent (see [`crate::feature`]):
    /// `slot.root_id()` is `.vfs/history` itself, `slot.base + 1` the `id/`
    /// subdirectory, `slot.base + 2` the `head` file, and dynamic snapshot
    /// ids/handles are minted from `slot.base + 3` upward.
    slot: FeatureSlot,
    /// The overlay's lower filesystem, when the mount is an overlay. Each
    /// snapshot rehydrates its checkpointed **delta** over this lower, so a
    /// historical view is "the overlay state as of the checkpoint, over the
    /// lower as it is *now*" — faithful for the CAS-owned delta, but subject
    /// to lower drift until `OpaqueTree` ingestion makes checkpoints fully
    /// self-contained.
    lower: Option<Arc<dyn VfsService>>,
    /// The per-checkpoint read-only snapshot mounts, remapped into this
    /// feature's slice (see [`SubMounts`]).
    subs: SubMounts<EventId>,
    /// Shared id/handle allocator (also used by `subs`).
    next_id: Arc<AtomicU64>,
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
        let next_id = Arc::new(AtomicU64::new(slot.base + 3));
        Self {
            history,
            slot,
            lower,
            subs: SubMounts::new(Arc::clone(&next_id)),
            next_id,
        }
    }

    /// The `.vfs/history/id` directory node.
    fn id_dir(&self) -> InodeId {
        InodeId(self.slot.base + 1)
    }

    /// The `.vfs/history/head` file node.
    fn head_file(&self) -> InodeId {
        InodeId(self.slot.base + 2)
    }

    /// Render `.vfs/history/head`: the current checkpoint's identity, giving
    /// shell users a root tree hash to feed straight into `.vfs/cas/`.
    fn render_head(&self) -> VfsResult<Vec<u8>> {
        let text = match self.history.head().map_err(history_error)? {
            None => "no checkpoints yet\n".to_owned(),
            Some(id) => {
                let event = self
                    .history
                    .event(&id)
                    .map_err(history_error)?
                    .ok_or(VfsError::NotFound)?;
                format!(
                    "event: {}\nroot: {}\ntimestamp_ms: {}\n",
                    id.to_hex(),
                    event.root.to_hex(),
                    event.timestamp_ms
                )
            }
        };
        Ok(text.into_bytes())
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

    /// Get-or-mount the read-only snapshot filesystem for `event`.
    fn snapshot(&self, event: EventId) -> VfsResult<Arc<dyn VfsService>> {
        self.subs.mount(event, || {
            let record = self
                .history
                .event(&event)
                .map_err(history_error)?
                .ok_or(VfsError::NotFound)?;
            let far = self.history.far().clone();
            let vfs: Arc<dyn VfsService> = match &self.lower {
                // Overlay mount: the checkpointed root is a sparse delta —
                // rehydrate it over the (shared) lower.
                Some(lower) => Arc::new(
                    CasVfs::from_overlay(far, Arc::clone(lower), record.root)
                        .map_err(|_| VfsError::Other("snapshot root unreadable".to_owned()))?,
                ),
                None => Arc::new(
                    CasVfs::from_root(far, record.root)
                        .map_err(|_| VfsError::Other("snapshot root unreadable".to_owned()))?,
                ),
            };
            Ok(vfs)
        })
    }

    /// The root entry of `event`'s snapshot, as seen from `.vfs/history`.
    fn snapshot_root_entry(&self, event: EventId, relative: bool) -> VfsResult<Entry> {
        self.snapshot(event)?;
        let mut entry = self.subs.root_entry(event)?;
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
            if name == OsStr::new("head") {
                let size = self.render_head()?.len() as u64;
                return Ok(Entry::new(cp_file_attr(self.head_file(), size)));
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
        let (event, local_parent) = self.subs.local_of(parent)?.ok_or(VfsError::NotFound)?;
        self.subs.lookup(event, local_parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == self.slot.root_id() || inode_id == self.id_dir() {
            return Ok(hist_dir_attr(inode_id));
        }
        if inode_id == self.head_file() {
            return Ok(cp_file_attr(inode_id, self.render_head()?.len() as u64));
        }
        let (event, local) = self.subs.local_of(inode_id)?.ok_or(VfsError::NotFound)?;
        self.subs.getattr(event, local)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == self.slot.root_id() {
            let mut entries = vec![
                DirEntry::new(self.slot.root_id(), ".", NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, "..", NodeKind::Directory),
                DirEntry::new(self.id_dir(), "id", NodeKind::Directory),
                DirEntry::new(self.head_file(), "head", NodeKind::RegularFile),
            ];
            for (event, record) in self.history.log(LIST_LIMIT).map_err(history_error)? {
                let id = self.subs.synthetic_id(event, epoch_vfs_api::ROOT_INODE_ID)?;
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
        let (event, local) = self.subs.local_of(inode_id)?.ok_or(VfsError::NotFound)?;
        self.subs.readdir(event, local)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if request.inode_id == self.slot.root_id() || request.inode_id == self.id_dir() {
            return Err(VfsError::IsDirectory);
        }
        if request.inode_id == self.head_file() {
            // Content renders fresh per read; the handle carries no state.
            return Ok(OpenedFile {
                handle: FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed)),
                direct_io: false,
                keep_cache: false,
            });
        }
        let (event, local) = self.subs.local_of(request.inode_id)?.ok_or(VfsError::NotFound)?;
        self.subs.open(event, local, request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if request.inode_id == self.head_file() {
            let content = self.render_head()?;
            return slice_bytes(&content, request.offset, request.size);
        }
        let (event, local) = self.subs.local_of(request.inode_id)?.ok_or(VfsError::NotFound)?;
        self.subs.read(event, local, request)
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
        self.subs.release(handle)
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
