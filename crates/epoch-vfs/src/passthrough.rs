//! Read-through base [`VfsService`] over a lower directory.
//!
//! [`PassthroughVfs`] is the bottom of the decorator stack: it owns the mapping
//! between [`InodeId`]s / [`FileHandle`]s and relative paths under a lower
//! directory, and answers read operations (`lookup`, `getattr`, `readdir`,
//! `open`, `read`) by stat-ing and reading the corresponding lower file. It does
//! no logging and knows nothing about any synthetic control plane.
//!
//! By default it is read-only: every mutation (`write`, `create`, `mkdir`,
//! `unlink`, `rmdir`, `truncate`) returns [`VfsError::ReadOnlyFilesystem`].
//! Those returns mark the seams where an upper (writable CAS / session) layer
//! attaches as another decorator above this one.
//!
//! [`PassthroughVfs::writable`] flips the service into direct-write mode: the
//! same inode/handle bookkeeping, but mutations apply straight to the lower
//! directory with `std::fs`. This is the "scratchpad" half of the
//! [`RouterVfs`](crate::RouterVfs) pair — subtrees that bypass the overlay
//! read *and write* the host directory directly.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, Metadata},
    io::{Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind, OpenMode,
    OpenRequest, OpenedFile, ROOT_INODE_ID, ReadRequest, SetAttrRequest, StatFs, VfsError,
    VfsResult, VfsService, WriteRequest,
};

/// Read-through [`VfsService`] backed by a lower directory on the host.
#[derive(Debug)]
pub struct PassthroughVfs {
    lower_root: PathBuf,
    writable: bool,
    state: RwLock<PathState>,
    next_node_id: AtomicU64,
    next_handle: AtomicU64,
}

/// Per-handle open state: which lower path the handle addresses and whether it
/// was opened in append mode (append writes ignore the request offset).
#[derive(Debug, Clone)]
struct OpenState {
    path: PathBuf,
    append: bool,
}

/// Bidirectional [`InodeId`]/[`FileHandle`] ↔ relative-path bookkeeping.
#[derive(Debug)]
struct PathState {
    path_by_id: HashMap<InodeId, PathBuf>,
    id_by_path: HashMap<PathBuf, InodeId>,
    handles: HashMap<FileHandle, OpenState>,
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
            writable: false,
            state: RwLock::new(PathState {
                path_by_id: HashMap::from([(ROOT_INODE_ID, root.clone())]),
                id_by_path: HashMap::from([(root, ROOT_INODE_ID)]),
                handles: HashMap::new(),
            }),
            next_node_id: AtomicU64::new(ROOT_INODE_ID.0 + 1),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Like [`new`](Self::new), but mutations (`create`, `write`, `mkdir`,
    /// `unlink`, `rmdir`, `truncate`, and writable/truncating/appending
    /// `open`s) apply directly to the lower directory instead of returning
    /// [`VfsError::ReadOnlyFilesystem`].
    pub fn writable(lower_root: impl Into<PathBuf>) -> Self {
        Self {
            writable: true,
            ..Self::new(lower_root)
        }
    }

    fn require_writable(&self) -> VfsResult<()> {
        if self.writable {
            Ok(())
        } else {
            Err(VfsError::ReadOnlyFilesystem)
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

    fn open_handle(&self, relative: PathBuf, append: bool) -> VfsResult<OpenedFile> {
        let mut state = self.write_state()?;
        let handle = FileHandle(self.next_handle.fetch_add(1, Ordering::Relaxed));
        state.handles.insert(
            handle,
            OpenState {
                path: relative,
                append,
            },
        );
        Ok(OpenedFile {
            handle,
            direct_io: false,
            keep_cache: false,
        })
    }

    /// Resolve the lower path a request addresses: the handle's recorded path
    /// when known, the inode's otherwise. The bool is the handle's append flag.
    fn path_for_request(&self, inode_id: InodeId, handle: FileHandle) -> VfsResult<(PathBuf, bool)> {
        let state = self.read_state()?;
        if let Some(open) = state.handles.get(&handle) {
            return Ok((open.path.clone(), open.append));
        }
        state
            .path_by_id
            .get(&inode_id)
            .cloned()
            .map(|path| (path, false))
            .ok_or(VfsError::NotFound)
    }

    /// Forget the id ↔ path mapping of a deleted node so a later node reusing
    /// the name mints a fresh identity.
    fn drop_path(&self, relative: &Path) -> VfsResult<()> {
        let mut state = self.write_state()?;
        if let Some(inode_id) = state.id_by_path.remove(relative) {
            state.path_by_id.remove(&inode_id);
        }
        Ok(())
    }

    /// Apply `mode`'s permission bits to the freshly-created `lower` node.
    fn apply_mode(lower: &Path, mode: u32) -> VfsResult<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(lower, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(map_io_error)?;
        }
        #[cfg(not(unix))]
        let _ = (lower, mode);
        Ok(())
    }

    /// Shared mutation path of `create`/`mkdir`: make the node on the host,
    /// apply the mode, and return its entry.
    fn create_node(&self, request: &CreateRequest) -> VfsResult<(PathBuf, Entry)> {
        self.require_writable()?;
        let parent_path = self.path_for_node(request.parent)?;
        let child = child_path(&parent_path, &request.name)?;
        let lower = self.lower_path(&child);
        match request.kind {
            NodeKind::RegularFile => {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lower)
                    .map_err(map_io_error)?;
            }
            NodeKind::Directory => fs::create_dir(&lower).map_err(map_io_error)?,
            NodeKind::Symlink => {
                return Err(VfsError::InvalidInput(
                    "create with kind Symlink; use symlink()".to_owned(),
                ));
            }
        }
        Self::apply_mode(&lower, request.mode)?;
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        let attr = attr_from_metadata(self.id_for_path(&child)?, &metadata);
        Ok((child, Entry::new(attr)))
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
        if !self.writable && (request.mode != OpenMode::ReadOnly || request.truncate || request.append)
        {
            return Err(VfsError::ReadOnlyFilesystem);
        }

        let path = self.path_for_node(request.inode_id)?;
        let lower = self.lower_path(&path);
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        if metadata.is_dir() {
            return Err(VfsError::IsDirectory);
        }

        if request.truncate {
            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&lower)
                .map_err(map_io_error)?;
        }

        self.open_handle(path, request.append)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        let (path, _) = self.path_for_request(request.inode_id, request.handle)?;
        let data = fs::read(self.lower_path(&path)).map_err(map_io_error)?;
        slice_bytes(&data, request.offset, request.size)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        self.require_writable()?;
        let (path, append) = self.path_for_request(request.inode_id, request.handle)?;
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if append {
            options.append(true);
        }
        let mut file = options.open(self.lower_path(&path)).map_err(map_io_error)?;
        if !append {
            file.seek(SeekFrom::Start(request.offset)).map_err(map_io_error)?;
        }
        file.write_all(request.data).map_err(map_io_error)?;
        u32::try_from(request.data.len())
            .map_err(|_| VfsError::InvalidInput("write larger than u32".to_owned()))
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        let (child, entry) = self.create_node(&request)?;
        let opened = match request.kind {
            NodeKind::RegularFile => Some(self.open_handle(child, false)?),
            _ => None,
        };
        Ok(CreatedNode { entry, opened })
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        if request.kind != NodeKind::Directory {
            return Err(VfsError::InvalidInput("mkdir of a non-directory".to_owned()));
        }
        Ok(self.create_node(&request)?.1)
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.require_writable()?;
        let parent_path = self.path_for_node(parent)?;
        let child = child_path(&parent_path, name)?;
        let lower = self.lower_path(&child);
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        if metadata.is_dir() {
            return Err(VfsError::IsDirectory);
        }
        fs::remove_file(lower).map_err(map_io_error)?;
        self.drop_path(&child)
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.require_writable()?;
        let parent_path = self.path_for_node(parent)?;
        let child = child_path(&parent_path, name)?;
        fs::remove_dir(self.lower_path(&child)).map_err(map_io_error)?;
        self.drop_path(&child)
    }

    fn rename(
        &self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()> {
        self.require_writable()?;
        let old_rel = child_path(&self.path_for_node(parent)?, name)?;
        let new_rel = child_path(&self.path_for_node(new_parent)?, new_name)?;
        fs::rename(self.lower_path(&old_rel), self.lower_path(&new_rel)).map_err(map_io_error)?;

        // Fix the id ↔ path bookkeeping: a replaced target's identity dies,
        // and every recorded path at or under the old name (the node itself,
        // its descendants, open handles) moves under the new one.
        let mut state = self.write_state()?;
        if let Some(dead) = state.id_by_path.remove(&new_rel) {
            state.path_by_id.remove(&dead);
        }
        let rebase = |path: &PathBuf| -> Option<PathBuf> {
            let suffix = path.strip_prefix(&old_rel).ok()?;
            Some(if suffix.as_os_str().is_empty() {
                new_rel.clone()
            } else {
                new_rel.join(suffix)
            })
        };
        let moved: Vec<(InodeId, PathBuf)> = state
            .path_by_id
            .iter()
            .filter_map(|(id, path)| rebase(path).map(|new_path| (*id, new_path)))
            .collect();
        for (id, new_path) in moved {
            if let Some(old_path) = state.path_by_id.insert(id, new_path.clone()) {
                state.id_by_path.remove(&old_path);
            }
            state.id_by_path.insert(new_path, id);
        }
        for open in state.handles.values_mut() {
            if let Some(new_path) = rebase(&open.path) {
                open.path = new_path;
            }
        }
        Ok(())
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        self.require_writable()?;
        let path = self.path_for_node(inode_id)?;
        let lower = self.lower_path(&path);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&lower)
            .map_err(map_io_error)?;
        file.set_len(size).map_err(map_io_error)?;
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        Ok(attr_from_metadata(inode_id, &metadata))
    }

    fn symlink(&self, parent: InodeId, name: &OsStr, target: &OsStr) -> VfsResult<Entry> {
        self.require_writable()?;
        let parent_path = self.path_for_node(parent)?;
        let child = child_path(&parent_path, name)?;
        let lower = self.lower_path(&child);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &lower).map_err(map_io_error)?;
        #[cfg(not(unix))]
        return Err(VfsError::Unsupported("symlink"));
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        Ok(Entry::new(attr_from_metadata(self.id_for_path(&child)?, &metadata)))
    }

    fn readlink(&self, inode_id: InodeId) -> VfsResult<OsString> {
        let path = self.path_for_node(inode_id)?;
        let target = fs::read_link(self.lower_path(&path)).map_err(map_io_error)?;
        Ok(target.into_os_string())
    }

    fn setattr(&self, request: SetAttrRequest) -> VfsResult<NodeAttr> {
        self.require_writable()?;
        let path = self.path_for_node(request.inode_id)?;
        let lower = self.lower_path(&path);
        if let Some(mode) = request.mode {
            Self::apply_mode(&lower, mode)?;
        }
        // atime/mtime: accepted but not applied (no `filetime` dependency;
        // documented best-effort).
        let metadata = fs::symlink_metadata(&lower).map_err(map_io_error)?;
        Ok(attr_from_metadata(request.inode_id, &metadata))
    }

    fn statfs(&self, inode_id: InodeId) -> VfsResult<StatFs> {
        // Read-only services answer too: `df` is a query, not a mutation.
        let path = self.path_for_node(inode_id)?;
        StatFs::of_path(&self.lower_path(&path))
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        if !self.writable {
            // Read-only view: nothing of ours to make durable.
            return Ok(());
        }
        let (path, _) = self.path_for_request(inode_id, handle)?;
        let file = fs::File::open(self.lower_path(&path)).map_err(map_io_error)?;
        if datasync {
            file.sync_data().map_err(map_io_error)
        } else {
            file.sync_all().map_err(map_io_error)
        }
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        let mut state = self.write_state()?;
        state.handles.remove(&handle);
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
        std::io::ErrorKind::DirectoryNotEmpty => VfsError::DirectoryNotEmpty,
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
    fn writable_mode_mutates_the_host_directory() {
        let dir = TempDir::new("passthrough-rw");
        let vfs = PassthroughVfs::writable(dir.path());

        // mkdir + create + write land on the host.
        vfs.mkdir(CreateRequest::directory(ROOT_INODE_ID, "sub", 0o755))
            .expect("mkdir");
        assert!(dir.path().join("sub").is_dir());

        let created = vfs
            .create(CreateRequest::file(ROOT_INODE_ID, "out.txt", 0o644))
            .expect("create");
        let node = created.entry.attr.inode_id;
        let handle = created.opened.expect("created file opens").handle;
        vfs.write(WriteRequest {
            inode_id: node,
            handle,
            offset: 0,
            data: b"hello",
        })
        .expect("write");
        vfs.release(handle).expect("release");
        assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"hello");

        // truncate shrinks the host file.
        let attr = vfs.truncate(node, 2).expect("truncate");
        assert_eq!(attr.size, 2);
        assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"he");

        // open with truncate empties it.
        let opened = vfs
            .open(OpenRequest {
                inode_id: node,
                mode: OpenMode::ReadWrite,
                truncate: true,
                append: false,
            })
            .expect("open truncate");
        vfs.release(opened.handle).expect("release");
        assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"");

        // unlink / rmdir remove host nodes and their id mappings.
        vfs.unlink(ROOT_INODE_ID, OsStr::new("out.txt")).expect("unlink");
        assert!(!dir.path().join("out.txt").exists());
        assert_eq!(vfs.getattr(node), Err(VfsError::NotFound));
        vfs.rmdir(ROOT_INODE_ID, OsStr::new("sub")).expect("rmdir");
        assert!(!dir.path().join("sub").exists());
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

    /// Symlinks pass through to the host: create + readlink land on the lower
    /// directory in writable mode, and a symlink already in the lower resolves
    /// (with kind `Symlink`) in read-only mode.
    #[cfg(unix)]
    #[test]
    fn symlinks_pass_through_to_the_host() {
        let dir = TempDir::new("passthrough-link");
        fs::write(dir.path().join("real.txt"), b"real").expect("write file");
        std::os::unix::fs::symlink("real.txt", dir.path().join("lower-link"))
            .expect("host symlink");

        // Read-only: a pre-existing lower symlink resolves.
        let ro = PassthroughVfs::new(dir.path());
        let entry = ro.lookup(ROOT_INODE_ID, OsStr::new("lower-link")).expect("lookup");
        assert_eq!(entry.attr.kind, NodeKind::Symlink);
        assert_eq!(
            ro.readlink(entry.attr.inode_id).expect("readlink"),
            OsString::from("real.txt")
        );
        // …but creating one is refused.
        assert_eq!(
            ro.symlink(ROOT_INODE_ID, OsStr::new("nope"), OsStr::new("real.txt")),
            Err(VfsError::ReadOnlyFilesystem)
        );

        // Writable: create lands on the host.
        let rw = PassthroughVfs::writable(dir.path());
        let created = rw
            .symlink(ROOT_INODE_ID, OsStr::new("upper-link"), OsStr::new("real.txt"))
            .expect("symlink");
        assert_eq!(created.attr.kind, NodeKind::Symlink);
        assert_eq!(
            fs::read_link(dir.path().join("upper-link")).expect("host readlink"),
            std::path::PathBuf::from("real.txt")
        );
        assert_eq!(
            rw.readlink(created.attr.inode_id).expect("readlink"),
            OsString::from("real.txt")
        );
    }

    /// `chmod` through `setattr` lands on the host file; `statfs` returns the
    /// host filesystem's numbers.
    #[cfg(unix)]
    #[test]
    fn setattr_mode_and_statfs_hit_the_host() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("passthrough-attr");
        fs::write(dir.path().join("f.sh"), b"#!/bin/sh\n").expect("write file");

        let vfs = PassthroughVfs::writable(dir.path());
        let entry = vfs.lookup(ROOT_INODE_ID, OsStr::new("f.sh")).expect("lookup");
        let id = entry.attr.inode_id;
        assert_eq!(entry.attr.perm, 0o644);

        let attr = vfs.setattr(SetAttrRequest::mode(id, 0o755)).expect("chmod");
        assert_eq!(attr.perm, 0o755);
        assert_eq!(
            fs::metadata(dir.path().join("f.sh")).unwrap().permissions().mode() & 0o7777,
            0o755,
            "the host file's mode changed"
        );

        let stats = vfs.statfs(ROOT_INODE_ID).expect("statfs");
        assert!(stats.blocks > 0, "real filesystem reports real blocks");
        assert!(stats.name_max > 0);
        // A read-only service answers statfs too (df is a query).
        let ro = PassthroughVfs::new(dir.path());
        assert_eq!(ro.statfs(ROOT_INODE_ID).unwrap().blocks, stats.blocks);
    }
}
