//! Integration tests for `epoch-worktree`: the read (hydrate) + write (seal)
//! roundtrip through the content-addressed object stack, plus edge cases.

use std::ffi::OsStr;

use epoch_cas::MemoryByteBackend;
use epoch_vfs_api::{NodeKind, VfsError};
use epoch_worktree::WorkingTree;

type Wt = WorkingTree<MemoryByteBackend>;

fn write_file(wt: &mut Wt, parent: epoch_vfs_api::InodeId, name: &str, content: &[u8]) {
    let id = wt.create(parent, OsStr::new(name), 0o100644).unwrap();
    wt.write_at(id, 0, content).unwrap();
}

fn names(entries: &[epoch_vfs_api::DirEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn seal_then_hydrate_roundtrip() {
    let backend = MemoryByteBackend::new();
    let mut wt = Wt::new(backend.clone());
    let root = wt.root();

    let dir = wt.mkdir(root, OsStr::new("dir"), 0o040755).unwrap();
    write_file(&mut wt, dir, "a.txt", b"alpha contents");
    write_file(&mut wt, root, "b.txt", b"beta");

    let sealed = wt.seal().unwrap();

    // Hydrate a fresh tree from the sealed root (shared backend storage).
    let mut wt2 = Wt::from_root(backend.clone(), sealed).unwrap();
    let root2 = wt2.root();

    let mut top = names(&wt2.readdir(root2).unwrap());
    top.sort();
    assert_eq!(top, vec!["b.txt".to_string(), "dir".to_string()]);

    let dir2 = wt2.lookup(root2, OsStr::new("dir")).unwrap();
    assert_eq!(names(&wt2.readdir(dir2).unwrap()), vec!["a.txt".to_string()]);

    let a = wt2.lookup(dir2, OsStr::new("a.txt")).unwrap();
    assert_eq!(wt2.read_at(a, 0, 1024).unwrap(), b"alpha contents");
    assert_eq!(wt2.getattr(a).unwrap().size, "alpha contents".len() as u64);

    let b = wt2.lookup(root2, OsStr::new("b.txt")).unwrap();
    assert_eq!(wt2.read_at(b, 0, 1024).unwrap(), b"beta");

    // Directory kind survives the roundtrip.
    assert_eq!(wt2.getattr(dir2).unwrap().kind, NodeKind::Directory);
    assert_eq!(wt2.getattr(a).unwrap().kind, NodeKind::RegularFile);
}

#[test]
fn write_at_offset_leaves_zero_hole() {
    let backend = MemoryByteBackend::new();
    let mut wt = Wt::new(backend.clone());
    let root = wt.root();

    let id = wt.create(root, OsStr::new("sparse"), 0o100644).unwrap();
    wt.write_at(id, 4, b"AB").unwrap();
    let sealed = wt.seal().unwrap();

    let mut wt2 = Wt::from_root(backend.clone(), sealed).unwrap();
    let root2 = wt2.root();
    let f = wt2.lookup(root2, OsStr::new("sparse")).unwrap();
    assert_eq!(wt2.read_at(f, 0, 6).unwrap(), vec![0, 0, 0, 0, b'A', b'B']);
    assert_eq!(wt2.getattr(f).unwrap().size, 6);
}

#[test]
fn truncate_shrinks_zero_extends_and_empties() {
    let mut wt = Wt::new(MemoryByteBackend::new());
    let root = wt.root();
    let f = wt.create(root, OsStr::new("f"), 0o100644).unwrap();
    wt.write_at(f, 0, b"hello world").unwrap();
    assert_eq!(wt.getattr(f).unwrap().size, 11);

    // Shrink.
    wt.truncate(f, 5).unwrap();
    assert_eq!(wt.getattr(f).unwrap().size, 5);
    assert_eq!(wt.read_at(f, 0, 1024).unwrap(), b"hello");

    // Zero-extend.
    wt.truncate(f, 8).unwrap();
    assert_eq!(wt.getattr(f).unwrap().size, 8);
    assert_eq!(wt.read_at(f, 0, 1024).unwrap(), b"hello\0\0\0");

    // Empty (the O_TRUNC fast path).
    wt.truncate(f, 0).unwrap();
    assert_eq!(wt.getattr(f).unwrap().size, 0);
    assert!(wt.read_at(f, 0, 1024).unwrap().is_empty());

    // Truncating a directory is rejected.
    let d = wt.mkdir(root, OsStr::new("d"), 0o040755).unwrap();
    assert_eq!(wt.truncate(d, 0).unwrap_err(), VfsError::IsDirectory);
}

#[test]
fn overwrite_then_reseal_changes_root() {
    let backend = MemoryByteBackend::new();
    let mut wt = Wt::new(backend.clone());
    let root = wt.root();
    write_file(&mut wt, root, "f", b"first");
    let root1 = wt.seal().unwrap();

    // Edit through a hydrated tree, reseal.
    let mut wt2 = Wt::from_root(backend.clone(), root1).unwrap();
    let r2 = wt2.root();
    let f = wt2.lookup(r2, OsStr::new("f")).unwrap();
    wt2.write_at(f, 0, b"second!").unwrap();
    let root2 = wt2.seal().unwrap();

    assert_ne!(root1, root2);

    let mut wt3 = Wt::from_root(backend.clone(), root2).unwrap();
    let r3 = wt3.root();
    let f3 = wt3.lookup(r3, OsStr::new("f")).unwrap();
    assert_eq!(wt3.read_at(f3, 0, 1024).unwrap(), b"second!");
}

#[test]
fn identical_trees_seal_to_the_same_id() {
    let build = || {
        let mut wt = Wt::new(MemoryByteBackend::new());
        let root = wt.root();
        let sub = wt.mkdir(root, OsStr::new("sub"), 0o040755).unwrap();
        write_file(&mut wt, sub, "x", b"same bytes");
        write_file(&mut wt, root, "top", b"hello");
        wt.seal().unwrap()
    };
    assert_eq!(build(), build());
}

#[test]
fn error_cases() {
    let mut wt = Wt::new(MemoryByteBackend::new());
    let root = wt.root();

    // lookup of a missing name
    assert_eq!(wt.lookup(root, OsStr::new("nope")).unwrap_err(), VfsError::NotFound);

    // create over an existing name
    wt.create(root, OsStr::new("dup"), 0o100644).unwrap();
    assert_eq!(
        wt.create(root, OsStr::new("dup"), 0o100644).unwrap_err(),
        VfsError::AlreadyExists
    );

    // rmdir on a non-empty directory
    let dir = wt.mkdir(root, OsStr::new("d"), 0o040755).unwrap();
    write_file(&mut wt, dir, "child", b"x");
    assert_eq!(
        wt.rmdir(root, OsStr::new("d")).unwrap_err(),
        VfsError::DirectoryNotEmpty
    );

    // readdir on a regular file
    let file = wt.lookup(dir, OsStr::new("child")).unwrap();
    assert_eq!(wt.readdir(file).unwrap_err(), VfsError::NotDirectory);

    // unlink a directory / rmdir a file are rejected
    assert_eq!(wt.unlink(root, OsStr::new("d")).unwrap_err(), VfsError::IsDirectory);
    assert_eq!(wt.rmdir(dir, OsStr::new("child")).unwrap_err(), VfsError::NotDirectory);
}

/// Copy-on-write overlay semantics: a `WorkingTree` built over a lower
/// [`VfsService`] must read untouched names through to the lower, copy a file up
/// into the CAS on first write (leaving the lower unmodified), whiteout deletions
/// with tombstones, and seal a **sparse** delta that holds only the mutated spine.
mod overlay {
    use std::collections::{BTreeMap, HashMap};
    use std::ffi::{OsStr, OsString};
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    use epoch_cas::MemoryByteBackend;
    use epoch_vfs_api::{
        CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind,
        OpenRequest, OpenedFile, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
        ROOT_INODE_ID,
    };
    use epoch_worktree::WorkingTree;

    use super::names;

    /// A tiny read-only, in-memory lower service used purely as an overlay base.
    /// Mutating operations return [`VfsError::ReadOnlyFilesystem`] so the tests
    /// also assert the overlay never writes down into the lower.
    enum Node {
        Dir(BTreeMap<OsString, InodeId>),
        File(Vec<u8>),
    }

    struct Lower {
        nodes: HashMap<InodeId, Node>,
    }

    /// Fluent builder for a [`Lower`] tree; every inserted node gets a fresh id.
    struct Build {
        nodes: HashMap<InodeId, Node>,
        next: u64,
    }

    impl Build {
        fn new() -> Self {
            let mut nodes = HashMap::new();
            nodes.insert(ROOT_INODE_ID, Node::Dir(BTreeMap::new()));
            Self {
                nodes,
                next: ROOT_INODE_ID.0 + 1,
            }
        }

        fn insert(&mut self, parent: InodeId, name: &str, node: Node) -> InodeId {
            let id = InodeId(self.next);
            self.next += 1;
            self.nodes.insert(id, node);
            if let Some(Node::Dir(m)) = self.nodes.get_mut(&parent) {
                m.insert(OsString::from(name), id);
            }
            id
        }

        fn file(&mut self, parent: InodeId, name: &str, content: &[u8]) -> InodeId {
            self.insert(parent, name, Node::File(content.to_vec()))
        }

        fn dir(&mut self, parent: InodeId, name: &str) -> InodeId {
            self.insert(parent, name, Node::Dir(BTreeMap::new()))
        }

        fn build(self) -> Arc<Lower> {
            Arc::new(Lower { nodes: self.nodes })
        }
    }

    impl Lower {
        fn attr(&self, id: InodeId) -> VfsResult<NodeAttr> {
            match self.nodes.get(&id).ok_or(VfsError::NotFound)? {
                Node::Dir(_) => Ok(NodeAttr::directory(id, UNIX_EPOCH)),
                Node::File(b) => Ok(NodeAttr::regular_file(id, b.len() as u64, UNIX_EPOCH)),
            }
        }
    }

    impl VfsService for Lower {
        fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
            let dir = match self.nodes.get(&parent).ok_or(VfsError::NotFound)? {
                Node::Dir(m) => m,
                _ => return Err(VfsError::NotDirectory),
            };
            let id = dir.get(name).copied().ok_or(VfsError::NotFound)?;
            Ok(Entry::new(self.attr(id)?))
        }

        fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
            self.attr(inode_id)
        }

        fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
            let dir = match self.nodes.get(&inode_id).ok_or(VfsError::NotFound)? {
                Node::Dir(m) => m,
                _ => return Err(VfsError::NotDirectory),
            };
            Ok(dir
                .iter()
                .map(|(name, &id)| {
                    let kind = match self.nodes.get(&id) {
                        Some(Node::Dir(_)) => NodeKind::Directory,
                        _ => NodeKind::RegularFile,
                    };
                    DirEntry::new(id, name.clone(), kind)
                })
                .collect())
        }

        fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
            Ok(OpenedFile {
                handle: FileHandle(request.inode_id.0),
                direct_io: false,
                keep_cache: true,
            })
        }

        fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
            match self.nodes.get(&request.inode_id).ok_or(VfsError::NotFound)? {
                Node::File(b) => {
                    let start = (request.offset as usize).min(b.len());
                    let end = start.saturating_add(request.size as usize).min(b.len());
                    Ok(b[start..end].to_vec())
                }
                Node::Dir(_) => Err(VfsError::IsDirectory),
            }
        }

        fn write(&self, _: WriteRequest<'_>) -> VfsResult<u32> {
            Err(VfsError::ReadOnlyFilesystem)
        }

        fn create(&self, _: CreateRequest) -> VfsResult<CreatedNode> {
            Err(VfsError::ReadOnlyFilesystem)
        }

        fn mkdir(&self, _: CreateRequest) -> VfsResult<Entry> {
            Err(VfsError::ReadOnlyFilesystem)
        }

        fn unlink(&self, _: InodeId, _: &OsStr) -> VfsResult<()> {
            Err(VfsError::ReadOnlyFilesystem)
        }

        fn rmdir(&self, _: InodeId, _: &OsStr) -> VfsResult<()> {
            Err(VfsError::ReadOnlyFilesystem)
        }

        fn flush(&self, _: FileHandle) -> VfsResult<()> {
            Ok(())
        }

        fn release(&self, _: FileHandle) -> VfsResult<()> {
            Ok(())
        }
    }

    type Wt = WorkingTree<MemoryByteBackend>;

    /// Untouched names are served straight from the lower, `readdir` merges the
    /// two layers, and an unmutated overlay seals to a delta that still resolves
    /// every lower entry on rehydrate.
    #[test]
    fn reads_fall_through_and_unmutated_seal_is_empty() {
        let mut b = Build::new();
        b.file(ROOT_INODE_ID, "keep.txt", b"lower content");
        let sub = b.dir(ROOT_INODE_ID, "sub");
        b.file(sub, "deep.txt", b"deep");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend.clone(), lower.clone());
        let root = wt.root();

        let keep = wt.lookup(root, OsStr::new("keep.txt")).unwrap();
        assert_eq!(wt.read_at(keep, 0, 1024).unwrap(), b"lower content");
        // A repeated lookup returns the *same* overlay id (stable identity).
        assert_eq!(wt.lookup(root, OsStr::new("keep.txt")).unwrap(), keep);

        let mut top = names(&wt.readdir(root).unwrap());
        top.sort();
        assert_eq!(top, vec!["keep.txt".to_string(), "sub".to_string()]);

        let delta = wt.seal().unwrap();
        let mut wt2 = Wt::from_overlay(backend.clone(), lower.clone(), delta).unwrap();
        let r2 = wt2.root();
        let keep2 = wt2.lookup(r2, OsStr::new("keep.txt")).unwrap();
        assert_eq!(wt2.read_at(keep2, 0, 1024).unwrap(), b"lower content");
        let sub2 = wt2.lookup(r2, OsStr::new("sub")).unwrap();
        let deep2 = wt2.lookup(sub2, OsStr::new("deep.txt")).unwrap();
        assert_eq!(wt2.read_at(deep2, 0, 1024).unwrap(), b"deep");
    }

    /// First write to a fall-through file copies its whole content up into the
    /// CAS, keeps the same [`InodeId`], reflects the edit, and never touches the
    /// lower.
    #[test]
    fn write_copies_up_and_leaves_lower_untouched() {
        let mut b = Build::new();
        let f_lower = b.file(ROOT_INODE_ID, "f.txt", b"original-from-lower");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend.clone(), lower.clone());
        let root = wt.root();

        let f = wt.lookup(root, OsStr::new("f.txt")).unwrap();
        wt.write_at(f, 0, b"OVERRIDE").unwrap();
        assert_eq!(wt.read_at(f, 0, 1024).unwrap(), b"OVERRIDE-from-lower");
        // Copy-up preserves the inode identity.
        assert_eq!(wt.lookup(root, OsStr::new("f.txt")).unwrap(), f);

        // The lower blob is unchanged.
        let from_lower = lower
            .read(ReadRequest {
                inode_id: f_lower,
                handle: FileHandle(f_lower.0),
                offset: 0,
                size: 1024,
            })
            .unwrap();
        assert_eq!(from_lower, b"original-from-lower");

        // The edit survives seal + rehydrate.
        let delta = wt.seal().unwrap();
        let mut wt2 = Wt::from_overlay(backend.clone(), lower, delta).unwrap();
        let r2 = wt2.root();
        let f2 = wt2.lookup(r2, OsStr::new("f.txt")).unwrap();
        assert_eq!(wt2.read_at(f2, 0, 1024).unwrap(), b"OVERRIDE-from-lower");
    }

    /// Truncating a fall-through file copies it up (same InodeId), reflects the
    /// new length, and never touches the lower.
    #[test]
    fn truncate_copies_up_and_leaves_lower_untouched() {
        let mut b = Build::new();
        let f_lower = b.file(ROOT_INODE_ID, "f.txt", b"original-from-lower");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend.clone(), lower.clone());
        let root = wt.root();

        // Truncate to a non-zero length copies the lower content up, then shrinks.
        let f = wt.lookup(root, OsStr::new("f.txt")).unwrap();
        wt.truncate(f, 4).unwrap();
        assert_eq!(wt.read_at(f, 0, 1024).unwrap(), b"orig");
        assert_eq!(wt.getattr(f).unwrap().size, 4);
        // Copy-up preserves the inode identity.
        assert_eq!(wt.lookup(root, OsStr::new("f.txt")).unwrap(), f);

        // The lower blob is unchanged.
        let from_lower = lower
            .read(ReadRequest {
                inode_id: f_lower,
                handle: FileHandle(f_lower.0),
                offset: 0,
                size: 1024,
            })
            .unwrap();
        assert_eq!(from_lower, b"original-from-lower");
    }

    /// Deleting a lower name leaves a whiteout: it disappears from `lookup` and
    /// `readdir`, and the whiteout survives seal + rehydrate.
    #[test]
    fn unlink_lower_leaves_a_whiteout() {
        let mut b = Build::new();
        b.file(ROOT_INODE_ID, "a.txt", b"a");
        b.file(ROOT_INODE_ID, "b.txt", b"b");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend.clone(), lower.clone());
        let root = wt.root();

        wt.unlink(root, OsStr::new("a.txt")).unwrap();
        assert_eq!(
            wt.lookup(root, OsStr::new("a.txt")).unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(names(&wt.readdir(root).unwrap()), vec!["b.txt".to_string()]);

        let delta = wt.seal().unwrap();
        let mut wt2 = Wt::from_overlay(backend.clone(), lower, delta).unwrap();
        let r2 = wt2.root();
        assert_eq!(
            wt2.lookup(r2, OsStr::new("a.txt")).unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(names(&wt2.readdir(r2).unwrap()), vec!["b.txt".to_string()]);
    }

    /// Recreating a name that was just whiteout-deleted replaces the tombstone
    /// with fresh upper content.
    #[test]
    fn create_over_a_whiteout() {
        let mut b = Build::new();
        b.file(ROOT_INODE_ID, "f", b"lower");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend, lower);
        let root = wt.root();

        wt.unlink(root, OsStr::new("f")).unwrap();
        let nf = wt.create(root, OsStr::new("f"), 0o100644).unwrap();
        wt.write_at(nf, 0, b"upper-new").unwrap();
        assert_eq!(wt.read_at(nf, 0, 1024).unwrap(), b"upper-new");
        assert_eq!(names(&wt.readdir(root).unwrap()), vec!["f".to_string()]);
    }

    /// The sealed delta holds **only** the mutated spine: rehydrating it over an
    /// *empty* lower shows the copied-up file (and its spine dir) and nothing
    /// else — the untouched sibling was never stored.
    #[test]
    fn seal_stores_only_the_mutated_spine() {
        let mut b = Build::new();
        b.file(ROOT_INODE_ID, "keep.txt", b"keep");
        let sub = b.dir(ROOT_INODE_ID, "sub");
        b.file(sub, "change.txt", b"old");
        let lower = b.build();

        let backend = MemoryByteBackend::new();
        let mut wt = Wt::overlay(backend.clone(), lower);
        let root = wt.root();
        let sub_id = wt.lookup(root, OsStr::new("sub")).unwrap();
        let ch = wt.lookup(sub_id, OsStr::new("change.txt")).unwrap();
        wt.write_at(ch, 0, b"new").unwrap();
        let delta = wt.seal().unwrap();

        // Rehydrate the delta over an EMPTY lower: only the mutated spine remains.
        let empty = Build::new().build();
        let mut wt2 = Wt::from_overlay(backend.clone(), empty, delta).unwrap();
        let r2 = wt2.root();
        // "keep.txt" was never stored (fell through), so it is absent now.
        assert_eq!(names(&wt2.readdir(r2).unwrap()), vec!["sub".to_string()]);
        let s2 = wt2.lookup(r2, OsStr::new("sub")).unwrap();
        assert_eq!(
            names(&wt2.readdir(s2).unwrap()),
            vec!["change.txt".to_string()]
        );
        let ch2 = wt2.lookup(s2, OsStr::new("change.txt")).unwrap();
        assert_eq!(wt2.read_at(ch2, 0, 1024).unwrap(), b"new");
    }
}

#[test]
fn unlink_and_rmdir_remove_entries() {
    let mut wt = Wt::new(MemoryByteBackend::new());
    let root = wt.root();
    write_file(&mut wt, root, "gone", b"bye");
    let dir = wt.mkdir(root, OsStr::new("empty"), 0o040755).unwrap();

    wt.unlink(root, OsStr::new("gone")).unwrap();
    assert_eq!(wt.lookup(root, OsStr::new("gone")).unwrap_err(), VfsError::NotFound);

    wt.rmdir(root, OsStr::new("empty")).unwrap();
    assert_eq!(wt.lookup(root, OsStr::new("empty")).unwrap_err(), VfsError::NotFound);
    let _ = dir;

    assert!(wt.readdir(root).unwrap().is_empty());
}

/// Hydration is lazy: a sealed tree faults in one spine level per access, and an
/// untouched subtree is neither loaded on `from_root` nor walked on `seal`.
mod lazy_hydration {
    use std::ffi::OsStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use epoch_cas::{Bytes, CASError, CASHash, MemoryByteBackend, SyncByteBackend};
    use epoch_worktree::WorkingTree;

    fn write_file(wt: &mut Wt, parent: epoch_vfs_api::InodeId, name: &str, content: &[u8]) {
        let id = wt.create(parent, OsStr::new(name), 0o100644).unwrap();
        wt.write_at(id, 0, content).unwrap();
    }

    /// A `SyncByteBackend` that counts object reads, delegating to an in-memory
    /// store. Lets a test observe exactly when the CAS is touched.
    #[derive(Clone)]
    struct CountingBackend {
        inner: MemoryByteBackend,
        gets: Arc<AtomicUsize>,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self { inner: MemoryByteBackend::new(), gets: Arc::new(AtomicUsize::new(0)) }
        }
        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
        fn reset(&self) {
            self.gets.store(0, Ordering::SeqCst);
        }
    }

    impl SyncByteBackend for CountingBackend {
        fn get(&self, key: &CASHash) -> Result<Option<Bytes>, CASError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.get(key)
        }
        fn put(&self, key: &CASHash, bytes: Bytes) -> Result<(), CASError> {
            self.inner.put(key, bytes)
        }
        fn has(&self, key: &CASHash) -> Result<bool, CASError> {
            self.inner.has(key)
        }
        fn delete(&self, key: &CASHash) -> Result<(), CASError> {
            self.inner.delete(key)
        }
    }

    type Wt = WorkingTree<CountingBackend>;

    /// Build `a/x.txt`, `a/sub/y.txt`, `b/z.txt` under a fresh root and seal it.
    fn seal_sample(backend: &CountingBackend) -> epoch_worktree::TreeId {
        let mut wt = Wt::new(backend.clone());
        let root = wt.root();
        let a = wt.mkdir(root, OsStr::new("a"), 0o040755).unwrap();
        write_file(&mut wt, a, "x.txt", b"x contents");
        let sub = wt.mkdir(a, OsStr::new("sub"), 0o040755).unwrap();
        write_file(&mut wt, sub, "y.txt", b"y contents");
        let b = wt.mkdir(root, OsStr::new("b"), 0o040755).unwrap();
        write_file(&mut wt, b, "z.txt", b"z contents");
        wt.seal().unwrap()
    }

    #[test]
    fn from_root_loads_nothing_until_accessed_and_siblings_fault_in_lazily() {
        let backend = CountingBackend::new();
        let sealed = seal_sample(&backend);

        // from_root must not read a single object — it only records the root id.
        backend.reset();
        let mut wt = Wt::from_root(backend.clone(), sealed).unwrap();
        assert_eq!(backend.gets(), 0, "from_root must not eagerly load any object");

        // Descending into `a` faults in the root tree, then a's, then a/sub's —
        // one object per level, on demand.
        let root = wt.root();
        let a = wt.lookup(root, OsStr::new("a")).unwrap();
        assert!(backend.gets() > 0, "the root tree faults in on first access");
        let sub = wt.lookup(a, OsStr::new("sub")).unwrap();
        let y = wt.lookup(sub, OsStr::new("y.txt")).unwrap();
        assert_eq!(wt.read_at(y, 0, 64).unwrap(), b"y contents");

        // `b` has not been touched. Accessing it now causes *new* reads — proof
        // its spine was never hydrated up front.
        backend.reset();
        let b = wt.lookup(root, OsStr::new("b")).unwrap();
        let z = wt.lookup(b, OsStr::new("z.txt")).unwrap();
        assert!(backend.gets() > 0, "an untouched sibling faults in only when accessed");
        assert_eq!(wt.read_at(z, 0, 64).unwrap(), b"z contents");
    }

    #[test]
    fn sealing_an_untouched_tree_walks_nothing_and_reseals_identically() {
        let backend = CountingBackend::new();
        let sealed = seal_sample(&backend);

        // Hydrate then immediately reseal without touching anything: the root is
        // still Sealed, so seal short-circuits to the original id with zero reads.
        backend.reset();
        let mut wt = Wt::from_root(backend.clone(), sealed).unwrap();
        let resealed = wt.seal().unwrap();
        assert_eq!(resealed, sealed, "an untouched tree reseals to the identical root id");
        assert_eq!(backend.gets(), 0, "sealing an untouched tree walks no objects");
    }
}
