//! End-to-end test of the store-side `epoch` verbs against a real durable
//! state dir: build a checkpointed overlay session in-process (the exact stack
//! `epoch-fuse cas --state` assembles), close the store, then run the compiled
//! `epoch` binary over the state dir and assert on its JSON output.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use epoch_cas::{MemoryByteBackend, TieredByteBackend};
use epoch_cas_redb::RedbByteBackend;
use epoch_history::History;
use epoch_timeline::{Attribution, TimelineStore};
use epoch_timeline_redb::RedbTimelineStore;
use epoch_vfs::PassthroughVfs;
use epoch_vfs_api::ROOT_INODE_ID;
use epoch_worktree::WorkingTree;

/// A unique scratch dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // nanos alone collide on coarse clocks when tests create fixtures in
        // parallel; the atomic sequence makes the name unique regardless.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "epoch-cli-{tag}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run the `epoch` binary, asserting success; return stdout.
fn epoch(args: &[&Path]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_epoch"))
        .args(args)
        .output()
        .expect("spawn epoch");
    assert!(
        out.status.success(),
        "epoch {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Extract `"head_event":"<hex>"` from a diff document.
fn head_event_of(diff_json: &str) -> String {
    diff_json
        .split("\"head_event\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("head_event in diff output")
        .to_string()
}

/// The oldest event on `main` (first entry of the audit log).
fn first_event_of(state: &Path) -> String {
    let audit = epoch(&[Path::new("audit"), state]);
    audit
        .split("\"event\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("an event in audit output")
        .to_string()
}

/// Build the fixture state dir: an overlay over `lower` with two checkpointed
/// writes, a delete, and two renames (upper-only and lower-file), checkpointed
/// with attribution. Returns (state dir, lower dir, scratch guard).
fn fixture() -> (PathBuf, PathBuf, Scratch) {
    let scratch = Scratch::new("fixture");
    let lower = scratch.0.join("lower");
    let state = scratch.0.join("state");
    fs::create_dir_all(lower.join("sub")).unwrap();
    fs::write(lower.join("keep.txt"), b"keep me").unwrap();
    fs::write(lower.join("edit.txt"), b"lower original").unwrap();
    fs::write(lower.join("victim.txt"), b"victim bytes").unwrap();
    fs::write(lower.join("delete.txt"), b"goodbye").unwrap();
    fs::write(lower.join("sub/nested.txt"), b"nested lower").unwrap();

    fs::create_dir_all(&state).unwrap();
    // The durable stack, exactly as mount_durable assembles it — scoped so the
    // redb handle drops (releasing the single-writer lock) before the verbs run.
    {
        let db = Arc::new(redb::Database::create(state.join("epoch.redb")).unwrap());
        let storage = RedbByteBackend::with_database(Arc::clone(&db)).unwrap();
        let tier = TieredByteBackend::new(MemoryByteBackend::new(), storage);
        let timeline: Arc<dyn TimelineStore> =
            Arc::new(RedbTimelineStore::new(Arc::clone(&db)).unwrap());
        let history = History::new(tier.clone(), timeline, "main");
        let lower_vfs: Arc<dyn epoch_vfs_api::VfsService> =
            Arc::new(PassthroughVfs::new(lower.clone()));
        let mut wt = WorkingTree::overlay(tier, Arc::clone(&lower_vfs));

        // Checkpoint 1: copy-up edit of a lower file, a brand-new upper file,
        // a lower file deleted (tombstone), a new directory with a file inside.
        let edit = wt.lookup(ROOT_INODE_ID, OsStr::new("edit.txt")).unwrap();
        wt.truncate(edit, 0).unwrap();
        wt.write_at(edit, 0, b"overlay version").unwrap();
        let fresh = wt.create(ROOT_INODE_ID, OsStr::new("fresh.txt"), 0o644).unwrap();
        wt.write_at(fresh, 0, b"brand new").unwrap();
        wt.unlink(ROOT_INODE_ID, OsStr::new("delete.txt")).unwrap();
        let made = wt.mkdir(ROOT_INODE_ID, OsStr::new("made"), 0o755).unwrap();
        let inner = wt.create(made, OsStr::new("inner.txt"), 0o644).unwrap();
        wt.write_at(inner, 0, b"inner").unwrap();

        let attribution = Attribution {
            mount: "m-testfixture".into(),
            head: "main".into(),
            user: Some("lane-test".into()),
            host: Some("testhost".into()),
            lower: Some(lower.display().to_string()),
        };
        let root1 = wt.seal().unwrap();
        history
            .commit_root_at(root1, 1_700_000_000_000, Some(attribution.to_meta_json()))
            .unwrap()
            .expect("a real session checkpoints");

        // Checkpoint 2: an upper rename (fresh.txt → renamed.txt) and a lower
        // file renamed (victim.txt → moved.txt: copy-up + move + tombstone).
        wt.rename(ROOT_INODE_ID, OsStr::new("fresh.txt"), ROOT_INODE_ID, OsStr::new("renamed.txt"))
            .unwrap();
        wt.rename(ROOT_INODE_ID, OsStr::new("victim.txt"), ROOT_INODE_ID, OsStr::new("moved.txt"))
            .unwrap();
        let root2 = wt.seal().unwrap();
        history
            .commit_root_at(root2, 1_700_000_000_500, Some(attribution.to_meta_json()))
            .unwrap()
            .expect("the second session checkpoints");
    }

    (state, lower, scratch)
}

#[test]
fn diff_reports_the_delta_with_reconstructed_renames() {
    let (state, _lower, _guard) = fixture();
    // No --lower: the verb reads it from the head event's attribution meta.
    let out = epoch(&[Path::new("diff"), &state]);

    assert!(out.contains("\"schema\":1"), "{out}");
    assert!(out.contains("\"type\":\"diff\""), "{out}");
    assert!(out.contains("\"base\":{\"kind\":\"lower\",\"lower\":\""), "{out}");
    // The rename across the lower boundary reconstructs (tombstone ↔ copy-up
    // paired by content bytes).
    assert!(
        out.contains("\"op\":\"rename\",\"from\":\"victim.txt\",\"to\":\"moved.txt\""),
        "{out}"
    );
    // Copy-up edit, mkdir, plain delete.
    assert!(out.contains("\"op\":\"write\",\"path\":\"edit.txt\""), "{out}");
    assert!(out.contains("\"op\":\"mkdir\",\"path\":\"made\""), "{out}");
    assert!(out.contains("\"op\":\"write\",\"path\":\"made/inner.txt\""), "{out}");
    assert!(out.contains("\"op\":\"delete\",\"path\":\"delete.txt\""), "{out}");
    // The untouched lower file is no diff entry; the within-delta rename
    // (fresh.txt appeared and moved between two checkpoints) leaves only the
    // final name in a delta-vs-base diff.
    assert!(!out.contains("keep.txt"), "{out}");
    assert!(!out.contains("nested.txt"), "{out}");
    assert!(!out.contains("fresh.txt"), "{out}");
    assert!(out.contains("renamed.txt"), "{out}");

    // The pair diff against the first checkpoint reconstructs the upper rename.
    let head_event = head_event_of(&out);
    let first = first_event_of(&state);
    assert_ne!(head_event, first);
    let pair = epoch(&[Path::new("diff"), &state, Path::new("--base"), Path::new(&first)]);
    assert!(
        pair.contains("\"op\":\"rename\",\"from\":\"fresh.txt\",\"to\":\"renamed.txt\""),
        "{pair}"
    );
    assert!(
        pair.contains("\"op\":\"rename\",\"from\":\"victim.txt\",\"to\":\"moved.txt\""),
        "{pair}"
    );
    assert!(!pair.contains("edit.txt"), "{pair}");
}

#[test]
fn export_materializes_the_merged_snapshot() {
    let (state, _lower, _guard) = fixture();
    let dest = state.parent().unwrap().join("out");
    let out = epoch(&[Path::new("export"), &state, &dest]);
    assert!(out.contains("\"type\":\"export\""), "{out}");

    let expect = [
        ("keep.txt", &b"keep me"[..]),
        ("edit.txt", b"overlay version"),
        ("renamed.txt", b"brand new"),
        ("moved.txt", b"victim bytes"),
        ("made/inner.txt", b"inner"),
        ("sub/nested.txt", b"nested lower"),
    ];
    for (path, content) in expect {
        assert_eq!(
            fs::read(dest.join(path)).unwrap_or_else(|e| panic!("read {path}: {e}")),
            content,
            "exported {path}"
        );
    }
    assert!(!dest.join("delete.txt").exists(), "tombstone honored");
    assert!(!dest.join("victim.txt").exists(), "rename source gone");
    assert!(!dest.join("fresh.txt").exists(), "rename source gone");

    // Exporting over a non-empty destination fails loudly.
    let again = Command::new(env!("CARGO_BIN_EXE_epoch"))
        .args([Path::new("export"), &state, &dest])
        .output()
        .unwrap();
    assert!(!again.status.success());
}

#[test]
fn audit_emits_attributed_timestamped_entries() {
    let (state, _lower, _guard) = fixture();
    let out = epoch(&[Path::new("audit"), &state]);

    assert!(out.contains("\"type\":\"audit\""), "{out}");
    assert!(out.contains("\"schema\":1"), "{out}");
    assert!(out.contains("reads are not recorded"), "{out}");
    assert!(out.contains("not a sandbox"), "{out}");
    // Every entry carries the event's timestamp and the mount's attribution.
    assert!(out.contains("\"ts\":1700000000000"), "{out}");
    assert!(out.contains("\"ts\":1700000000500"), "{out}");
    assert!(out.contains("\"mount\":\"m-testfixture\""), "{out}");
    assert!(out.contains("\"user\":\"lane-test\""), "{out}");
    assert!(out.contains("\"host\":\"testhost\""), "{out}");
    assert!(out.contains("\"op\":\"rename\",\"from\":\"victim.txt\""), "{out}");

    // An unknown head fails loudly.
    let bad = Command::new(env!("CARGO_BIN_EXE_epoch"))
        .args([Path::new("audit"), &state, Path::new("--head"), Path::new("nope")])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown head"));
}

#[test]
fn diff_against_an_explicit_base_event() {
    let (state, _lower, _guard) = fixture();
    // The only event is the head; diffing head vs itself is empty.
    let full = epoch(&[Path::new("diff"), &state]);
    let head_event = full
        .split("\"head_event\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("head_event in diff output");
    let out = epoch(&[
        Path::new("diff"),
        &state,
        Path::new("--base"),
        Path::new(head_event),
    ]);
    assert!(out.contains("\"base\":{\"kind\":\"event\""), "{out}");
    assert!(out.contains("\"entries\":[]"), "{out}");
}
