//! End-to-end tests for `RouterVfs`: `.epochignore`-matching subtrees bypass
//! the copy-on-write overlay — reads and writes go straight to the lower host
//! directory and never reach the CAS or its checkpoints.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use epoch_cas::{MemoryByteBackend, TieredByteBackend};
use epoch_history::History;
use epoch_timeline::MemoryTimelineStore;
use epoch_vfs::control_plane::CP_BASE;
use epoch_vfs::router::ROUTE_BASE;
use epoch_vfs::{
    CasVfs, ControlPlaneVfs, DebugStats, HistoryDir, PassthroughVfs, RouterVfs,
};
use epoch_vfs_api::{
    CreateRequest, InodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, ROOT_INODE_ID, VfsError,
    VfsResult, VfsService, WriteRequest,
};

type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

struct Fixture {
    live: Arc<CasVfs<Tiered>>,
    history: Arc<History<MemoryByteBackend, MemoryByteBackend>>,
    router: Arc<RouterVfs>,
    /// The read-only lower service the overlay falls through to (shared with
    /// history snapshots).
    lower: Arc<dyn VfsService>,
}

/// Build the routed overlay stack over `lower_dir`, loading whatever
/// `.epochignore` it holds (missing file = no rules).
fn fixture(lower_dir: &Path) -> Fixture {
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let lower: Arc<dyn VfsService> = Arc::new(PassthroughVfs::new(lower_dir));
    let live = Arc::new(CasVfs::overlay(tier.clone(), Arc::clone(&lower)));
    let history = Arc::new(History::new(
        tier,
        Arc::new(MemoryTimelineStore::new()),
        "main",
    ));
    let rules = RouterVfs::load_rules(lower_dir).expect("rules compile");
    let passthrough: Arc<dyn VfsService> = Arc::new(PassthroughVfs::writable(lower_dir));
    let overlay: Arc<dyn VfsService> = live.clone();
    let router = Arc::new(RouterVfs::new(overlay, passthrough, rules));
    Fixture {
        live,
        history,
        router,
        lower,
    }
}

/// A lower directory holding an `.epochignore` (node_modules/, cache/, *.log)
/// and a pre-existing `node_modules` with one dependency file.
fn ignoring_lower() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".epochignore"),
        "node_modules/\ncache/\n*.log\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("node_modules")).unwrap();
    std::fs::write(dir.path().join("node_modules/dep.txt"), b"dep v1").unwrap();
    dir
}

/// Create (or truncate) `dir/name` through `vfs` and write `content`.
fn write_file(vfs: &dyn VfsService, dir: InodeId, name: &str, content: &[u8]) -> VfsResult<InodeId> {
    let node = match vfs.lookup(dir, OsStr::new(name)) {
        Ok(entry) => entry.attr.inode_id,
        Err(_) => {
            vfs.create(CreateRequest::file(dir, name, 0o644))?
                .entry
                .attr
                .inode_id
        }
    };
    let opened = vfs.open(OpenRequest {
        inode_id: node,
        mode: OpenMode::ReadWrite,
        truncate: true,
        append: false,
    })?;
    vfs.write(WriteRequest {
        inode_id: node,
        handle: opened.handle,
        offset: 0,
        data: content,
    })?;
    vfs.release(opened.handle)?;
    Ok(node)
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

fn in_router_range(id: InodeId) -> bool {
    (ROUTE_BASE..CP_BASE).contains(&id.0)
}

#[test]
fn ignored_writes_land_on_host_and_skip_the_cas() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());

    // `node_modules` resolves through the router into the routed id range.
    let nm = f
        .router
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap();
    assert_eq!(nm.attr.kind, NodeKind::Directory);
    assert!(in_router_range(nm.attr.inode_id), "id: {:?}", nm.attr.inode_id);

    // A write inside it lands in the real host directory...
    write_file(f.router.as_ref(), nm.attr.inode_id, "generated.txt", b"built").unwrap();
    assert_eq!(
        std::fs::read(lower.path().join("node_modules/generated.txt")).unwrap(),
        b"built"
    );

    // ...and the overlay/CAS never saw it: nothing materialized, nothing dirty.
    let snapshot = f.live.debug_snapshot();
    assert_eq!(snapshot.dirty_files, 0);
    assert!(
        snapshot.overlay_paths.is_empty(),
        "overlay captured: {:?}",
        snapshot.overlay_paths
    );
}

#[test]
fn ignored_reads_reflect_live_host_content() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());
    let nm = f
        .router
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap()
        .attr
        .inode_id;

    assert_eq!(read_all(f.router.as_ref(), nm, "dep.txt").unwrap(), b"dep v1");

    // Mutate the host directly; the router sees the live content.
    std::fs::write(lower.path().join("node_modules/dep.txt"), b"dep v2").unwrap();
    assert_eq!(read_all(f.router.as_ref(), nm, "dep.txt").unwrap(), b"dep v2");
}

#[test]
fn non_ignored_writes_stay_copy_on_write() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());

    write_file(f.router.as_ref(), ROOT_INODE_ID, "notes.txt", b"private").unwrap();

    // The host directory is untouched; the overlay captured the file.
    assert!(!lower.path().join("notes.txt").exists());
    assert_eq!(
        read_all(f.router.as_ref(), ROOT_INODE_ID, "notes.txt").unwrap(),
        b"private"
    );
    assert!(
        !f.live.debug_snapshot().overlay_paths.is_empty(),
        "the overlay should have materialized notes.txt"
    );
}

#[test]
fn readdir_mixes_routed_and_overlay_entries_without_id_collisions() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());
    write_file(f.router.as_ref(), ROOT_INODE_ID, "ours.txt", b"overlay").unwrap();

    let entries = f.router.readdir(ROOT_INODE_ID).unwrap();
    let nm = entries
        .iter()
        .find(|e| e.name == OsStr::new("node_modules"))
        .expect("node_modules listed");
    let ours = entries
        .iter()
        .find(|e| e.name == OsStr::new("ours.txt"))
        .expect("ours.txt listed");

    assert_eq!(nm.kind, NodeKind::Directory);
    assert!(in_router_range(nm.inode_id), "routed id: {:?}", nm.inode_id);
    assert!(
        ours.inode_id.0 < ROUTE_BASE,
        "overlay id below the router range: {:?}",
        ours.inode_id
    );
    // readdir agrees with lookup about the routed id.
    let looked_up = f
        .router
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap();
    assert_eq!(nm.inode_id, looked_up.attr.inode_id);
}

#[test]
fn mkdir_and_create_of_matching_names_route_to_the_host() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());

    // `mkdir cache` from the overlay root lands on the host.
    let cache = f
        .router
        .mkdir(CreateRequest::directory(ROOT_INODE_ID, "cache", 0o755))
        .unwrap();
    assert!(in_router_range(cache.attr.inode_id));
    assert!(lower.path().join("cache").is_dir());

    // `create build.log` (matches `*.log`) lands on the host too.
    let created = f
        .router
        .create(CreateRequest::file(ROOT_INODE_ID, "build.log", 0o644))
        .unwrap();
    assert!(in_router_range(created.entry.attr.inode_id));
    let opened = created.opened.expect("created file opens");
    f.router
        .write(WriteRequest {
            inode_id: created.entry.attr.inode_id,
            handle: opened.handle,
            offset: 0,
            data: b"log line",
        })
        .unwrap();
    f.router.release(opened.handle).unwrap();
    assert_eq!(std::fs::read(lower.path().join("build.log")).unwrap(), b"log line");

    // The overlay never captured either.
    assert!(f.live.debug_snapshot().overlay_paths.is_empty());
}

#[test]
fn checkpoints_capture_a_delta_that_excludes_routed_content() {
    let lower = ignoring_lower();
    let f = fixture(lower.path());

    // One overlay write, one routed write.
    write_file(f.router.as_ref(), ROOT_INODE_ID, "kept.txt", b"captured").unwrap();
    let nm = f
        .router
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap()
        .attr
        .inode_id;
    write_file(f.router.as_ref(), nm, "scratch.txt", b"never captured").unwrap();

    let root = f.live.seal().unwrap();
    f.history
        .commit_root_at(root, 1_000, None)
        .unwrap()
        .expect("the overlay delta changed, so a checkpoint lands");

    // View the checkpoint through `.vfs/history` (over the shared lower).
    let cp = ControlPlaneVfs::new(f.router.clone()).with_feature("history", {
        let history = Arc::clone(&f.history);
        let lower = Arc::clone(&f.lower);
        move |slot| Arc::new(HistoryDir::new(history, slot, Some(lower)))
    });
    let vfs_dir = cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    let hist = cp
        .lookup(vfs_dir.attr.inode_id, OsStr::new("history"))
        .unwrap()
        .attr
        .inode_id;
    let snap = cp.lookup(hist, OsStr::new("1000")).unwrap().attr.inode_id;

    // The captured file is in the delta.
    assert_eq!(read_all(&cp, snap, "kept.txt").unwrap(), b"captured");

    // The routed file is not: delete it from the host and it vanishes from the
    // snapshot too — proof it only ever lived as lower fall-through.
    let snap_nm = cp
        .lookup(snap, OsStr::new("node_modules"))
        .unwrap()
        .attr
        .inode_id;
    assert_eq!(
        read_all(&cp, snap_nm, "scratch.txt").unwrap(),
        b"never captured",
        "before deletion the snapshot falls through to the live host file"
    );
    std::fs::remove_file(lower.path().join("node_modules/scratch.txt")).unwrap();
    assert!(
        matches!(
            cp.lookup(snap_nm, OsStr::new("scratch.txt")),
            Err(VfsError::NotFound)
        ),
        "routed content was never part of the checkpoint delta"
    );

    // And the delta listing itself never mentions node_modules.
    assert!(
        f.live
            .debug_snapshot()
            .overlay_paths
            .iter()
            .all(|path| !path.contains("node_modules")),
        "delta: {:?}",
        f.live.debug_snapshot().overlay_paths
    );
}

#[test]
fn missing_epochignore_is_pure_delegation() {
    let lower = tempfile::tempdir().unwrap();
    std::fs::create_dir(lower.path().join("node_modules")).unwrap();
    std::fs::write(lower.path().join("node_modules/dep.txt"), b"dep").unwrap();
    let f = fixture(lower.path());

    // Without rules nothing routes: ids come from the overlay, unchanged.
    let nm = f
        .router
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap();
    assert!(nm.attr.inode_id.0 < ROUTE_BASE, "id: {:?}", nm.attr.inode_id);
    let direct = f
        .live
        .lookup(ROOT_INODE_ID, OsStr::new("node_modules"))
        .unwrap();
    assert_eq!(nm.attr.inode_id, direct.attr.inode_id);

    // Writes under node_modules are captured copy-on-write as usual.
    write_file(f.router.as_ref(), nm.attr.inode_id, "generated.txt", b"cow").unwrap();
    assert!(!lower.path().join("node_modules/generated.txt").exists());
    assert_eq!(
        read_all(f.router.as_ref(), nm.attr.inode_id, "generated.txt").unwrap(),
        b"cow"
    );
    assert!(!f.live.debug_snapshot().overlay_paths.is_empty());
}
