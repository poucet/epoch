//! REALWORLD — Chris's addition to §2.4: *actual usage*, run twice, once on
//! the bare lower and once inside the epoch overlay, wall-clock compared.
//! The micro numbers explain; these numbers decide.
//!
//! Workload pairs:
//!
//! - **git-flow** — `git init` + `git add` + `git commit` + `git status` +
//!   `git clone` of a procedurally generated 200-file repo. git's object
//!   writes use write-tmp+rename, so this is KNOWN-FAIL on macFUSE 5.x until
//!   the §1.1 lane lands (gated on a rename probe, not on git's error text).
//! - **jj-flow** — `jj git init` + commit + log. Same rename caveat, plus jj
//!   snapshots symlinks (§1.2). Skipped when `jj` is absent.
//! - **sed-loop** — `sed -i` in-place edits across a 300-file tree (the
//!   editor-session shape: sed -i *is* write-tmp+rename per file). Same probe.
//! - **cargo-build** — `cargo build --offline` of a generated dependency-free
//!   fixture crate, `target/` INSIDE the measured dir so writes traverse the
//!   overlay. Two modes: `overlay` (target/ goes through the overlay) and
//!   `ignore` (the lower has `.epochignore` with `/target/`, routing build
//!   output straight to the host — the §2.3 contract, measured). The ignore
//!   mode requires the mount to have been made *after* the `.epochignore`
//!   was written (rules load once at mount) — the runner script does the
//!   two-phase mount; standalone, pass `--cargo-mode`.
//!
//! Not wired (documented, not silently dropped): npm/pip package-manager
//! workload — the git and cargo pairs were the required ones.
//!
//! Rules (same as the rest of the suite): ×bare AND absolute, same machine,
//! same run; writes forced inside the mount; no sleep-bound anything; known
//! breakage degrades to KNOWN-FAIL, never a crash.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::bench::{Ceiling, Pair, Row, Timer};
use crate::micro::{classify, timed_rename_probe};
use crate::resource;
use crate::target::{Target, TargetError, TargetResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CargoMode {
    Overlay,
    Ignore,
    Both,
}

fn have(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate the shared 200-file source corpus into `dir`.
fn seed_corpus(dir: &Path, files: u32) -> std::io::Result<()> {
    for i in 0..files {
        let sub = dir.join(format!("src/{:02}", i % 10));
        std::fs::create_dir_all(&sub)?;
        std::fs::write(
            sub.join(format!("file{i:03}.txt")),
            format!("OLD value {i}\nrepeated content line\n").repeat(16),
        )?;
    }
    Ok(())
}

/// Run one shell step; 60 s budget each so a wedge can't hang the suite.
fn step(dir: &Path, prog: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("CARGO_NET_OFFLINE", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir) // keep tool config hermetic; jj/git read $HOME
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{prog} {:?} exited {}: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()
        ))
    }
}

/// A realworld workload = wall-timed sequence of steps in `dir`.
fn measure(
    name: &str,
    target: &Target,
    subdir: &str,
    daemon: Option<u32>,
    needs_rename: bool,
    run: impl FnOnce(&Path) -> Result<(), String>,
) -> Row {
    let Some(root) = target.host_path() else {
        return Row::skip(name, "realworld workloads are subprocesses: needs a kernel-visible path");
    };
    let dir = root.join("ws").join(subdir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Row::fail(name, e.to_string());
    }
    if needs_rename
        && let Err(e) = timed_rename_probe(&dir)
    {
        return classify(name, &TargetError::Io(e));
    }
    let mut out: TargetResult<Duration> = Ok(Duration::ZERO);
    let res = resource::capture(daemon, None, || {
        let t = Timer::start();
        match run(&dir) {
            Ok(()) => out = Ok(t.stop()),
            Err(msg) => {
                out = Err(TargetError::Vfs(epoch_vfs_api::VfsError::Io(msg)));
            }
        }
    });
    match out {
        Ok(wall) => Row::ok(name, 1, wall).with_resources(res).with_ceiling(Ceiling::None),
        Err(e) => classify(name, &e),
    }
}

fn git_flow(name: &str, target: &Target, daemon: Option<u32>) -> Row {
    if !have("git") {
        return Row::skip(name, "git not on PATH");
    }
    measure(name, target, "git", daemon, true, |dir| {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).map_err(|e| e.to_string())?;
        seed_corpus(&repo, 200).map_err(|e| e.to_string())?;
        step(&repo, "git", &["init", "-q"])?;
        step(&repo, "git", &["add", "-A"])?;
        step(&repo, "git", &["-c", "user.email=bench@epoch", "-c", "user.name=bench", "commit", "-qm", "corpus"])?;
        step(&repo, "git", &["status", "--porcelain"])?;
        step(dir, "git", &["clone", "-q", "repo", "clone"])?;
        Ok(())
    })
    .with_note("init + add + commit + status + clone of 200 files".to_string())
}

fn jj_flow(name: &str, target: &Target, daemon: Option<u32>) -> Row {
    if !have("jj") {
        return Row::skip(name, "jj not on PATH");
    }
    measure(name, target, "jj", daemon, true, |dir| {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).map_err(|e| e.to_string())?;
        seed_corpus(&repo, 200).map_err(|e| e.to_string())?;
        step(&repo, "jj", &["git", "init"])?;
        step(&repo, "jj", &["commit", "-m", "corpus"])?;
        step(&repo, "jj", &["log", "-r", "@", "--no-graph", "-T", "description"])?;
        Ok(())
    })
    .with_note("jj git init + commit + log over 200 files".to_string())
}

/// `sed -i` argument style differs BSD↔GNU; detect once.
fn sed_inplace_prefix() -> Vec<&'static str> {
    let gnu = Command::new("sed")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if gnu {
        vec!["-i", "-e"]
    } else {
        vec!["-i", "", "-e"]
    }
}

fn sed_loop(name: &str, target: &Target, daemon: Option<u32>) -> Row {
    measure(name, target, "sed", daemon, true, |dir| {
        seed_corpus(dir, 300).map_err(|e| e.to_string())?;
        let prefix = sed_inplace_prefix();
        // 300 files, one sed invocation per 25 — per-file tmp+rename either way.
        for chunk in 0..12 {
            let mut files: Vec<String> = Vec::new();
            for i in (chunk * 25)..((chunk + 1) * 25) {
                files.push(format!("src/{:02}/file{i:03}.txt", i % 10));
            }
            let mut args: Vec<&str> = prefix.clone();
            args.push("s/OLD/NEW/g");
            let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
            args.extend(file_refs);
            step(dir, "sed", &args)?;
        }
        Ok(())
    })
    .with_note("300 files edited in place (write-tmp+rename per file)".to_string())
}

/// Generate the dependency-free fixture crate (~8 modules × 40 fns).
fn seed_crate(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        // The empty [workspace] detaches the fixture from any parent
        // workspace (a scratch dir under the repo would otherwise fail with
        // "current package believes it's in a workspace").
        "[package]\nname = \"bench-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\n[workspace]\n",
    )?;
    let mut lib = String::new();
    for m in 0..8 {
        lib.push_str(&format!("pub mod m{m};\n"));
        let mut body = String::new();
        for f in 0..40 {
            body.push_str(&format!(
                "pub fn f{m}_{f}(x: u64) -> u64 {{ x.wrapping_mul({m}).wrapping_add({f}) ^ (x << {} ) }}\n",
                (f % 13) + 1
            ));
        }
        body.push_str("#[cfg(test)]\nmod tests { #[test] fn t() { assert!(super::f0_0(1) != 999999); } }\n");
        std::fs::write(dir.join(format!("src/m{m}.rs")), body)?;
    }
    std::fs::write(dir.join("src/lib.rs"), lib)?;
    Ok(())
}

fn cargo_build(name: &str, target: &Target, daemon: Option<u32>) -> Row {
    if !have("cargo") {
        return Row::skip(name, "cargo not on PATH");
    }
    measure(name, target, name, daemon, true, |dir| {
        seed_crate(dir).map_err(|e| e.to_string())?;
        // target/ stays INSIDE the measured dir so writes traverse the overlay
        // (or the .epochignore route, in the ignore variant).
        step(
            dir,
            "cargo",
            &["build", "--offline", "--target-dir", "target"],
        )?;
        Ok(())
    })
    .with_note("cargo build --offline of generated 8-mod crate, target/ in-dir".to_string())
}

pub fn run(
    bare: &Target,
    mount: &Target,
    daemon_pid: Option<u32>,
    cargo_mode: CargoMode,
) -> Vec<Pair> {
    let mut pairs = vec![
        Pair::new(git_flow("git-flow", bare, None), git_flow("git-flow", mount, daemon_pid)),
        Pair::new(jj_flow("jj-flow", bare, None), jj_flow("jj-flow", mount, daemon_pid)),
        Pair::new(sed_loop("sed-loop", bare, None), sed_loop("sed-loop", mount, daemon_pid)),
    ];
    let overlay = matches!(cargo_mode, CargoMode::Overlay | CargoMode::Both);
    let ignore = matches!(cargo_mode, CargoMode::Ignore | CargoMode::Both);
    if overlay {
        pairs.push(Pair::new(
            cargo_build("cargo-build-overlay", bare, None),
            cargo_build("cargo-build-overlay", mount, daemon_pid),
        ));
    }
    if ignore {
        let m = if mount.host_path().is_some() {
            cargo_build("cargo-build-ignore", mount, daemon_pid)
        } else {
            Row::skip("cargo-build-ignore", "needs a live mount made with /target/ in .epochignore")
        };
        pairs.push(Pair::new(cargo_build("cargo-build-ignore", bare, None), m));
    }
    pairs
}
