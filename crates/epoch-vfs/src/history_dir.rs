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
//! - `.vfs/history/head` — exactly the head checkpoint's **root tree hash**
//!   (empty before the first checkpoint): `ls .vfs/cas/$(cat .vfs/history/head)`.
//!   Note the raw object is the sparse *delta*; for the merged view use:
//! - `.vfs/history/latest` — the head checkpoint's **merged** snapshot (delta
//!   rehydrated over the lower), i.e. the filesystem as of the last checkpoint.
//! - `.vfs/history/id/<64-hex event id>` — any event, on this head's chain or
//!   not. `id/` itself is intentionally not listable (ids are addressable, not
//!   enumerable). Listing `.vfs/history` shows the newest checkpoints (up to
//!   [`LIST_LIMIT`]) by their millisecond names.
//!
//! Each resolved checkpoint resolves to its root `TreeId` in the **shared**
//! [`TreeMounts`] table owned by the `.vfs/cas` portal — the same instance and
//! id space whether a tree is reached by time, by event id, or by hash.
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
    DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind, OpenRequest, OpenedFile,
    ReadRequest, VfsError, VfsResult,
};

use crate::feature::{FeatureSlot, ReadOnlyFeature, cp_file_attr, slice_bytes};
use crate::submount::{TreeBrowseFeature, TreeMounts};

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
    /// The **shared** tree-mount table (owned by the `.vfs/cas` portal
    /// feature): this feature only *resolves* time/event names to root
    /// [`TreeId`]s and hands out entries from there, so a checkpoint browsed
    /// through history and through the portal is one filesystem with one id
    /// space. Every id under a snapshot lives in the portal's slice and is
    /// routed there by the control plane — this feature never serves them.
    mounts: Arc<TreeMounts>,
    /// Allocator for this feature's own (stateless) `head`-file handles.
    next_id: Arc<AtomicU64>,
}

impl<Near, Far> HistoryDir<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone + 'static,
    Far: SyncByteBackend<CASHash> + Clone + 'static,
{
    /// Build over the mount's [`History`] in the given feature `slot`,
    /// resolving into the `.vfs/cas` portal's shared [`TreeMounts`] table
    /// (which already knows the mount's lower, if any).
    pub fn new(
        history: Arc<History<Near, Far>>,
        slot: FeatureSlot,
        mounts: Arc<TreeMounts>,
    ) -> Self {
        Self {
            history,
            slot,
            mounts,
            next_id: Arc::new(AtomicU64::new(slot.base + 3)),
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

    /// Render `.vfs/history/head`: exactly the head checkpoint's **root tree
    /// hash** (newline-terminated), so it composes directly with the object
    /// portal: `ls .vfs/cas/$(cat .vfs/history/head)`. Empty before the first
    /// checkpoint.
    fn render_head(&self) -> VfsResult<Vec<u8>> {
        let text = match self.history.head().map_err(history_error)? {
            None => String::new(),
            Some(id) => {
                let event = self
                    .history
                    .event(&id)
                    .map_err(history_error)?
                    .ok_or(VfsError::NotFound)?;
                format!("{}\n", event.root.to_hex())
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

    /// The checkpointed root [`TreeId`] of `event`.
    fn root_of(&self, event: EventId) -> VfsResult<epoch_tree::TreeId> {
        Ok(self
            .history
            .event(&event)
            .map_err(history_error)?
            .ok_or(VfsError::NotFound)?
            .root)
    }

    /// The root entry of `event`'s snapshot, as seen from `.vfs/history`.
    fn snapshot_root_entry(&self, event: EventId, relative: bool) -> VfsResult<Entry> {
        let mut entry = self.mounts.root_entry(self.root_of(event)?)?;
        if relative {
            // "5s-ago" names a different checkpoint five seconds from now —
            // forbid the kernel from caching the resolution.
            entry.ttl = Duration::ZERO;
        }
        Ok(entry)
    }
}

impl<Near, Far> TreeBrowseFeature for HistoryDir<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone + 'static,
    Far: SyncByteBackend<CASHash> + Clone + 'static,
{
    fn mounts(&self) -> &TreeMounts {
        &self.mounts
    }

    fn lookup_own(&self, parent: InodeId, name: &OsStr) -> VfsResult<Option<Entry>> {
        if parent == self.slot.root_id() {
            if name == OsStr::new("id") {
                return Ok(Some(Entry::new(hist_dir_attr(self.id_dir()))));
            }
            if name == OsStr::new("head") {
                let size = self.render_head()?.len() as u64;
                return Ok(Some(Entry::new(cp_file_attr(self.head_file(), size))));
            }
            if name == OsStr::new("latest") {
                // The merged view of the head checkpoint (delta over lower).
                // TTL 0: "latest" names a different snapshot after the next
                // checkpoint, so the kernel must re-resolve it.
                let head = self
                    .history
                    .head()
                    .map_err(history_error)?
                    .ok_or(VfsError::NotFound)?;
                return self.snapshot_root_entry(head, true).map(Some);
            }
            return match self.resolve_time_name(name)? {
                Some((event, relative)) => self.snapshot_root_entry(event, relative).map(Some),
                None => Err(VfsError::NotFound),
            };
        }
        if parent == self.id_dir() {
            let text = name.to_str().ok_or(VfsError::NotFound)?;
            let event = EventId::from_str(text).map_err(|_| VfsError::NotFound)?;
            return self.snapshot_root_entry(event, false).map(Some);
        }
        // Snapshot-internal ids live in the portal's slice; the control plane
        // never routes them here.
        Ok(None)
    }

    fn getattr_own(&self, inode_id: InodeId) -> VfsResult<Option<NodeAttr>> {
        if inode_id == self.slot.root_id() || inode_id == self.id_dir() {
            return Ok(Some(hist_dir_attr(inode_id)));
        }
        if inode_id == self.head_file() {
            return Ok(Some(cp_file_attr(
                inode_id,
                self.render_head()?.len() as u64,
            )));
        }
        Ok(None)
    }

    fn readdir_own(&self, inode_id: InodeId) -> VfsResult<Option<Vec<DirEntry>>> {
        if inode_id == self.slot.root_id() {
            let mut entries = vec![
                DirEntry::new(self.slot.root_id(), ".", NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, "..", NodeKind::Directory),
                DirEntry::new(self.id_dir(), "id", NodeKind::Directory),
                DirEntry::new(self.head_file(), "head", NodeKind::RegularFile),
            ];
            if let Some(head) = self.history.head().map_err(history_error)? {
                let id = self.mounts.root_id(self.root_of(head)?)?;
                entries.push(DirEntry::new(id, "latest", NodeKind::Directory));
            }
            for (_event, record) in self.history.log(LIST_LIMIT).map_err(history_error)? {
                let id = self.mounts.root_id(record.root)?;
                entries.push(DirEntry::new(
                    id,
                    record.timestamp_ms.to_string(),
                    NodeKind::Directory,
                ));
            }
            return Ok(Some(entries));
        }
        if inode_id == self.id_dir() {
            // Intentionally not listable: events are addressable by id only.
            return Ok(Some(vec![
                DirEntry::new(self.id_dir(), ".", NodeKind::Directory),
                DirEntry::new(self.slot.root_id(), "..", NodeKind::Directory),
            ]));
        }
        Ok(None)
    }

    fn open_own(&self, request: OpenRequest) -> VfsResult<Option<OpenedFile>> {
        if request.inode_id == self.slot.root_id() || request.inode_id == self.id_dir() {
            return Err(VfsError::IsDirectory);
        }
        if request.inode_id == self.head_file() {
            // Content renders fresh per read; the handle carries no state.
            return Ok(Some(OpenedFile {
                handle: FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed)),
                direct_io: false,
                keep_cache: false,
            }));
        }
        Ok(None)
    }

    fn read_own(&self, request: ReadRequest) -> VfsResult<Option<Vec<u8>>> {
        if request.inode_id == self.head_file() {
            let content = self.render_head()?;
            return slice_bytes(&content, request.offset, request.size).map(Some);
        }
        Ok(None)
    }
}

impl<Near, Far> ReadOnlyFeature for HistoryDir<Near, Far>
where
    Near: SyncByteBackend<CASHash> + Clone + 'static,
    Far: SyncByteBackend<CASHash> + Clone + 'static,
{
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        self.serve_lookup(parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        self.serve_getattr(inode_id)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        self.serve_readdir(inode_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        self.serve_open(request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        self.serve_read(request)
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        self.serve_release(handle)
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
