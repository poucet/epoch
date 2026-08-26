//! Synthetic `.vfs` control-plane decorator.
//!
//! [`ControlPlaneVfs`] wraps an `inner: Arc<dyn VfsService>` and grafts a small
//! synthetic subtree onto it at the mount root:
//!
//! - `.vfs` — a read-only directory that surfaces the control plane.
//! - `.vfs/cas` — a read-only directory that is intentionally **not listable**:
//!   content-addressed entries are addressable by id, not enumerable.
//! - `.vfs/cas/<id>` — a synthetic read-only file whose contents are `<id>\n`.
//! - `.vfs/debug` — when configured via [`with_debug`](ControlPlaneVfs::with_debug),
//!   a listable read-only directory of live statistics
//!   (`near-objects`, `dirty-files`, `overlay`), each rendered fresh from the
//!   provider's [`DebugSnapshot`](crate::debug::DebugSnapshot) on every access.
//!
//! Everything outside `.vfs` is delegated unchanged to `inner`.
//!
//! ## Id-space partitioning
//!
//! So the control plane's nodes never collide with the inner service's, this
//! decorator owns a high range of the [`InodeId`]/[`FileHandle`] space: every id
//! at or above [`CP_BASE`] (`0x8000_0000_0000_0000`) belongs to the control
//! plane, and every id below it is delegated to `inner`. The two fixed entries
//! get reserved ids ([`VFS_NODE_ID`] for `.vfs`, [`CAS_NODE_ID`] for
//! `.vfs/cas`); each looked-up `.vfs/cas/<id>` is assigned a fresh high-range
//! [`InodeId`] from a counter, recorded in a map so the id can be resolved back
//! to its content. Synthetic file handles are minted from the same high range.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind, OpenMode,
    OpenRequest, OpenedFile, ROOT_INODE_ID, ReadRequest, VfsError, VfsResult, VfsService,
    WriteRequest,
};

use crate::debug::DebugStats;
use crate::history_dir::{HIST_ROOT_ID, is_hist};

/// Name of the synthetic control-plane directory at the mount root.
const CONTROL_DIR: &str = ".vfs";
/// Name of the content-addressed-store directory inside the control plane.
const CAS_DIR: &str = "cas";
/// Name of the time-travel directory inside the control plane.
const HISTORY_DIR: &str = "history";
/// Name of the live-statistics directory inside the control plane.
const DEBUG_DIR: &str = "debug";

/// First [`InodeId`]/[`FileHandle`] value owned by the control plane.
///
/// Any id `>= CP_BASE` is handled synthetically; anything below delegates to the
/// wrapped inner service.
pub const CP_BASE: u64 = 0x8000_0000_0000_0000;

/// Reserved node id of the `.vfs` directory.
pub(crate) const VFS_NODE_ID: InodeId = InodeId(CP_BASE);
/// Reserved node id of the `.vfs/cas` directory.
const CAS_NODE_ID: InodeId = InodeId(CP_BASE + 1);
/// Reserved node id of the `.vfs/debug` directory.
const DEBUG_NODE_ID: InodeId = InodeId(CP_BASE + 2);
/// Reserved node id of the `.vfs/debug/near-objects` file.
const DEBUG_NEAR_ID: InodeId = InodeId(CP_BASE + 3);
/// Reserved node id of the `.vfs/debug/dirty-files` file.
const DEBUG_DIRTY_ID: InodeId = InodeId(CP_BASE + 4);
/// Reserved node id of the `.vfs/debug/overlay` file.
const DEBUG_OVERLAY_ID: InodeId = InodeId(CP_BASE + 5);
/// First mintable id for `.vfs/cas/<id>` entries and synthetic file handles.
const FIRST_DYNAMIC: u64 = CP_BASE + 6;

/// The three `.vfs/debug` files, as `(name, reserved node id)` pairs — the
/// single source for lookup, readdir, and content routing.
const DEBUG_FILES: [(&str, InodeId); 3] = [
    ("near-objects", DEBUG_NEAR_ID),
    ("dirty-files", DEBUG_DIRTY_ID),
    ("overlay", DEBUG_OVERLAY_ID),
];

/// Returns `true` if `id` is one of the reserved `.vfs/debug/<file>` nodes.
fn is_debug_file(id: InodeId) -> bool {
    DEBUG_FILES.iter().any(|&(_, file_id)| file_id == id)
}

/// Returns `true` if `id` belongs to the control plane's reserved high range.
fn is_cp(id: u64) -> bool {
    id >= CP_BASE
}

/// `VfsService` decorator that owns the synthetic `.vfs/*` subtree.
pub struct ControlPlaneVfs {
    inner: Arc<dyn VfsService>,
    state: RwLock<CasState>,
    next_id: AtomicU64,
    /// The `.vfs/history` sub-service (a [`crate::HistoryDir`]), when the
    /// mount has a timeline. It owns every id at/above
    /// [`HIST_BASE`](crate::history_dir::HIST_BASE) — a sub-range of the
    /// control plane's — so routing checks `is_hist` before `is_cp`.
    history: Option<Arc<dyn VfsService>>,
    /// The `.vfs/debug` statistics provider (in practice the live
    /// [`CasVfs`](crate::CasVfs)), when configured via
    /// [`with_debug`](Self::with_debug). Without one, `.vfs/debug` does not
    /// exist.
    debug: Option<Arc<dyn DebugStats>>,
}

impl std::fmt::Debug for ControlPlaneVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneVfs")
            .field("state", &self.state)
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

/// Maps each looked-up `.vfs/cas/<id>` to and from its synthetic [`InodeId`].
#[derive(Debug, Default)]
struct CasState {
    /// Synthetic node id → the cas id string it addresses.
    id_to_cas: HashMap<InodeId, OsString>,
    /// cas id string → its assigned synthetic node id (for lookup idempotence).
    cas_to_id: HashMap<OsString, InodeId>,
}

impl ControlPlaneVfs {
    /// Wrap `inner`, grafting the synthetic `.vfs` control plane onto its root.
    pub fn new(inner: Arc<dyn VfsService>) -> Self {
        Self {
            inner,
            state: RwLock::new(CasState::default()),
            next_id: AtomicU64::new(FIRST_DYNAMIC),
            history: None,
            debug: None,
        }
    }

    /// Like [`new`](Self::new), additionally grafting `history` (a
    /// [`crate::HistoryDir`]) at `.vfs/history`.
    pub fn with_history(inner: Arc<dyn VfsService>, history: Arc<dyn VfsService>) -> Self {
        Self {
            history: Some(history),
            ..Self::new(inner)
        }
    }

    /// Additionally serve `.vfs/debug` from `debug`'s live statistics
    /// (consuming builder, orthogonal to [`with_history`](Self::with_history)).
    pub fn with_debug(self, debug: Arc<dyn DebugStats>) -> Self {
        Self {
            debug: Some(debug),
            ..self
        }
    }

    /// The history sub-service, or `NotFound` where a caller reached a
    /// history id without one configured (impossible in practice: only the
    /// sub-service mints those ids).
    fn history(&self) -> VfsResult<&Arc<dyn VfsService>> {
        self.history.as_ref().ok_or(VfsError::NotFound)
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, CasState>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("control plane read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, CasState>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("control plane write lock poisoned".to_owned()))
    }

    /// Assign (or reuse) a synthetic high-range node id for cas id `name`.
    fn cas_node_id(&self, name: &OsStr) -> VfsResult<InodeId> {
        if let Some(inode_id) = self.read_state()?.cas_to_id.get(name) {
            return Ok(*inode_id);
        }
        let mut state = self.write_state()?;
        // Re-check under the write lock to avoid a race minting two ids.
        if let Some(inode_id) = state.cas_to_id.get(name) {
            return Ok(*inode_id);
        }
        let inode_id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.cas_to_id.insert(name.to_owned(), inode_id);
        state.id_to_cas.insert(inode_id, name.to_owned());
        Ok(inode_id)
    }

    /// The cas id string a synthetic node id addresses, if any.
    fn cas_name(&self, inode_id: InodeId) -> VfsResult<Option<OsString>> {
        Ok(self.read_state()?.id_to_cas.get(&inode_id).cloned())
    }

    /// Mint a synthetic file handle in the control plane's high range.
    fn cp_handle(&self) -> FileHandle {
        FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The debug provider, or `NotFound` where a caller reached a debug id
    /// without one configured (impossible in practice: without a provider the
    /// debug nodes are never handed out).
    fn debug(&self) -> VfsResult<&Arc<dyn DebugStats>> {
        self.debug.as_ref().ok_or(VfsError::NotFound)
    }

    /// Render the current contents of the `.vfs/debug` file `inode_id` — a
    /// fresh snapshot per call, so `getattr` sizes and `read` bytes both track
    /// the live state (procfs-style; a read racing a mutation may see a size
    /// from a slightly older snapshot).
    fn debug_content(&self, inode_id: InodeId) -> VfsResult<Vec<u8>> {
        let snapshot = self.debug()?.debug_snapshot();
        let text = match inode_id {
            DEBUG_NEAR_ID => match snapshot.near_objects {
                Some(count) => format!("{count}\n"),
                // The backend cannot count its staging tier cheaply.
                None => "unknown\n".to_owned(),
            },
            DEBUG_DIRTY_ID => format!("{}\n", snapshot.dirty_files),
            DEBUG_OVERLAY_ID => snapshot
                .overlay_paths
                .iter()
                .map(|path| format!("{path}\n"))
                .collect(),
            _ => return Err(VfsError::NotFound),
        };
        Ok(text.into_bytes())
    }
}

impl VfsService for ControlPlaneVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        // `.vfs` at the mount root.
        if parent == ROOT_INODE_ID && name == OsStr::new(CONTROL_DIR) {
            return Ok(Entry::new(control_dir_attr(VFS_NODE_ID)));
        }

        // History ids (including `.vfs/history` itself) route to the
        // sub-service, which owns everything below that directory.
        if is_hist(parent.0) {
            return self.history()?.lookup(parent, name);
        }

        // Resolve within an existing control-plane node.
        if is_cp(parent.0) {
            if parent == VFS_NODE_ID {
                if name == OsStr::new(CAS_DIR) {
                    return Ok(Entry::new(control_dir_attr(CAS_NODE_ID)));
                }
                if self.history.is_some() && name == OsStr::new(HISTORY_DIR) {
                    return self.history()?.getattr(HIST_ROOT_ID).map(Entry::new);
                }
                if self.debug.is_some() && name == OsStr::new(DEBUG_DIR) {
                    return Ok(Entry::new(control_dir_attr(DEBUG_NODE_ID)));
                }
                // `.vfs/<other>` is not a defined control entry yet.
                return Err(VfsError::NotFound);
            }
            if parent == DEBUG_NODE_ID {
                let (_, inode_id) = DEBUG_FILES
                    .iter()
                    .find(|(file_name, _)| name == OsStr::new(file_name))
                    .ok_or(VfsError::NotFound)?;
                let size = self.debug_content(*inode_id)?.len() as u64;
                return Ok(Entry::new(cp_file_attr(*inode_id, size)));
            }
            if parent == CAS_NODE_ID {
                let inode_id = self.cas_node_id(name)?;
                let size = cas_content(name).len() as u64;
                return Ok(Entry::new(cp_file_attr(inode_id, size)));
            }
            // cas ids and any other control node have no children.
            return Err(VfsError::NotFound);
        }

        self.inner.lookup(parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == VFS_NODE_ID || inode_id == CAS_NODE_ID || inode_id == DEBUG_NODE_ID {
            return Ok(control_dir_attr(inode_id));
        }
        if is_hist(inode_id.0) {
            return self.history()?.getattr(inode_id);
        }
        if is_debug_file(inode_id) {
            return Ok(cp_file_attr(
                inode_id,
                self.debug_content(inode_id)?.len() as u64,
            ));
        }
        if is_cp(inode_id.0) {
            let name = self.cas_name(inode_id)?.ok_or(VfsError::NotFound)?;
            return Ok(cp_file_attr(inode_id, cas_content(&name).len() as u64));
        }
        self.inner.getattr(inode_id)
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

        if inode_id == VFS_NODE_ID {
            // `.vfs` exposes only the feature directories it implements.
            let mut entries = vec![
                DirEntry::new(VFS_NODE_ID, OsString::from("."), NodeKind::Directory),
                DirEntry::new(ROOT_INODE_ID, OsString::from(".."), NodeKind::Directory),
                DirEntry::new(CAS_NODE_ID, OsString::from(CAS_DIR), NodeKind::Directory),
            ];
            if self.history.is_some() {
                entries.push(DirEntry::new(
                    HIST_ROOT_ID,
                    OsString::from(HISTORY_DIR),
                    NodeKind::Directory,
                ));
            }
            if self.debug.is_some() {
                entries.push(DirEntry::new(
                    DEBUG_NODE_ID,
                    OsString::from(DEBUG_DIR),
                    NodeKind::Directory,
                ));
            }
            return Ok(entries);
        }

        if is_hist(inode_id.0) {
            return self.history()?.readdir(inode_id);
        }

        if inode_id == DEBUG_NODE_ID {
            // Unlike `.vfs/cas`, the debug files are a fixed set: list them.
            self.debug()?;
            let mut entries = vec![
                DirEntry::new(DEBUG_NODE_ID, OsString::from("."), NodeKind::Directory),
                DirEntry::new(VFS_NODE_ID, OsString::from(".."), NodeKind::Directory),
            ];
            entries.extend(DEBUG_FILES.iter().map(|&(name, file_id)| {
                DirEntry::new(file_id, OsString::from(name), NodeKind::RegularFile)
            }));
            return Ok(entries);
        }

        if inode_id == CAS_NODE_ID {
            // Intentionally not listable: cas entries are addressable by id only.
            return Ok(vec![
                DirEntry::new(CAS_NODE_ID, OsString::from("."), NodeKind::Directory),
                DirEntry::new(VFS_NODE_ID, OsString::from(".."), NodeKind::Directory),
            ]);
        }

        if is_cp(inode_id.0) {
            // A cas entry is a file, not a directory.
            return Err(VfsError::NotDirectory);
        }

        self.inner.readdir(inode_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if is_hist(request.inode_id.0) {
            return self.history()?.open(request);
        }
        if is_cp(request.inode_id.0) {
            if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
                return Err(VfsError::ReadOnlyFilesystem);
            }
            if request.inode_id == VFS_NODE_ID
                || request.inode_id == CAS_NODE_ID
                || request.inode_id == DEBUG_NODE_ID
            {
                return Err(VfsError::IsDirectory);
            }
            if is_debug_file(request.inode_id) {
                // A debug file: verify the provider exists, then hand out a
                // synthetic read-only handle (content renders fresh per read).
                self.debug()?;
            } else {
                // A synthetic cas entry: register a synthetic read-only handle.
                self.cas_name(request.inode_id)?.ok_or(VfsError::NotFound)?;
            }
            return Ok(OpenedFile {
                handle: self.cp_handle(),
                direct_io: false,
                keep_cache: false,
            });
        }
        self.inner.open(request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if is_hist(request.inode_id.0) {
            return self.history()?.read(request);
        }
        if is_cp(request.inode_id.0) {
            if request.inode_id == VFS_NODE_ID
                || request.inode_id == CAS_NODE_ID
                || request.inode_id == DEBUG_NODE_ID
            {
                return Err(VfsError::IsDirectory);
            }
            if is_debug_file(request.inode_id) {
                let content = self.debug_content(request.inode_id)?;
                return slice_bytes(&content, request.offset, request.size);
            }
            let name = self.cas_name(request.inode_id)?.ok_or(VfsError::NotFound)?;
            return slice_bytes(&cas_content(&name), request.offset, request.size);
        }
        self.inner.read(request)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        if is_cp(request.inode_id.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.write(request)
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        if is_cp(request.parent.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.create(request)
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        if is_cp(request.parent.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.mkdir(request)
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        if is_cp(parent.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.unlink(parent, name)
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        if is_cp(parent.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.rmdir(parent, name)
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        if is_hist(inode_id.0) {
            return self.history()?.truncate(inode_id, size);
        }
        if is_cp(inode_id.0) {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        self.inner.truncate(inode_id, size)
    }

    fn flush(&self, handle: FileHandle) -> VfsResult<()> {
        if is_hist(handle.0) {
            return self.history()?.flush(handle);
        }
        if is_cp(handle.0) {
            return Ok(());
        }
        self.inner.flush(handle)
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        if is_cp(handle.0) {
            // Synthetic nodes are read-only; nothing to make durable.
            return Ok(());
        }
        self.inner.fsync(inode_id, handle, datasync)
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        if is_hist(handle.0) {
            return self.history()?.release(handle);
        }
        if is_cp(handle.0) {
            // Synthetic handles carry no state to drop.
            return Ok(());
        }
        self.inner.release(handle)
    }
}

fn control_dir_attr(inode_id: InodeId) -> NodeAttr {
    let mut attr = NodeAttr::directory(inode_id, SystemTime::now());
    attr.perm = 0o555;
    attr
}

/// Attrs of a synthetic read-only control-plane file (`.vfs/cas/<id>` and the
/// `.vfs/debug` files).
fn cp_file_attr(inode_id: InodeId, size: u64) -> NodeAttr {
    let mut attr = NodeAttr::regular_file(inode_id, size, SystemTime::now());
    attr.perm = 0o444;
    attr
}

/// Contents of a `.vfs/cas/<cas-id>` file: the id itself, newline-terminated.
fn cas_content(name: &OsStr) -> Vec<u8> {
    let mut bytes = os_str_bytes(name);
    bytes.push(b'\n');
    bytes
}

#[cfg(unix)]
fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// Clamp a read to the bytes available for `[offset, offset + size)`.
fn slice_bytes(data: &[u8], offset: u64, size: u32) -> VfsResult<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StubVfs;

    #[test]
    fn vfs_lookup_readdir_read_echo() {
        let stub = Arc::new(StubVfs::with_root_entries(&[]));
        let cp = ControlPlaneVfs::new(stub);

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

        // `.vfs` lists only `cas` (plus `.`/`..`).
        let vfs_names: Vec<_> = cp
            .readdir(VFS_NODE_ID)
            .expect(".vfs readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(vfs_names.contains(&OsString::from(CAS_DIR)));
        assert!(!vfs_names.contains(&OsString::from("nope")));

        // `.vfs/cas` is not listable: only `.`/`..`.
        let cas_names: Vec<_> = cp
            .readdir(CAS_NODE_ID)
            .expect(".vfs/cas readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            cas_names,
            vec![OsString::from("."), OsString::from("..")]
        );

        // `.vfs/cas/<id>` echoes its id, newline-terminated.
        let cas = Arc::new(ControlPlaneVfs::new(Arc::new(StubVfs::with_root_entries(
            &[],
        ))));
        let entry = cas.lookup(CAS_NODE_ID, OsStr::new("abc123")).expect("cas lookup");
        assert_eq!(entry.attr.kind, NodeKind::RegularFile);
        assert_eq!(entry.attr.size, b"abc123\n".len() as u64);
        assert!(is_cp(entry.attr.inode_id.0));

        let opened = cas
            .open(OpenRequest {
                inode_id: entry.attr.inode_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("cas open");
        assert!(is_cp(opened.handle.0));
        let data = cas
            .read(ReadRequest {
                inode_id: entry.attr.inode_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("cas read");
        assert_eq!(data, b"abc123\n");
        cas.release(opened.handle).expect("cas release");
    }

    #[test]
    fn non_vfs_calls_delegate() {
        let stub = Arc::new(StubVfs::with_root_entries(&[("file.txt", b"payload")]));
        let cp = ControlPlaneVfs::new(stub.clone());

        // lookup of a normal name delegates to the stub.
        let entry = cp
            .lookup(ROOT_INODE_ID, OsStr::new("file.txt"))
            .expect("delegate lookup");
        let inner_id = entry.attr.inode_id;
        assert!(!is_cp(inner_id.0));

        // getattr / read on the inner node delegate too.
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

        // The root readdir surfaces both the inner file and synthetic `.vfs`.
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
    fn cp_and_inner_ids_do_not_collide() {
        let stub = Arc::new(StubVfs::with_root_entries(&[("a", b"a"), ("b", b"b")]));
        let cp = ControlPlaneVfs::new(stub);

        let inner_a = cp.lookup(ROOT_INODE_ID, OsStr::new("a")).unwrap().attr.inode_id;
        let inner_b = cp.lookup(ROOT_INODE_ID, OsStr::new("b")).unwrap().attr.inode_id;
        let cas_x = cp.lookup(CAS_NODE_ID, OsStr::new("x")).unwrap().attr.inode_id;
        let cas_y = cp.lookup(CAS_NODE_ID, OsStr::new("y")).unwrap().attr.inode_id;

        // Inner ids stay below the base; cp ids stay at/above it.
        assert!(!is_cp(inner_a.0));
        assert!(!is_cp(inner_b.0));
        assert!(is_cp(cas_x.0));
        assert!(is_cp(cas_y.0));

        // Distinct cas ids get distinct synthetic node ids; the same id is stable.
        assert_ne!(cas_x, cas_y);
        let cas_x_again = cp.lookup(CAS_NODE_ID, OsStr::new("x")).unwrap().attr.inode_id;
        assert_eq!(cas_x, cas_x_again);
    }

    #[test]
    fn mutations_on_cp_nodes_are_read_only() {
        let stub = Arc::new(StubVfs::with_root_entries(&[]));
        let cp = ControlPlaneVfs::new(stub);

        assert_eq!(
            cp.mkdir(CreateRequest::directory(VFS_NODE_ID, "x", 0o755)),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            cp.create(CreateRequest::file(CAS_NODE_ID, "x", 0o644)),
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
    }
}
