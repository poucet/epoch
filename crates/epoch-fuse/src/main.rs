use std::{env, fs, sync::Arc};

use epoch_cas::MemoryByteBackend;
use epoch_fuse::fuser_adapter::mount_spike;
use epoch_vfs::{overlay_probe, CasVfs, PassthroughVfs};
use epoch_vfs_api::VfsService;

const USAGE: &str = "usage:
  epoch-fuse <lower-dir> <mountpoint>         read-through overlay probe
  epoch-fuse cas <mountpoint> [<lower-dir>]   copy-on-write CAS filesystem
                                              (empty in-memory, or overlaying <lower-dir>)";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let first = args.next().ok_or(USAGE)?;

    // `cas` mode: a copy-on-write CAS filesystem, optionally overlaying a lower
    // host directory. Everything untouched falls through to the lower; writes,
    // creates and deletes are copied up / whiteout-ed into the in-memory CAS.
    if first.to_str() == Some("cas") {
        let mountpoint = args.next().ok_or(USAGE)?;
        let vfs: Arc<dyn VfsService> = match args.next() {
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
