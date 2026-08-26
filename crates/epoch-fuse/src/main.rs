use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use epoch_cas::{CachingByteBackend, MemoryByteBackend, TieredByteBackend};
use epoch_cas_moka::MokaByteBackend;
use epoch_cas_redb::RedbByteBackend;
use epoch_fuse::fuser_adapter::mount_spike;
use epoch_history::{CheckpointPolicy, History, Scheduler};
use epoch_timeline_redb::RedbTimelineStore;
use epoch_vfs::router::EPOCHIGNORE_FILE;
use epoch_vfs::{
    overlay_probe, CasObjectDir, CasVfs, CheckpointedFs, ControlPlaneVfs, DebugDir, DebugStats,
    HistoryDir, PassthroughVfs, RouterVfs, SignalVfs,
};
use epoch_vfs_api::VfsService;

const USAGE: &str = "usage:
  epoch-fuse <lower-dir> <mountpoint>         read-through overlay probe
  epoch-fuse cas <mountpoint> [<lower-dir>]   copy-on-write CAS filesystem
                                              (empty in-memory, or overlaying <lower-dir>)
  epoch-fuse cas <mountpoint> --state <dir>   durable, checkpointed CAS filesystem,
        [<lower-dir>] [--head <name>]         standalone or overlaying <lower-dir>
        [--hard-fsync]                        (resumes from <name>'s last checkpoint;
                                              time-travel under .vfs/history)";

/// The moka read-cache in front of redb (bytes).
const CACHE_BYTES: u64 = 256 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let first = args.next().ok_or(USAGE)?;

    // `cas` mode: a copy-on-write CAS filesystem — in-memory by default,
    // durable and checkpointed with `--state`.
    if first.to_str() == Some("cas") {
        let mut state_dir: Option<PathBuf> = None;
        let mut head = String::from("main");
        let mut hard_fsync = false;
        let mut positional: Vec<OsString> = Vec::new();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--state") => state_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
                Some("--head") => {
                    head = args
                        .next()
                        .and_then(|s| s.to_str().map(str::to_owned))
                        .ok_or(USAGE)?;
                }
                Some("--hard-fsync") => hard_fsync = true,
                _ => positional.push(arg),
            }
        }
        let mut positional = positional.into_iter();
        let mountpoint = positional.next().ok_or(USAGE)?;
        let lower_dir = positional.next();

        if let Some(state_dir) = state_dir {
            return mount_durable(mountpoint, state_dir, lower_dir, head, hard_fsync);
        }

        let backend = MemoryByteBackend::new();
        let (live, lower_path): (Arc<CasVfs<MemoryByteBackend>>, Option<PathBuf>) = match lower_dir
        {
            Some(lower_dir) => {
                let lower_dir = fs::canonicalize(lower_dir)?;
                eprintln!(
                    "[epoch-fuse] mounting copy-on-write CAS overlay lower={lower_dir:?} mountpoint={mountpoint:?}"
                );
                let lower: Arc<dyn VfsService> = Arc::new(PassthroughVfs::new(lower_dir.clone()));
                (
                    Arc::new(CasVfs::overlay(backend.clone(), lower)),
                    Some(lower_dir),
                )
            }
            None => {
                eprintln!(
                    "[epoch-fuse] mounting empty in-memory CAS filesystem mountpoint={mountpoint:?}"
                );
                (Arc::new(CasVfs::new(backend.clone())), None)
            }
        };
        // Graft `.vfs` on so `.vfs/debug` serves the live tree's statistics.
        let debug: Arc<dyn DebugStats> = live.clone();
        let overlay: Arc<dyn VfsService> = live.clone();
        let inner = match &lower_path {
            Some(lower_dir) => route_ignored(overlay, lower_dir)?,
            None => overlay,
        };
        let vfs = Arc::new(
            ControlPlaneVfs::new(inner)
                .with_feature("cas", move |slot| Arc::new(CasObjectDir::new(slot, backend)))
                .with_feature("debug", move |slot| Arc::new(DebugDir::new(slot, debug))),
        );
        mount_spike(mountpoint, vfs)?;
        return Ok(());
    }

    // Default mode: read-through overlay probe over a lower host directory.
    let lower = fs::canonicalize(first)?;
    let mountpoint = args.next().ok_or(USAGE)?;
    eprintln!("[epoch-fuse] mounting lower overlay probe lower={lower:?} mountpoint={mountpoint:?}");
    let vfs = overlay_probe(lower);
    mount_spike(mountpoint, vfs)?;
    Ok(())
}

/// Assemble and mount the durable, checkpointed stack:
///
/// ```text
/// ControlPlaneVfs (.vfs/history time-travel)
///   └─ SignalVfs (checkpoint trigger events)
///        └─ CasVfs over Tiered< Memory staging,
///                               Caching< Moka LRU, Redb > >  ← one epoch.redb
///                                                              (objects + timeline)
/// ```
///
/// With a `lower`, the live tree is a copy-on-write overlay and every
/// checkpoint records the **sparse delta** (copied-up content and tombstones —
/// fully CAS-owned). Time-travel snapshots rehydrate a checkpoint's delta over
/// the *current* lower: exact for everything the overlay touched, but
/// untouched fall-through paths track lower drift until `OpaqueTree` ingestion
/// makes checkpoints fully self-contained.
fn mount_durable(
    mountpoint: OsString,
    state_dir: PathBuf,
    lower_dir: Option<OsString>,
    head: String,
    hard_fsync: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(&state_dir)?;
    let db_path = state_dir.join("epoch.redb");
    let lower_path: Option<PathBuf> = lower_dir.map(fs::canonicalize).transpose()?;
    let lower: Option<Arc<dyn VfsService>> = lower_path
        .as_ref()
        .map(|dir| -> Arc<dyn VfsService> { Arc::new(PassthroughVfs::new(dir.clone())) });
    eprintln!(
        "[epoch-fuse] mounting durable CAS filesystem state={db_path:?} head={head} overlay={} hard_fsync={hard_fsync} mountpoint={mountpoint:?}",
        lower.is_some(),
    );

    // One shared database hosts the object table and the timeline tables.
    let db = Arc::new(redb::Database::create(&db_path)?);
    let storage = RedbByteBackend::with_database(Arc::clone(&db))?;
    let far = CachingByteBackend::new(MokaByteBackend::new(CACHE_BYTES), storage);
    let tier = TieredByteBackend::new(MemoryByteBackend::new(), far);
    let timeline = Arc::new(RedbTimelineStore::new(db)?);

    let history = Arc::new(History::new(tier.clone(), timeline, head));

    // Resume from the head's last checkpoint, or start fresh. An overlay mount
    // resumes its sparse delta over the lower; a standalone one its full root.
    let live = match history.head()? {
        Some(id) => {
            let event = history
                .event(&id)?
                .ok_or("timeline head points at a missing event")?;
            eprintln!(
                "[epoch-fuse] resuming from checkpoint {id} (t={}ms)",
                event.timestamp_ms
            );
            match &lower {
                Some(lower) => Arc::new(CasVfs::from_overlay(
                    tier.clone(),
                    Arc::clone(lower),
                    event.root,
                )?),
                None => Arc::new(CasVfs::from_root(tier.clone(), event.root)?),
            }
        }
        None => match &lower {
            Some(lower) => Arc::new(CasVfs::overlay(tier.clone(), Arc::clone(lower))),
            None => Arc::new(CasVfs::new(tier.clone())),
        },
    };

    let checkpointed = CheckpointedFs::new(Arc::clone(&live), Arc::clone(&history));
    let scheduler = Scheduler::spawn(
        CheckpointPolicy::default(),
        Box::new(move || checkpointed.checkpoint()),
    );

    // Signal only real filesystem activity (inside the control plane, so
    // `.vfs/*` reads never feed the trigger). The debug provider is the CasVfs
    // itself (not the SignalVfs wrapper), so `.vfs/debug` reads bypass it too.
    let debug: Arc<dyn DebugStats> = live.clone();
    let signaled: Arc<dyn VfsService> =
        Arc::new(SignalVfs::new(live, scheduler.handle(), hard_fsync));
    // The router wraps SignalVfs(CasVfs): routed traffic bypasses the overlay
    // *and* never emits checkpoint trigger signals.
    let inner = match &lower_path {
        Some(lower_dir) => route_ignored(signaled, lower_dir)?,
        None => signaled,
    };
    let portal_tier = tier.clone();
    let vfs = Arc::new(
        ControlPlaneVfs::new(inner)
            .with_feature("cas", move |slot| {
                Arc::new(CasObjectDir::new(slot, portal_tier))
            })
            .with_feature("history", move |slot| {
                Arc::new(HistoryDir::new(history, slot, lower))
            })
            .with_feature("debug", move |slot| Arc::new(DebugDir::new(slot, debug))),
    );

    mount_spike(mountpoint, vfs)?;
    // Unmounted: dropping the scheduler runs a final checkpoint if needed.
    drop(scheduler);
    Ok(())
}

/// Wrap `overlay` in a [`RouterVfs`] when `<lower_dir>/.epochignore` exists:
/// rule-matching subtrees then bypass the overlay entirely — reads and writes
/// go straight to the lower host directory (via a writable
/// [`PassthroughVfs`]) and are never captured into CAS/checkpoints. Without
/// the file, the stack is returned unchanged.
fn route_ignored(
    overlay: Arc<dyn VfsService>,
    lower_dir: &Path,
) -> Result<Arc<dyn VfsService>, Box<dyn std::error::Error>> {
    if !lower_dir.join(EPOCHIGNORE_FILE).is_file() {
        return Ok(overlay);
    }
    let rules = RouterVfs::load_rules(lower_dir)?;
    eprintln!(
        "[epoch-fuse] {EPOCHIGNORE_FILE}: {} rule(s) route straight to the lower directory",
        rules.num_ignores() + rules.num_whitelists()
    );
    let passthrough: Arc<dyn VfsService> = Arc::new(PassthroughVfs::writable(lower_dir));
    Ok(Arc::new(RouterVfs::new(overlay, passthrough, rules)))
}
