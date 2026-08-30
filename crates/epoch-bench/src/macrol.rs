//! MACRO — the §2.4.2 envelope-shaped benches:
//!
//! - **churn** — a sleep-free, spawn-heavy FS-churn workload with writes
//!   FORCED INSIDE the measured directory (the §2.1/§2.4.5 lesson: the repo's
//!   23 s gate was ~85% sleeps and wrote to host /tmp, so it measured no
//!   overlay I/O at all). No sleep appears anywhere in this suite.
//! - **stat-storm** — a synthetic 10⁴-entry lookup+getattr storm.
//! - **atomic-save** — the editor pattern: write-tmp + fsync + rename over
//!   the target. KNOWN-FAIL on macOS until the rename wire fix lands (§1.1).
//! - **bulk-write** — GB-class writes inside the overlay, reporting daemon
//!   peak-RSS per GiB written: the §2.3 in-memory/non-evictable staging-tier
//!   hazard, measured rather than asserted. Explicitly report-only (§2.3).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::bench::{Ceiling, Pair, Row, Timer};
use crate::micro::{classify, timed_rename_probe};
use crate::resource;
use crate::target::{Target, TargetResult};
use crate::tree::{self, TreeSpec};

#[derive(Clone, Copy, Debug)]
pub struct MacroConfig {
    pub churn_workers: u32,
    pub storm_entries: u64,
    pub save_iters: u64,
    pub bulk_mib: u64,
    pub rounds: u32,
}

impl Default for MacroConfig {
    fn default() -> Self {
        Self {
            churn_workers: 16,
            storm_entries: 10_000,
            save_iters: 200,
            bulk_mib: 256,
            rounds: 3,
        }
    }
}

/// Hidden child-process entry for the churn workload. Ops chosen to be
/// rename-free so the workload itself survives the known macFUSE 5.x rename
/// breakage (rename is covered — and known-failed — by `atomic-save`).
pub fn churn_worker_main(dir: &Path, files: u32) -> std::io::Result<()> {
    let body = vec![0xC0u8; 2048];
    for i in 0..files {
        let p = dir.join(format!("c{i:03}.dat"));
        std::fs::write(&p, &body)?;
        let _ = std::fs::read(&p)?;
        std::fs::write(&p, &body[..1024])?; // truncate-rewrite
    }
    for i in 0..files / 2 {
        std::fs::remove_file(dir.join(format!("c{i:03}.dat")))?;
    }
    Ok(())
}

/// Spawn-heavy churn: spawn `workers` copies of this binary, each doing
/// create/read/rewrite/unlink bursts *inside* `dir` (the mount or the bare
/// lower). No sleeps; the workload is pure FS + spawn cost.
fn measure_churn(
    name: &str,
    target: &Target,
    cfg: &MacroConfig,
    daemon: Option<u32>,
) -> Row {
    let Some(root) = target.host_path() else {
        return Row::skip(name, "churn spawns processes: needs a kernel-visible path");
    };
    let dir = root.join("ws").join("churn");
    let _ = std::fs::create_dir_all(&dir);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("epoch-bench"));
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, None, || {
        let t = Timer::start();
        for w in 0..cfg.churn_workers {
            let wdir = dir.join(format!("w{w:02}"));
            let _ = std::fs::create_dir_all(&wdir);
            let status = Command::new(&exe)
                .args(["__churn-worker"])
                .arg(&wdir)
                .arg("24")
                .status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    out = Err(crate::target::TargetError::Vfs(
                        epoch_vfs_api::VfsError::Io(format!("churn worker exited {s}")),
                    ));
                    return;
                }
                Err(e) => {
                    out = Err(e.into());
                    return;
                }
            }
        }
        out = Ok(t.stop());
    });
    match out {
        Ok(wall) => Row::ok(name, cfg.churn_workers as u64, wall)
            .with_resources(res)
            .with_note(format!(
                "{} spawned workers × 24 create/read/rewrite + 12 unlink, writes in-dir",
                cfg.churn_workers
            )),
        Err(e) => classify(name, &e),
    }
}

/// Populate the 10⁴-entry storm tree (host path; idempotent).
pub fn populate_storm(root: &Path, cfg: &MacroConfig) -> std::io::Result<()> {
    let storm = root.join("storm");
    if !storm.exists() {
        tree::generate(
            &storm,
            TreeSpec {
                files: cfg.storm_entries,
                bytes: cfg.storm_entries * 1024,
                dirs: cfg.storm_entries / 100,
                sparse: false,
            },
        )?;
    }
    Ok(())
}

fn measure_storm(name: &str, target: &Target, cfg: &MacroConfig, daemon: Option<u32>) -> Row {
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, None, || {
        let mut best: Option<Duration> = None;
        for _ in 0..cfg.rounds.max(1) {
            let t = Timer::start();
            match target.stat_walk(Path::new("storm")) {
                Ok(_) => {
                    let d = t.stop();
                    best = Some(best.map_or(d, |b: Duration| b.min(d)));
                }
                Err(e) => {
                    out = Err(e);
                    return;
                }
            }
        }
        out = Ok(best.unwrap_or_default());
    });
    match out {
        Ok(wall) => Row::ok(name, cfg.storm_entries, wall)
            .with_resources(res)
            .with_note(format!(
                "best of {} walks over {} entries",
                cfg.rounds, cfg.storm_entries
            )),
        Err(e) => classify(name, &e),
    }
}

/// Editor atomic-save loop: write `<f>.tmp`, fsync, rename over `<f>`.
fn measure_save(name: &str, target: &Target, cfg: &MacroConfig, daemon: Option<u32>) -> Row {
    // Gate on the rename probe first so macOS reports KNOWN-FAIL cheaply
    // instead of 200 identical EINVALs.
    if let Target::Host(root) = target
        && let Err(sig) = timed_rename_probe(root)
    {
        return classify(name, &crate::target::TargetError::Io(sig));
    }
    let _ = target.mkdir_all(&PathBuf::from("ws"));
    let target_file = PathBuf::from("ws/save.txt");
    let tmp_file = PathBuf::from("ws/save.txt.tmp");
    let _ = target.write_small(&target_file, b"initial", false);
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, None, || {
        let t = Timer::start();
        for _ in 0..cfg.save_iters {
            if let Err(e) = target
                .write_small(&tmp_file, b"updated body, not slept", true)
                .and_then(|_| target.rename(&tmp_file, &target_file))
            {
                out = Err(e);
                return;
            }
        }
        out = Ok(t.stop());
    });
    match out {
        Ok(wall) => Row::ok(name, cfg.save_iters, wall)
            .with_resources(res)
            .with_note(format!(
                "{} × (write 4 KiB tmp + fsync + rename over target)",
                cfg.save_iters
            )),
        Err(e) => classify(name, &e),
    }
}

/// GB-class writes inside the overlay: the §2.3 staging-tier RAM hazard,
/// measured. Report-only — §2.3 sets no ceiling for bulk overlay writes (the
/// intended answer for build dirs is `.epochignore`; this row quantifies what
/// ignoring that advice costs).
fn measure_bulk(name: &str, target: &Target, cfg: &MacroConfig, daemon: Option<u32>) -> Row {
    let _ = target.mkdir_all(&PathBuf::from("ws/bulk"));
    let chunk = vec![0x5Au8; 1 << 20];
    let files = cfg.bulk_mib; // 1 MiB per file
    let bytes = files * (chunk.len() as u64);
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, Some(bytes), || {
        let t = Timer::start();
        for i in 0..files {
            if let Err(e) =
                target.write_small(&PathBuf::from(format!("ws/bulk/b{i:05}.bin")), &chunk, false)
            {
                out = Err(e);
                return;
            }
        }
        out = Ok(t.stop());
    });
    match out {
        Ok(wall) => {
            let mib_s = cfg.bulk_mib as f64 / wall.as_secs_f64();
            Row::ok(name, files, wall)
                .with_resources(res)
                .with_ceiling(Ceiling::None)
                .with_note(format!(
                    "{} in {} × 1 MiB files ({mib_s:.0} MiB/s)",
                    crate::bench::fmt_bytes(bytes),
                    files
                ))
        }
        Err(e) => classify(name, &e),
    }
}

/// The four macro benches, paired bare-vs-mount with ceilings applied.
pub fn run(bare: &Target, mount: &Target, cfg: &MacroConfig, daemon_pid: Option<u32>) -> Vec<Pair> {
    let mut pairs = Vec::new();

    // churn — report only (envelope shape, no §2.2 ceiling).
    let b = measure_churn("churn-spawn", bare, cfg, None);
    let m = measure_churn("churn-spawn", mount, cfg, daemon_pid);
    pairs.push(Pair::new(b, m));

    // stat-storm — the §2.2 stat-walk ceiling at 10⁴ scale.
    let b = measure_storm("stat-storm-10k", bare, cfg, None);
    let m = measure_storm("stat-storm-10k", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxRatio(3.0));
    let mut pair = Pair::new(b, m);
    pair.eval_ratio_ceiling(3.0);
    pairs.push(pair);

    // atomic-save — known-fail on macOS until §1.1 lands.
    let b = measure_save("atomic-save", bare, cfg, None);
    let m = measure_save("atomic-save", mount, cfg, daemon_pid)
        .with_ceiling(Ceiling::MaxRatio(2.0));
    let mut pair = Pair::new(b, m);
    pair.eval_ratio_ceiling(2.0);
    pairs.push(pair);

    // bulk-write — §2.3 staging-tier RAM hazard, report only.
    let b = measure_bulk("bulk-write", bare, cfg, None);
    let m = measure_bulk("bulk-write", mount, cfg, daemon_pid);
    pairs.push(Pair::new(b, m));

    pairs
}
