use std::{env, fs};

use epoch_fuse::fuser_adapter::mount_spike;
use epoch_vfs::overlay_probe;

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
    let vfs = overlay_probe(lower);
    mount_spike(mountpoint, vfs)?;
    Ok(())
}
