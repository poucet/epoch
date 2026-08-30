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
//! addressed by [`InodeId`] (which the kernel passes alongside the handle), so the
//! handle carries no per-open state and [`flush`](VfsService::flush) /
//! [`release`](VfsService::release) are no-ops.

use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use epoch_cas::{MemoryByteBackend, SyncByteBackend};
use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind,
    OpenRequest, OpenedFile, ReadRequest, SetAttrRequest, StatFs, VfsError, VfsResult, VfsService,
    WriteRequest,
};
use epoch_worktree::{TreeId, WorkingTree};

use crate::debug::{DebugSnapshot, DebugStats, NearCount};

/// A [`VfsService`] backed by a copy-on-write [`WorkingTree`] over the
/// content-addressed object store. See the [module docs](self) for the two
/// shapes (pure vs overlay) and the handle model.
pub struct CasVfs<Bk> {
    tree: Mutex<WorkingTree<Bk>>,
    /// The byte backend the tree stores into (backends are cheap `Arc`-backed
    /// clones), kept so [`DebugStats`] can ask it for a near-tier count.
    backend: Bk,
    /// Monotonic source of opaque file-handle tokens.
    next_handle: AtomicU64,
}

impl<Bk: SyncByteBackend + Clone> CasVfs<Bk> {
    fn wrap(backend: Bk, tree: WorkingTree<Bk>) -> Self {
        Self {
            tree: Mutex::new(tree),
            backend,
            next_handle: AtomicU64::new(1),
        }
    }

    /// An empty **pure-mode** filesystem (a single root directory), no lower.
    pub fn new(backend: Bk) -> Self {
        Self::wrap(backend.clone(), WorkingTree::new(backend))
    }

    /// A **pure-mode** filesystem hydrated from a sealed root [`TreeId`].
    pub fn from_root(backend: Bk, root: TreeId) -> VfsResult<Self> {
        Ok(Self::wrap(
            backend.clone(),
            WorkingTree::from_root(backend, root)?,
        ))
    }

    /// An empty copy-on-write **overlay** over `lower`: untouched names fall
    /// through to it; writes/deletes materialize into the CAS.
    pub fn overlay(backend: Bk, lower: Arc<dyn VfsService>) -> Self {
        Self::wrap(backend.clone(), WorkingTree::overlay(backend, lower))
    }

    /// An **overlay** rehydrated from a persisted sparse `delta` on top of `lower`.
    pub fn from_overlay(backend: Bk, lower: Arc<dyn VfsService>, delta: TreeId) -> VfsResult<Self> {
        Ok(Self::wrap(
            backend.clone(),
            WorkingTree::from_overlay(backend, lower, delta)?,
        ))
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

/// The `.vfs/debug` provider: a fresh point-in-time snapshot of the working
/// tree under its lock, plus the backend's near-tier count where the backend
/// shape supports one (see [`NearCount`]).
impl<Bk: SyncByteBackend + Clone + NearCount> DebugStats for CasVfs<Bk> {
    fn debug_snapshot(&self) -> DebugSnapshot {
        let tree = self.lock();
        DebugSnapshot {
            near_objects: self.backend.near_count(),
            dirty_files: tree.dirty_file_count(),
            overlay_paths: tree
                .overlay_entries()
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

impl<Bk: SyncByteBackend + Clone> VfsService for CasVfs<Bk> {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        let mut tree = self.lock();
        let id = tree.lookup(parent, name)?;
        Ok(Entry::new(tree.getattr(id)?))
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        self.lock().getattr(inode_id)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        self.lock().readdir(inode_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        {
            let mut tree = self.lock();
            // Validate the target exists and is a file; directories are opened
            // via readdir, not here.
            if tree.getattr(request.inode_id)?.kind == NodeKind::Directory {
                return Err(VfsError::IsDirectory);
            }
            // O_TRUNC: empty the file on open (copies up a fall-through file).
            if request.truncate {
                tree.truncate(request.inode_id, 0)?;
            }
        }
        Ok(OpenedFile {
            handle: self.next_handle(),
            direct_io: false,
            keep_cache: true,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        self.lock()
            .read_at(request.inode_id, request.offset, request.size)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        self.lock()
            .write_at(request.inode_id, request.offset, request.data)
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

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.lock().unlink(parent, name)
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.lock().rmdir(parent, name)
    }

    fn rename(
        &self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()> {
        self.lock().rename(parent, name, new_parent, new_name)
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        let mut tree = self.lock();
        tree.truncate(inode_id, size)?;
        tree.getattr(inode_id)
    }

    fn symlink(&self, parent: InodeId, name: &OsStr, target: &OsStr) -> VfsResult<Entry> {
        let mut tree = self.lock();
        let id = tree.symlink(parent, name, target)?;
        Ok(Entry::new(tree.getattr(id)?))
    }

    fn readlink(&self, inode_id: InodeId) -> VfsResult<std::ffi::OsString> {
        self.lock().readlink(inode_id)
    }

    fn setattr(&self, request: SetAttrRequest) -> VfsResult<NodeAttr> {
        let mut tree = self.lock();
        if let Some(mode) = request.mode {
            tree.set_mode(request.inode_id, mode)?;
        }
        // atime/mtime persist for the session: they live on the in-memory
        // inode (never in the CAS), set exactly as requested.
        if request.atime.is_some() || request.mtime.is_some() {
            tree.set_times(request.inode_id, request.mtime, request.atime)?;
        }
        tree.getattr(request.inode_id)
    }

    fn statfs(&self, _inode_id: InodeId) -> VfsResult<StatFs> {
        self.lock().statfs()
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, _inode_id: InodeId, _handle: FileHandle, _datasync: bool) -> VfsResult<()> {
        // Durability is the checkpoint layer's job (soft-fsync contract); a
        // signaling decorator turns this call into a checkpoint trigger event.
        Ok(())
    }

    fn release(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_vfs_api::{OpenMode, ROOT_INODE_ID};

    type Svc = CasVfs<MemoryByteBackend>;

    /// Create+write a file through the service, returning its node id.
    fn write_file(svc: &Svc, parent: InodeId, name: &str, data: &[u8]) -> InodeId {
        let created = svc.create(CreateRequest::file(parent, name, 0o644)).unwrap();
        let id = created.entry.attr.inode_id;
        let handle = created.opened.expect("create opens the file").handle;
        let n = svc
            .write(WriteRequest {
                inode_id: id,
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
    fn read_all(svc: &Svc, id: InodeId) -> Vec<u8> {
        let opened = svc
            .open(OpenRequest {
                inode_id: id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .unwrap();
        let bytes = svc
            .read(ReadRequest {
                inode_id: id,
                handle: opened.handle,
                offset: 0,
                size: 4096,
            })
            .unwrap();
        svc.release(opened.handle).unwrap();
        bytes
    }

    fn names(svc: &Svc, dir: InodeId) -> Vec<String> {
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
        let root = ROOT_INODE_ID;

        let dir = svc
            .mkdir(CreateRequest::directory(root, "dir", 0o755))
            .unwrap()
            .attr
            .inode_id;
        write_file(&svc, dir, "a.txt", b"alpha");
        write_file(&svc, root, "b.txt", b"beta");

        assert_eq!(names(&svc, root), vec!["b.txt".to_string(), "dir".to_string()]);

        let a = svc.lookup(dir, OsStr::new("a.txt")).unwrap().attr.inode_id;
        assert_eq!(read_all(&svc, a), b"alpha");
        assert_eq!(svc.getattr(a).unwrap().kind, NodeKind::RegularFile);

        // Opening a directory as a file is rejected.
        assert_eq!(
            svc.open(OpenRequest {
                inode_id: dir,
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
        let root = ROOT_INODE_ID;
        write_file(&svc, root, "f", b"first");

        let sealed = svc.seal().unwrap();

        // A fresh service hydrated from the sealed root (shared backend) sees it.
        let svc2 = CasVfs::from_root(backend, sealed).unwrap();
        let f = svc2.lookup(root, OsStr::new("f")).unwrap().attr.inode_id;
        assert_eq!(read_all(&svc2, f), b"first");
        assert_eq!(names(&svc2, root), vec!["f".to_string()]);
    }

    #[test]
    fn open_with_truncate_empties_the_file() {
        let svc = CasVfs::in_memory();
        let root = ROOT_INODE_ID;
        let id = write_file(&svc, root, "f", b"some existing content");
        assert_eq!(svc.getattr(id).unwrap().size, 21);

        // O_TRUNC on open empties the file.
        let opened = svc
            .open(OpenRequest {
                inode_id: id,
                mode: OpenMode::WriteOnly,
                truncate: true,
                append: false,
            })
            .unwrap();
        svc.release(opened.handle).unwrap();

        assert_eq!(svc.getattr(id).unwrap().size, 0);
        assert!(read_all(&svc, id).is_empty());
    }

    /// The `setattr(size)` path kernels use for `> file` and `truncate(2)`:
    /// shrink, grow (zero-filled), and the returned attrs reflect the change.
    #[test]
    fn truncate_shrinks_and_grows_through_the_service() {
        let svc = CasVfs::in_memory();
        let id = write_file(&svc, ROOT_INODE_ID, "f", b"0123456789");

        let attr = svc.truncate(id, 4).unwrap();
        assert_eq!(attr.size, 4);
        assert_eq!(read_all(&svc, id), b"0123");

        let attr = svc.truncate(id, 6).unwrap();
        assert_eq!(attr.size, 6);
        assert_eq!(read_all(&svc, id), b"0123\0\0");
    }

    /// `symlink` + `readlink` through the service, including a seal →
    /// `from_root` roundtrip (a checkpoint's view of the link).
    #[test]
    fn symlink_readlink_and_seal_roundtrip_through_the_service() {
        let backend = MemoryByteBackend::new();
        let svc = CasVfs::new(backend.clone());
        let root = ROOT_INODE_ID;
        write_file(&svc, root, "target.txt", b"contents");

        let entry = svc.symlink(root, OsStr::new("link"), OsStr::new("target.txt")).unwrap();
        assert_eq!(entry.attr.kind, NodeKind::Symlink);
        let link = entry.attr.inode_id;
        assert_eq!(svc.readlink(link).unwrap(), OsStr::new("target.txt"));

        // The link lists as a symlink.
        let row = svc
            .readdir(root)
            .unwrap()
            .into_iter()
            .find(|e| e.name == "link")
            .unwrap();
        assert_eq!(row.kind, NodeKind::Symlink);

        // readlink of a non-link is an error.
        let target = svc.lookup(root, OsStr::new("target.txt")).unwrap().attr.inode_id;
        assert!(matches!(svc.readlink(target), Err(VfsError::InvalidInput(_))));

        // Seal (checkpoint) → a fresh service over the sealed root resolves it.
        let sealed = svc.seal().unwrap();
        let svc2 = CasVfs::from_root(backend, sealed).unwrap();
        let link2 = svc2.lookup(root, OsStr::new("link")).unwrap().attr.inode_id;
        assert_eq!(svc2.getattr(link2).unwrap().kind, NodeKind::Symlink);
        assert_eq!(svc2.readlink(link2).unwrap(), OsStr::new("target.txt"));
    }

    /// `setattr` with a mode persists through seal; atime/mtime persist for
    /// the session (in-memory only — a rehydrated tree reports the zeroed
    /// defaults).
    #[test]
    fn setattr_chmod_persists_through_the_service() {
        let backend = MemoryByteBackend::new();
        let svc = CasVfs::new(backend.clone());
        let id = write_file(&svc, ROOT_INODE_ID, "run.sh", b"#!/bin/sh\n");
        assert_eq!(svc.getattr(id).unwrap().perm, 0o644);

        let attr = svc
            .setattr(SetAttrRequest {
                inode_id: id,
                mode: Some(0o755),
                atime: Some(std::time::SystemTime::now()),
                mtime: Some(std::time::SystemTime::now()),
            })
            .unwrap();
        assert_eq!(attr.perm, 0o755);

        let sealed = svc.seal().unwrap();
        let svc2 = CasVfs::from_root(backend, sealed).unwrap();
        let id2 = svc2.lookup(ROOT_INODE_ID, OsStr::new("run.sh")).unwrap().attr.inode_id;
        assert_eq!(svc2.getattr(id2).unwrap().perm, 0o755);
    }

    /// `utimes` through the service sets atime/mtime exactly (and reports the
    /// full attr back); the set times hold for the session's lifetime.
    #[test]
    fn setattr_utimes_persist_for_the_session() {
        use std::time::{Duration, UNIX_EPOCH};
        let svc = CasVfs::in_memory();
        let id = write_file(&svc, ROOT_INODE_ID, "stamped.txt", b"x");
        let t = UNIX_EPOCH + Duration::new(1_577_836_800, 0); // 2020-01-01

        let attr = svc
            .setattr(SetAttrRequest {
                inode_id: id,
                mode: None,
                atime: Some(t),
                mtime: Some(t),
            })
            .unwrap();
        assert_eq!((attr.atime, attr.mtime), (t, t));
        // A later getattr still reports the set times (session persistence).
        let again = svc.getattr(id).unwrap();
        assert_eq!((again.atime, again.mtime), (t, t));
        // The returned size was synthesized against the live writer, not lost.
        assert_eq!(attr.size, 1);
    }

    /// A write through the service bumps mtime; a read does not touch atime.
    #[test]
    fn write_bumps_mtime_read_leaves_atime() {
        use std::time::{Duration, UNIX_EPOCH};
        let svc = CasVfs::in_memory();
        let id = write_file(&svc, ROOT_INODE_ID, "f.txt", b"data");
        let t0 = UNIX_EPOCH + Duration::new(1_577_836_800, 0);
        svc.setattr(SetAttrRequest {
            inode_id: id,
            mode: None,
            atime: Some(t0),
            mtime: Some(t0),
        })
        .unwrap();

        let _ = read_all(&svc, id);
        assert_eq!(svc.getattr(id).unwrap().atime, t0, "a read never touches atime");

        let opened = svc
            .open(OpenRequest {
                inode_id: id,
                mode: OpenMode::ReadWrite,
                truncate: false,
                append: false,
            })
            .unwrap();
        svc.write(WriteRequest {
            inode_id: id,
            handle: opened.handle,
            offset: 4,
            data: b"!",
        })
        .unwrap();
        svc.release(opened.handle).unwrap();
        let attr = svc.getattr(id).unwrap();
        assert!(attr.mtime > t0, "the write bumps mtime to now");
    }

    /// A lower-less service answers `statfs` with the synthetic shape.
    #[test]
    fn statfs_without_a_lower_is_synthetic() {
        let svc = CasVfs::in_memory();
        let stats = svc.statfs(ROOT_INODE_ID).unwrap();
        assert_eq!(stats, StatFs::synthetic());
    }
}
