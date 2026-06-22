//! Composable [`VfsService`] decorators for the epoch VFS spike.
//!
//! The overlay-probe backend is a thin stack of single-responsibility
//! decorators rather than one monolith. From the bottom up:
//!
//! 1. [`PassthroughVfs`] — read-through to a lower host directory; owns the
//!    [`NodeId`](epoch_vfs_api::NodeId) ↔ path and
//!    [`FileHandle`](epoch_vfs_api::FileHandle) ↔ path maps. Read-only:
//!    mutations return [`ReadOnlyFilesystem`](epoch_vfs_api::VfsError::ReadOnlyFilesystem),
//!    marking the seam for a future writable upper layer.
//! 2. [`ControlPlaneVfs`] — grafts the synthetic `.vfs` control plane
//!    (`.vfs/cas/<id>` echoes the id) onto the root, delegating everything else.
//!    It partitions the id space so its synthetic nodes never collide with the
//!    layer below (see [`control_plane`]).
//! 3. [`LoggingVfs`] — logs every call, then delegates unchanged.
//!
//! [`overlay_probe`] composes all three into the behavior the old
//! `OverlayProbeVfs` provided: `LoggingVfs(ControlPlaneVfs(PassthroughVfs))`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{path::PathBuf, sync::Arc};

use epoch_vfs_api::VfsService;

pub mod control_plane;
pub mod logging;
pub mod passthrough;

#[cfg(test)]
mod test_support;

pub use control_plane::ControlPlaneVfs;
pub use logging::LoggingVfs;
pub use passthrough::PassthroughVfs;

/// Compose the overlay-probe decorator stack over a lower directory.
///
/// The returned service behaves like the original monolithic `OverlayProbeVfs`:
/// it reads through to `lower`, serves the synthetic `.vfs` control plane, and
/// logs every call — assembled as
/// `LoggingVfs(ControlPlaneVfs(PassthroughVfs(lower)))`.
pub fn overlay_probe(lower: PathBuf) -> Arc<dyn VfsService> {
    let passthrough = Arc::new(PassthroughVfs::new(lower));
    let control_plane = Arc::new(ControlPlaneVfs::new(passthrough));
    Arc::new(LoggingVfs::new(control_plane))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use epoch_vfs_api::{NodeKind, OpenMode, OpenRequest, ROOT_NODE_ID, ReadRequest};
    use std::{
        ffi::{OsStr, OsString},
        fs,
    };

    #[test]
    fn full_stack_serves_passthrough_and_control_plane() {
        let dir = TempDir::new("overlay-probe");
        fs::write(dir.path().join("doc.txt"), b"contents").expect("write file");

        let vfs = overlay_probe(dir.path().to_path_buf());

        // Passthrough file works through the whole stack.
        let entry = vfs
            .lookup(ROOT_NODE_ID, OsStr::new("doc.txt"))
            .expect("passthrough lookup");
        assert_eq!(entry.attr.kind, NodeKind::RegularFile);
        let opened = vfs
            .open(OpenRequest {
                node_id: entry.attr.node_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("passthrough open");
        let data = vfs
            .read(ReadRequest {
                node_id: entry.attr.node_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("passthrough read");
        assert_eq!(data, b"contents");
        vfs.release(opened.handle).expect("release");

        // Root listing surfaces both the passthrough file and the `.vfs` plane.
        let root_names: Vec<_> = vfs
            .readdir(ROOT_NODE_ID)
            .expect("root readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(root_names.contains(&OsString::from("doc.txt")));
        assert!(root_names.contains(&OsString::from(".vfs")));

        // `.vfs/cas/<id>` is served synthetically and echoes the id.
        let vfs_dir = vfs.lookup(ROOT_NODE_ID, OsStr::new(".vfs")).expect(".vfs");
        let cas_dir = vfs
            .lookup(vfs_dir.attr.node_id, OsStr::new("cas"))
            .expect(".vfs/cas");
        let cas_entry = vfs
            .lookup(cas_dir.attr.node_id, OsStr::new("deadbeef"))
            .expect(".vfs/cas/deadbeef");
        assert_eq!(cas_entry.attr.kind, NodeKind::RegularFile);

        let cas_opened = vfs
            .open(OpenRequest {
                node_id: cas_entry.attr.node_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("cas open");
        let cas_data = vfs
            .read(ReadRequest {
                node_id: cas_entry.attr.node_id,
                handle: cas_opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("cas read");
        assert_eq!(cas_data, b"deadbeef\n");
        vfs.release(cas_opened.handle).expect("cas release");
    }
}
