//! [`CasVfs`] — a [`VfsService`] backed by the copy-on-write [`WorkingTree`].
//!
//! This is the *leaf* service that turns the content-addressed object stack into
//! a mountable filesystem. It owns a [`WorkingTree`] (the mutable CoW overlay)
//! behind a [`Mutex`] and adapts its `&mut self` API to the `&self`
//! [`VfsService`] contract the FUSE adapter speaks.
//!
//! Two shapes, mirroring [`WorkingTree`]:
//!
//! - **pure** ([`new`](CasVfs::new) / [`from_root`](CasVfs::from_root) /
//!   [`in_memory`](CasVfs::in_memory)) — a self-contained tree.
//! - **overlay** ([`overlay`](CasVfs::overlay) / [`from_overlay`](CasVfs::from_overlay))
//!   — a sparse copy-on-write layer over a lower [`VfsService`]: untouched names
//!   fall through to the lower, writes copy up into the CAS, deletions leave
//!   whiteouts. [`seal`](CasVfs::seal) freezes the current delta into a
//!   [`TreeId`].
//!
//! File handles are opaque, monotonically-allocated tokens: reads and writes are
//! addressed by [`NodeId`] (which the kernel passes alongside the handle), so the
//! handle carries no per-open state and [`flush`](VfsService::flush) /
//! [`release`](VfsService::release) are no-ops.

use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use epoch_cas::{MemoryByteBackend, SyncByteBackend};
use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, NodeId, NodeKind,
    OpenRequest, OpenedFile, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
};
use epoch_worktree::{TreeId, WorkingTree};

/// A [`VfsService`] backed by a copy-on-write [`WorkingTree`] over the
/// content-addressed object store. See the [module docs](self) for the two
/// shapes (pure vs overlay) and the handle model.
pub struct CasVfs<Bk> {
    tree: Mutex<WorkingTree<Bk>>,
    /// Monotonic source of opaque file-handle tokens.
    next_handle: AtomicU64,
}

impl<Bk: SyncByteBackend + Clone> CasVfs<Bk> {
    fn wrap(tree: WorkingTree<Bk>) -> Self {
        Self {
            tree: Mutex::new(tree),
            next_handle: AtomicU64::new(1),
        }
    }

    /// An empty **pure-mode** filesystem (a single root directory), no lower.
    pub fn new(backend: Bk) -> Self {
        Self::wrap(WorkingTree::new(backend))
    }

    /// A **pure-mode** filesystem hydrated from a sealed root [`TreeId`].
    pub fn from_root(backend: Bk, root: TreeId) -> VfsResult<Self> {
        Ok(Self::wrap(WorkingTree::from_root(backend, root)?))
    }

    /// An empty copy-on-write **overlay** over `lower`: untouched names fall
    /// through to it; writes/deletes materialize into the CAS.
    pub fn overlay(backend: Bk, lower: Arc<dyn VfsService>) -> Self {
        Self::wrap(WorkingTree::overlay(backend, lower))
    }

    /// An **overlay** rehydrated from a persisted sparse `delta` on top of `lower`.
    pub fn from_overlay(backend: Bk, lower: Arc<dyn VfsService>, delta: TreeId) -> VfsResult<Self> {
        Ok(Self::wrap(WorkingTree::from_overlay(backend, lower, delta)?))
    }

    /// Seal the current state into immutable objects, returning the new root
    /// [`TreeId`] (a sparse delta in overlay mode, a complete tree otherwise).
    pub fn seal(&self) -> VfsResult<TreeId> {
        self.lock().seal()
    }

    /// Lock the working tree, recovering from a poisoned mutex (a panic in a
    /// prior op leaves the tree usable; we do not propagate poison as an error).
    fn lock(&self) -> MutexGuard<'_, WorkingTree<Bk>> {
        self.tree.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hand out a fresh opaque handle token.
    fn next_handle(&self) -> FileHandle {
        FileHandle(self.next_handle.fetch_add(1, Ordering::Relaxed))
    }
}

impl CasVfs<MemoryByteBackend> {
    /// An empty in-memory pure-mode filesystem — the convenient default for a
    /// throwaway mount (`CasVfs::new(MemoryByteBackend::new())`).
    pub fn in_memory() -> Self {
        Self::new(MemoryByteBackend::new())
    }
}

impl<Bk: SyncByteBackend + Clone> VfsService for CasVfs<Bk> {
    fn lookup(&self, parent: NodeId, name: &OsStr) -> VfsResult<Entry> {
        let mut tree = self.lock();
        let id = tree.lookup(parent, name)?;
        Ok(Entry::new(tree.getattr(id)?))
    }

    fn getattr(&self, node_id: NodeId) -> VfsResult<NodeAttr> {
        self.lock().getattr(node_id)
    }

    fn readdir(&self, node_id: NodeId) -> VfsResult<Vec<DirEntry>> {
        self.lock().readdir(node_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        // Validate the target exists and is a file; directories are opened via
        // readdir, not here.
        if self.lock().getattr(request.node_id)?.kind == NodeKind::Directory {
            return Err(VfsError::IsDirectory);
        }
        // NOTE(v1): the `truncate` flag (O_TRUNC) is not yet honored — see the
        // crate TODO; WorkingTree has no truncate primitive.
        Ok(OpenedFile {
            handle: self.next_handle(),
            direct_io: false,
            keep_cache: true,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        self.lock()
            .read_at(request.node_id, request.offset, request.size)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        self.lock()
            .write_at(request.node_id, request.offset, request.data)
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        // WorkingTree stores the full mode (type bits + perms); the kernel hands
        // us perms, so re-attach the regular-file type bits.
        let mode = 0o100000 | (request.mode & 0o7777);
        let attr = {
            let mut tree = self.lock();
            let id = tree.create(request.parent, request.name.as_os_str(), mode)?;
            tree.getattr(id)?
        };
        Ok(CreatedNode {
            entry: Entry::new(attr),
            opened: Some(OpenedFile {
                handle: self.next_handle(),
                direct_io: false,
                keep_cache: true,
            }),
        })
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        let mode = 0o040000 | (request.mode & 0o7777);
        let mut tree = self.lock();
        let id = tree.mkdir(request.parent, request.name.as_os_str(), mode)?;
        Ok(Entry::new(tree.getattr(id)?))
    }

    fn unlink(&self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        self.lock().unlink(parent, name)
    }

    fn rmdir(&self, parent: NodeId, name: &OsStr) -> VfsResult<()> {
        self.lock().rmdir(parent, name)
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn release(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_vfs_api::{OpenMode, ROOT_NODE_ID};

    type Svc = CasVfs<MemoryByteBackend>;

    /// Create+write a file through the service, returning its node id.
    fn write_file(svc: &Svc, parent: NodeId, name: &str, data: &[u8]) -> NodeId {
        let created = svc.create(CreateRequest::file(parent, name, 0o644)).unwrap();
        let id = created.entry.attr.node_id;
        let handle = created.opened.expect("create opens the file").handle;
        let n = svc
            .write(WriteRequest {
                node_id: id,
                handle,
                offset: 0,
                data,
            })
            .unwrap();
        assert_eq!(n as usize, data.len());
        svc.release(handle).unwrap();
        id
    }

    /// Open→read→release a whole (small) file through the service.
    fn read_all(svc: &Svc, id: NodeId) -> Vec<u8> {
        let opened = svc
            .open(OpenRequest {
                node_id: id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .unwrap();
        let bytes = svc
            .read(ReadRequest {
                node_id: id,
                handle: opened.handle,
                offset: 0,
                size: 4096,
            })
            .unwrap();
        svc.release(opened.handle).unwrap();
        bytes
    }

    fn names(svc: &Svc, dir: NodeId) -> Vec<String> {
        let mut out: Vec<String> = svc
            .readdir(dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn create_write_read_through_the_service() {
        let svc = CasVfs::in_memory();
        let root = ROOT_NODE_ID;

        let dir = svc
            .mkdir(CreateRequest::directory(root, "dir", 0o755))
            .unwrap()
            .attr
            .node_id;
        write_file(&svc, dir, "a.txt", b"alpha");
        write_file(&svc, root, "b.txt", b"beta");

        assert_eq!(names(&svc, root), vec!["b.txt".to_string(), "dir".to_string()]);

        let a = svc.lookup(dir, OsStr::new("a.txt")).unwrap().attr.node_id;
        assert_eq!(read_all(&svc, a), b"alpha");
        assert_eq!(svc.getattr(a).unwrap().kind, NodeKind::RegularFile);

        // Opening a directory as a file is rejected.
        assert_eq!(
            svc.open(OpenRequest {
                node_id: dir,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .unwrap_err(),
            VfsError::IsDirectory
        );
    }

    #[test]
    fn seal_then_from_root_roundtrip_through_the_service() {
        let backend = MemoryByteBackend::new();
        let svc = CasVfs::new(backend.clone());
        let root = ROOT_NODE_ID;
        write_file(&svc, root, "f", b"first");

        let sealed = svc.seal().unwrap();

        // A fresh service hydrated from the sealed root (shared backend) sees it.
        let svc2 = CasVfs::from_root(backend, sealed).unwrap();
        let f = svc2.lookup(root, OsStr::new("f")).unwrap().attr.node_id;
        assert_eq!(read_all(&svc2, f), b"first");
        assert_eq!(names(&svc2, root), vec!["f".to_string()]);
    }
}
