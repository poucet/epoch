use std::{env, ffi::OsString, fs, path::PathBuf, sync::Arc};

use epoch_cas::{CachingByteBackend, MemoryByteBackend, TieredByteBackend};
use epoch_cas_moka::MokaByteBackend;
use epoch_cas_redb::RedbByteBackend;
use epoch_fuse::fuser_adapter::mount_spike;
use epoch_history::{CheckpointPolicy, History, Scheduler};
use epoch_timeline_redb::RedbTimelineStore;
use epoch_vfs::control_plane::CP_BASE;
use epoch_vfs::{
    overlay_probe, CasVfs, CheckpointedFs, ControlPlaneVfs, HistoryDir, PassthroughVfs, SignalVfs,
};
use epoch_vfs_api::{InodeId, VfsService};

const USAGE: &str = "usage:
  epoch-fuse <lower-dir> <mountpoint>         read-through overlay probe
  epoch-fuse cas <mountpoint> [<lower-dir>]   copy-on-write CAS filesystem
                                              (empty in-memory, or overlaying <lower-dir>)
  epoch-fuse cas <mountpoint> --state <dir>   durable, checkpointed CAS filesystem
        [--head <name>] [--hard-fsync]        (resumes from <name>'s last checkpoint;
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
            // Checkpointing is CAS-native only (no OpaqueTree yet), so a lower
            // overlay cannot be made durable — refuse the combination.
            if lower_dir.is_some() {
                return Err("--state cannot be combined with a <lower-dir>".into());
            }
            return mount_durable(mountpoint, state_dir, head, hard_fsync);
        }

        let vfs: Arc<dyn VfsService> = match lower_dir {
            Some(lower_dir) => {
                let lower_dir = fs::canonicalize(lower_dir)?;
                eprintln!(
                    "[epoch-fuse] mounting copy-on-write CAS overlay lower={lower_dir:?} mountpoint={mountpoint:?}"
                );
                let lower: Arc<dyn VfsService> = Arc::new(PassthroughVfs::new(lower_dir));
                Arc::new(CasVfs::overlay(MemoryByteBackend::new(), lower))
            }
            None => {
                eprintln!(
                    "[epoch-fuse] mounting empty in-memory CAS filesystem mountpoint={mountpoint:?}"
                );
                Arc::new(CasVfs::in_memory())
            }
        };
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
fn mount_durable(
    mountpoint: OsString,
    state_dir: PathBuf,
    head: String,
    hard_fsync: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(&state_dir)?;
    let db_path = state_dir.join("epoch.redb");
    eprintln!(
        "[epoch-fuse] mounting durable CAS filesystem state={db_path:?} head={head} hard_fsync={hard_fsync} mountpoint={mountpoint:?}"
    );

    // One shared database hosts the object table and the timeline tables.
    let db = Arc::new(redb::Database::create(&db_path)?);
    let storage = RedbByteBackend::with_database(Arc::clone(&db))?;
    let far = CachingByteBackend::new(MokaByteBackend::new(CACHE_BYTES), storage);
    let tier = TieredByteBackend::new(MemoryByteBackend::new(), far);
    let timeline = Arc::new(RedbTimelineStore::new(db)?);

    let history = Arc::new(History::new(tier.clone(), timeline, head));

    // Resume from the head's last checkpoint, or start empty.
    let live = match history.head()? {
        Some(id) => {
            let event = history
                .event(&id)?
                .ok_or("timeline head points at a missing event")?;
            eprintln!(
                "[epoch-fuse] resuming from checkpoint {id} (t={}ms)",
                event.timestamp_ms
            );
            Arc::new(CasVfs::from_root(tier.clone(), event.root)?)
        }
        None => Arc::new(CasVfs::new(tier.clone())),
    };

    let checkpointed = CheckpointedFs::new(Arc::clone(&live), Arc::clone(&history));
    let scheduler = Scheduler::spawn(
        CheckpointPolicy::default(),
        Box::new(move || checkpointed.checkpoint()),
    );

    // Signal only real filesystem activity (inside the control plane, so
    // `.vfs/*` reads never feed the trigger).
    let signaled: Arc<dyn VfsService> =
        Arc::new(SignalVfs::new(live, scheduler.handle(), hard_fsync));
    let history_dir = Arc::new(HistoryDir::new(history, InodeId(CP_BASE)));
    let vfs = Arc::new(ControlPlaneVfs::with_history(signaled, history_dir));

    mount_spike(mountpoint, vfs)?;
    // Unmounted: dropping the scheduler runs a final checkpoint if needed.
    drop(scheduler);
    Ok(())
}
