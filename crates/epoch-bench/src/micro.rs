//! MICRO — the §2.4.1 per-op-class benches. Each class runs on the bare lower
//! and on the mount side in the same process run, and is reported as ×bare
//! and absolute against its §2.2 ceiling:
//!
//! | class          | ceiling                        |
//! |----------------|--------------------------------|
//! | stat-walk      | ≤ 3× bare                      |
//! | full-file read | ≤ 2× bare                      |
//! | write+fsync    | ≤ 2 ms absolute (≤ 4 KiB file) |
//! | copy-up        | ≤ 50 ms for ≤ 10 MB            |
//! | rename (upper) | ≤ 2× bare                      |
//! | unlink         | report only                    |

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bench::{fmt_bytes, time_iters, Ceiling, Pair, Row, Timer};
use crate::resource::{self, Resources};
use crate::target::{Target, TargetError, TargetResult};
use crate::tree::{self, TreeSpec};

/// Known, other-lane-owned breakage signatures (EPOCH-FIXES §1.1/§1.2). Each
/// entry is (error-signature needle, row-name needle, reason): the row-name
/// needle scopes generic errnos (ENOENT is only the rename bug when the row
/// is rename-shaped) so genuine bugs elsewhere still FAIL loudly.
const KNOWN: &[(&str, &str, &str)] = &[
    // rename(2) arrives with empty names on macFUSE 5.x; epoch answers
    // ENOENT on the empty source, `mv` sees EINVAL (research §3, §4.2.1).
    ("errno 2", "rename", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 2", "atomic-save", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 2", "git-flow", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 2", "jj-flow", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 2", "sed-loop", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 2", "cargo-build", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 22", "rename", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 22", "atomic-save", "rename(2) on macFUSE 5.x (wire-layout bug, §1.1)"),
    ("errno 78", "", "symlink/readlink ENOSYS (§1.2)"),
    ("unsupported operation", "", "operation not in VfsService yet (§1.2)"),
];

/// Turn an error into a Row: KNOWN-FAIL when the signature matches breakage
/// another lane owns, FAIL otherwise.
pub fn classify(name: &str, err: &TargetError) -> Row {
    let sig = err.signature();
    for (needle, row_scope, reason) in KNOWN {
        if sig.contains(needle) && (row_scope.is_empty() || name.contains(row_scope)) {
            return Row::known_fail(name, *reason);
        }
    }
    Row::fail(name, sig)
}

/// Cheap rename liveness probe for a host dir: create + rename. On macFUSE
/// 5.x this fails EINVAL today (§1.1) — the caller classifies KNOWN-FAIL.
pub fn timed_rename_probe(dir: &std::path::Path) -> Result<(), std::io::Error> {
    let a = dir.join(".epoch-bench-rename-probe.a");
    let b = dir.join(".epoch-bench-rename-probe.b");
    std::fs::write(&a, b"probe")?;
    let r = std::fs::rename(&a, &b);
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    r
}

/// Fixture shape for the micro suite.
#[derive(Clone, Copy, Debug)]
pub struct MicroConfig {
    pub files: u64,
    pub file_bytes: u64,
    pub copyup_files: u64,
    pub copyup_bytes: u64,
    pub write_iters: u64,
    pub rounds: u32,
}

impl Default for MicroConfig {
    fn default() -> Self {
        Self {
            files: 512,
            file_bytes: 4096,
            copyup_files: 8,
            copyup_bytes: 10 << 20,
            write_iters: 200,
            rounds: 3,
        }
    }
}

/// Populate the shared fixture (read/stat corpus + copy-up victims) under
/// `root` (a host path — for the mount side this is the mount's *lower* dir,
/// later reached through the mount). Idempotent so re-runs are cheap.
pub fn populate(root: &std::path::Path, cfg: &MicroConfig) -> std::io::Result<()> {
    let fixture = root.join("fixture");
    if !fixture.exists() {
        tree::generate(
            &fixture,
            TreeSpec {
                files: cfg.files,
                bytes: cfg.files * cfg.file_bytes,
                dirs: (cfg.files / 64).max(1),
                sparse: false,
            },
        )?;
    }
    let copyup = root.join("copyup");
    if !copyup.exists() {
        tree::generate(
            &copyup,
            TreeSpec {
                files: cfg.copyup_files,
                bytes: cfg.copyup_files * cfg.copyup_bytes,
                dirs: 1,
                sparse: false,
            },
        )?;
    }
    Ok(())
}

/// Bracket a fallible measurement with resource capture; errors classify.
fn timed(
    name: &str,
    daemon_pid: Option<u32>,
    bytes_written: Option<u64>,
    f: impl FnOnce() -> TargetResult<Duration>,
) -> Row {
    let mut out = Ok(Duration::ZERO);
    let res: Resources = resource::capture(daemon_pid, bytes_written, || out = f());
    match out {
        Ok(wall) => Row::ok(name, 1, wall).with_resources(res),
        Err(e) => classify(name, &e),
    }
}

/// Best-of-`rounds` for a fallible op; first error aborts the measurement.
fn try_best_of(rounds: u32, mut f: impl FnMut() -> TargetResult<()>) -> TargetResult<Duration> {
    let mut best: Option<Duration> = None;
    for _ in 0..rounds.max(1) {
        let t = Timer::start();
        f()?;
        let d = t.stop();
        best = Some(best.map_or(d, |b: Duration| b.min(d)));
    }
    Ok(best.unwrap_or_default())
}

/// Full micro suite over a (bare, mount) target pair.
pub fn run(bare: &Target, mount: &Target, cfg: &MicroConfig, daemon_pid: Option<u32>) -> Vec<Pair> {
    let mut pairs = Vec::new();

    // ── stat-walk (≤ 3× bare) ────────────────────────────────────────────────
    let bare_row = measure_walk("stat-walk", bare, cfg, None);
    let mount_row = measure_walk("stat-walk", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxRatio(3.0));
    let mut pair = Pair::new(bare_row, mount_row);
    pair.eval_ratio_ceiling(3.0);
    pairs.push(pair);

    // ── full-file reads (≤ 2× bare) ──────────────────────────────────────────
    let bare_row = measure_reads("read-tree", bare, cfg, None);
    let mount_row = measure_reads("read-tree", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxRatio(2.0));
    let mut pair = Pair::new(bare_row, mount_row);
    pair.eval_ratio_ceiling(2.0);
    pairs.push(pair);

    // ── small-file write+fsync (≤ 2 ms absolute) ─────────────────────────────
    let bare_row = measure_writes("write+fsync-4k", bare, cfg, None);
    let mut mount_row = measure_writes("write+fsync-4k", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxAbs(Duration::from_millis(2)));
    mount_row.eval_abs_ceiling();
    pairs.push(Pair::new(bare_row, mount_row));

    // ── copy-up on first write to a lower file (≤ 50 ms for ≤ 10 MB) ────────
    let bare_row = measure_copyup("copy-up-10m", bare, cfg, None);
    let mut mount_row = measure_copyup("copy-up-10m", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxAbs(Duration::from_millis(50)));
    mount_row.eval_abs_ceiling();
    pairs.push(Pair::new(bare_row, mount_row));

    // ── rename of an upper file (≤ 2× bare) ─────────────────────────────────
    let bare_row = measure_rename("rename-upper", bare, cfg, None);
    let mount_row = measure_rename("rename-upper", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxRatio(2.0));
    let mut pair = Pair::new(bare_row, mount_row);
    pair.eval_ratio_ceiling(2.0);
    pairs.push(pair);

    // ── unlink (report only) ─────────────────────────────────────────────────
    let bare_row = measure_unlink("unlink", bare, cfg, None);
    let mount_row = measure_unlink("unlink", mount, cfg, daemon_pid);
    pairs.push(Pair::new(bare_row, mount_row));

    pairs
}

fn measure_walk(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    let mut row = timed(name, daemon, None, || {
        try_best_of(cfg.rounds, || target.stat_walk(Path::new("fixture")).map(|_| ()))
    });
    row.iters = cfg.files;
    row.note = format!("best of {} full-tree walks ({} files)", cfg.rounds, cfg.files);
    row
}

fn measure_reads(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    let dirs = (cfg.files / 64).max(1);
    let mut row = timed(name, daemon, None, || {
        try_best_of(cfg.rounds, || {
            let mut buf = Vec::with_capacity(cfg.file_bytes as usize);
            for i in 0..cfg.files {
                let rel = PathBuf::from(format!("fixture/d{:05}/f{:07}.bin", i % dirs, i));
                target.read_file(&rel, &mut buf)?;
                buf.clear();
            }
            Ok(())
        })
    });
    row.iters = cfg.files;
    row.note = format!("best of {} full-tree reads ({} files)", cfg.rounds, cfg.files);
    row
}

fn measure_writes(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    let data = vec![0xABu8; 4096];
    if let Err(e) = target.mkdir_all(&PathBuf::from("ws")) {
        return classify(name, &e);
    }
    let mut row = timed(name, daemon, Some(cfg.write_iters * data.len() as u64), || {
        let mut last: TargetResult<()> = Ok(());
        let wall = time_iters(cfg.write_iters, |i| {
            if last.is_ok() {
                last = target.write_small(&PathBuf::from(format!("ws/w{i:05}.txt")), &data, true);
            }
        });
        last?;
        Ok(Duration::from_secs_f64(wall.as_secs_f64() / cfg.write_iters as f64))
    });
    row.iters = cfg.write_iters;
    row.note = format!("avg of {} create+write+fsync", cfg.write_iters);
    row
}

fn measure_copyup(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    let mut row = timed(name, daemon, None, || {
        let mut total = Duration::ZERO;
        for i in 0..cfg.copyup_files {
            let rel = PathBuf::from(format!("copyup/d00000/f{i:07}.bin"));
            let t = Timer::start();
            target.write_at(&rel, cfg.copyup_bytes / 2, &[0xEEu8; 4096])?;
            total += t.stop();
        }
        Ok(Duration::from_secs_f64(total.as_secs_f64() / cfg.copyup_files as f64))
    });
    row.iters = cfg.copyup_files;
    row.note = format!(
        "avg first-write over {} × {} lower files",
        cfg.copyup_files,
        fmt_bytes(cfg.copyup_bytes)
    );
    row
}

fn measure_rename(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    // Seed the corpus *through the target* so files are upper files on the
    // mount side — the §2.2 class is "rename (upper file)".
    if let Err(e) = seed_files(target, cfg.write_iters, "r") {
        return classify(name, &e);
    }
    let mut row = timed(name, daemon, None, || {
        let mut last: TargetResult<()> = Ok(());
        let wall = time_iters(cfg.write_iters, |i| {
            if last.is_ok() {
                last = target.rename(
                    &PathBuf::from(format!("ws/r{i:05}.txt")),
                    &PathBuf::from(format!("ws/r{i:05}.renamed")),
                );
            }
        });
        last?;
        Ok(Duration::from_secs_f64(wall.as_secs_f64() / cfg.write_iters as f64))
    });
    row.iters = cfg.write_iters;
    row.note = format!("avg of {} upper-file renames", cfg.write_iters);
    row
}

fn measure_unlink(name: &str, target: &Target, cfg: &MicroConfig, daemon: Option<u32>) -> Row {
    if let Err(e) = seed_files(target, cfg.write_iters, "u") {
        return classify(name, &e);
    }
    let mut row = timed(name, daemon, None, || {
        let mut last: TargetResult<()> = Ok(());
        let wall = time_iters(cfg.write_iters, |i| {
            if last.is_ok() {
                last = target.unlink(&PathBuf::from(format!("ws/u{i:05}.txt")));
            }
        });
        last?;
        Ok(Duration::from_secs_f64(wall.as_secs_f64() / cfg.write_iters as f64))
    });
    row.iters = cfg.write_iters;
    row.note = format!("avg of {} unlinks", cfg.write_iters);
    row
}

fn seed_files(target: &Target, n: u64, prefix: &str) -> TargetResult<()> {
    target.mkdir_all(&PathBuf::from("ws"))?;
    for i in 0..n {
        target.write_small(&PathBuf::from(format!("ws/{prefix}{i:05}.txt")), b"x", false)?;
    }
    Ok(())
}
