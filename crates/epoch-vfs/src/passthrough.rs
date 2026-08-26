//! Read-through base [`VfsService`] over a lower directory.
//!
//! [`PassthroughVfs`] is the bottom of the decorator stack: it owns the mapping
//! between [`InodeId`]s / [`FileHandle`]s and relative paths under a lower
//! directory, and answers read operations (`lookup`, `getattr`, `readdir`,
//! `open`, `read`) by stat-ing and reading the corresponding lower file. It does
//! no logging and knows nothing about any synthetic control plane.
//!
//! It is intentionally read-only: every mutation (`write`, `create`, `mkdir`,
//! `unlink`, `rmdir`) returns [`VfsError::ReadOnlyFilesystem`]. Those returns
//! mark the seams where a future upper (writable CAS / session) layer will
//! attach as another decorator above this one.

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
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind, OpenMode,
    OpenRequest, OpenedFile, ROOT_INODE_ID, ReadRequest, VfsError, VfsResult, VfsService,
    WriteRequest,
};

/// Read-through [`VfsService`] backed by a lower directory on the host.
#[derive(Debug)]
pub struct PassthroughVfs {
    lower_root: PathBuf,
    state: RwLock<PathState>,
    next_node_id: AtomicU64,
    next_handle: AtomicU64,
}

/// Bidirectional [`InodeId`]/[`FileHandle`] ↔ relative-path bookkeeping.
#[derive(Debug)]
struct PathState {
    path_by_id: HashMap<InodeId, PathBuf>,
    id_by_path: HashMap<PathBuf, InodeId>,
    path_by_handle: HashMap<FileHandle, PathBuf>,
}

impl PassthroughVfs {
    /// Create a read-through service rooted at `lower_root`.
    ///
    /// The mount root ([`ROOT_INODE_ID`]) maps to the empty relative path, i.e.
    /// `lower_root` itself.
    pub fn new(lower_root: impl Into<PathBuf>) -> Self {
        let root = PathBuf::new();
        Self {
            lower_root: lower_root.into(),
            state: RwLock::new(PathState {
                path_by_id: HashMap::from([(ROOT_INODE_ID, root.clone())]),
                id_by_path: HashMap::from([(root, ROOT_INODE_ID)]),
                path_by_handle: HashMap::new(),
            }),
            next_node_id: AtomicU64::new(ROOT_INODE_ID.0 + 1),
            next_handle: AtomicU64::new(1),
        }
    }

    /// The lower directory this service reads through to.
    pub fn lower_root(&self) -> &Path {
        &self.lower_root
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, PathState>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("passthrough read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, PathState>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("passthrough write lock poisoned".to_owned()))
    }

    fn lower_path(&self, relative: &Path) -> PathBuf {
        self.lower_root.join(relative)
    }

    fn path_for_node(&self, inode_id: InodeId) -> VfsResult<PathBuf> {
        let state = self.read_state()?;
        state
            .path_by_id
            .get(&inode_id)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn id_for_path(&self, relative: &Path) -> VfsResult<InodeId> {
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

    /// Build the leading `.`/`..` entries shared by every directory listing.
    fn dot_entries(&self, inode_id: InodeId, path: &Path) -> VfsResult<Vec<DirEntry>> {
        let parent_id = parent_path(path)
            .map(|parent| self.id_for_path(&parent))
            .transpose()?
            .unwrap_or(inode_id);
        Ok(vec![
            DirEntry::new(inode_id, OsString::from("."), NodeKind::Directory),
            DirEntry::new(parent_id, OsString::from(".."), NodeKind::Directory),
        ])
    }
}

impl VfsService for PassthroughVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        let parent_path = self.path_for_node(parent)?;
        let child = child_path(&parent_path, name)?;
        let metadata = fs::symlink_metadata(self.lower_path(&child)).map_err(map_io_error)?;
        let attr = attr_from_metadata(self.id_for_path(&child)?, &metadata);
        Ok(Entry::new(attr))
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        let path = self.path_for_node(inode_id)?;
        let metadata = fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
        Ok(attr_from_metadata(inode_id, &metadata))
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        let path = self.path_for_node(inode_id)?;
        let mut entries = self.dot_entries(inode_id, &path)?;

        let lower_path = self.lower_path(&path);
        let metadata = fs::symlink_metadata(&lower_path).map_err(map_io_error)?;
        if !metadata.is_dir() {
            return Err(VfsError::NotDirectory);
        }

        for entry in fs::read_dir(lower_path).map_err(map_io_error)? {
            let entry = entry.map_err(map_io_error)?;
            let name = entry.file_name();
            let child = child_path(&path, &name)?;
            let metadata = entry.metadata().map_err(map_io_error)?;
            let child_id = self.id_for_path(&child)?;
            let kind = kind_from_metadata(&metadata);
            entries.push(DirEntry::new(child_id, name, kind));
        }

        Ok(entries)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }

        let path = self.path_for_node(request.inode_id)?;
        let metadata = fs::symlink_metadata(self.lower_path(&path)).map_err(map_io_error)?;
        if metadata.is_dir() {
            return Err(VfsError::IsDirectory);
        }

        self.open_handle(path)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        let path = {
            let state = self.read_state()?;
            state
                .path_by_handle
                .get(&request.handle)
                .cloned()
                .or_else(|| state.path_by_id.get(&request.inode_id).cloned())
                .ok_or(VfsError::NotFound)?
        };
        let data = fs::read(self.lower_path(&path)).map_err(map_io_error)?;
        slice_bytes(&data, request.offset, request.size)
    }

    fn write(&self, _request: WriteRequest<'_>) -> VfsResult<u32> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn create(&self, _request: CreateRequest) -> VfsResult<CreatedNode> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn mkdir(&self, _request: CreateRequest) -> VfsResult<Entry> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn unlink(&self, _parent: InodeId, _name: &OsStr) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn rmdir(&self, _parent: InodeId, _name: &OsStr) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, _inode_id: InodeId, _handle: FileHandle, _datasync: bool) -> VfsResult<()> {
        // Read-only view: nothing of ours to make durable.
        Ok(())
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        let mut state = self.write_state()?;
        state.path_by_handle.remove(&handle);
        Ok(())
    }
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

fn id_for_path(state: &mut PathState, next_node_id: &AtomicU64, path: PathBuf) -> InodeId {
    if let Some(inode_id) = state.id_by_path.get(&path) {
        return *inode_id;
    }

    let inode_id = InodeId(next_node_id.fetch_add(1, Ordering::Relaxed));
    state.path_by_id.insert(inode_id, path.clone());
    state.id_by_path.insert(path, inode_id);
    inode_id
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

fn attr_from_metadata(inode_id: InodeId, metadata: &Metadata) -> NodeAttr {
    let now = SystemTime::now();
    let kind = kind_from_metadata(metadata);
    let default_perm = match kind {
        NodeKind::Directory => 0o755,
        NodeKind::RegularFile | NodeKind::Symlink => 0o644,
    };

    let mut attr = NodeAttr {
        inode_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn read_through_known_file() {
        let dir = TempDir::new("passthrough");
        fs::write(dir.path().join("hello.txt"), b"hello world").expect("write file");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");

        let vfs = PassthroughVfs::new(dir.path());

        // lookup + getattr a known file.
        let entry = vfs
            .lookup(ROOT_INODE_ID, OsStr::new("hello.txt"))
            .expect("lookup");
        assert_eq!(entry.attr.kind, NodeKind::RegularFile);
        assert_eq!(entry.attr.size, b"hello world".len() as u64);

        let attr = vfs.getattr(entry.attr.inode_id).expect("getattr");
        assert_eq!(attr.inode_id, entry.attr.inode_id);
        assert_eq!(attr.kind, NodeKind::RegularFile);

        // readdir of root lists both children plus `.`/`..`.
        let names: Vec<_> = vfs
            .readdir(ROOT_INODE_ID)
            .expect("readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&OsString::from("hello.txt")));
        assert!(names.contains(&OsString::from("sub")));
        assert!(names.contains(&OsString::from(".")));
        assert!(names.contains(&OsString::from("..")));

        // open + read the file contents.
        let opened = vfs
            .open(OpenRequest {
                inode_id: entry.attr.inode_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("open");
        let data = vfs
            .read(ReadRequest {
                inode_id: entry.attr.inode_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("read");
        assert_eq!(data, b"hello world");

        vfs.release(opened.handle).expect("release");
    }

    #[test]
    fn mutations_are_read_only() {
        let dir = TempDir::new("passthrough-ro");
        let vfs = PassthroughVfs::new(dir.path());

        assert_eq!(
            vfs.mkdir(CreateRequest::directory(ROOT_INODE_ID, "x", 0o755)),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            vfs.create(CreateRequest::file(ROOT_INODE_ID, "x", 0o644)),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            vfs.unlink(ROOT_INODE_ID, OsStr::new("x")),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            vfs.rmdir(ROOT_INODE_ID, OsStr::new("x")),
            Err(VfsError::ReadOnlyFilesystem)
        );
        assert_eq!(
            vfs.write(WriteRequest {
                inode_id: ROOT_INODE_ID,
                handle: FileHandle(0),
                offset: 0,
                data: b"x",
            }),
            Err(VfsError::ReadOnlyFilesystem)
        );
    }
}
