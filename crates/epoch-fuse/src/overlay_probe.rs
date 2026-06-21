use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, Metadata},
    path::{Component, Path, PathBuf},
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use epoch_vfs_api::{
    BoxFuture, CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, NodeId, NodeKind,
    OpenMode, OpenRequest, OpenedFile, ROOT_NODE_ID, ReadRequest, VfsError, VfsResult, VfsService,
    WriteRequest,
};

#[derive(Debug)]
pub struct OverlayProbeVfs {
    lower_root: PathBuf,
    state: RwLock<ProbeState>,
    next_node_id: AtomicU64,
    next_handle: AtomicU64,
}

#[derive(Debug)]
struct ProbeState {
    path_by_id: HashMap<NodeId, PathBuf>,
    id_by_path: HashMap<PathBuf, NodeId>,
    path_by_handle: HashMap<FileHandle, PathBuf>,
}

impl OverlayProbeVfs {
    pub fn new(lower_root: impl Into<PathBuf>) -> Self {
        let root = PathBuf::new();
        Self {
            lower_root: lower_root.into(),
            state: RwLock::new(ProbeState {
                path_by_id: HashMap::from([(ROOT_NODE_ID, root.clone())]),
                id_by_path: HashMap::from([(root, ROOT_NODE_ID)]),
                path_by_handle: HashMap::new(),
            }),
            next_node_id: AtomicU64::new(ROOT_NODE_ID.0 + 1),
            next_handle: AtomicU64::new(1),
        }
    }

    pub fn lower_root(&self) -> &Path {
        &self.lower_root
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, ProbeState>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("overlay probe read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, ProbeState>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("overlay probe write lock poisoned".to_owned()))
    }

    fn lower_path(&self, relative: &Path) -> PathBuf {
        self.lower_root.join(relative)
    }

    fn path_for_node(&self, node_id: NodeId) -> VfsResult<PathBuf> {
        let state = self.read_state()?;
        state
            .path_by_id
            .get(&node_id)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn id_for_path(&self, relative: &Path) -> VfsResult<NodeId> {
        let mut state = self.write_state()?;
        Ok(id_for_path(
            &mut state,
            &self.next_node_id,
            relative.to_path_buf(),
        ))
    }

    fn open_handle(&self, relative: PathBuf) -> VfsResult<OpenedFile> {
        let mut state = self.write_state()?;
        let handle = FileHandle(self.next_handle.fetch_add(1, Ordering::Relaxed));
        state.path_by_handle.insert(handle, relative);
        Ok(OpenedFile {
            handle,
            direct_io: false,
            keep_cache: false,
        })
    }

    /// Synthesize attributes for a node inside the `.vfs` control plane.
    fn control_attr(&self, path: &Path, control: ControlPlane) -> VfsResult<NodeAttr> {
        let node_id = self.id_for_path(path)?;
        Ok(match control {
            ControlPlane::Root | ControlPlane::CasDir => control_dir_attr(node_id),
            ControlPlane::CasEntry => cas_entry_attr(node_id, cas_content(path).len() as u64),
            ControlPlane::Passthrough => return Err(VfsError::NotFound),
        })
    }

    /// Build the leading `.`/`..` entries shared by every directory listing.
    fn dot_entries(&self, node_id: NodeId, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let parent_id = parent_path(path)
            .map(|parent| self.id_for_path(&parent))
            .transpose()?
            .unwrap_or(node_id);
        Ok(vec![
            DirEntry::new(node_id, OsString::from("."), NodeKind::Directory),
            DirEntry::new(parent_id, OsString::from(".."), NodeKind::Directory),
        ])
    }
}

impl VfsService for OverlayProbeVfs {
    fn lookup<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<Entry>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] lookup parent={} name={:?}",
                parent.0, name
            );
            let parent_path = self.path_for_node(parent)?;
            let child = child_path(&parent_path, name)?;
            let attr = match classify(&child) {
                ControlPlane::Passthrough => {
                    let metadata =
                        fs::symlink_metadata(self.lower_path(&child)).map_err(map_io_error)?;
                    attr_from_metadata(self.id_for_path(&child)?, &metadata)
                }
                control => self.control_attr(&child, control)?,
            };
            let entry = Entry::new(attr);
            eprintln!(
                "[epoch-fuse:vfs] lookup -> node={} kind={:?} size={}",
                entry.attr.node_id.0, entry.attr.kind, entry.attr.size
            );
            Ok(entry)
        })
    }

    fn getattr<'a>(&'a self, node_id: NodeId) -> BoxFuture<'a, VfsResult<NodeAttr>> {
        Box::pin(async move {
            eprintln!("[epoch-fuse:vfs] getattr node={}", node_id.0);
            let path = self.path_for_node(node_id)?;
            let attr = match classify(&path) {
                ControlPlane::Passthrough => {
                    let metadata =
                        fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
                    attr_from_metadata(node_id, &metadata)
                }
                control => self.control_attr(&path, control)?,
            };
            eprintln!(
                "[epoch-fuse:vfs] getattr -> kind={:?} size={}",
                attr.kind, attr.size
            );
            Ok(attr)
        })
    }

    fn readdir<'a>(&'a self, node_id: NodeId) -> BoxFuture<'a, VfsResult<Vec<DirEntry>>> {
        Box::pin(async move {
            eprintln!("[epoch-fuse:vfs] readdir node={}", node_id.0);
            let path = self.path_for_node(node_id)?;
            let mut entries = self.dot_entries(node_id, &path)?;
            match classify(&path) {
                ControlPlane::Root => {
                    // `.vfs` exposes only the feature directories it implements.
                    let cas = self.id_for_path(&path.join(CAS_DIR))?;
                    entries.push(DirEntry::new(cas, OsString::from(CAS_DIR), NodeKind::Directory));
                }
                ControlPlane::CasDir => {
                    // Intentionally not listable: cas entries are addressable by id only.
                    eprintln!("[epoch-fuse:vfs] readdir cas -> entries not listable");
                }
                ControlPlane::CasEntry => return Err(VfsError::NotDirectory),
                ControlPlane::Passthrough => {
                    let lower_path = self.lower_path(&path);
                    let metadata = fs::symlink_metadata(&lower_path).map_err(map_io_error)?;
                    if !metadata.is_dir() {
                        return Err(VfsError::NotDirectory);
                    }

                    for entry in fs::read_dir(lower_path).map_err(map_io_error)? {
                        let entry = entry.map_err(map_io_error)?;
                        let name = entry.file_name();
                        // The synthetic control plane shadows any lower entry of the same name.
                        if path.as_os_str().is_empty() && name == OsStr::new(CONTROL_DIR) {
                            continue;
                        }
                        let child = child_path(&path, &name)?;
                        let metadata = entry.metadata().map_err(map_io_error)?;
                        let child_id = self.id_for_path(&child)?;
                        let kind = kind_from_metadata(&metadata);
                        eprintln!(
                            "[epoch-fuse:vfs] readdir child node={} name={:?} kind={kind:?}",
                            child_id.0, name
                        );
                        entries.push(DirEntry::new(child_id, name, kind));
                    }

                    // Surface the control plane at the root so it is discoverable.
                    if path.as_os_str().is_empty() {
                        let vfs = self.id_for_path(Path::new(CONTROL_DIR))?;
                        entries.push(DirEntry::new(
                            vfs,
                            OsString::from(CONTROL_DIR),
                            NodeKind::Directory,
                        ));
                    }
                }
            }

            eprintln!("[epoch-fuse:vfs] readdir -> {} entries", entries.len());
            Ok(entries)
        })
    }

    fn open<'a>(&'a self, request: OpenRequest) -> BoxFuture<'a, VfsResult<OpenedFile>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] open node={} mode={:?} truncate={} append={}",
                request.node_id.0, request.mode, request.truncate, request.append
            );
            if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
                eprintln!("[epoch-fuse:vfs] open would copy lower file into upper overlay");
                return Err(VfsError::ReadOnlyFilesystem);
            }

            let path = self.path_for_node(request.node_id)?;
            match classify(&path) {
                // Synthetic read-only file; fall through to register a handle.
                ControlPlane::CasEntry => {}
                ControlPlane::Root | ControlPlane::CasDir => return Err(VfsError::IsDirectory),
                ControlPlane::Passthrough => {
                    let metadata =
                        fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
                    if metadata.is_dir() {
                        return Err(VfsError::IsDirectory);
                    }
                }
            }

            let opened = self.open_handle(path)?;
            eprintln!("[epoch-fuse:vfs] open -> fh={}", opened.handle.0);
            Ok(opened)
        })
    }

    fn read<'a>(&'a self, request: ReadRequest) -> BoxFuture<'a, VfsResult<Vec<u8>>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] read node={} fh={} offset={} size={}",
                request.node_id.0, request.handle.0, request.offset, request.size
            );
            let path = {
                let state = self.read_state()?;
                state
                    .path_by_handle
                    .get(&request.handle)
                    .cloned()
                    .or_else(|| state.path_by_id.get(&request.node_id).cloned())
                    .ok_or(VfsError::NotFound)?
            };
            let slice = match classify(&path) {
                ControlPlane::CasEntry => {
                    slice_bytes(&cas_content(&path), request.offset, request.size)?
                }
                ControlPlane::Passthrough => {
                    let data = fs::read(self.lower_path(&path)).map_err(map_io_error)?;
                    slice_bytes(&data, request.offset, request.size)?
                }
                ControlPlane::Root | ControlPlane::CasDir => return Err(VfsError::IsDirectory),
            };
            eprintln!("[epoch-fuse:vfs] read -> {} bytes", slice.len());
            Ok(slice)
        })
    }

    fn write<'a>(&'a self, request: WriteRequest) -> BoxFuture<'a, VfsResult<u32>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] write node={} fh={} offset={} bytes={} would write upper overlay",
                request.node_id.0,
                request.handle.0,
                request.offset,
                request.data.len()
            );
            Err(VfsError::ReadOnlyFilesystem)
        })
    }

    fn create<'a>(&'a self, request: CreateRequest) -> BoxFuture<'a, VfsResult<CreatedNode>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] create parent={} name={:?} kind={:?} mode={:o} would create upper overlay node",
                request.parent.0, request.name, request.kind, request.mode
            );
            Err(VfsError::ReadOnlyFilesystem)
        })
    }

    fn mkdir<'a>(&'a self, request: CreateRequest) -> BoxFuture<'a, VfsResult<Entry>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] mkdir parent={} name={:?} mode={:o} would create upper overlay dir",
                request.parent.0, request.name, request.mode
            );
            Err(VfsError::ReadOnlyFilesystem)
        })
    }

    fn unlink<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<()>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] unlink parent={} name={:?} would create whiteout",
                parent.0, name
            );
            Err(VfsError::ReadOnlyFilesystem)
        })
    }

    fn rmdir<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<()>> {
        Box::pin(async move {
            eprintln!(
                "[epoch-fuse:vfs] rmdir parent={} name={:?} would create directory whiteout",
                parent.0, name
            );
            Err(VfsError::ReadOnlyFilesystem)
        })
    }

    fn flush<'a>(&'a self, handle: FileHandle) -> BoxFuture<'a, VfsResult<()>> {
        Box::pin(async move {
            eprintln!("[epoch-fuse:vfs] flush fh={}", handle.0);
            Ok(())
        })
    }

    fn release<'a>(&'a self, handle: FileHandle) -> BoxFuture<'a, VfsResult<()>> {
        Box::pin(async move {
            eprintln!("[epoch-fuse:vfs] release fh={}", handle.0);
            let mut state = self.write_state()?;
            state.path_by_handle.remove(&handle);
            Ok(())
        })
    }
}

/// Name of the synthetic control-plane directory at the mount root.
const CONTROL_DIR: &str = ".vfs";
/// Name of the content-addressed-store directory inside the control plane.
const CAS_DIR: &str = "cas";

/// Classification of a relative path against the `.vfs` control plane.
#[derive(Clone, Copy, Debug)]
enum ControlPlane {
    /// Outside the control plane; served from the lower directory.
    Passthrough,
    /// The `.vfs` directory itself.
    Root,
    /// The `.vfs/cas` directory (entries are addressable but not listable).
    CasDir,
    /// A `.vfs/cas/<cas-id>` synthetic file whose contents are the id.
    CasEntry,
}

fn classify(path: &Path) -> ControlPlane {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == OsStr::new(CONTROL_DIR) => {
            match components.next() {
                None => ControlPlane::Root,
                Some(Component::Normal(second)) if second == OsStr::new(CAS_DIR) => {
                    match (components.next(), components.next()) {
                        (None, _) => ControlPlane::CasDir,
                        (Some(Component::Normal(_)), None) => ControlPlane::CasEntry,
                        // cas ids have no children: deeper paths fall through to the
                        // lower directory, which reports them as missing.
                        _ => ControlPlane::Passthrough,
                    }
                }
                // `.vfs/<other>` is not a defined control entry yet.
                _ => ControlPlane::Passthrough,
            }
        }
        _ => ControlPlane::Passthrough,
    }
}

fn control_dir_attr(node_id: NodeId) -> NodeAttr {
    let mut attr = NodeAttr::directory(node_id, SystemTime::now());
    attr.perm = 0o555;
    attr
}

fn cas_entry_attr(node_id: NodeId, size: u64) -> NodeAttr {
    let mut attr = NodeAttr::regular_file(node_id, size, SystemTime::now());
    attr.perm = 0o444;
    attr
}

/// Contents of a `.vfs/cas/<cas-id>` file: the id itself, newline-terminated.
fn cas_content(path: &Path) -> Vec<u8> {
    let mut bytes = path.file_name().map(os_str_bytes).unwrap_or_default();
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

fn id_for_path(state: &mut ProbeState, next_node_id: &AtomicU64, path: PathBuf) -> NodeId {
    if let Some(node_id) = state.id_by_path.get(&path) {
        return *node_id;
    }

    let node_id = NodeId(next_node_id.fetch_add(1, Ordering::Relaxed));
    state.path_by_id.insert(node_id, path.clone());
    state.id_by_path.insert(path, node_id);
    node_id
}

fn child_path(parent: &Path, name: &OsStr) -> VfsResult<PathBuf> {
    let name_path = Path::new(name);
    let mut components = name_path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {
            let mut child = parent.to_path_buf();
            child.push(name);
            Ok(child)
        }
        _ => Err(VfsError::InvalidInput(format!(
            "invalid child name: {:?}",
            name
        ))),
    }
}

fn parent_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }

    path.parent().map(|parent| {
        if parent.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            parent.to_path_buf()
        }
    })
}

fn attr_from_metadata(node_id: NodeId, metadata: &Metadata) -> NodeAttr {
    let now = SystemTime::now();
    let kind = kind_from_metadata(metadata);
    let default_perm = match kind {
        NodeKind::Directory => 0o755,
        NodeKind::RegularFile | NodeKind::Symlink => 0o644,
    };

    let mut attr = NodeAttr {
        node_id,
        kind,
        size: metadata.len(),
        blocks: metadata.len().div_ceil(512),
        atime: metadata.accessed().unwrap_or(now),
        mtime: metadata.modified().unwrap_or(now),
        ctime: metadata.modified().unwrap_or(now),
        created: metadata.created().unwrap_or(now),
        perm: default_perm,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        block_size: 4096,
        flags: 0,
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        attr.perm = (metadata.mode() & 0o7777) as u16;
        attr.nlink = metadata.nlink() as u32;
        attr.uid = metadata.uid();
        attr.gid = metadata.gid();
        attr.rdev = metadata.rdev() as u32;
        attr.blocks = metadata.blocks();
        attr.block_size = metadata.blksize() as u32;
    }

    attr
}

fn kind_from_metadata(metadata: &Metadata) -> NodeKind {
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::RegularFile
    }
}

fn map_io_error(error: std::io::Error) -> VfsError {
    match error.kind() {
        std::io::ErrorKind::NotFound => VfsError::NotFound,
        std::io::ErrorKind::AlreadyExists => VfsError::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => VfsError::PermissionDenied,
        std::io::ErrorKind::InvalidInput => VfsError::InvalidInput(error.to_string()),
        _ => VfsError::Io(error.to_string()),
    }
}
