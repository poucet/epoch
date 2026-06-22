//! # epoch-worktree — the mutable working-tree / inode layer
//!
//! `WorkingTree` is the live, mutable view that sits *above* the immutable
//! content-addressed object store (`epoch-file-cas` + `epoch-tree` +
//! `epoch-tree-cas`) and *below* a VFS/FUSE adapter. It is the home of the two
//! things the object layer deliberately lacks:
//!
//! - **mutable identity** — stable [`NodeId`] inodes that survive edits, so a
//!   file keeps its identity while its content (and thus its `BlobId`) changes;
//! - **the read/write bridge** — [`hydrate`](WorkingTree::from_root) turns a
//!   [`TreeId`] into a tree of inodes, and [`seal`](WorkingTree::seal) walks the
//!   dirty inodes back down into fresh immutable objects, returning a new root
//!   [`TreeId`].
//!
//! It is intentionally **FUSE-independent** (it speaks the `epoch-vfs-api`
//! types but knows nothing of `FileHandle`s, `open`, or the kernel protocol) so
//! it can be unit-tested on its own. A thin `VfsService` adapter layers on top.
//!
//! ## First-cut scope (v1)
//!
//! - Inodes are **session-scoped**: allocated from a monotonic counter, stable
//!   for the life of this `WorkingTree`, not persisted and not preserved across
//!   history. Hardlinks, a durable inode table, and per-inode uid/gid/timestamps
//!   are future work; timestamps are reported as [`UNIX_EPOCH`].
//! - [`from_root`](WorkingTree::from_root) hydrates **eagerly** (the whole tree
//!   is loaded up front); lazy, on-demand hydration is a later optimization.
//! - [`seal`](WorkingTree::seal) rebuilds every directory's [`Tree`] object each
//!   call. Clean files keep their existing `BlobId` (no re-hashing); only
//!   *dirty* files are committed. Dirty-subtree tracking — so untouched
//!   directories are not re-encoded — is a later optimization that slots in
//!   without changing this API.
//! - Symlinks are out of scope for v1 (the object model already supports them as
//!   blobs with [`SYMLINK_MODE`](epoch_tree::SYMLINK_MODE); the worktree just
//!   doesn't create them yet).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::time::UNIX_EPOCH;

use epoch_cas::{Bytes, CASError, SyncByteBackend};
use epoch_file_cas::{BlobId, FileCas, FileWriter};
use epoch_tree::{EntryId, Tree, TreeEntry, TreeId};
use epoch_tree_cas::TreeCas;
use epoch_vfs_api::{DirEntry, NodeAttr, NodeId, NodeKind, VfsError, VfsResult, ROOT_NODE_ID};

/// A mutable, in-memory tree of inodes backed by content-addressed storage.
///
/// Generic over the byte backend `Bk`. Files are stored via [`FileCas`] and
/// addressed by [`BlobId`]; directories are sealed into [`Tree<BlobId>`] objects
/// via [`TreeCas`]. See the [crate docs](crate) for the layer's role and v1
/// scope.
pub struct WorkingTree<Bk> {
    files: FileCas<Bk>,
    trees: TreeCas<Bk, BlobId>,
    inodes: HashMap<NodeId, Inode>,
    /// Next inode number to hand out (monotonic; starts just past the root).
    next: u64,
}

/// One node in the live tree: its mode bits and kind-specific body.
///
/// (A back-pointer to the parent will be added when rename / `..` resolution
/// needs it; v1 has no use for it yet.)
struct Inode {
    /// Unix mode bits as supplied by the caller / read from the sealed entry,
    /// preserved verbatim across seal/hydrate.
    mode: u32,
    body: Body,
}

enum Body {
    /// A directory: its children by name (kept sorted by `BTreeMap`).
    Dir(BTreeMap<OsString, NodeId>),
    /// A regular file.
    File(FileBody),
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
    /// Create an empty working tree: a single root directory (mode `0o755`).
    pub fn new(backend: Bk) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_NODE_ID,
            Inode {
                mode: 0o040755,
                body: Body::Dir(BTreeMap::new()),
            },
        );
        Self {
            files: FileCas::new(backend.clone()),
            trees: TreeCas::new(backend),
            inodes,
            next: ROOT_NODE_ID.0 + 1,
        }
    }

    /// Hydrate a working tree from a sealed root [`TreeId`].
    ///
    /// v1 loads the whole tree eagerly (see [crate docs](crate)). The `backend`
    /// must be able to read the objects `root` references — pass a clone of the
    /// backend the tree was sealed into.
    pub fn from_root(backend: Bk, root: TreeId) -> VfsResult<Self> {
        let mut wt = Self::new(backend);
        wt.hydrate_into(ROOT_NODE_ID, root)?;
        Ok(wt)
    }

    /// Allocate a fresh inode number.
    fn alloc(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next += 1;
        id
    }

    /// Recursively populate `dir`'s children from the sealed `tree_id`.
    fn hydrate_into(&mut self, dir: NodeId, tree_id: TreeId) -> VfsResult<()> {
        let tree: Tree<BlobId> = self.trees.get(&tree_id).map_err(io)?.ok_or(VfsError::NotFound)?;
        // Collect first so we don't borrow `tree` while mutating `self`.
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
                    self.inodes.insert(
                        child,
                        Inode {
                            mode,
                            body: Body::File(FileBody::Clean(blob)),
                        },
                    );
                }
                EntryId::Tree(sub) => {
                    self.inodes.insert(
                        child,
                        Inode {
                            mode,
                            body: Body::Dir(BTreeMap::new()),
                        },
                    );
                    self.hydrate_into(child, sub)?;
                }
                EntryId::Tombstone => {
                    // A tombstone is an overlay whiteout; a pure (non-overlay)
                    // hydrate has no lower layer to hide, so it cannot occur
                    // here. Skip it.
                    // TODO(overlay): handled in the upcoming overlay WorkingTree.
                    continue;
                }
            }
            children.insert(name, child);
        }

        if let Some(Inode { body: Body::Dir(map), .. }) = self.inodes.get_mut(&dir) {
            *map = children;
        }
        Ok(())
    }

    /// The root directory's inode id ([`ROOT_NODE_ID`]).
    pub fn root(&self) -> NodeId {
        ROOT_NODE_ID
    }

    /// Resolve `name` within directory `parent`.
    pub fn lookup(&self, parent: NodeId, name: &OsStr) -> VfsResult<NodeId> {
        self.dir_children(parent)?
            .get(name)
            .copied()
            .ok_or(VfsError::NotFound)
    }

    /// Attributes of `node` (kind, mode, size; timestamps are [`UNIX_EPOCH`] in
    /// v1).
    pub fn getattr(&self, node: NodeId) -> VfsResult<NodeAttr> {
        let inode = self.inodes.get(&node).ok_or(VfsError::NotFound)?;
        let mut attr = match &inode.body {
            Body::Dir(_) => NodeAttr::directory(node, UNIX_EPOCH),
            Body::File(fb) => NodeAttr::regular_file(node, self.file_size(fb)?, UNIX_EPOCH),
        };
        attr.perm = (inode.mode & 0o7777) as u16;
        Ok(attr)
    }

    /// List the children of directory `node`, sorted by name.
    pub fn readdir(&self, node: NodeId) -> VfsResult<Vec<DirEntry>> {
        let children = self.dir_children(node)?;
        let mut out = Vec::with_capacity(children.len());
        for (name, &id) in children {
            let kind = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                Body::Dir(_) => NodeKind::Directory,
                Body::File(_) => NodeKind::RegularFile,
            };
            out.push(DirEntry::new(id, name.clone(), kind));
        }
        Ok(out)
    }

    /// Read up to `len` bytes of file `node` at offset `off` (clamped to EOF).
    pub fn read_at(&self, node: NodeId, off: u64, len: u32) -> VfsResult<Vec<u8>> {
        match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir(_) => Err(VfsError::IsDirectory),
            Body::File(FileBody::Clean(blob)) => {
                Ok(self.files.read_at(blob, off, len).map_err(io)?.to_vec())
            }
            Body::File(FileBody::Dirty(writer)) => Ok(writer.read_at(off, len).to_vec()),
        }
    }

    /// Write `data` into file `node` at offset `off`, returning bytes written.
    ///
    /// A clean file is transparently re-opened for editing (its content is
    /// materialized into a [`FileWriter`]) on first write.
    pub fn write_at(&mut self, node: NodeId, off: u64, data: &[u8]) -> VfsResult<u32> {
        // Decide whether we must open a writer, without holding a borrow across
        // the `self.files.edit` call.
        let blob = match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir(_) => return Err(VfsError::IsDirectory),
            Body::File(FileBody::Dirty(_)) => None,
            Body::File(FileBody::Clean(b)) => Some(*b),
        };
        if let Some(b) = blob {
            let writer = self.files.edit(Some(b)).map_err(io)?;
            if let Some(inode) = self.inodes.get_mut(&node) {
                inode.body = Body::File(FileBody::Dirty(writer));
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

    /// Create an empty regular file `name` under `parent`.
    pub fn create(&mut self, parent: NodeId, name: &OsStr, mode: u32) -> VfsResult<NodeId> {
        self.ensure_vacant(parent, name)?;
        let writer = self.files.edit(None).map_err(io)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode {
                mode,
                body: Body::File(FileBody::Dirty(writer)),
            },
        );
        self.link(parent, name, id);
        Ok(id)
    }

    /// Create an empty subdirectory `name` under `parent`.
    pub fn mkdir(&mut self, parent: NodeId, name: &OsStr, mode: u32) -> VfsResult<NodeId> {
        self.ensure_vacant(parent, name)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode {
                mode,
                body: Body::Dir(BTreeMap::new()),
            },
        );
        self.link(parent, name, id);
        Ok(id)
    }

    /// Remove the regular file `name` from `parent`.
    pub fn unlink(&mut self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir(_) => return Err(VfsError::IsDirectory),
            Body::File(_) => {}
        }
        self.unlink_child(parent, name);
        self.inodes.remove(&id);
        Ok(())
    }

    /// Remove the empty directory `name` from `parent`.
    pub fn rmdir(&mut self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir(map) if map.is_empty() => {}
            Body::Dir(_) => return Err(VfsError::DirectoryNotEmpty),
            Body::File(_) => return Err(VfsError::NotDirectory),
        }
        self.unlink_child(parent, name);
        self.inodes.remove(&id);
        Ok(())
    }

    /// Seal the current state into immutable objects, returning the new root
    /// [`TreeId`].
    ///
    /// Walks the tree bottom-up: dirty files are committed to fresh `BlobId`s
    /// (and marked clean), clean files keep their id, and every directory is
    /// rebuilt into a [`Tree<BlobId>`]. See [crate docs](crate) for the v1
    /// re-seal behaviour and the optimizations it leaves room for.
    pub fn seal(&mut self) -> VfsResult<TreeId> {
        self.seal_dir(ROOT_NODE_ID)
    }

    fn seal_dir(&mut self, dir: NodeId) -> VfsResult<TreeId> {
        // Snapshot the child list so we can recurse with `&mut self`.
        let children: Vec<(OsString, NodeId)> = match &self.inodes.get(&dir).ok_or(VfsError::NotFound)?.body {
            Body::Dir(map) => map.iter().map(|(n, &i)| (n.clone(), i)).collect(),
            Body::File(_) => return Err(VfsError::NotDirectory),
        };

        let mut entries: Vec<TreeEntry<BlobId>> = Vec::with_capacity(children.len());
        for (name, id) in children {
            let mode = self.inodes.get(&id).ok_or(VfsError::NotFound)?.mode;
            let name_bytes = Bytes::copy_from_slice(name.as_bytes());
            let is_dir = matches!(self.inodes.get(&id).map(|n| &n.body), Some(Body::Dir(_)));
            if is_dir {
                let sub = self.seal_dir(id)?;
                entries.push(TreeEntry::dir(name_bytes, sub, mode));
            } else {
                let (blob, size) = self.seal_file(id)?;
                entries.push(TreeEntry::file(name_bytes, blob, mode, size));
            }
        }

        let tree = Tree::from_entries(entries).map_err(|e| VfsError::Other(e.to_string()))?;
        self.trees.put(tree).map_err(io)
    }

    /// Seal a single file inode, returning its blob id and logical size. A dirty
    /// writer is committed and the inode flipped to clean; a clean file is left
    /// as-is.
    fn seal_file(&mut self, id: NodeId) -> VfsResult<(BlobId, u64)> {
        // Take ownership of a dirty writer (if any) without holding a borrow
        // across `self.files.commit`. We temporarily swap the body for an empty
        // dir sentinel that is overwritten immediately below.
        let dirty: Option<FileWriter> = {
            let inode = self.inodes.get_mut(&id).ok_or(VfsError::NotFound)?;
            match &inode.body {
                Body::File(FileBody::Clean(_)) => None,
                Body::File(FileBody::Dirty(_)) => {
                    let taken = std::mem::replace(&mut inode.body, Body::Dir(BTreeMap::new()));
                    match taken {
                        Body::File(FileBody::Dirty(w)) => Some(w),
                        _ => None,
                    }
                }
                Body::Dir(_) => return Err(VfsError::IsDirectory),
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

    fn dir_children(&self, node: NodeId) -> VfsResult<&BTreeMap<OsString, NodeId>> {
        match self.inodes.get(&node) {
            None => Err(VfsError::NotFound),
            Some(Inode { body: Body::Dir(map), .. }) => Ok(map),
            Some(_) => Err(VfsError::NotDirectory),
        }
    }

    fn file_size(&self, body: &FileBody) -> VfsResult<u64> {
        match body {
            FileBody::Clean(b) => Ok(self.files.size(b).map_err(io)?.unwrap_or(0)),
            FileBody::Dirty(w) => Ok(w.len()),
        }
    }

    /// Error unless `parent` is a directory that does not already contain `name`.
    fn ensure_vacant(&self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        if self.dir_children(parent)?.contains_key(name) {
            Err(VfsError::AlreadyExists)
        } else {
            Ok(())
        }
    }

    fn link(&mut self, parent: NodeId, name: &OsStr, id: NodeId) {
        if let Some(Inode { body: Body::Dir(map), .. }) = self.inodes.get_mut(&parent) {
            map.insert(name.to_os_string(), id);
        }
    }

    fn unlink_child(&mut self, parent: NodeId, name: &OsStr) {
        if let Some(Inode { body: Body::Dir(map), .. }) = self.inodes.get_mut(&parent) {
            map.remove(name);
        }
    }
}
