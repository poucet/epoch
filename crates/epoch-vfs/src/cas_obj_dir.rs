//! `.vfs/cas` — the **typed** hash-addressed object portal, as a control-plane
//! feature.
//!
//! `lookup` of a 64-hex content hash under `.vfs/cas/` resolves the object by
//! its [`object_kind`] domain tag (the first byte of the canonical encoding,
//! hashed into the id — so type confusion is structurally impossible):
//!
//! - a **tree** hash appears as a read-only *directory* — the main
//!   [`CasVfs`] filesystem machinery mounted over that tree via
//!   [`SubMounts`], browsable like any filesystem;
//! - a **file** hash appears as a read-only *file* whose contents are the
//!   blob's bytes (`cat`-able), served from the [`FileCas`] manifest/chunk
//!   stores;
//! - **chunk** hashes and unknown/non-hex names are `NotFound` (a raw
//!   fragment is meaningless alone).
//!
//! The portal reads through the mount's full tier (staging *and* durable), so
//! even not-yet-checkpointed objects are inspectable. The directory itself is
//! intentionally **not listable** — content-addressed objects are addressable,
//! not enumerable — and the whole subtree is strictly read-only.
//!
//! On an overlay mount, tree objects are rehydrated **over the lower** (like
//! `.vfs/history` snapshots), so browsing a checkpoint's root hash shows the
//! merged filesystem, tombstones and fall-through included. Quirk: a hash of
//! an interior *subtree* also merges against the lower's root — a harmless
//! (read-only) oddity of overlay semantics. Without a lower, objects are
//! shown standalone (delta content only).

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use epoch_cas::{CASHash, SyncByteBackend, decompress, object_kind};
use epoch_file_cas::{BlobId, FileCas};
use epoch_tree::TreeId;
use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind, OpenMode,
    OpenRequest, OpenedFile, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
};

use crate::cas_vfs::CasVfs;
use crate::feature::{FeatureSlot, control_dir_attr, cp_file_attr};
use crate::submount::SubMounts;

/// The `.vfs/cas` typed object portal. See the [module docs](self).
pub struct CasObjectDir<Bk> {
    slot: FeatureSlot,
    files: FileCas<Bk>,
    backend: Bk,
    /// The overlay's lower filesystem, when the mount is an overlay: browsed
    /// trees rehydrate over it (see the [module docs](self)).
    lower: Option<Arc<dyn VfsService>>,
    /// Shared id/handle allocator: tree sub-mounts and blob nodes mint from
    /// the same slice-based counter.
    next_id: Arc<AtomicU64>,
    /// Browsed tree objects, mounted read-only via the main [`CasVfs`]
    /// machinery and remapped into this feature's slice.
    trees: SubMounts<TreeId>,
    blobs: RwLock<BlobState>,
}

/// Blob file nodes: stable synthetic id per browsed blob.
#[derive(Default)]
struct BlobState {
    node_of: HashMap<BlobId, InodeId>,
    blob_of: HashMap<InodeId, BlobId>,
    /// Blob read handles are stateless; the set only tracks liveness.
    handles: HashMap<FileHandle, BlobId>,
}

impl<Bk> CasObjectDir<Bk>
where
    Bk: SyncByteBackend<CASHash> + Clone + 'static,
{
    /// Build the portal in `slot` over the mount's shared `backend` (a clone
    /// of the same tier the live tree writes into). Pass the overlay's
    /// `lower` when the mount overlays one, so browsed trees show the merged
    /// view.
    pub fn new(
        slot: FeatureSlot,
        backend: Bk,
        lower: Option<Arc<dyn VfsService>>,
    ) -> CasObjectDir<Bk> {
        let next_id = Arc::new(AtomicU64::new(slot.base + 1));
        CasObjectDir {
            slot,
            files: FileCas::new(backend.clone()),
            backend,
            lower,
            trees: SubMounts::new(Arc::clone(&next_id)),
            next_id,
            blobs: RwLock::new(BlobState::default()),
        }
    }

    fn read_blobs(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, BlobState>> {
        self.blobs
            .read()
            .map_err(|_| VfsError::Other("cas portal read lock poisoned".to_owned()))
    }

    fn write_blobs(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, BlobState>> {
        self.blobs
            .write()
            .map_err(|_| VfsError::Other("cas portal write lock poisoned".to_owned()))
    }

    /// Peek an object's [`object_kind`] tag: fetch the stored bytes and read
    /// the first canonical byte (objects are stored zstd-compressed).
    fn kind_of(&self, hash: &CASHash) -> VfsResult<Option<u8>> {
        let Some(stored) = self.backend.get(hash).map_err(|_| VfsError::NotFound)? else {
            return Ok(None);
        };
        let canonical = decompress(&stored).map_err(|_| VfsError::NotFound)?;
        Ok(canonical.first().copied())
    }

    /// The stable synthetic file node for a blob (minted on first use).
    fn blob_node(&self, blob: BlobId) -> VfsResult<InodeId> {
        if let Some(id) = self.read_blobs()?.node_of.get(&blob) {
            return Ok(*id);
        }
        let mut state = self.write_blobs()?;
        if let Some(id) = state.node_of.get(&blob) {
            return Ok(*id);
        }
        let id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.node_of.insert(blob, id);
        state.blob_of.insert(id, blob);
        Ok(id)
    }

    /// A blob file's attrs (blobs are immutable, so the size never goes stale).
    fn blob_attr(&self, id: InodeId, blob: BlobId) -> VfsResult<NodeAttr> {
        let size = self
            .files
            .size(&blob)
            .map_err(|_| VfsError::NotFound)?
            .ok_or(VfsError::NotFound)?;
        Ok(cp_file_attr(id, size))
    }

    /// Resolve a 64-hex name to a typed object entry, or `NotFound`.
    fn resolve_hash(&self, name: &OsStr) -> VfsResult<Entry> {
        let hash = name
            .to_str()
            .and_then(|text| CASHash::from_str(text).ok())
            .ok_or(VfsError::NotFound)?;

        match self.kind_of(&hash)? {
            Some(object_kind::TREE) => {
                let root = TreeId::from_cas_hash(hash);
                let backend = self.backend.clone();
                let lower = self.lower.clone();
                self.trees.mount(root, || {
                    match lower {
                        // Overlay mount: show the merged view, like the live
                        // tree and history snapshots do.
                        Some(lower) => CasVfs::from_overlay(backend, lower, root),
                        None => CasVfs::from_root(backend, root),
                    }
                    .map(|vfs| -> Arc<dyn VfsService> { Arc::new(vfs) })
                    .map_err(|_| VfsError::NotFound)
                })?;
                self.trees.root_entry(root)
            }
            Some(object_kind::FILE) => {
                let blob = BlobId::from_cas_hash(hash);
                let id = self.blob_node(blob)?;
                Ok(Entry::new(self.blob_attr(id, blob)?))
            }
            // Chunks (and any future kind) are internal fragments: unexposed.
            _ => Err(VfsError::NotFound),
        }
    }

    fn blob_of(&self, id: InodeId) -> VfsResult<Option<BlobId>> {
        Ok(self.read_blobs()?.blob_of.get(&id).copied())
    }
}

impl<Bk> VfsService for CasObjectDir<Bk>
where
    Bk: SyncByteBackend<CASHash> + Clone + 'static,
{
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if parent == self.slot.root_id() {
            return self.resolve_hash(name);
        }
        let (root, local) = self.trees.local_of(parent)?.ok_or(VfsError::NotFound)?;
        self.trees.lookup(root, local, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == self.slot.root_id() {
            return Ok(control_dir_attr(inode_id));
        }
        if let Some((root, local)) = self.trees.local_of(inode_id)? {
            return self.trees.getattr(root, local);
        }
        let blob = self.blob_of(inode_id)?.ok_or(VfsError::NotFound)?;
        self.blob_attr(inode_id, blob)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == self.slot.root_id() {
            // Intentionally not listable: objects are addressable by hash only.
            return Ok(vec![
                DirEntry::new(inode_id, OsString::from("."), NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, OsString::from(".."), NodeKind::Directory),
            ]);
        }
        if let Some((root, local)) = self.trees.local_of(inode_id)? {
            return self.trees.readdir(root, local);
        }
        if self.blob_of(inode_id)?.is_some() {
            return Err(VfsError::NotDirectory);
        }
        Err(VfsError::NotFound)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        if request.inode_id == self.slot.root_id() {
            return Err(VfsError::IsDirectory);
        }
        if let Some((root, local)) = self.trees.local_of(request.inode_id)? {
            return self.trees.open(root, local, request);
        }
        let blob = self.blob_of(request.inode_id)?.ok_or(VfsError::NotFound)?;
        let handle = FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.write_blobs()?.handles.insert(handle, blob);
        Ok(OpenedFile {
            handle,
            direct_io: false,
            keep_cache: true,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if let Some((root, local)) = self.trees.local_of(request.inode_id)? {
            return self.trees.read(root, local, request);
        }
        let blob = self.blob_of(request.inode_id)?.ok_or(VfsError::NotFound)?;
        self.files
            .read_at(&blob, request.offset, request.size)
            .map(|bytes| bytes.to_vec())
            .map_err(|_| VfsError::NotFound)
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
        if self.write_blobs()?.handles.remove(&handle).is_some() {
            return Ok(());
        }
        self.trees.release(handle)
    }
}
