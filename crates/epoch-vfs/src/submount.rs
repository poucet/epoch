//! [`SubMounts`] — read-only sub-filesystems remapped into a feature's id
//! slice.
//!
//! Several control-plane features present whole filesystems *inside* their
//! synthetic directory: `.vfs/history/<time>` mounts a checkpoint snapshot,
//! `.vfs/cas/<hash>` mounts a tree object. The sub-filesystem is always the
//! **main [`VfsService`] machinery itself** (a [`CasVfs`](crate::CasVfs) over
//! the shared backend) — nothing is reimplemented — but its session-scoped
//! inode ids and handles collide across instances and with the live tree, and
//! it is technically writable. This helper owns the whole recurring apparatus,
//! keyed by whatever identifies a sub-mount (`EventId`, `TreeId`, …):
//!
//! - lazy **get-or-mount** construction, one shared instance per key;
//! - stable bidirectional `(key, local id) ↔ synthetic id` maps, minted from
//!   the owning feature's allocator (so everything stays inside its
//!   [`FeatureSlot`](crate::feature::FeatureSlot) slice), and the same for
//!   file handles;
//! - remapping of every id in every result (entry attrs, `getattr`, `readdir`
//!   rows), with write permission bits stripped;
//! - **read-only enforcement**: non-read opens are refused and mutating ops
//!   never reach the inner service, so the (writable) `CasVfs` inside is
//!   unreachable for writes.
//!
//! Sub-mount instances and id mappings are never evicted — the same deferred
//! bounded-memory problem class as FUSE-`forget` inode eviction.

use std::{
    collections::HashMap,
    ffi::OsStr,
    hash::Hash,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use epoch_tree::TreeId;
use epoch_vfs_api::{
    DirEntry, Entry, FileHandle, InodeId, NodeAttr, OpenMode, OpenRequest, OpenedFile,
    ROOT_INODE_ID, ReadRequest, VfsError, VfsResult, VfsService,
};

/// The keyed sub-mount table. See the [module docs](self).
pub(crate) struct SubMounts<K> {
    /// The owning feature's id/handle allocator (already slice-based).
    next_id: Arc<AtomicU64>,
    state: RwLock<SubState<K>>,
}

struct SubState<K> {
    /// Lazily-built sub-filesystems, one shared instance per key.
    mounts: HashMap<K, Arc<dyn VfsService>>,
    /// Synthetic id → (key, sub-mount-local id).
    to_local: HashMap<InodeId, (K, InodeId)>,
    /// (key, sub-mount-local id) → synthetic id (minting idempotence).
    from_local: HashMap<(K, InodeId), InodeId>,
    /// Synthetic handle → (key, sub-mount-local handle).
    handles: HashMap<FileHandle, (K, FileHandle)>,
}

impl<K> SubMounts<K>
where
    K: Eq + Hash + Copy,
{
    /// Build over the owning feature's id allocator.
    pub fn new(next_id: Arc<AtomicU64>) -> SubMounts<K> {
        SubMounts {
            next_id,
            state: RwLock::new(SubState {
                mounts: HashMap::new(),
                to_local: HashMap::new(),
                from_local: HashMap::new(),
                handles: HashMap::new(),
            }),
        }
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, SubState<K>>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("sub-mount read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, SubState<K>>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("sub-mount write lock poisoned".to_owned()))
    }

    /// The sub-filesystem for `key`, built with `build` on first use (racing
    /// builders keep whichever instance landed first, so every caller shares
    /// one).
    pub fn mount(
        &self,
        key: K,
        build: impl FnOnce() -> VfsResult<Arc<dyn VfsService>>,
    ) -> VfsResult<Arc<dyn VfsService>> {
        if let Some(vfs) = self.read_state()?.mounts.get(&key) {
            return Ok(Arc::clone(vfs));
        }
        let vfs = build()?;
        let mut state = self.write_state()?;
        Ok(Arc::clone(state.mounts.entry(key).or_insert(vfs)))
    }

    fn mounted(&self, key: K) -> VfsResult<Arc<dyn VfsService>> {
        self.read_state()?
            .mounts
            .get(&key)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    /// The stable synthetic id for a sub-mount-local id (minted on first use).
    pub fn synthetic_id(&self, key: K, local: InodeId) -> VfsResult<InodeId> {
        if let Some(id) = self.read_state()?.from_local.get(&(key, local)) {
            return Ok(*id);
        }
        let mut state = self.write_state()?;
        if let Some(id) = state.from_local.get(&(key, local)) {
            return Ok(*id);
        }
        let id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.from_local.insert((key, local), id);
        state.to_local.insert(id, (key, local));
        Ok(id)
    }

    /// The `(key, local id)` a synthetic id stands for, or `None` if the id
    /// was not minted here (the caller's own fixed nodes, a blob node, …).
    pub fn local_of(&self, id: InodeId) -> VfsResult<Option<(K, InodeId)>> {
        Ok(self.read_state()?.to_local.get(&id).copied())
    }

    /// Rewrite a sub-mount-local attr into the feature's id space, stripping
    /// write permission bits (sub-mounts are immutable views).
    pub fn remap_attr(&self, key: K, mut attr: NodeAttr) -> VfsResult<NodeAttr> {
        attr.inode_id = self.synthetic_id(key, attr.inode_id)?;
        attr.perm &= 0o555;
        Ok(attr)
    }

    /// The remapped root entry of `key`'s sub-mount (its `.vfs/<feature>/<name>`
    /// directory entry).
    pub fn root_entry(&self, key: K) -> VfsResult<Entry> {
        let attr = self.mounted(key)?.getattr(ROOT_INODE_ID)?;
        Ok(Entry::new(self.remap_attr(key, attr)?))
    }

    /// `lookup` inside a sub-mount (the parent is a synthetic id minted here).
    pub fn lookup(&self, key: K, local_parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        let entry = self.mounted(key)?.lookup(local_parent, name)?;
        Ok(Entry {
            attr: self.remap_attr(key, entry.attr)?,
            ..entry
        })
    }

    /// `getattr` on a synthetic id minted here.
    pub fn getattr(&self, key: K, local: InodeId) -> VfsResult<NodeAttr> {
        let attr = self.mounted(key)?.getattr(local)?;
        self.remap_attr(key, attr)
    }

    /// `readdir` of a sub-mount directory, every row remapped.
    pub fn readdir(&self, key: K, local: InodeId) -> VfsResult<Vec<DirEntry>> {
        let mut entries = self.mounted(key)?.readdir(local)?;
        for entry in &mut entries {
            entry.inode_id = self.synthetic_id(key, entry.inode_id)?;
        }
        Ok(entries)
    }

    /// Read-only `open` inside a sub-mount; mints and maps a synthetic handle.
    pub fn open(&self, key: K, local: InodeId, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        let opened = self.mounted(key)?.open(OpenRequest {
            inode_id: local,
            ..request
        })?;
        let handle = FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.write_state()?.handles.insert(handle, (key, opened.handle));
        Ok(OpenedFile { handle, ..opened })
    }

    /// `read` through a handle minted by [`open`](Self::open); validates the
    /// handle belongs to the same sub-mount as the inode.
    pub fn read(&self, key: K, local: InodeId, request: ReadRequest) -> VfsResult<Vec<u8>> {
        let local_handle = match self.read_state()?.handles.get(&request.handle) {
            Some((handle_key, local_handle)) if *handle_key == key => *local_handle,
            _ => return Err(VfsError::NotFound),
        };
        self.mounted(key)?.read(ReadRequest {
            inode_id: local,
            handle: local_handle,
            ..request
        })
    }

    /// Release a handle minted here (a no-op for foreign handles, so callers
    /// can forward releases blindly).
    pub fn release(&self, handle: FileHandle) -> VfsResult<()> {
        let removed = self.write_state()?.handles.remove(&handle);
        match removed {
            Some((key, local_handle)) => self.mounted(key)?.release(local_handle),
            None => Ok(()),
        }
    }
}

/// The **shared tree-mount table**: read-only filesystems keyed by root
/// [`TreeId`], one instance per root no matter who asked.
///
/// Both `.vfs/cas/<tree-hash>` and every `.vfs/history` snapshot are the same
/// thing — the main [`CasVfs`](crate::CasVfs) machinery hydrated from a root
/// `TreeId` (over the mount's lower, when it has one). This table makes them
/// literally share: the history feature *resolves* a time/event name to a
/// root and hands out entries from here, so browsing a checkpoint through
/// history and through the portal yields the same instance and the same
/// synthetic inode ids (minted in the owning feature's slice, which is where
/// the control plane routes every operation on them).
pub struct TreeMounts {
    /// Builds the filesystem for a root (captures backend + optional lower).
    build: Box<dyn Fn(TreeId) -> VfsResult<Arc<dyn VfsService>> + Send + Sync>,
    subs: SubMounts<TreeId>,
}

impl TreeMounts {
    /// Build over the owning feature's allocator and a per-root constructor.
    pub(crate) fn new(
        next_id: Arc<AtomicU64>,
        build: impl Fn(TreeId) -> VfsResult<Arc<dyn VfsService>> + Send + Sync + 'static,
    ) -> TreeMounts {
        TreeMounts {
            build: Box::new(build),
            subs: SubMounts::new(next_id),
        }
    }

    /// The underlying keyed sub-mount ops.
    pub(crate) fn subs(&self) -> &SubMounts<TreeId> {
        &self.subs
    }

    /// Get-or-mount `root`, returning its remapped root [`Entry`].
    pub(crate) fn root_entry(&self, root: TreeId) -> VfsResult<Entry> {
        self.subs.mount(root, || (self.build)(root))?;
        self.subs.root_entry(root)
    }

    /// Get-or-mount `root`, returning the stable synthetic id of its root
    /// directory (for `readdir` listings).
    pub(crate) fn root_id(&self, root: TreeId) -> VfsResult<InodeId> {
        self.subs.mount(root, || (self.build)(root))?;
        self.subs.synthetic_id(root, ROOT_INODE_ID)
    }
}
