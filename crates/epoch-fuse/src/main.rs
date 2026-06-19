use std::{env, fs, sync::Arc};

use epoch_fuse::{OverlayProbeVfs, fuser_adapter::mount_spike};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let lower = args
        .next()
        .ok_or("usage: epoch-fuse <lower-dir> <mountpoint>")?;
    let mountpoint = args
        .next()
        .ok_or("usage: epoch-fuse <lower-dir> <mountpoint>")?;

    let lower = fs::canonicalize(lower)?;
    eprintln!(
        "[epoch-fuse] mounting lower overlay probe lower={:?} mountpoint={:?}",
        lower, mountpoint
    );
    let vfs = Arc::new(OverlayProbeVfs::new(lower));
    mount_spike(mountpoint, vfs)?;
    Ok(())
}
