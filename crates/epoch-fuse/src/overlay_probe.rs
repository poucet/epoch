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
            let metadata = fs::symlink_metadata(self.lower_path(&child)).map_err(map_io_error)?;
            let node_id = self.id_for_path(&child)?;
            let entry = Entry::new(attr_from_metadata(node_id, &metadata));
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
            let metadata = fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
            let attr = attr_from_metadata(node_id, &metadata);
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
            let lower_path = self.lower_path(&path);
            let metadata = fs::symlink_metadata(&lower_path).map_err(map_io_error)?;
            if !metadata.is_dir() {
                return Err(VfsError::NotDirectory);
            }

            let parent_id = parent_path(&path)
                .map(|parent| self.id_for_path(&parent))
                .transpose()?
                .unwrap_or(node_id);
            let mut entries = vec![
                DirEntry::new(node_id, OsString::from("."), NodeKind::Directory),
                DirEntry::new(parent_id, OsString::from(".."), NodeKind::Directory),
            ];

            for entry in fs::read_dir(lower_path).map_err(map_io_error)? {
                let entry = entry.map_err(map_io_error)?;
                let name = entry.file_name();
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

            eprintln!(
                "[epoch-fuse:vfs] readdir -> {} entries",
                entries.len()
            );
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
            let metadata = fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
            if metadata.is_dir() {
                return Err(VfsError::IsDirectory);
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
            let data = fs::read(self.lower_path(&path)).map_err(map_io_error)?;
            let start = usize::try_from(request.offset)
                .map_err(|_| VfsError::InvalidInput("read offset exceeds usize".to_owned()))?;
            let size = usize::try_from(request.size)
                .map_err(|_| VfsError::InvalidInput("read size exceeds usize".to_owned()))?;
            let end = start.saturating_add(size).min(data.len());
            let slice = if start >= data.len() {
                Vec::new()
            } else {
                data[start..end].to_vec()
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
