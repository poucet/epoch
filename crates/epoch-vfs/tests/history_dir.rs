//! End-to-end tests for the `.vfs/history` time-travel directory: a live
//! `CasVfs` over a tiered backend, checkpointed into a timeline, exposed
//! read-only through the control plane `history` feature.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use epoch_cas::{MemoryByteBackend, TieredByteBackend};
use epoch_history::History;
use epoch_timeline::MemoryTimelineStore;
use epoch_vfs::control_plane::CP_BASE;
use epoch_vfs::{CasVfs, ControlPlaneVfs, HistoryDir};
use epoch_vfs_api::{
    CreateRequest, InodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, ROOT_INODE_ID,
    VfsError, VfsService, WriteRequest,
};

type Tiered = TieredByteBackend<MemoryByteBackend, MemoryByteBackend>;

struct Fixture {
    live: Arc<CasVfs<Tiered>>,
    history: Arc<History<MemoryByteBackend, MemoryByteBackend>>,
    cp: ControlPlaneVfs,
}

fn fixture() -> Fixture {
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let live = Arc::new(CasVfs::new(tier.clone()));
    let history = Arc::new(History::new(
        tier,
        Arc::new(MemoryTimelineStore::new()),
        "main",
    ));
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("history", {
        let history = Arc::clone(&history);
        move |slot| Arc::new(HistoryDir::new(history, slot, None))
    });
    Fixture { live, history, cp }
}

/// An overlay fixture: the live tree overlays `lower_dir` (a host directory
/// served by `PassthroughVfs`), and snapshots rehydrate deltas over it.
fn overlay_fixture(lower_dir: &std::path::Path) -> Fixture {
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let lower: Arc<dyn VfsService> = Arc::new(epoch_vfs::PassthroughVfs::new(lower_dir));
    let live = Arc::new(CasVfs::overlay(tier.clone(), Arc::clone(&lower)));
    let history = Arc::new(History::new(
        tier,
        Arc::new(MemoryTimelineStore::new()),
        "main",
    ));
    let cp = ControlPlaneVfs::new(live.clone()).with_feature("history", {
        let history = Arc::clone(&history);
        move |slot| Arc::new(HistoryDir::new(history, slot, Some(lower)))
    });
    Fixture { live, history, cp }
}

/// Write `content` to `/name` on the live fs and checkpoint at `ts_ms`.
fn write_and_checkpoint(f: &Fixture, name: &str, content: &[u8], ts_ms: u64) {
    let node = match f.live.lookup(ROOT_INODE_ID, OsStr::new(name)) {
        Ok(entry) => entry.attr.inode_id,
        Err(_) => {
            f.live
                .create(CreateRequest::file(ROOT_INODE_ID, name, 0o644))
                .unwrap()
                .entry
                .attr
                .inode_id
        }
    };
    let opened = f
        .live
        .open(OpenRequest {
            inode_id: node,
            mode: OpenMode::ReadWrite,
            truncate: true,
            append: false,
        })
        .unwrap();
    f.live
        .write(WriteRequest {
            inode_id: node,
            handle: opened.handle,
            offset: 0,
            data: content,
        })
        .unwrap();
    f.live.release(opened.handle).unwrap();
    let root = f.live.seal().unwrap();
    f.history
        .commit_root_at(root, ts_ms, None)
        .unwrap()
        .expect("content changed, so a checkpoint lands");
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

fn history_dir_id(cp: &ControlPlaneVfs) -> InodeId {
    let vfs = cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    cp.lookup(vfs.attr.inode_id, OsStr::new("history"))
        .unwrap()
        .attr
        .inode_id
}

#[test]
fn timestamps_resolve_to_nearest_checkpoint_at_or_before() {
    let f = fixture();
    write_and_checkpoint(&f, "log.txt", b"v1", 1_000);
    write_and_checkpoint(&f, "log.txt", b"v2", 2_000);
    write_and_checkpoint(&f, "log.txt", b"v3", 3_000);
    let hist = history_dir_id(&f.cp);

    // Exact and in-between times both resolve, ZFS-style.
    for (ts, expect) in [("1000", "v1"), ("1500", "v1"), ("2000", "v2"), ("9999", "v3")] {
        let snap = f.cp.lookup(hist, OsStr::new(ts)).unwrap();
        assert_eq!(snap.attr.kind, NodeKind::Directory);
        let data = read_all(&f.cp, snap.attr.inode_id, "log.txt").unwrap();
        assert_eq!(data, expect.as_bytes(), "at t={ts}");
    }

    // Before all history: nothing to show.
    assert!(matches!(
        f.cp.lookup(hist, OsStr::new("999")),
        Err(VfsError::NotFound)
    ));

    // The live filesystem still serves the newest content.
    assert_eq!(read_all(&f.cp, ROOT_INODE_ID, "log.txt").unwrap(), b"v3");
}

#[test]
fn snapshots_by_event_id_and_listing() {
    let f = fixture();
    write_and_checkpoint(&f, "a", b"one", 1_000);
    write_and_checkpoint(&f, "a", b"two", 2_000);
    let hist = history_dir_id(&f.cp);

    // `.vfs/history` lists both checkpoints by millisecond name (plus ./../id).
    let names: Vec<String> = f
        .cp
        .readdir(hist)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"id".to_string()));
    assert!(names.contains(&"1000".to_string()));
    assert!(names.contains(&"2000".to_string()));

    // `id/` resolves any event by hex id but is itself unlistable.
    let id_dir = f.cp.lookup(hist, OsStr::new("id")).unwrap().attr.inode_id;
    assert_eq!(f.cp.readdir(id_dir).unwrap().len(), 2, "only . and ..");
    let (first_event, _) = f.history.log(10).unwrap().pop().unwrap();
    let snap = f
        .cp
        .lookup(id_dir, OsStr::new(&first_event.to_hex()))
        .unwrap();
    assert_eq!(
        read_all(&f.cp, snap.attr.inode_id, "a").unwrap(),
        b"one",
        "id-addressed snapshot shows the old state"
    );
    assert!(matches!(
        f.cp.lookup(id_dir, OsStr::new("not-hex")),
        Err(VfsError::NotFound)
    ));
}

#[test]
fn relative_names_resolve_with_zero_ttl() {
    let f = fixture();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // A checkpoint firmly 10 minutes in the past, and one just now.
    write_and_checkpoint(&f, "f", b"old", now - 600_000);
    write_and_checkpoint(&f, "f", b"new", now);
    let hist = history_dir_id(&f.cp);

    let five_min_ago = f.cp.lookup(hist, OsStr::new("5m-ago")).unwrap();
    assert_eq!(five_min_ago.ttl, Duration::ZERO, "relative names never cache");
    assert_eq!(
        read_all(&f.cp, five_min_ago.attr.inode_id, "f").unwrap(),
        b"old"
    );

    let just_now = f.cp.lookup(hist, OsStr::new("0s-ago")).unwrap();
    assert_eq!(read_all(&f.cp, just_now.attr.inode_id, "f").unwrap(), b"new");
}

#[test]
fn snapshots_are_read_only_and_ids_never_collide() {
    let f = fixture();
    write_and_checkpoint(&f, "x", b"1", 1_000);
    write_and_checkpoint(&f, "x", b"2", 2_000);
    let hist = history_dir_id(&f.cp);

    let s1 = f.cp.lookup(hist, OsStr::new("1000")).unwrap().attr.inode_id;
    let s2 = f.cp.lookup(hist, OsStr::new("2000")).unwrap().attr.inode_id;
    // Both snapshots' roots are locally inode 1 — synthetic ids must differ
    // from each other and from the live root.
    assert_ne!(s1, s2);
    assert_ne!(s1, ROOT_INODE_ID);
    // Stable across repeated lookups.
    assert_eq!(f.cp.lookup(hist, OsStr::new("1000")).unwrap().attr.inode_id, s1);

    // Every mutation path answers EROFS.
    assert!(matches!(
        f.cp.create(CreateRequest::file(s1, "nope", 0o644)),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    assert!(matches!(
        f.cp.mkdir(CreateRequest::directory(s1, "nope", 0o755)),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    assert!(matches!(
        f.cp.unlink(s1, OsStr::new("x")),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    let file = f.cp.lookup(s1, OsStr::new("x")).unwrap().attr.inode_id;
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
        f.cp.open(OpenRequest {
            inode_id: file,
            mode: OpenMode::ReadOnly,
            truncate: true,
            append: false,
        }),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    // Attrs come back write-protected and in the synthetic range.
    let attr = f.cp.getattr(file).unwrap();
    assert_eq!(attr.perm & 0o222, 0, "no write bits on snapshot nodes");

    // readdir of a snapshot remaps every child id out of the local space.
    for entry in f.cp.readdir(s1).unwrap() {
        assert!(entry.inode_id.0 >= CP_BASE, "no leaked local id: {entry:?}");
    }
}

#[test]
fn overlay_checkpoints_record_deltas_and_snapshots_rehydrate_over_lower() {
    let lower = tempfile::tempdir().unwrap();
    std::fs::write(lower.path().join("base.txt"), b"from lower").unwrap();
    std::fs::write(lower.path().join("doomed.txt"), b"soon gone").unwrap();
    let f = overlay_fixture(lower.path());

    // Checkpoint 1: copy up one new file. Checkpoint 2: delete a lower file
    // (a tombstone in the delta).
    write_and_checkpoint(&f, "ours.txt", b"overlay v1", 1_000);
    f.live.unlink(ROOT_INODE_ID, OsStr::new("doomed.txt")).unwrap();
    let root = f.live.seal().unwrap();
    f.history.commit_root_at(root, 2_000, None).unwrap().unwrap();

    let hist = history_dir_id(&f.cp);
    let s1 = f.cp.lookup(hist, OsStr::new("1000")).unwrap().attr.inode_id;
    let s2 = f.cp.lookup(hist, OsStr::new("2000")).unwrap().attr.inode_id;

    // Snapshot 1: overlay content + lower fall-through, doomed still visible.
    assert_eq!(read_all(&f.cp, s1, "ours.txt").unwrap(), b"overlay v1");
    assert_eq!(read_all(&f.cp, s1, "base.txt").unwrap(), b"from lower");
    assert_eq!(read_all(&f.cp, s1, "doomed.txt").unwrap(), b"soon gone");

    // Snapshot 2: the tombstone hides the lower file.
    assert!(matches!(
        f.cp.lookup(s2, OsStr::new("doomed.txt")),
        Err(VfsError::NotFound)
    ));
    assert_eq!(read_all(&f.cp, s2, "base.txt").unwrap(), b"from lower");

    // The delta closure is fully CAS-owned: lower content was never promoted.
    // (base.txt reads through the snapshot come from the lower service.)
    // Snapshots stay read-only even for fall-through names.
    assert!(matches!(
        f.cp.unlink(s1, OsStr::new("base.txt")),
        Err(VfsError::ReadOnlyFilesystem)
    ));
}

#[test]
fn head_file_names_the_checkpoint_and_feeds_the_cas_portal() {
    // A control plane with BOTH history and the typed cas portal, so the
    // root hash printed by `.vfs/history/head` can be browsed immediately.
    let tier = Tiered::new(MemoryByteBackend::new(), MemoryByteBackend::new());
    let live = Arc::new(CasVfs::new(tier.clone()));
    let history = Arc::new(History::new(
        tier.clone(),
        Arc::new(MemoryTimelineStore::new()),
        "main",
    ));
    let cp = ControlPlaneVfs::new(live.clone())
        .with_feature("cas", {
            let tier = tier.clone();
            move |slot| Arc::new(epoch_vfs::CasObjectDir::new(slot, tier, None))
        })
        .with_feature("history", {
            let history = Arc::clone(&history);
            move |slot| Arc::new(HistoryDir::new(history, slot, None))
        });
    let f = Fixture { live, history, cp };
    let hist = history_dir_id(&f.cp);

    // Before any checkpoint the file exists and is empty.
    assert_eq!(read_all(&f.cp, hist, "head").unwrap(), b"");

    write_and_checkpoint(&f, "log.txt", b"v1", 1_000);
    write_and_checkpoint(&f, "log.txt", b"v2", 2_000);

    // `head` is exactly the newest checkpoint's root hash; readdir lists it.
    let head = String::from_utf8(read_all(&f.cp, hist, "head").unwrap()).unwrap();
    let names: Vec<String> = f
        .cp
        .readdir(hist)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"head".to_string()));

    // The printed root hash browses through the cas portal.
    let root_hex = head.trim();
    assert_eq!(root_hex.len(), 64, "head is a bare 64-hex hash: {head:?}");
    let vfs_dir = f.cp.lookup(ROOT_INODE_ID, OsStr::new(".vfs")).unwrap();
    let cas = f
        .cp
        .lookup(vfs_dir.attr.inode_id, OsStr::new("cas"))
        .unwrap()
        .attr
        .inode_id;
    let tree = f.cp.lookup(cas, OsStr::new(root_hex)).unwrap();
    assert_eq!(tree.attr.kind, NodeKind::Directory);
    assert_eq!(
        read_all(&f.cp, tree.attr.inode_id, "log.txt").unwrap(),
        b"v2",
        "the head root shows the newest checkpointed content"
    );
}

/// `.vfs/history/latest` is the merged head snapshot: overlay delta over the
/// (current) lower — unlike the raw delta object behind `.vfs/history/head`.
#[test]
fn latest_shows_the_merged_head_snapshot() {
    let lower = tempfile::tempdir().unwrap();
    std::fs::write(lower.path().join("base.txt"), b"from lower").unwrap();
    let f = overlay_fixture(lower.path());
    let hist = history_dir_id(&f.cp);

    // No checkpoints yet: latest doesn't resolve.
    assert!(matches!(
        f.cp.lookup(hist, OsStr::new("latest")),
        Err(VfsError::NotFound)
    ));

    write_and_checkpoint(&f, "foo", b"captured", 1_000);

    let latest = f.cp.lookup(hist, OsStr::new("latest")).unwrap();
    assert_eq!(latest.ttl, Duration::ZERO, "latest must never dentry-cache");
    let names: Vec<String> = f
        .cp
        .readdir(latest.attr.inode_id)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"foo".to_string()), "delta content: {names:?}");
    assert!(names.contains(&"base.txt".to_string()), "lower fall-through: {names:?}");
    assert_eq!(read_all(&f.cp, latest.attr.inode_id, "base.txt").unwrap(), b"from lower");

    // And the history root lists it.
    let listed: Vec<String> = f
        .cp
        .readdir(hist)
        .unwrap()
        .into_iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert!(listed.contains(&"latest".to_string()));
}

#[test]
fn snapshots_read_from_durable_tier_only() {
    let f = fixture();
    write_and_checkpoint(&f, "durable.txt", b"safe", 1_000);

    // Stage a new version but do NOT checkpoint it.
    let node = f
        .live
        .lookup(ROOT_INODE_ID, OsStr::new("durable.txt"))
        .unwrap()
        .attr
        .inode_id;
    let opened = f
        .live
        .open(OpenRequest {
            inode_id: node,
            mode: OpenMode::ReadWrite,
            truncate: true,
            append: false,
        })
        .unwrap();
    f.live
        .write(WriteRequest {
            inode_id: node,
            handle: opened.handle,
            offset: 0,
            data: b"staged only",
        })
        .unwrap();

    let hist = history_dir_id(&f.cp);
    let snap = f.cp.lookup(hist, OsStr::new("1000")).unwrap().attr.inode_id;
    assert_eq!(
        read_all(&f.cp, snap, "durable.txt").unwrap(),
        b"safe",
        "snapshot shows the checkpointed state, not staged writes"
    );
}
