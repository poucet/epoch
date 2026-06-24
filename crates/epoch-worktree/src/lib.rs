    //! # epoch-worktree — the mutable copy-on-write overlay
//!
//! `WorkingTree` is the live, mutable view that sits *above* the immutable
//! content-addressed object store (`epoch-file-cas` + `epoch-tree` +
//! `epoch-tree-cas`) and *below* a VFS/FUSE adapter. It is the home of the two
//! things the object layer deliberately lacks:
//!
//! - **mutable identity** — stable [`NodeId`] inodes that survive edits, so a
//!   file keeps its identity while its content (and thus its `BlobId`) changes;
//! - **the read/write bridge** — [`from_root`](WorkingTree::from_root) turns a
//!   [`TreeId`] into a tree of inodes, and [`seal`](WorkingTree::seal) walks the
//!   dirty inodes back down into fresh immutable objects, returning a new root
//!   [`TreeId`].
//!
//! ## ONE service, two modes
//!
//! `WorkingTree` is *the* copy-on-write overlay. It is a single type that holds
//! an **upper spine** (the actively-mutated inodes), **tombstones** (whiteouts
//! that hide lower entries), and an optional **lower** [`VfsService`] it falls
//! through to for everything it has not itself materialized:
//!
//! - **pure mode** (`lower = None`) — there is no base layer; the working tree
//!   *is* the whole tree. [`new`](WorkingTree::new) /
//!   [`from_root`](WorkingTree::from_root) build this. This is the original
//!   pure-tree behaviour, unchanged.
//! - **overlay mode** (`lower = Some(..)`) — the working tree is a *sparse*
//!   copy-on-write layer over a lower service. [`overlay`](WorkingTree::overlay)
//!   starts an empty layer; [`from_overlay`](WorkingTree::from_overlay)
//!   rehydrates a persisted delta on top of a lower. Only the actively-mutated
//!   spine is stored in the upper; every untouched name falls through to the
//!   lower.
//!
//! The overlay **owns the [`NodeId`] space** (allocated from a monotonic
//! counter). Lower-service node ids live only *inside* the overlay's nodes
//! ([`LowerFile`](Body::LowerFile) and [`Dir::lower`](Body::Dir)); whenever an
//! attribute or dir-entry is derived from the lower, its `node_id` is **remapped**
//! to the overlay's id, so a lower id is never leaked to a caller and a node's
//! identity is stable across repeated lookups (and matches `readdir`).
//!
//! It is intentionally **FUSE-independent** (it speaks the `epoch-vfs-api` types
//! but knows nothing of the kernel protocol) so it can be unit-tested on its own.
//!
//! ## First-cut scope (v1)
//!
//! - Inodes are **session-scoped**: allocated from a monotonic counter, stable
//!   for the life of this `WorkingTree`, not persisted and not preserved across
//!   history. Hardlinks, a durable inode table, and per-inode uid/gid/timestamps
//!   are future work; timestamps for upper nodes are reported as [`UNIX_EPOCH`]
//!   (a `LowerFile` reports the lower's own timestamps).
//! - [`from_root`](WorkingTree::from_root) / [`from_overlay`](WorkingTree::from_overlay)
//!   hydrate the **upper spine** eagerly; lower entries are materialized lazily
//!   on first lookup/readdir.
//! - [`read_at`](WorkingTree::read_at) on a fall-through file opens/reads/releases
//!   the lower per call in v1; a per-handle cache is a later optimization.
//! - [`seal`](WorkingTree::seal) is **sparse**: only the materialized spine and
//!   tombstones are emitted; fall-through directories that produced nothing emit
//!   nothing, so untouched siblings are absent from the delta.
//! - Symlinks are out of scope for v1.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use epoch_cas::{Bytes, CASError, SyncByteBackend, SyncCAS};
use epoch_file_cas::{BlobId, FileCas, FileWriter};
use epoch_tree::{EntryId, Tree, TreeEntry, TreeStore};
use epoch_vfs_api::{
    DirEntry, NodeAttr, NodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, VfsError, VfsResult,
    VfsService, ROOT_NODE_ID,
};

/// Re-exported so consumers of [`WorkingTree`] can name the sealed-tree id type
/// — returned by [`seal`](WorkingTree::seal), taken by
/// [`from_root`](WorkingTree::from_root) — without also depending on `epoch-tree`.
pub use epoch_tree::TreeId;

/// A mutable, in-memory copy-on-write overlay backed by content-addressed
/// storage.
///
/// Generic over the byte backend `Bk`. Upper files are stored via [`FileCas`]
/// and addressed by [`BlobId`]; directories are sealed into [`Tree<BlobId>`]
/// objects via [`TreeStore`]. With `lower = Some(..)` the tree is a sparse overlay
/// over that [`VfsService`]; with `lower = None` it is a complete tree. See the
/// [crate docs](crate) for the layer's role and v1 scope.
pub struct WorkingTree<Bk> {
    files: FileCas<Bk>,
    trees: TreeStore<Bk, BlobId>,
    /// The overlay base. `None` is *pure mode* (today's behaviour): no lower
    /// layer, the working tree is the whole tree.
    lower: Option<Arc<dyn VfsService>>,
    inodes: HashMap<NodeId, Inode>,
    /// Next inode number to hand out (monotonic; starts just past the root).
    next: u64,
}

/// One node in the live tree: its mode bits and kind-specific body.
struct Inode {
    /// Unix mode bits as supplied by the caller / read from the sealed entry or
    /// the lower's attrs, preserved verbatim across seal/hydrate.
    mode: u32,
    body: Body,
}

enum Body {
    /// A directory in the upper spine. `children` caches **all** resolved
    /// children (upper values, tombstones, and cached fall-throughs). `lower`
    /// is the corresponding lower-service directory [`NodeId`] this dir overlays
    /// (`None` if the dir has no lower counterpart, e.g. a freshly `mkdir`'d
    /// dir).
    Dir {
        children: BTreeMap<OsString, NodeId>,
        lower: Option<NodeId>,
    },
    /// An upper-materialized regular file: `Clean(BlobId)` or `Dirty(FileWriter)`.
    File(FileBody),
    /// A fall-through regular file served from the lower service. The wrapped id
    /// is the file's *lower* [`NodeId`]; the overlay never exposes it directly.
    LowerFile(NodeId),
    /// A whiteout: hides the lower entry of this name (overlay deletion marker).
    Tombstone,
}

enum FileBody {
    /// Content is sealed in the CAS under this id.
    Clean(BlobId),
    /// Content is being edited; the writer holds the live bytes until `seal`.
    Dirty(FileWriter),
}

/// Map a kernel CAS error into a VFS I/O error.
fn io(err: CASError) -> VfsError {
    VfsError::Io(err.to_string())
}

impl<Bk: SyncByteBackend + Clone> WorkingTree<Bk> {
    /// Create an empty **pure-mode** working tree: a single root directory
    /// (mode `0o755`), no lower layer.
    pub fn new(backend: Bk) -> Self {
        Self::with_root(backend, None, 0o040755, None)
    }

    /// Hydrate a **pure-mode** working tree from a sealed root [`TreeId`].
    ///
    /// v1 loads the whole tree eagerly. The `backend` must be able to read the
    /// objects `root` references — pass a clone of the backend the tree was
    /// sealed into.
    pub fn from_root(backend: Bk, root: TreeId) -> VfsResult<Self> {
        let mut wt = Self::new(backend);
        wt.hydrate_into(ROOT_NODE_ID, root)?;
        Ok(wt)
    }

    /// Create an empty **overlay** over `lower`: the root falls through to the
    /// lower's root ([`ROOT_NODE_ID`]). The root's mode is taken from the
    /// lower's root attrs (fallback `0o755`).
    pub fn overlay(backend: Bk, lower: Arc<dyn VfsService>) -> Self {
        let mode = lower
            .getattr(ROOT_NODE_ID)
            .map(|a| 0o040000 | (a.perm as u32 & 0o7777))
            .unwrap_or(0o040755);
        Self::with_root(backend, Some(lower), mode, Some(ROOT_NODE_ID))
    }

    /// Rehydrate a persisted delta layer on top of `lower`.
    ///
    /// `delta` is a sparse overlay tree (as produced by [`seal`](Self::seal)):
    /// a `Blob` entry becomes an upper [`File`](Body::File) (clean), a `Tree`
    /// becomes an upper [`Dir`](Body::Dir) (recursively), and a `Tombstone`
    /// becomes a [`Tombstone`](Body::Tombstone) whiteout. Each materialized
    /// directory's `lower` is resolved by walking `lower` from its root along the
    /// directory's path (`None` if the lower has no such directory). Names absent
    /// from the delta fall through to the lower.
    pub fn from_overlay(backend: Bk, lower: Arc<dyn VfsService>, delta: TreeId) -> VfsResult<Self> {
        let mut wt = Self::overlay(backend, lower);
        // The root's lower counterpart is the lower's root.
        wt.hydrate_overlay_into(ROOT_NODE_ID, delta, Some(ROOT_NODE_ID))?;
        Ok(wt)
    }

    /// Shared constructor: install a single root directory.
    fn with_root(
        backend: Bk,
        lower: Option<Arc<dyn VfsService>>,
        mode: u32,
        root_lower: Option<NodeId>,
    ) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_NODE_ID,
            Inode {
                mode,
                body: Body::Dir {
                    children: BTreeMap::new(),
                    lower: root_lower,
                },
            },
        );
        Self {
            files: FileCas::new(backend.clone()),
            trees: TreeStore::new(backend),
            lower,
            inodes,
            next: ROOT_NODE_ID.0 + 1,
        }
    }

    /// Allocate a fresh inode number.
    fn alloc(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next += 1;
        id
    }

    /// The root directory's inode id ([`ROOT_NODE_ID`]).
    pub fn root(&self) -> NodeId {
        ROOT_NODE_ID
    }

    // --- hydration -----------------------------------------------------------

    /// Recursively populate a **pure-mode** `dir`'s children from a complete
    /// sealed `tree_id`.
    fn hydrate_into(&mut self, dir: NodeId, tree_id: TreeId) -> VfsResult<()> {
        let tree: Tree<BlobId> = self.trees.get(&tree_id).map_err(io)?.ok_or(VfsError::NotFound)?;
        let entries: Vec<(OsString, EntryId<BlobId>, u32)> = tree
            .entries()
            .iter()
            .map(|e| (OsString::from_vec(e.name.to_vec()), e.id, e.mode))
            .collect();

        let mut children = BTreeMap::new();
        for (name, id, mode) in entries {
            let child = self.alloc();
            match id {
                EntryId::Blob(blob) => {
                    self.inodes.insert(child, Inode { mode, body: Body::File(FileBody::Clean(blob)) });
                }
                EntryId::Tree(sub) => {
                    self.inodes.insert(
                        child,
                        Inode { mode, body: Body::Dir { children: BTreeMap::new(), lower: None } },
                    );
                    self.hydrate_into(child, sub)?;
                }
                EntryId::Tombstone => {
                    // A pure (non-overlay) tree has no lower to hide; skip.
                    continue;
                }
            }
            children.insert(name, child);
        }

        if let Some(Inode { body: Body::Dir { children: map, .. }, .. }) = self.inodes.get_mut(&dir) {
            *map = children;
        }
        Ok(())
    }

    /// Recursively populate an **overlay** `dir`'s upper spine from the sparse
    /// `delta` tree, wiring each materialized directory to its lower counterpart.
    ///
    /// `dir_lower` is `dir`'s lower-service [`NodeId`] (the dir to resolve child
    /// directories' lower counterparts against), or `None` if the lower has no
    /// such dir.
    fn hydrate_overlay_into(
        &mut self,
        dir: NodeId,
        delta: TreeId,
        dir_lower: Option<NodeId>,
    ) -> VfsResult<()> {
        let tree: Tree<BlobId> = self.trees.get(&delta).map_err(io)?.ok_or(VfsError::NotFound)?;
        let entries: Vec<(OsString, EntryId<BlobId>, u32)> = tree
            .entries()
            .iter()
            .map(|e| (OsString::from_vec(e.name.to_vec()), e.id, e.mode))
            .collect();

        let mut children = BTreeMap::new();
        for (name, id, mode) in entries {
            let child = self.alloc();
            match id {
                EntryId::Blob(blob) => {
                    self.inodes.insert(child, Inode { mode, body: Body::File(FileBody::Clean(blob)) });
                }
                EntryId::Tree(sub) => {
                    // Resolve this child dir's lower counterpart by walking the
                    // lower one step from `dir_lower`.
                    let child_lower = self.lower_child_dir(dir_lower, &name);
                    self.inodes.insert(
                        child,
                        Inode { mode, body: Body::Dir { children: BTreeMap::new(), lower: child_lower } },
                    );
                    self.hydrate_overlay_into(child, sub, child_lower)?;
                }
                EntryId::Tombstone => {
                    self.inodes.insert(child, Inode { mode, body: Body::Tombstone });
                }
            }
            children.insert(name, child);
        }

        if let Some(Inode { body: Body::Dir { children: map, .. }, .. }) = self.inodes.get_mut(&dir) {
            *map = children;
        }
        Ok(())
    }

    /// Resolve the lower-service [`NodeId`] of the *directory* child `name` under
    /// the lower dir `parent_lower`, or `None` if there is no lower, no such
    /// child, or the child is not a directory.
    fn lower_child_dir(&self, parent_lower: Option<NodeId>, name: &OsStr) -> Option<NodeId> {
        let lower = self.lower.as_ref()?;
        let pl = parent_lower?;
        let entry = lower.lookup(pl, name).ok()?;
        if entry.attr.kind == NodeKind::Directory {
            Some(entry.attr.node_id)
        } else {
            None
        }
    }

    // --- resolve / lookup ----------------------------------------------------

    /// Resolve `name` within directory `parent`, materializing a lower entry on
    /// first sight so its overlay id is stable across repeated lookups (and
    /// matches `readdir`).
    pub fn lookup(&mut self, parent: NodeId, name: &OsStr) -> VfsResult<NodeId> {
        let (cached, parent_lower) = match self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { ref children, lower } => (children.get(name).copied(), lower),
            _ => return Err(VfsError::NotDirectory),
        };

        if let Some(id) = cached {
            // A tombstone resolves to NotFound (the lower entry is hidden).
            return match self.inodes.get(&id).map(|n| &n.body) {
                Some(Body::Tombstone) => Err(VfsError::NotFound),
                _ => Ok(id),
            };
        }

        let ld = parent_lower.ok_or(VfsError::NotFound)?;
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
        let entry = lower.lookup(ld, name)?;
        let id = self.materialize_lower(&entry.attr);
        self.link(parent, name, id);
        Ok(id)
    }

    /// Build an overlay node from a lower entry's attrs and insert it, returning
    /// its (newly allocated) overlay [`NodeId`]. A directory becomes a fall-
    /// through `Dir` wired to its lower id; a regular file becomes a
    /// `LowerFile`. (Symlinks are treated as regular files for v1.)
    fn materialize_lower(&mut self, attr: &NodeAttr) -> NodeId {
        let id = self.alloc();
        let mode_perm = attr.perm as u32 & 0o7777;
        let body = match attr.kind {
            NodeKind::Directory => Body::Dir { children: BTreeMap::new(), lower: Some(attr.node_id) },
            NodeKind::RegularFile | NodeKind::Symlink => Body::LowerFile(attr.node_id),
        };
        let mode = match attr.kind {
            NodeKind::Directory => 0o040000 | mode_perm,
            _ => 0o100000 | mode_perm,
        };
        self.inodes.insert(id, Inode { mode, body });
        id
    }

    // --- getattr / readdir ---------------------------------------------------

    /// Attributes of `node` (kind, mode, size). Upper nodes report
    /// [`UNIX_EPOCH`] timestamps; a fall-through file reports the lower's own
    /// attrs (with the `node_id` remapped to the overlay id).
    pub fn getattr(&self, node: NodeId) -> VfsResult<NodeAttr> {
        let inode = self.inodes.get(&node).ok_or(VfsError::NotFound)?;
        let mut attr = match &inode.body {
            Body::Dir { .. } => NodeAttr::directory(node, UNIX_EPOCH),
            Body::File(fb) => NodeAttr::regular_file(node, self.file_size(fb)?, UNIX_EPOCH),
            Body::LowerFile(ld) => {
                let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
                let mut a = lower.getattr(*ld)?;
                a.node_id = node;
                return Ok(a);
            }
            Body::Tombstone => return Err(VfsError::NotFound),
        };
        attr.perm = (inode.mode & 0o7777) as u16;
        Ok(attr)
    }

    /// List the children of directory `node`: cached children (skipping
    /// tombstones) merged with the lower's children whose names are not already
    /// overridden/tombstoned/cached. Lower entries are materialized+cached so a
    /// later `lookup` returns the same id; every entry's `node_id` is the
    /// overlay id.
    pub fn readdir(&mut self, node: NodeId) -> VfsResult<Vec<DirEntry>> {
        let (cached, lower_id) = match self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { ref children, lower } => (
                children.iter().map(|(n, &i)| (n.clone(), i)).collect::<Vec<_>>(),
                lower,
            ),
            _ => return Err(VfsError::NotDirectory),
        };

        let mut out = Vec::new();
        for (name, id) in &cached {
            let kind = match &self.inodes.get(id).ok_or(VfsError::NotFound)?.body {
                Body::Dir { .. } => NodeKind::Directory,
                Body::File(_) | Body::LowerFile(_) => NodeKind::RegularFile,
                Body::Tombstone => continue,
            };
            out.push(DirEntry::new(*id, name.clone(), kind));
        }

        if let Some(ld) = lower_id {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
            for de in lower.readdir(ld)? {
                if cached.iter().any(|(n, _)| n == &de.name) {
                    // Already overridden / tombstoned / cached — skip.
                    continue;
                }
                // Materialize+cache the lower child via its full attrs so its id
                // is stable and matches a later lookup.
                let attr = lower.getattr(de.node_id)?;
                let id = self.materialize_lower(&attr);
                self.link(node, &de.name, id);
                let kind = match attr.kind {
                    NodeKind::Directory => NodeKind::Directory,
                    _ => NodeKind::RegularFile,
                };
                out.push(DirEntry::new(id, de.name, kind));
            }
        }
        Ok(out)
    }

    // --- read / write --------------------------------------------------------

    /// Read up to `len` bytes of file `node` at offset `off` (clamped to EOF).
    /// A fall-through file is served from the lower (open/read/release per call
    /// in v1).
    pub fn read_at(&self, node: NodeId, off: u64, len: u32) -> VfsResult<Vec<u8>> {
        match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => Err(VfsError::IsDirectory),
            Body::File(FileBody::Clean(blob)) => {
                Ok(self.files.read_at(blob, off, len).map_err(io)?.to_vec())
            }
            Body::File(FileBody::Dirty(writer)) => Ok(writer.read_at(off, len).to_vec()),
            Body::LowerFile(ld) => self.read_lower(*ld, off, len),
            Body::Tombstone => Err(VfsError::NotFound),
        }
    }

    /// Read a range from a lower file: open read-only, read, release.
    fn read_lower(&self, ld: NodeId, off: u64, len: u32) -> VfsResult<Vec<u8>> {
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        let opened = lower.open(OpenRequest {
            node_id: ld,
            mode: OpenMode::ReadOnly,
            truncate: false,
            append: false,
        })?;
        let res = lower.read(ReadRequest {
            node_id: ld,
            handle: opened.handle,
            offset: off,
            size: len,
        });
        // Release regardless of the read outcome.
        let _ = lower.release(opened.handle);
        res
    }

    /// Read the entire lower file (used by copy-up).
    fn read_lower_all(&self, ld: NodeId) -> VfsResult<Vec<u8>> {
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        let size = lower.getattr(ld)?.size;
        let opened = lower.open(OpenRequest {
            node_id: ld,
            mode: OpenMode::ReadOnly,
            truncate: false,
            append: false,
        })?;
        let mut buf = Vec::with_capacity(size as usize);
        let mut off = 0u64;
        loop {
            // Read in bounded chunks so a giant lower file does not need a single
            // u32-sized request.
            let want = (size - off).min(1 << 20) as u32;
            if want == 0 {
                break;
            }
            let chunk = lower.read(ReadRequest {
                node_id: ld,
                handle: opened.handle,
                offset: off,
                size: want,
            });
            match chunk {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        break;
                    }
                    off += bytes.len() as u64;
                    buf.extend_from_slice(&bytes);
                }
                Err(e) => {
                    let _ = lower.release(opened.handle);
                    return Err(e);
                }
            }
        }
        let _ = lower.release(opened.handle);
        Ok(buf)
    }

    /// Write `data` into file `node` at offset `off`, returning bytes written.
    ///
    /// - A dirty upper file is written in place.
    /// - A clean upper file is re-opened for editing (its content materialized
    ///   into a [`FileWriter`]) on first write.
    /// - A **fall-through** file is **copied up**: its whole lower content is
    ///   read into a fresh writer, the inode flips to [`File`](Body::File)
    ///   keeping the **same [`NodeId`]**, and the write is applied. The lower is
    ///   never mutated.
    pub fn write_at(&mut self, node: NodeId, off: u64, data: &[u8]) -> VfsResult<u32> {
        // Decide the materialization step without holding a borrow across the
        // FileCas / lower calls.
        enum Step {
            Ready,
            EditClean(BlobId),
            CopyUp(NodeId),
        }
        let step = match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => return Err(VfsError::IsDirectory),
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::File(FileBody::Dirty(_)) => Step::Ready,
            Body::File(FileBody::Clean(b)) => Step::EditClean(*b),
            Body::LowerFile(ld) => Step::CopyUp(*ld),
        };

        match step {
            Step::Ready => {}
            Step::EditClean(b) => {
                let writer = self.files.edit(Some(b)).map_err(io)?;
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::File(FileBody::Dirty(writer));
                }
            }
            Step::CopyUp(ld) => {
                // Read the whole lower file, then seed a fresh writer with it.
                let bytes = self.read_lower_all(ld)?;
                let mut writer = self.files.edit(None).map_err(io)?;
                writer.write_at(0, &bytes);
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::File(FileBody::Dirty(writer));
                }
            }
        }

        match &mut self.inodes.get_mut(&node).ok_or(VfsError::NotFound)?.body {
            Body::File(FileBody::Dirty(writer)) => {
                writer.write_at(off, data);
                Ok(data.len() as u32)
            }
            // Unreachable: we just ensured the body is Dirty.
            _ => Err(VfsError::Io("write target changed unexpectedly".into())),
        }
    }

    // --- create / mkdir ------------------------------------------------------

    /// Create an empty regular file `name` under `parent` (upper only).
    ///
    /// Recreating over a tombstone (a previously-deleted lower name) replaces
    /// the whiteout.
    pub fn create(&mut self, parent: NodeId, name: &OsStr, mode: u32) -> VfsResult<NodeId> {
        self.ensure_creatable(parent, name)?;
        let writer = self.files.edit(None).map_err(io)?;
        let id = self.alloc();
        self.inodes.insert(id, Inode { mode, body: Body::File(FileBody::Dirty(writer)) });
        self.link(parent, name, id);
        Ok(id)
    }

    /// Create an empty subdirectory `name` under `parent` (upper only, `lower =
    /// None`). Recreating over a tombstone replaces the whiteout.
    pub fn mkdir(&mut self, parent: NodeId, name: &OsStr, mode: u32) -> VfsResult<NodeId> {
        self.ensure_creatable(parent, name)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode { mode, body: Body::Dir { children: BTreeMap::new(), lower: None } },
        );
        self.link(parent, name, id);
        Ok(id)
    }

    // --- unlink / rmdir ------------------------------------------------------

    /// Remove the regular file `name` from `parent`.
    ///
    /// If the name also exists in the lower a **tombstone** (whiteout) is left in
    /// its place; a pure-upper name is simply dropped.
    pub fn unlink(&mut self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => return Err(VfsError::IsDirectory),
            Body::File(_) | Body::LowerFile(_) => {}
            Body::Tombstone => return Err(VfsError::NotFound),
        }
        self.remove_child(parent, name, id)
    }

    /// Remove the empty directory `name` from `parent`.
    ///
    /// "Empty" means no non-tombstone upper children **and** no un-tombstoned
    /// lower children. If the name also exists in the lower a tombstone is left;
    /// a pure-upper directory is simply dropped.
    pub fn rmdir(&mut self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::File(_) | Body::LowerFile(_) => return Err(VfsError::NotDirectory),
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::Dir { .. } => {}
        }
        if !self.dir_is_empty(id)? {
            return Err(VfsError::DirectoryNotEmpty);
        }
        self.remove_child(parent, name, id)
    }

    /// Whether directory `id` has no visible children (no non-tombstone upper
    /// child, and no lower child not shadowed by a tombstone).
    fn dir_is_empty(&self, id: NodeId) -> VfsResult<bool> {
        let (children, lower) = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, lower } => (children, *lower),
            _ => return Err(VfsError::NotDirectory),
        };
        // Any non-tombstone upper child means non-empty.
        for cid in children.values() {
            if !matches!(self.inodes.get(cid).map(|n| &n.body), Some(Body::Tombstone)) {
                return Ok(false);
            }
        }
        // Any lower child whose name is not tombstoned in the upper means
        // non-empty.
        if let Some(ld) = lower {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
            for de in lower.readdir(ld)? {
                let shadowed = children
                    .get(&de.name)
                    .map(|cid| matches!(self.inodes.get(cid).map(|n| &n.body), Some(Body::Tombstone)))
                    .unwrap_or(false);
                if !shadowed {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Detach `name`→`id` from `parent`: leave a tombstone if the name also
    /// exists in the lower, otherwise drop it. Frees the removed inode unless it
    /// is replaced by a tombstone (a tombstone is its own fresh node).
    fn remove_child(&mut self, parent: NodeId, name: &OsStr, id: NodeId) -> VfsResult<()> {
        if self.name_in_lower(parent, name)? {
            // Whiteout: replace the resolved child with a tombstone, reusing its
            // slot. We free the old inode and install a tombstone in `children`.
            self.inodes.remove(&id);
            let tomb = self.alloc();
            self.inodes.insert(tomb, Inode { mode: 0, body: Body::Tombstone });
            self.link(parent, name, tomb);
        } else {
            self.unlink_child(parent, name);
            self.inodes.remove(&id);
        }
        Ok(())
    }

    /// Whether `name` resolves to an entry in the lower of `parent` (a
    /// `LowerFile`/fall-through dir already cached counts, as does a fresh lower
    /// lookup).
    fn name_in_lower(&self, parent: NodeId, name: &OsStr) -> VfsResult<bool> {
        let parent_lower = match &self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { lower, .. } => *lower,
            _ => return Err(VfsError::NotDirectory),
        };
        let ld = match parent_lower {
            None => return Ok(false),
            Some(ld) => ld,
        };
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        Ok(lower.lookup(ld, name).is_ok())
    }

    // --- seal ----------------------------------------------------------------

    /// Seal the current state into immutable objects, returning the new root
    /// [`TreeId`].
    ///
    /// In pure mode this is a complete tree. In overlay mode it is a **sparse
    /// delta**: only the materialized spine and tombstones are emitted; a pure
    /// fall-through directory (no upper entries, has a lower) emits nothing, so
    /// untouched siblings are absent from the delta. The root always emits.
    pub fn seal(&mut self) -> VfsResult<TreeId> {
        // The root always materializes (even empty), so `seal_dir` returns Some.
        match self.seal_dir(ROOT_NODE_ID, true)? {
            Some(id) => Ok(id),
            None => {
                // Root with no entries and a lower: still emit an empty tree.
                let tree = Tree::<BlobId>::empty();
                self.trees.put(tree).map_err(io)
            }
        }
    }

    /// Seal directory `dir`. Returns `None` iff this is a pure fall-through dir
    /// (produced no entries AND has a lower) and `force_emit` is false — such a
    /// dir is absent from the delta. Otherwise builds and stores a tree.
    fn seal_dir(&mut self, dir: NodeId, force_emit: bool) -> VfsResult<Option<TreeId>> {
        let (children, has_lower) = match &self.inodes.get(&dir).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, lower } => (
                children.iter().map(|(n, &i)| (n.clone(), i)).collect::<Vec<_>>(),
                lower.is_some(),
            ),
            _ => return Err(VfsError::NotDirectory),
        };

        let mut entries: Vec<TreeEntry<BlobId>> = Vec::with_capacity(children.len());
        for (name, id) in children {
            let mode = self.inodes.get(&id).ok_or(VfsError::NotFound)?.mode;
            let name_bytes = Bytes::copy_from_slice(name.as_bytes());
            let kind = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                Body::Dir { .. } => 0u8,
                Body::File(_) => 1u8,
                Body::Tombstone => 2u8,
                // A fall-through file that was never copied up is absent (it
                // falls through to the lower).
                Body::LowerFile(_) => 3u8,
            };
            match kind {
                0 => {
                    if let Some(sub) = self.seal_dir(id, false)? {
                        entries.push(TreeEntry::dir(name_bytes, sub, mode));
                    }
                    // None ⇒ pure fall-through dir; skip (falls through).
                }
                1 => {
                    let (blob, size) = self.seal_file(id)?;
                    entries.push(TreeEntry::file(name_bytes, blob, mode, size));
                }
                2 => {
                    entries.push(TreeEntry::tombstone(name_bytes));
                }
                _ => {
                    // LowerFile ⇒ skip (absent = fall-through).
                }
            }
        }

        // A pure fall-through dir (no entries, has a lower) emits nothing unless
        // forced (the root). A new dir (lower = None) emits even when empty.
        if entries.is_empty() && has_lower && !force_emit {
            return Ok(None);
        }

        let tree = Tree::from_entries(entries).map_err(|e| VfsError::Other(e.to_string()))?;
        Ok(Some(self.trees.put(tree).map_err(io)?))
    }

    /// Seal a single upper file inode, returning its blob id and logical size. A
    /// dirty writer is committed and the inode flipped to clean; a clean file is
    /// left as-is.
    fn seal_file(&mut self, id: NodeId) -> VfsResult<(BlobId, u64)> {
        // Take ownership of a dirty writer (if any) without holding a borrow
        // across `self.files.commit`. Swap in a Tombstone sentinel that is
        // overwritten immediately below.
        let dirty: Option<FileWriter> = {
            let inode = self.inodes.get_mut(&id).ok_or(VfsError::NotFound)?;
            match &inode.body {
                Body::File(FileBody::Clean(_)) => None,
                Body::File(FileBody::Dirty(_)) => {
                    let taken = std::mem::replace(&mut inode.body, Body::Tombstone);
                    match taken {
                        Body::File(FileBody::Dirty(w)) => Some(w),
                        _ => None,
                    }
                }
                _ => return Err(VfsError::IsDirectory),
            }
        };

        let blob = match dirty {
            None => match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                Body::File(FileBody::Clean(b)) => *b,
                _ => return Err(VfsError::Io("seal_file: file vanished".into())),
            },
            Some(writer) => {
                let b = self.files.commit(writer).map_err(io)?;
                if let Some(inode) = self.inodes.get_mut(&id) {
                    inode.body = Body::File(FileBody::Clean(b));
                }
                b
            }
        };

        let size = self.files.size(&blob).map_err(io)?.unwrap_or(0);
        Ok((blob, size))
    }

    // --- small helpers -------------------------------------------------------

    fn file_size(&self, body: &FileBody) -> VfsResult<u64> {
        match body {
            FileBody::Clean(b) => Ok(self.files.size(b).map_err(io)?.unwrap_or(0)),
            FileBody::Dirty(w) => Ok(w.len()),
        }
    }

    /// Error unless `parent` is a directory where `name` can be created: it must
    /// not already resolve to a live (non-tombstone) entry — neither in the
    /// upper nor (un-tombstoned) in the lower. A tombstone is replaceable.
    fn ensure_creatable(&self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        let (cached, parent_lower) = match &self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, lower } => (children.get(name).copied(), *lower),
            _ => return Err(VfsError::NotDirectory),
        };
        if let Some(id) = cached {
            return match self.inodes.get(&id).map(|n| &n.body) {
                // A tombstone is a deleted lower name; creating over it is fine.
                Some(Body::Tombstone) => Ok(()),
                _ => Err(VfsError::AlreadyExists),
            };
        }
        // Not cached upper: a live lower name would collide.
        if let Some(ld) = parent_lower {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
            if lower.lookup(ld, name).is_ok() {
                return Err(VfsError::AlreadyExists);
            }
        }
        Ok(())
    }

    fn link(&mut self, parent: NodeId, name: &OsStr, id: NodeId) {
        if let Some(Inode { body: Body::Dir { children, .. }, .. }) = self.inodes.get_mut(&parent) {
            children.insert(name.to_os_string(), id);
        }
    }

    fn unlink_child(&mut self, parent: NodeId, name: &OsStr) {
        if let Some(Inode { body: Body::Dir { children, .. }, .. }) = self.inodes.get_mut(&parent) {
            children.remove(name);
        }
    }
}
