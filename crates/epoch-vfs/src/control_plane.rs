//! Synthetic `.vfs` control-plane decorator — a **feature registry**.
//!
//! [`ControlPlaneVfs`] wraps an `inner: Arc<dyn VfsService>` and grafts the
//! synthetic `.vfs` directory onto its root. Everything *inside* `.vfs` is a
//! pluggable **feature** (see [`crate::feature`]): an ordinary [`VfsService`]
//! subtree registered by name via [`with_feature`](ControlPlaneVfs::with_feature)
//! and owning a dedicated [`FEATURE_RANGE`]-sized slice of the id space. The
//! standard mount registers [`CasEchoDir`](crate::CasEchoDir) (`.vfs/cas`),
//! [`HistoryDir`](crate::HistoryDir) (`.vfs/history`), and
//! [`DebugDir`](crate::DebugDir) (`.vfs/debug`), but the control plane itself
//! knows nothing about any of them.
//!
//! Everything outside `.vfs` is delegated unchanged to `inner`.
//!
//! ## Id-space partitioning
//!
//! Every id at/above [`CP_BASE`] (`0x8000_0000_0000_0000`) is synthetic;
//! everything below delegates to the wrapped inner service. The synthetic
//! half is divided into [`FEATURE_RANGE`] slices: slice 0 holds only the
//! `.vfs` directory node itself ([`VFS_NODE_ID`]); registered features get
//! slices 1, 2, … in registration order, so routing an id (or file handle) to
//! its feature is pure arithmetic.

use std::{
    ffi::{OsStr, OsString},
    sync::Arc,
};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind,
    OpenRequest, OpenedFile, ROOT_INODE_ID, ReadRequest, VfsError, VfsResult, VfsService,
    WriteRequest,
};

use crate::feature::{FEATURE_RANGE, FeatureSlot, control_dir_attr};

/// Name of the synthetic control-plane directory at the mount root.
const CONTROL_DIR: &str = ".vfs";

/// First [`InodeId`]/[`FileHandle`] value owned by the control plane.
///
/// Any id `>= CP_BASE` is handled synthetically; anything below delegates to the
/// wrapped inner service.
pub const CP_BASE: u64 = 0x8000_0000_0000_0000;

/// Reserved node id of the `.vfs` directory (slice 0 holds nothing else).
pub(crate) const VFS_NODE_ID: InodeId = InodeId(CP_BASE);

/// Returns `true` if `id` belongs to the control plane's reserved high range.
fn is_cp(id: u64) -> bool {
    id >= CP_BASE
}

/// A registered `.vfs/<name>` feature.
struct Feature {
    name: &'static str,
    slot: FeatureSlot,
    service: Arc<dyn VfsService>,
}

/// `VfsService` decorator that owns `.vfs` and routes to registered features.
pub struct ControlPlaneVfs {
    inner: Arc<dyn VfsService>,
    features: Vec<Feature>,
}

impl std::fmt::Debug for ControlPlaneVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneVfs")
            .field(
                "features",
                &self.features.iter().map(|f| f.name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl ControlPlaneVfs {
    /// Wrap `inner`, grafting an (initially empty) `.vfs` control plane onto
    /// its root. Register features with [`with_feature`](Self::with_feature).
    pub fn new(inner: Arc<dyn VfsService>) -> Self {
        Self {
            inner,
            features: Vec::new(),
        }
    }

    /// Mount a feature at `.vfs/<name>` (consuming builder).
    ///
    /// `build` receives the feature's [`FeatureSlot`] — its id slice and the
    /// `.vfs` directory id to report as `..` — and returns the service. The
    /// service's root directory must be `slot.root_id()`, and every synthetic
    /// id/handle it mints must stay inside its slice.
    pub fn with_feature(
        mut self,
        name: &'static str,
        build: impl FnOnce(FeatureSlot) -> Arc<dyn VfsService>,
    ) -> Self {
        let slot = FeatureSlot {
            // Slice 0 is the control plane's own; features start at slice 1.
            base: CP_BASE + (self.features.len() as u64 + 1) * FEATURE_RANGE,
            vfs_dir_id: VFS_NODE_ID,
        };
        self.features.push(Feature {
            name,
            slot,
            service: build(slot),
        });
        self
    }

    /// The feature owning `id`'s slice, if any. Slice 0 (the `.vfs` node) and
    /// slices past the registry yield `None`.
    fn feature_of(&self, id: u64) -> Option<&Feature> {
        if !is_cp(id) {
            return None;
        }
        let slice = (id - CP_BASE) / FEATURE_RANGE;
        match slice {
            0 => None,
            n => self.features.get(n as usize - 1),
        }
    }

    /// Route by feature slice, with fallbacks for the `.vfs` node itself and
    /// the inner service.
    fn route(&self, id: u64) -> Route<'_> {
        match self.feature_of(id) {
            Some(feature) => Route::Feature(feature),
            None if is_cp(id) => Route::ControlPlane,
            None => Route::Inner,
        }
    }
}

enum Route<'a> {
    /// The id belongs to a registered feature's slice.
    Feature(&'a Feature),
    /// The id is in slice 0 — the `.vfs` directory node (or unassigned).
    ControlPlane,
    /// The id belongs to the wrapped inner service.
    Inner,
}

impl VfsService for ControlPlaneVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        // `.vfs` at the mount root.
        if parent == ROOT_INODE_ID && name == OsStr::new(CONTROL_DIR) {
            return Ok(Entry::new(control_dir_attr(VFS_NODE_ID)));
        }
        match self.route(parent.0) {
            Route::Feature(feature) => feature.service.lookup(parent, name),
            Route::ControlPlane => {
                if parent != VFS_NODE_ID {
                    return Err(VfsError::NotFound);
                }
                // `.vfs/<feature>`: hand the feature its own root's entry.
                let feature = self
                    .features
                    .iter()
                    .find(|feature| name == OsStr::new(feature.name))
                    .ok_or(VfsError::NotFound)?;
                feature
                    .service
                    .getattr(feature.slot.root_id())
                    .map(Entry::new)
            }
            Route::Inner => self.inner.lookup(parent, name),
        }
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        match self.route(inode_id.0) {
            Route::Feature(feature) => feature.service.getattr(inode_id),
            Route::ControlPlane if inode_id == VFS_NODE_ID => Ok(control_dir_attr(inode_id)),
            Route::ControlPlane => Err(VfsError::NotFound),
            Route::Inner => self.inner.getattr(inode_id),
        }
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == ROOT_INODE_ID {
            // Pass the real root listing through, then surface `.vfs`.
            let mut entries = self.inner.readdir(ROOT_INODE_ID)?;
            // The synthetic control plane shadows any lower entry of the same name.
            entries.retain(|entry| entry.name != OsStr::new(CONTROL_DIR));
            entries.push(DirEntry::new(
                VFS_NODE_ID,
                OsString::from(CONTROL_DIR),
                NodeKind::Directory,
            ));
            return Ok(entries);
        }
        match self.route(inode_id.0) {
            Route::Feature(feature) => feature.service.readdir(inode_id),
            Route::ControlPlane if inode_id == VFS_NODE_ID => {
                // `.vfs` lists exactly the registered features.
                let mut entries = vec![
                    DirEntry::new(VFS_NODE_ID, OsString::from("."), NodeKind::Directory),
                    DirEntry::new(ROOT_INODE_ID, OsString::from(".."), NodeKind::Directory),
                ];
                entries.extend(self.features.iter().map(|feature| {
                    DirEntry::new(
                        feature.slot.root_id(),
                        OsString::from(feature.name),
                        NodeKind::Directory,
                    )
                }));
                Ok(entries)
            }
            Route::ControlPlane => Err(VfsError::NotFound),
            Route::Inner => self.inner.readdir(inode_id),
        }
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        match self.route(request.inode_id.0) {
            Route::Feature(feature) => feature.service.open(request),
            Route::ControlPlane if request.inode_id == VFS_NODE_ID => Err(VfsError::IsDirectory),
            Route::ControlPlane => Err(VfsError::NotFound),
            Route::Inner => self.inner.open(request),
        }
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        match self.route(request.inode_id.0) {
            Route::Feature(feature) => feature.service.read(request),
            Route::ControlPlane if request.inode_id == VFS_NODE_ID => Err(VfsError::IsDirectory),
            Route::ControlPlane => Err(VfsError::NotFound),
            Route::Inner => self.inner.read(request),
        }
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        match self.route(request.inode_id.0) {
            Route::Feature(feature) => feature.service.write(request),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.write(request),
        }
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        match self.route(request.parent.0) {
            Route::Feature(feature) => feature.service.create(request),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.create(request),
        }
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        match self.route(request.parent.0) {
            Route::Feature(feature) => feature.service.mkdir(request),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.mkdir(request),
        }
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        match self.route(parent.0) {
            Route::Feature(feature) => feature.service.unlink(parent, name),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.unlink(parent, name),
        }
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        match self.route(parent.0) {
            Route::Feature(feature) => feature.service.rmdir(parent, name),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.rmdir(parent, name),
        }
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        match self.route(inode_id.0) {
            Route::Feature(feature) => feature.service.truncate(inode_id, size),
            Route::ControlPlane => Err(VfsError::ReadOnlyFilesystem),
            Route::Inner => self.inner.truncate(inode_id, size),
        }
    }

    fn flush(&self, handle: FileHandle) -> VfsResult<()> {
        match self.route(handle.0) {
            Route::Feature(feature) => feature.service.flush(handle),
            Route::ControlPlane => Ok(()),
            Route::Inner => self.inner.flush(handle),
        }
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        match self.route(handle.0) {
            Route::Feature(feature) => feature.service.fsync(inode_id, handle, datasync),
            // Synthetic nodes are read-only; nothing to make durable.
            Route::ControlPlane => Ok(()),
            Route::Inner => self.inner.fsync(inode_id, handle, datasync),
        }
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        match self.route(handle.0) {
            Route::Feature(feature) => feature.service.release(handle),
            // Synthetic handles carry no state to drop.
            Route::ControlPlane => Ok(()),
            Route::Inner => self.inner.release(handle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas_dir::CasEchoDir;
    use crate::test_support::StubVfs;
    use epoch_vfs_api::{OpenMode, ReadRequest};

    /// The standard test composition: a control plane with the cas feature.
    fn with_cas(inner: Arc<dyn VfsService>) -> ControlPlaneVfs {
        ControlPlaneVfs::new(inner).with_feature("cas", |slot| Arc::new(CasEchoDir::new(slot)))
    }

    fn lookup_id(vfs: &ControlPlaneVfs, parent: InodeId, name: &str) -> InodeId {
        vfs.lookup(parent, OsStr::new(name)).unwrap().attr.inode_id
    }

    #[test]
    fn vfs_lookup_readdir_read_echo() {
        let cp = with_cas(Arc::new(StubVfs::with_root_entries(&[])));

        // `.vfs` is discoverable at the root.
        let vfs = cp.lookup(ROOT_INODE_ID, OsStr::new(CONTROL_DIR)).expect(".vfs");
        assert_eq!(vfs.attr.inode_id, VFS_NODE_ID);
        assert_eq!(vfs.attr.kind, NodeKind::Directory);

        let root_names: Vec<_> = cp
            .readdir(ROOT_INODE_ID)
            .expect("root readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(root_names.contains(&OsString::from(CONTROL_DIR)));

        // `.vfs` lists exactly the registered features (plus `.`/`..`).
        let vfs_names: Vec<_> = cp
            .readdir(VFS_NODE_ID)
            .expect(".vfs readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(vfs_names.contains(&OsString::from("cas")));
        assert!(!vfs_names.contains(&OsString::from("nope")));
        assert_eq!(cp.lookup(VFS_NODE_ID, OsStr::new("nope")).unwrap_err(), VfsError::NotFound);

        // `.vfs/cas` is not listable: only `.`/`..`.
        let cas_dir = lookup_id(&cp, VFS_NODE_ID, "cas");
        assert_eq!(cp.readdir(cas_dir).expect(".vfs/cas readdir").len(), 2);

        // `.vfs/cas/<id>` echoes its id, newline-terminated.
        let entry = cp.lookup(cas_dir, OsStr::new("abc123")).expect("cas lookup");
        assert_eq!(entry.attr.kind, NodeKind::RegularFile);
        assert_eq!(entry.attr.size, b"abc123\n".len() as u64);
        assert!(is_cp(entry.attr.inode_id.0));

        let opened = cp
            .open(OpenRequest {
                inode_id: entry.attr.inode_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("cas open");
        assert!(is_cp(opened.handle.0));
        let data = cp
            .read(ReadRequest {
                inode_id: entry.attr.inode_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("cas read");
        assert_eq!(data, b"abc123\n");
        cp.release(opened.handle).expect("cas release");
    }

    #[test]
    fn non_vfs_calls_delegate() {
        let stub = Arc::new(StubVfs::with_root_entries(&[("file.txt", b"payload")]));
        let cp = with_cas(stub.clone());

        let entry = cp
            .lookup(ROOT_INODE_ID, OsStr::new("file.txt"))
            .expect("delegate lookup");
        let inner_id = entry.attr.inode_id;
        assert!(!is_cp(inner_id.0));

        let attr = cp.getattr(inner_id).expect("delegate getattr");
        assert_eq!(attr.size, b"payload".len() as u64);

        let opened = cp
            .open(OpenRequest {
                inode_id: inner_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("delegate open");
        let data = cp
            .read(ReadRequest {
                inode_id: inner_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("delegate read");
        assert_eq!(data, b"payload");

        let names: Vec<_> = cp
            .readdir(ROOT_INODE_ID)
            .expect("root readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&OsString::from("file.txt")));
        assert!(names.contains(&OsString::from(CONTROL_DIR)));
    }

    #[test]
    fn features_get_disjoint_slices_and_ids_do_not_collide() {
        let stub = Arc::new(StubVfs::with_root_entries(&[("a", b"a")]));
        let cp = ControlPlaneVfs::new(stub)
            .with_feature("cas", |slot| Arc::new(CasEchoDir::new(slot)))
            .with_feature("cas2", |slot| Arc::new(CasEchoDir::new(slot)));

        let inner_a = lookup_id(&cp, ROOT_INODE_ID, "a");
        let cas = lookup_id(&cp, VFS_NODE_ID, "cas");
        let cas2 = lookup_id(&cp, VFS_NODE_ID, "cas2");
        assert!(!is_cp(inner_a.0));
        assert_eq!(cas.0, CP_BASE + FEATURE_RANGE);
        assert_eq!(cas2.0, CP_BASE + 2 * FEATURE_RANGE);

        // Dynamic ids minted by each feature stay inside its own slice, and
        // the same name resolves stably.
        let x = lookup_id(&cp, cas, "x");
        let x2 = lookup_id(&cp, cas2, "x");
        assert_ne!(x, x2);
        assert!((cas.0..cas.0 + FEATURE_RANGE).contains(&x.0));
        assert!((cas2.0..cas2.0 + FEATURE_RANGE).contains(&x2.0));
        assert_eq!(lookup_id(&cp, cas, "x"), x);
    }

    #[test]
    fn mutations_on_cp_nodes_are_read_only() {
        let cp = with_cas(Arc::new(StubVfs::with_root_entries(&[])));
        let cas_dir = lookup_id(&cp, VFS_NODE_ID, "cas");

        assert_eq!(
            cp.mkdir(CreateRequest::directory(VFS_NODE_ID, "x", 0o755)),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            cp.create(CreateRequest::file(cas_dir, "x", 0o644)),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            cp.unlink(VFS_NODE_ID, OsStr::new("x")),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            cp.rmdir(VFS_NODE_ID, OsStr::new("x")),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            cp.truncate(VFS_NODE_ID, 0),
            Err(VfsError::ReadOnlyFilesystem)
        );
    }
}
