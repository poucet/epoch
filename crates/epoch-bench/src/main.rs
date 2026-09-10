//! epoch-bench — the benchmark suite EPOCH-FIXES §2.4 requires epoch to ship.
//!
//! Every workload runs twice — bare lower and epoch overlay, same machine,
//! same run — and is reported as ×bare AND absolute against the §2.2
//! ceilings. No sleep-bound suite may appear in a perf claim (§2.4.5), so
//! this binary contains no sleeps except mount-readiness polling.
//!
//! Usage:
//!   epoch-bench <group>… [options]     groups: micro macro spinup mmap realworld all
//!   epoch-bench __churn-worker <dir> <files>   (hidden; spawned by `macro`)
//!
//! Options:
//!   --bare <dir>       bare-side scratch root       (default: fresh tempdir)
//!   --mount <dir>      a live epoch FUSE mountpoint (kernel-path numbers)
//!   --lower <dir>      the lower dir backing --mount (fixtures land here)
//!   --daemon-pid <pid> epoch-fuse pid serving --mount (daemon CPU/RSS capture)
//!   --fuse-bin <path>  epoch-fuse binary (spinup mounts its own cells;
//!                      absent → in-process VFS numbers, no FUSE)
//!   --scratch <dir>    spinup/realworld scratch     (default: fresh tempdir)
//!   --cargo-mode <overlay|ignore|both>              (default: both)
//!   --dense            spinup cells get PRNG content instead of sparse files
//!   --quick            reduced grid for smoke runs
//!   --iters <n>        override per-class iteration counts
//!
//! Without --mount, the mount side runs in-process against a CasVfs overlay:
//! VFS-layer cost without the kernel round-trip, headless, and immune to the
//! macFUSE 5.x rename wire bug (which lives in the wire decode, §1.1).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use epoch_cas::MemoryByteBackend;
use epoch_vfs::{CasVfs, PassthroughVfs};
use epoch_vfs_api::VfsService;

mod bench;
mod macrol;
mod micro;
mod mmap;
mod realworld;
mod resource;
mod spinup;
mod target;
mod tree;

use target::Target;

struct Opts {
    groups: Vec<String>,
    bare: Option<PathBuf>,
    mount: Option<PathBuf>,
    lower: Option<PathBuf>,
    daemon_pid: Option<u32>,
    fuse_bin: Option<PathBuf>,
    scratch: Option<PathBuf>,
    cargo_mode: realworld::CargoMode,
    dense: bool,
    quick: bool,
    iters: Option<u64>,
    no_fail_exit: bool,
    markdown: Option<PathBuf>,
}

fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts {
        groups: Vec::new(),
        bare: None,
        mount: None,
        lower: None,
        daemon_pid: None,
        fuse_bin: None,
        scratch: None,
        cargo_mode: realworld::CargoMode::Both,
        dense: false,
        quick: false,
        iters: None,
        no_fail_exit: false,
        markdown: None,
    };
    let known = ["micro", "macro", "spinup", "mmap", "realworld", "all"];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bare" => opts.bare = Some(PathBuf::from(args.next().ok_or("--bare needs a value")?)),
            "--mount" => opts.mount = Some(PathBuf::from(args.next().ok_or("--mount needs a value")?)),
            "--lower" => opts.lower = Some(PathBuf::from(args.next().ok_or("--lower needs a value")?)),
            "--daemon-pid" => {
                opts.daemon_pid =
                    Some(args.next().ok_or("--daemon-pid needs a value")?.parse().map_err(|_| "--daemon-pid must be a pid")?)
            }
            "--fuse-bin" => {
                opts.fuse_bin = Some(PathBuf::from(args.next().ok_or("--fuse-bin needs a value")?))
            }
            "--scratch" => {
                opts.scratch = Some(PathBuf::from(args.next().ok_or("--scratch needs a value")?))
            }
            "--cargo-mode" => {
                opts.cargo_mode = match args.next().as_deref() {
                    Some("overlay") => realworld::CargoMode::Overlay,
                    Some("ignore") => realworld::CargoMode::Ignore,
                    Some("both") => realworld::CargoMode::Both,
                    _ => return Err("--cargo-mode must be overlay|ignore|both".into()),
                }
            }
            "--dense" => opts.dense = true,
            "--quick" => opts.quick = true,
            "--no-fail-exit" => opts.no_fail_exit = true,
            "--markdown" => {
                opts.markdown = Some(PathBuf::from(args.next().ok_or("--markdown needs a file path")?))
            }
            "--iters" => {
                opts.iters =
                    Some(args.next().ok_or("--iters needs a value")?.parse().map_err(|_| "--iters must be a number")?)
            }
            "-h" | "--help" => {
                println!("{}", module_doc());
                std::process::exit(0);
            }
            g if known.contains(&g) => opts.groups.push(g.to_string()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if opts.groups.is_empty() {
        return Err("no group given (micro|macro|spinup|mmap|realworld|all)".into());
    }
    if opts.mount.is_some() && opts.lower.is_none() {
        return Err("--mount requires --lower (fixtures are written into the lower)".into());
    }
    Ok(opts)
}

fn module_doc() -> &'static str {
    "epoch-bench <micro|macro|spinup|mmap|realworld|all>… [--bare D] [--mount M --lower L]
  [--daemon-pid P] [--fuse-bin B] [--scratch D] [--cargo-mode MODE] [--dense] [--quick] [--iters N]"
}

fn fresh_tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("epoch-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create bench tempdir");
    dir
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Hidden churn worker (spawned by the macro suite against a host path).
    if args.get(1).map(String::as_str) == Some("__churn-worker") {
        let dir = PathBuf::from(args.get(2).expect("churn dir"));
        let files: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(24);
        return match macrol::churn_worker_main(&dir, files) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("churn worker: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\nusage: {}", module_doc());
            return ExitCode::from(2);
        }
    };

    let scratch = opts.scratch.clone().unwrap_or_else(|| fresh_tempdir("scratch"));
    std::fs::create_dir_all(&scratch).expect("scratch dir");

    // ── assemble the two sides ───────────────────────────────────────────────
    let bare_root = opts.bare.clone().unwrap_or_else(|| fresh_tempdir("bare"));
    std::fs::create_dir_all(&bare_root).expect("bare dir");
    let bare = Target::host(&bare_root);

    let (mount, mode): (Target, String) = match &opts.mount {
        Some(mnt) => {
            let lower = opts.lower.clone().expect("checked in parse_args");
            populate_fixtures(&lower, &opts);
            populate_fixtures(&bare_root, &opts);
            (
                Target::host(mnt),
                format!("kernel mount at {} (lower {})", mnt.display(), lower.display()),
            )
        }
        None => {
            let lower = scratch.join("inproc-lower");
            populate_fixtures(&lower, &opts);
            populate_fixtures(&bare_root, &opts);
            let backend = MemoryByteBackend::new();
            let lower_vfs: Arc<dyn VfsService> = Arc::new(PassthroughVfs::new(&lower));
            let vfs = Arc::new(CasVfs::overlay(backend, lower_vfs));
            let target = Target::vfs(vfs, "").expect("in-process mount");
            (target, format!("in-process CasVfs overlay (lower {})", lower.display()))
        }
    };

    println!("epoch-bench — {}", env!("CARGO_PKG_VERSION"));
    println!("  bare side : {}", bare_root.display());
    println!("  mount side: {mode}");
    println!(
        "  resources : {}",
        match opts.daemon_pid {
            Some(pid) => format!("daemon pid {pid} (ps-sampled CPU/RSS) + workload rusage"),
            None => "in-process: RUSAGE_SELF deltas (mount threads included), self RSS".into(),
        }
    );

    // ── run the groups ───────────────────────────────────────────────────────
    let mut micro_cfg = micro::MicroConfig::default();
    let mut macro_cfg = macrol::MacroConfig::default();
    if opts.quick {
        micro_cfg.files = 128;
        micro_cfg.copyup_files = 4;
        micro_cfg.write_iters = 64;
        macro_cfg.storm_entries = 2_000;
        macro_cfg.save_iters = 64;
        macro_cfg.bulk_mib = 64;
        macro_cfg.churn_workers = 8;
    }
    if let Some(n) = opts.iters {
        micro_cfg.write_iters = n;
        macro_cfg.save_iters = n;
    }

    let wants = |g: &str| opts.groups.iter().any(|x| x == g || x == "all");
    let (mut ok, mut fail, mut known, mut skip) = (0, 0, 0, 0);
    let mut markdown_chunks: Vec<String> = Vec::new();

    let mut tally = |pairs: &[bench::Pair], title: &str| {
        let (o, f, k, s) = bench::report(title, pairs);
        ok += o;
        fail += f;
        known += k;
        skip += s;
        if opts.markdown.is_some() {
            markdown_chunks.push(bench::to_markdown(title, pairs));
        }
    };

    if wants("micro") {
        tally(&micro::run(&bare, &mount, &micro_cfg, opts.daemon_pid), "MICRO — per op class (§2.4.1)");
    }
    if wants("macro") {
        tally(&macrol::run(&bare, &mount, &macro_cfg, opts.daemon_pid), "MACRO — envelope shapes (§2.4.2)");
    }
    if wants("mmap") {
        tally(&mmap::run(&bare, &mount, opts.daemon_pid), "MMAP SMOKE (§2.4.4)");
    }
    if wants("realworld") {
        tally(
            &realworld::run(&bare, &mount, opts.daemon_pid, opts.cargo_mode),
            "REALWORLD — actual usage, bare vs overlay",
        );
    }
    if wants("spinup") {
        let spin_cfg = if opts.quick {
            spinup::SpinupConfig::quick_grid(scratch.clone(), opts.fuse_bin.clone())
        } else {
            spinup::SpinupConfig::default_grid(scratch.clone(), opts.fuse_bin.clone())
        };
        let mut spin_cfg = spin_cfg;
        spin_cfg.dense = opts.dense;
        tally(&spinup::run(&spin_cfg), "SPIN-UP AT SCALE (§2.4.3)");
    }

    if let Some(md_path) = &opts.markdown {
        let mut md_content = String::new();
        let already_exists = md_path.exists() && std::fs::metadata(md_path).map(|m| m.len() > 0).unwrap_or(false);
        if !already_exists {
            md_content.push_str("## Epoch Benchmark Results\n\n");
        }
        md_content.push_str(&format!(
            "**Run summary**: {ok} passed, {fail} failed, {known} known failures, {skip} skipped\n\n"
        ));
        for chunk in markdown_chunks {
            md_content.push_str(&chunk);
        }
        if let Some(parent) = md_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(md_path)
            .and_then(|mut f| f.write_all(md_content.as_bytes()));
        if let Err(e) = res {
            eprintln!("failed to write markdown to {}: {e}", md_path.display());
        } else {
            eprintln!("[bench] wrote markdown report to {}", md_path.display());
        }
    }

    println!("\n══ TOTAL: {ok} ok · {fail} FAIL · {known} known-fail · {skip} skipped ══");
    if fail > 0 && !opts.no_fail_exit {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Fixtures shared by micro/macro: the read/stat corpus, copy-up victims, and
/// the 10⁴-entry storm tree. Idempotent.
fn populate_fixtures(root: &Path, opts: &Opts) {
    let mut cfg = micro::MicroConfig::default();
    if opts.quick {
        cfg.files = 128;
        cfg.copyup_files = 4;
    }
    let mut macro_cfg = macrol::MacroConfig::default();
    if opts.quick {
        macro_cfg.storm_entries = 2_000;
    }
    eprintln!("[bench] populating fixtures in {}", root.display());
    micro::populate(root, &cfg).expect("populate micro fixture");
    macrol::populate_storm(root, &macro_cfg).expect("populate storm fixture");
}
