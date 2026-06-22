//! Integration tests for `epoch-worktree`: the read (hydrate) + write (seal)
//! roundtrip through the content-addressed object stack, plus edge cases.

use std::ffi::OsStr;

use epoch_cas::MemoryByteBackend;
use epoch_vfs_api::{NodeKind, VfsError};
use epoch_worktree::WorkingTree;

type Wt = WorkingTree<MemoryByteBackend>;

fn write_file(wt: &mut Wt, parent: epoch_vfs_api::NodeId, name: &str, content: &[u8]) {
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
    let wt2 = Wt::from_root(backend.clone(), sealed).unwrap();
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

    let wt2 = Wt::from_root(backend.clone(), sealed).unwrap();
    let f = wt2.lookup(wt2.root(), OsStr::new("sparse")).unwrap();
    assert_eq!(wt2.read_at(f, 0, 6).unwrap(), vec![0, 0, 0, 0, b'A', b'B']);
    assert_eq!(wt2.getattr(f).unwrap().size, 6);
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
    let f = wt2.lookup(wt2.root(), OsStr::new("f")).unwrap();
    wt2.write_at(f, 0, b"second!").unwrap();
    let root2 = wt2.seal().unwrap();

    assert_ne!(root1, root2);

    let wt3 = Wt::from_root(backend.clone(), root2).unwrap();
    let f3 = wt3.lookup(wt3.root(), OsStr::new("f")).unwrap();
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
