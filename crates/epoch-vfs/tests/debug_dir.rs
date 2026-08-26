//! End-to-end tests for the `.vfs/debug` statistics directory: a live
//! `CasVfs` over a tiered backend exposed as the `DebugStats` provider through
//! the control plane `debug` feature.

use std::ffi::OsStr;
use std::sync::Arc;

use epoch_cas::{MemoryByteBackend, TieredByteBackend};
use epoch_history::History;
use epoch_timeline::MemoryTimelineStore;
use epoch_vfs::{CasVfs, ControlPlaneVfs};
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
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("debug", {
        let live = live.clone();
        move |slot| Arc::new(epoch_vfs::DebugDir::new(slot, live))
    });
    Fixture { live, tier, cp }
}

/// An overlay fixture: the live tree overlays `lower_dir` (a host directory
/// served by `PassthroughVfs`).
fn overlay_fixture(lower_dir: &std::path::Path) -> Fixture {
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let lower: Arc<dyn VfsService> = Arc::new(epoch_vfs::PassthroughVfs::new(lower_dir));
    let live = Arc::new(CasVfs::overlay(tier.clone(), lower));
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("debug", {
        let live = live.clone();
        move |slot| Arc::new(epoch_vfs::DebugDir::new(slot, live))
    });
    Fixture { live, tier, cp }
}

/// Create (or reopen) `/name` on the live fs and write `content` at offset 0.
fn write_file(live: &CasVfs<Tiered>, name: &str, content: &[u8]) {
    let node = match live.lookup(ROOT_INODE_ID, OsStr::new(name)) {
        Ok(entry) => entry.attr.inode_id,
        Err(_) => {
            live.create(CreateRequest::file(ROOT_INODE_ID, name, 0o644))
                .unwrap()
                .entry
                .attr
                .inode_id
        }
    };
    let opened = live
        .open(OpenRequest {
            inode_id: node,
            mode: OpenMode::ReadWrite,
            truncate: true,
            append: false,
        })
        .unwrap();
    live.write(WriteRequest {
        inode_id: node,
        handle: opened.handle,
        offset: 0,
        data: content,
    })
    .unwrap();
    live.release(opened.handle).unwrap();
}

/// Read a whole file through any `VfsService`.
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

fn debug_dir_id(cp: &ControlPlaneVfs) -> InodeId {
    let vfs = cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    cp.lookup(vfs.attr.inode_id, OsStr::new("debug"))
        .unwrap()
        .attr
        .inode_id
}

/// Read `.vfs/debug/<name>` as a UTF-8 string.
fn debug_read(f: &Fixture, name: &str) -> String {
    let debug = debug_dir_id(&f.cp);
    String::from_utf8(read_all(&f.cp, debug, name).unwrap()).unwrap()
}

#[test]
fn debug_dir_lists_the_three_files_and_needs_a_provider() {
    let f = fixture();
    let vfs = f.cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();

    // `.vfs` lists `debug`, and `.vfs/debug` IS listable: exactly 3 files.
    let vfs_names: Vec<String> = f
        .cp
        .readdir(vfs.attr.inode_id)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(vfs_names.contains(&"debug".to_string()));

    let debug = debug_dir_id(&f.cp);
    let entries = f.cp.readdir(debug).unwrap();
    let names: Vec<String> = entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![".", "..", "near-objects", "dirty-files", "overlay"]
    );
    for entry in entries.iter().skip(2) {
        assert_eq!(entry.kind, NodeKind::RegularFile);
        let attr = f.cp.getattr(entry.inode_id).unwrap();
        assert_eq!(attr.perm & 0o222, 0, "debug files carry no write bits");
    }

    // The files are read-only: any writable open is refused.
    let near = f.cp.lookup(debug, OsStr::new("near-objects")).unwrap();
    assert!(matches!(
        f.cp.open(OpenRequest {
            inode_id: near.attr.inode_id,
            mode: OpenMode::ReadWrite,
            truncate: false,
            append: false,
        }),
        Err(VfsError::ReadOnlyFilesystem)
    ));

    // Without a provider, `.vfs/debug` does not exist at all.
    let bare = ControlPlaneVfs::new(f.live.clone());
    let vfs = bare.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    assert!(matches!(
        bare.lookup(vfs.attr.inode_id, OsStr::new("debug")),
        Err(VfsError::NotFound)
    ));
    let names: Vec<String> = bare
        .readdir(vfs.attr.inode_id)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(!names.contains(&"debug".to_string()));
}

#[test]
fn near_objects_reflects_the_staging_tier_and_drops_after_checkpoint_drain() {
    let f = fixture();
    assert_eq!(debug_read(&f, "near-objects"), "0\n", "nothing staged yet");

    // Sealing stages tree + blob objects into the near tier.
    write_file(&f.live, "a.txt", b"alpha");
    let root = f.live.seal().unwrap();
    let staged = f.tier.near().len();
    assert!(staged > 0, "seal stages objects into near");
    assert_eq!(debug_read(&f, "near-objects"), format!("{staged}\n"));

    // A checkpoint promotes the closure into far and drains near.
    let history = History::new(f.tier.clone(), Arc::new(MemoryTimelineStore::new()), "main");
    history.commit_root_at(root, 1_000, None).unwrap().unwrap();
    assert_eq!(f.tier.near().len(), 0, "checkpoint drained the staging tier");
    assert_eq!(debug_read(&f, "near-objects"), "0\n");
}

#[test]
fn dirty_files_counts_written_but_unsealed_files() {
    let f = fixture();
    assert_eq!(debug_read(&f, "dirty-files"), "0\n");

    write_file(&f.live, "wip.txt", b"in flight");
    assert_eq!(debug_read(&f, "dirty-files"), "1\n", "an un-sealed writer counts");

    f.live.seal().unwrap();
    assert_eq!(debug_read(&f, "dirty-files"), "0\n", "seal commits the writer");
}

#[test]
fn overlay_lists_created_files_and_tombstones() {
    let lower = tempfile::tempdir().unwrap();
    std::fs::write(lower.path().join("base.txt"), b"from lower").unwrap();
    std::fs::write(lower.path().join("doomed.txt"), b"soon gone").unwrap();
    let f = overlay_fixture(lower.path());

    // Untouched overlay: reading a lower file must not surface delta.
    assert_eq!(read_all(&f.cp, ROOT_INODE_ID, "base.txt").unwrap(), b"from lower");
    assert_eq!(debug_read(&f, "overlay"), "");

    write_file(&f.live, "new.txt", b"created up here");
    f.live.unlink(ROOT_INODE_ID, OsStr::new("doomed.txt")).unwrap();

    assert_eq!(
        debug_read(&f, "overlay"),
        "/doomed.txt (tombstone)\n/new.txt\n"
    );
}

#[test]
fn overlay_reports_sealed_subtrees_without_hydrating_them() {
    // Build and seal /top.txt + /sub/inner.txt, then rehydrate from the root.
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let builder = CasVfs::new(tier.clone());
    write_file(&builder, "top.txt", b"top");
    let sub = builder
        .mkdir(CreateRequest::directory(ROOT_INODE_ID, "sub", 0o755))
        .unwrap()
        .attr
        .inode_id;
    builder
        .create(CreateRequest::file(sub, "inner.txt", 0o644))
        .unwrap();
    let root = builder.seal().unwrap();

    let live = Arc::new(CasVfs::from_root(tier.clone(), root).unwrap());
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("debug", {
        let live = live.clone();
        move |slot| Arc::new(epoch_vfs::DebugDir::new(slot, live))
    });
    let f = Fixture { live, tier, cp };

    // Nothing faulted in yet: the root itself is one sealed subtree.
    assert_eq!(debug_read(&f, "overlay"), "/ (sealed subtree)\n");

    // One root-level lookup opens the root spine; `sub` stays sealed and is
    // reported without being decoded.
    f.live.lookup(ROOT_INODE_ID, OsStr::new("top.txt")).unwrap();
    assert_eq!(
        debug_read(&f, "overlay"),
        "/sub/ (sealed subtree)\n/top.txt\n"
    );
}
