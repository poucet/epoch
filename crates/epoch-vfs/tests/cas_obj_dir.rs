//! End-to-end tests for the `.vfs/cas` typed object portal: tree hashes
//! browse as read-only directories (via the shared sub-mount machinery), file
//! hashes `cat` as read-only files, everything else is absent.

use std::ffi::OsStr;
use std::sync::Arc;

use epoch_cas::{Bytes, MemoryByteBackend, TieredByteBackend};
use epoch_file_cas::FileCas;
use epoch_vfs::control_plane::CP_BASE;
use epoch_vfs::{CasObjectDir, CasVfs, ControlPlaneVfs};
use epoch_vfs_api::{
    CreateRequest, InodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, ROOT_INODE_ID, VfsError,
    VfsService, WriteRequest,
};

type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

struct Fixture {
    live: Arc<CasVfs<Tiered>>,
    tier: Tiered,
    cp: ControlPlaneVfs,
}

fn fixture() -> Fixture {
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let live = Arc::new(CasVfs::new(tier.clone()));
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("cas", {
        let tier = tier.clone();
        move |slot| Arc::new(CasObjectDir::new(slot, tier))
    });
    Fixture { live, tier, cp }
}

/// Create `/dir/name` files on the live fs and seal, returning the root hash.
fn build_and_seal(f: &Fixture) -> String {
    let dir = f
        .live
        .mkdir(CreateRequest::directory(ROOT_INODE_ID, "sub", 0o755))
        .unwrap()
        .attr
        .inode_id;
    for (parent, name, content) in [
        (ROOT_INODE_ID, "top.txt", b"top content".as_slice()),
        (dir, "inner.txt", b"inner content".as_slice()),
    ] {
        let created = f
            .live
            .create(CreateRequest::file(parent, name, 0o644))
            .unwrap();
        let node = created.entry.attr.inode_id;
        let handle = created.opened.unwrap().handle;
        f.live
            .write(WriteRequest {
                inode_id: node,
                handle,
                offset: 0,
                data: content,
            })
            .unwrap();
        f.live.release(handle).unwrap();
    }
    f.live.seal().unwrap().to_hex()
}

fn cas_dir_id(cp: &ControlPlaneVfs) -> InodeId {
    let vfs = cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    cp.lookup(vfs.attr.inode_id, OsStr::new("cas"))
        .unwrap()
        .attr
        .inode_id
}

fn read_all(vfs: &dyn VfsService, dir: InodeId, name: &str) -> Result<Vec<u8>, VfsError> {
    let entry = vfs.lookup(dir, OsStr::new(name))?;
    let opened = vfs.open(OpenRequest {
        inode_id: entry.attr.inode_id,
        mode: OpenMode::ReadOnly,
        truncate: false,
        append: false,
    })?;
    let data = vfs.read(ReadRequest {
        inode_id: entry.attr.inode_id,
        handle: opened.handle,
        offset: 0,
        size: u32::MAX,
    })?;
    vfs.release(opened.handle)?;
    Ok(data)
}

#[test]
fn tree_hashes_browse_as_readonly_directories() {
    let f = fixture();
    let root_hex = build_and_seal(&f);
    let cas = cas_dir_id(&f.cp);

    // The root tree hash is a browsable directory.
    let tree = f.cp.lookup(cas, OsStr::new(&root_hex)).unwrap();
    assert_eq!(tree.attr.kind, NodeKind::Directory);
    assert_eq!(tree.attr.perm & 0o222, 0, "write bits stripped");

    let names: Vec<String> = f
        .cp
        .readdir(tree.attr.inode_id)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"top.txt".to_string()));
    assert!(names.contains(&"sub".to_string()));

    // Files inside read correctly; nested subtrees browse too.
    assert_eq!(read_all(&f.cp, tree.attr.inode_id, "top.txt").unwrap(), b"top content");
    let sub = f
        .cp
        .lookup(tree.attr.inode_id, OsStr::new("sub"))
        .unwrap()
        .attr
        .inode_id;
    assert_eq!(read_all(&f.cp, sub, "inner.txt").unwrap(), b"inner content");

    // Every id stays inside the control-plane range (no leaked local ids).
    for entry in f.cp.readdir(sub).unwrap() {
        assert!(entry.inode_id.0 >= CP_BASE, "leaked local id: {entry:?}");
    }

    // Mutations are refused throughout.
    assert!(matches!(
        f.cp.create(CreateRequest::file(tree.attr.inode_id, "nope", 0o644)),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    let file = f
        .cp
        .lookup(tree.attr.inode_id, OsStr::new("top.txt"))
        .unwrap()
        .attr
        .inode_id;
    assert!(matches!(
        f.cp.open(OpenRequest {
            inode_id: file,
            mode: OpenMode::ReadWrite,
            truncate: false,
            append: false,
        }),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    assert!(matches!(
        f.cp.truncate(file, 0),
        Err(VfsError::ReadOnlyFilesystem)
    ));
}

#[test]
fn file_hashes_cat_as_readonly_files() {
    let f = fixture();
    // Store a blob directly (large enough to be chunked: 2.5 MiB).
    let files = FileCas::new(f.tier.clone());
    let content: Vec<u8> = (0..(2 * 1024 * 1024 + 512 * 1024)).map(|i| (i % 251) as u8).collect();
    let blob = files.put(Bytes::from(content.clone())).unwrap();
    let cas = cas_dir_id(&f.cp);

    let entry = f.cp.lookup(cas, OsStr::new(&blob.to_hex())).unwrap();
    assert_eq!(entry.attr.kind, NodeKind::RegularFile);
    assert_eq!(entry.attr.size, content.len() as u64);

    // Offset reads reassemble across chunk boundaries.
    let opened = f
        .cp
        .open(OpenRequest {
            inode_id: entry.attr.inode_id,
            mode: OpenMode::ReadOnly,
            truncate: false,
            append: false,
        })
        .unwrap();
    let span = f
        .cp
        .read(ReadRequest {
            inode_id: entry.attr.inode_id,
            handle: opened.handle,
            offset: 1024 * 1024 - 10,
            size: 20,
        })
        .unwrap();
    assert_eq!(span, content[1024 * 1024 - 10..1024 * 1024 + 10]);
    f.cp.release(opened.handle).unwrap();

    // A blob is not a directory.
    assert!(matches!(
        f.cp.readdir(entry.attr.inode_id),
        Err(VfsError::NotDirectory)
    ));
}

#[test]
fn chunks_unknown_hashes_and_garbage_names_are_absent() {
    let f = fixture();
    let files = FileCas::new(f.tier.clone());
    let content: Vec<u8> = vec![7u8; 2 * 1024 * 1024];
    let blob = files.put(Bytes::from(content)).unwrap();
    let cas = cas_dir_id(&f.cp);

    // A chunk id (reachable via the manifest) is deliberately unexposed.
    let manifest = files.manifest(&blob).unwrap().unwrap();
    let chunk_hex = match manifest {
        epoch_file_cas::FileObject::Chunked { chunks } => chunks[0].id.to_hex(),
        other => panic!("expected chunked, got {other:?}"),
    };
    assert!(matches!(
        f.cp.lookup(cas, OsStr::new(&chunk_hex)),
        Err(VfsError::NotFound)
    ));

    // Unknown-but-valid hex and garbage both resolve to nothing.
    let missing = "ab".repeat(32);
    assert!(matches!(
        f.cp.lookup(cas, OsStr::new(&missing)),
        Err(VfsError::NotFound)
    ));
    assert!(matches!(
        f.cp.lookup(cas, OsStr::new("not-a-hash")),
        Err(VfsError::NotFound)
    ));
}
