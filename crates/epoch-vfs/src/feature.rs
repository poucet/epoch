//! The control-plane **feature** (plugin) contract.
//!
//! Every entry under `.vfs/` — `cas`, `history`, `debug`, and whatever comes
//! next — is a *feature*: an ordinary [`VfsService`] serving a synthetic
//! subtree, mounted by name into [`ControlPlaneVfs`](crate::ControlPlaneVfs)
//! and owning a dedicated slice of the inode/handle id space. The control
//! plane holds a list of them and routes purely by arithmetic — no
//! per-feature special cases.
//!
//! ## Id-space slotting
//!
//! The high half of the id space (at/above
//! [`CP_BASE`](crate::control_plane::CP_BASE)) is divided into
//! [`FEATURE_RANGE`]-sized slots. Slot 0 belongs to the control plane itself
//! (the `.vfs` directory node); each feature registered via
//! [`with_feature`](crate::ControlPlaneVfs::with_feature) receives the next
//! slot as a [`FeatureSlot`]. A feature must:
//!
//! - use `slot.root_id()` as the inode id of its root directory (`.vfs/<name>`),
//! - mint every synthetic inode id **and file handle** inside
//!   `[slot.base, slot.base + FEATURE_RANGE)`,
//! - report `slot.vfs_dir_id` as its root's `..` in `readdir`.
//!
//! Ids below the feature's slot are never routed to it, so a feature can (and
//! [`HistoryDir`](crate::HistoryDir) does) wrap further sub-services freely
//! inside its own slice.

use epoch_vfs_api::{InodeId, NodeAttr, VfsError, VfsResult};
use std::time::SystemTime;

/// The size of one feature's id slice. 2^56 ids per feature leaves room for
/// 127 features above [`CP_BASE`](crate::control_plane::CP_BASE) — both
/// effectively inexhaustible.
pub const FEATURE_RANGE: u64 = 1 << 56;

/// The id slice and mount context handed to a feature at registration.
#[derive(Debug, Clone, Copy)]
pub struct FeatureSlot {
    /// First id owned by the feature; also its root directory's inode id.
    pub base: u64,
    /// The inode id of the `.vfs` directory — what the feature's root reports
    /// as `..`.
    pub vfs_dir_id: InodeId,
}

impl FeatureSlot {
    /// The feature's root directory node (`.vfs/<name>`).
    pub fn root_id(&self) -> InodeId {
        InodeId(self.base)
    }

    /// Whether `id` falls inside this feature's slice.
    pub fn contains(&self, id: u64) -> bool {
        (self.base..self.base + FEATURE_RANGE).contains(&id)
    }
}

/// Attrs of a synthetic read-only control-plane directory.
pub(crate) fn control_dir_attr(inode_id: InodeId) -> NodeAttr {
    let mut attr = NodeAttr::directory(inode_id, SystemTime::now());
    attr.perm = 0o555;
    attr
}

/// Attrs of a synthetic read-only control-plane file.
pub(crate) fn cp_file_attr(inode_id: InodeId, size: u64) -> NodeAttr {
    let mut attr = NodeAttr::regular_file(inode_id, size, SystemTime::now());
    attr.perm = 0o444;
    attr
}

/// Clamp a read to the bytes available for `[offset, offset + size)`.
pub(crate) fn slice_bytes(data: &[u8], offset: u64, size: u32) -> VfsResult<Vec<u8>> {
    let start = usize::try_from(offset)
        .map_err(|_| VfsError::InvalidInput("read offset exceeds usize".to_owned()))?;
    let size = usize::try_from(size)
        .map_err(|_| VfsError::InvalidInput("read size exceeds usize".to_owned()))?;
    if start >= data.len() {
        return Ok(Vec::new());
    }
    let end = start.saturating_add(size).min(data.len());
    Ok(data[start..end].to_vec())
}
