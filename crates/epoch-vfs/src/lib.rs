//! Reference `VfsService` backends for the epoch VFS spike.
//!
//! `OverlayProbeVfs` reads through to a lower directory, logs every VFS call, and
//! serves a synthetic `.vfs` control plane (`.vfs/cas/<cas-id>` echoes the id).
//! Mutations return read-only errors — they mark the seams where the future upper
//! CAS/session layer will attach.

pub mod overlay_probe;

pub use overlay_probe::OverlayProbeVfs;
