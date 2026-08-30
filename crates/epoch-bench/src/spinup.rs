//! SPIN-UP AT SCALE — §2.4.3. Synthetic lower trees at {10³, 10⁴, 10⁵} files
//! × {10 MB, 1 GB, 10 GB}, reporting mount→first-read, first readdir of root,
//! and first-write latency, vs the bare lower.
//!
//! Content choice (documented in the suite README): cells are **sparse by
//! default** (zero-filled via `set_len`, one metadata op per file). Spin-up
//! latency is a *metadata* cost — epoch hydrates directory spines lazily
//! (research §5, `DirChildren::Sealed`), so zero pages exercise the same
//! lookup/readdir paths without spending ten minutes writing entropy. Pass
//! `--dense` to fill cells with PRNG bytes instead.
//!
//! Cold-cache caveat (stated honestly, per §2.4.3): macOS has no unprivileged
//! cache drop (`purge` needs sudo), so these are WARM-CACHE numbers.
//!
//! Ceiling (§2.2): mount→first-read ≤ 0.5 s at 10 GB / 10⁵ files — applied to
//! every cell (smaller cells should pass trivially).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use epoch_cas::MemoryByteBackend;
use epoch_vfs::{CasVfs, PassthroughVfs};
use epoch_vfs_api::VfsService;

use crate::bench::{fmt_bytes, Ceiling, Pair, Row, Timer};
use crate::micro::classify;
use crate::target::{Target, TargetResult};
use crate::tree::{self, TreeSpec};

#[derive(Clone, Debug)]
pub struct SpinupConfig {
    /// (files, bytes) grid.
    pub cells: Vec<(u64, u64)>,
    pub dense: bool,
    pub scratch: PathBuf,
    /// epoch-fuse binary for real mounts; absent → in-process VFS numbers.
    pub fuse_bin: Option<PathBuf>,
    pub quick: bool,
}

impl SpinupConfig {
    pub fn default_grid(scratch: PathBuf, fuse_bin: Option<PathBuf>) -> Self {
        Self {
            cells: vec![
                (1_000, 10 << 20),
                (1_000, 1 << 30),
                (1_000, 10 << 30),
                (10_000, 10 << 20),
                (10_000, 1 << 30),
                (10_000, 10 << 30),
                (100_000, 10 << 20),
                (100_000, 1 << 30),
                (100_000, 10 << 30),
            ],
            dense: false,
            scratch,
            fuse_bin,
            quick: false,
        }
    }

    pub fn quick_grid(scratch: PathBuf, fuse_bin: Option<PathBuf>) -> Self {
        let mut cfg = Self::default_grid(scratch, fuse_bin);
        cfg.cells = vec![(1_000, 10 << 20), (10_000, 1 << 30)];
        cfg.quick = true;
        cfg
    }
}

fn cell_dirs(scratch: &Path, files: u64, bytes: u64, dense: bool) -> (PathBuf, PathBuf, PathBuf) {
    let tag = format!(
        "f{}-b{}-{}",
        files,
        bytes,
        if dense { "dense" } else { "sparse" }
    );
    let base = scratch.join("spinup").join(&tag);
    (base.join("lower"), base.join("mnt"), base.join("state"))
}

fn ensure_lower(lower: &Path, files: u64, bytes: u64, dense: bool) -> std::io::Result<()> {
    let marker = lower.join(".epoch-bench-complete");
    if marker.exists() {
        return Ok(());
    }
    if lower.exists() {
        std::fs::remove_dir_all(lower)?;
    }
    let spec = TreeSpec::scaled(files, bytes, !dense);
    tree::generate(lower, spec)?;
    std::fs::write(marker, b"ok")?;
    Ok(())
}

/// Read one byte of the first fixture file through `target`.
fn first_read(target: &Target) -> TargetResult<Duration> {
    let t = Timer::start();
    let mut buf = Vec::new();
    target.read_file(Path::new("d00000/f0000000.bin"), &mut buf)?;
    Ok(t.stop())
}

fn first_readdir(target: &Target) -> TargetResult<Duration> {
    let t = Timer::start();
    target.stat_walk_shallow(Path::new(""))?;
    Ok(t.stop())
}

fn first_write(target: &Target) -> TargetResult<Duration> {
    target.mkdir_all(Path::new("ws"))?;
    let t = Timer::start();
    target.write_small(Path::new("ws/first-write.bin"), b"epoch bench first write", false)?;
    Ok(t.stop())
}

/// A live `epoch-fuse` subprocess mount, unmounted on drop.
struct FuseMount {
    child: Child,
    mountpoint: PathBuf,
}

impl FuseMount {
    /// Spawn the mount and return the instant the mount became readable
    /// (i.e. mount→first-successful-read is measurable from `spawned_at`).
    fn spawn(
        fuse_bin: &Path,
        lower: &Path,
        mnt: &Path,
        state: &Path,
    ) -> std::io::Result<(Self, Instant)> {
        std::fs::create_dir_all(mnt)?;
        std::fs::create_dir_all(state)?;
        let spawned_at = Instant::now();
        let child = Command::new(fuse_bin)
            .arg("cas")
            .arg(mnt)
            .arg("--state")
            .arg(state)
            .arg("--head")
            .arg("bench")
            .arg(lower)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok((Self { child, mountpoint: mnt.into() }, spawned_at))
    }

    /// Poll until `rel` is readable through the mount (or 30 s elapse).
    fn wait_readable(&self, rel: &Path) -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if std::fs::File::open(self.mountpoint.join(rel)).is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "mount never became readable",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mountpoint).status();
        let _ = Command::new("diskutil")
            .args(["unmount", "force"])
            .arg(&self.mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    break;
                }
                Err(_) => break,
            }
        }
    }
}

/// One cell: (files × bytes). Returns three pairs: mount→first-read,
/// first-readdir, first-write. Bare rows measure the same ops on the lower.
fn run_cell(cfg: &SpinupConfig, files: u64, bytes: u64) -> Vec<Pair> {
    let label = format!("{}×{}", files, fmt_bytes(bytes));
    let (lower, mnt, state) = cell_dirs(&cfg.scratch, files, bytes, cfg.dense);
    if let Err(e) = ensure_lower(&lower, files, bytes, cfg.dense) {
        return vec![Pair::new(
            Row::fail(format!("spinup/{label}/first-read"), e.to_string()),
            Row::fail(format!("spinup/{label}/first-read"), e.to_string()),
        )];
    }

    // Bare side: same ops straight on the lower dir.
    let bare = Target::host(&lower);
    let bare_read = match first_read(&bare) {
        Ok(d) => Row::ok(format!("spinup/{label}/first-read"), 1, d),
        Err(e) => classify(&format!("spinup/{label}/first-read"), &e),
    };
    let bare_readdir = match first_readdir(&bare) {
        Ok(d) => Row::ok(format!("spinup/{label}/first-readdir"), 1, d),
        Err(e) => classify(&format!("spinup/{label}/first-readdir"), &e),
    };
    let bare_write = match first_write(&bare) {
        Ok(d) => Row::ok(format!("spinup/{label}/first-write"), 1, d),
        Err(e) => classify(&format!("spinup/{label}/first-write"), &e),
    };
    // The bare first-write pollutes the lower fixture (the mount side's write
    // lands in its overlay instead) — remove it so both sides see the same
    // pristine lower and re-runs stay idempotent.
    let _ = std::fs::remove_dir_all(lower.join("ws"));

    // Mount side.
    let (mount_read, mount_readdir, mount_write) = match &cfg.fuse_bin {
        Some(bin) => match FuseMount::spawn(bin, &lower, &mnt, &state) {
            Ok((mount, spawned_at)) => {
                let read_row = match mount.wait_readable(Path::new("d00000/f0000000.bin")) {
                    Ok(()) => {
                        Row::ok(format!("spinup/{label}/first-read"), 1, spawned_at.elapsed())
                            .with_note("spawn→first successful read through the mount")
                    }
                    Err(e) => Row::fail(format!("spinup/{label}/first-read"), e.to_string()),
                };
                let target = Target::host(&mnt);
                let readdir_row = match first_readdir(&target) {
                    Ok(d) => Row::ok(format!("spinup/{label}/first-readdir"), 1, d),
                    Err(e) => classify(&format!("spinup/{label}/first-readdir"), &e),
                };
                let write_row = match first_write(&target) {
                    Ok(d) => Row::ok(format!("spinup/{label}/first-write"), 1, d),
                    Err(e) => classify(&format!("spinup/{label}/first-write"), &e),
                };
                (read_row, readdir_row, write_row)
            }
            Err(e) => {
                let row = Row::fail(format!("spinup/{label}/first-read"), format!("mount: {e}"));
                (row.clone(), row.clone(), row)
            }
        },
        None => {
            // In-process mode: stack construction + first ops through the VFS.
            let t = Timer::start();
            let backend = MemoryByteBackend::new();
            let lower_vfs: Arc<dyn VfsService> = Arc::new(PassthroughVfs::new(&lower));
            let vfs = Arc::new(CasVfs::overlay(backend, lower_vfs));
            let build = t.stop();
            match Target::vfs(vfs, "") {
                Ok(target) => {
                    let read_row = match first_read(&target) {
                        Ok(d) => Row::ok(format!("spinup/{label}/first-read"), 1, build + d)
                            .with_note("in-process: stack build + first read (no FUSE)"),
                        Err(e) => classify(&format!("spinup/{label}/first-read"), &e),
                    };
                    let readdir_row = match first_readdir(&target) {
                        Ok(d) => Row::ok(format!("spinup/{label}/first-readdir"), 1, d),
                        Err(e) => classify(&format!("spinup/{label}/first-readdir"), &e),
                    };
                    let write_row = match first_write(&target) {
                        Ok(d) => Row::ok(format!("spinup/{label}/first-write"), 1, d),
                        Err(e) => classify(&format!("spinup/{label}/first-write"), &e),
                    };
                    (read_row, readdir_row, write_row)
                }
                Err(e) => {
                    let row = classify(&format!("spinup/{label}/first-read"), &e);
                    (row.clone(), row.clone(), row)
                }
            }
        }
    };

    let mut read_pair = Pair::new(bare_read, mount_read.with_ceiling(Ceiling::MaxAbs(Duration::from_millis(500))));
    read_pair.mount.eval_abs_ceiling();
    let readdir_pair = Pair::new(bare_readdir, mount_readdir);
    let write_pair = Pair::new(bare_write, mount_write);
    vec![read_pair, readdir_pair, write_pair]
}

/// The §2.4.3 grid. Prints the warm-cache caveat once, honestly.
pub fn run(cfg: &SpinupConfig) -> Vec<Pair> {
    println!(
        "\n(note: macOS has no unprivileged cache drop — `purge` needs sudo — so\n\
         spin-up numbers are WARM-CACHE; a privileged cold-cache profile is future work.\n\
         Cells are {}.)",
        if cfg.dense { "dense (PRNG content)" } else { "sparse (zero-fill): spin-up is a metadata cost; see suite README" }
    );
    let mut pairs = Vec::new();
    for &(files, bytes) in &cfg.cells {
        eprintln!("[spinup] cell {files} files × {}", fmt_bytes(bytes));
        pairs.extend(run_cell(cfg, files, bytes));
    }
    pairs
}
