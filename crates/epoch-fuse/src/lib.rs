//! Thin overlay-FUSE spike components.
//!
//! `OverlayProbeVfs` reads through to a lower directory and logs every async VFS
//! call. Mutations intentionally return read-only errors for now; they mark the
//! exact places where the future upper CAS/session layer will attach.

pub mod overlay_probe;

#[cfg(feature = "fuser-support")]
pub mod fuser_adapter;

pub use overlay_probe::OverlayProbeVfs;
