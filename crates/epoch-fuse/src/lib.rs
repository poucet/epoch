//! FUSE adapter for the epoch VFS spike.
//!
//! `fuser_adapter` bridges fuser's synchronous kernel callbacks to a
//! `VfsService` from `epoch-vfs-api`. The reference overlay-probe backend (the
//! `epoch_vfs::overlay_probe` decorator stack) lives in the `epoch-vfs` crate.

#[cfg(feature = "fuser-support")]
pub mod fuser_adapter;

/// macOS-only: mount through macFUSE's privileged `mount_macfuse` helper instead
/// of fuser's in-process `mount()` (which macOS 26 rejects with EPERM).
#[cfg(all(feature = "fuser-support", target_os = "macos"))]
mod macfuse;
