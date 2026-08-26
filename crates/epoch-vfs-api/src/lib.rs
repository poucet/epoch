//! Synchronous VFS contracts that stay independent from FUSE, CAS, timeline
//! stores, daemon process state, and concrete runtimes.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    time::{Duration, SystemTime},
};

pub type VfsResult<T> = Result<T, VfsError>;

pub const ROOT_INODE_ID: InodeId = InodeId(1);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InodeId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    Directory,
    RegularFile,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeAttr {
    pub inode_id: InodeId,
    pub kind: NodeKind,
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub created: SystemTime,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub block_size: u32,
    pub flags: u32,
}

impl NodeAttr {
    pub fn directory(inode_id: InodeId, now: SystemTime) -> Self {
        Self {
            inode_id,
            kind: NodeKind::Directory,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            created: now,
            perm: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
            block_size: 4096,
            flags: 0,
        }
    }

    pub fn regular_file(inode_id: InodeId, size: u64, now: SystemTime) -> Self {
        Self {
            inode_id,
            kind: NodeKind::RegularFile,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            created: now,
            perm: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            block_size: 4096,
            flags: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub attr: NodeAttr,
    pub generation: u64,
    pub ttl: Duration,
}

impl Entry {
    pub fn new(attr: NodeAttr) -> Self {
        Self {
            attr,
            generation: 0,
            ttl: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    pub inode_id: InodeId,
    pub name: OsString,
    pub kind: NodeKind,
}

impl DirEntry {
    pub fn new(inode_id: InodeId, name: impl Into<OsString>, kind: NodeKind) -> Self {
        Self {
            inode_id,
            name: name.into(),
            kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenRequest {
    pub inode_id: InodeId,
    pub mode: OpenMode,
    pub truncate: bool,
    pub append: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenedFile {
    pub handle: FileHandle,
    pub direct_io: bool,
    pub keep_cache: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub inode_id: InodeId,
    pub handle: FileHandle,
    pub offset: u64,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteRequest<'a> {
    pub inode_id: InodeId,
    pub handle: FileHandle,
    pub offset: u64,
    pub data: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateRequest {
    pub parent: InodeId,
    pub name: OsString,
    pub kind: NodeKind,
    pub mode: u32,
}

impl CreateRequest {
    pub fn file(parent: InodeId, name: impl Into<OsString>, mode: u32) -> Self {
        Self {
            parent,
            name: name.into(),
            kind: NodeKind::RegularFile,
            mode,
        }
    }

    pub fn directory(parent: InodeId, name: impl Into<OsString>, mode: u32) -> Self {
        Self {
            parent,
            name: name.into(),
            kind: NodeKind::Directory,
            mode,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedNode {
    pub entry: Entry,
    pub opened: Option<OpenedFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VfsError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    PermissionDenied,
    ReadOnlyFilesystem,
    InvalidInput(String),
    Unsupported(&'static str),
    Io(String),
    Other(String),
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::AlreadyExists => f.write_str("already exists"),
            Self::NotDirectory => f.write_str("not a directory"),
            Self::IsDirectory => f.write_str("is a directory"),
            Self::DirectoryNotEmpty => f.write_str("directory not empty"),
            Self::PermissionDenied => f.write_str("permission denied"),
            Self::ReadOnlyFilesystem => f.write_str("read-only filesystem"),
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Unsupported(operation) => write!(f, "unsupported operation: {operation}"),
            Self::Io(message) => write!(f, "io error: {message}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl Error for VfsError {}

pub trait VfsService: Send + Sync {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry>;

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr>;

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>>;

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile>;

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>>;

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32>;

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode>;

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry>;

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()>;

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()>;

    /// Set the file's size (`truncate(2)` / `O_TRUNC`, or `ftruncate` growth),
    /// returning the updated attributes. Kernels commonly deliver `O_TRUNC` as
    /// a size-setting `setattr` rather than an open flag, so writable
    /// filesystems must implement this for shell redirection (`>`) to work.
    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr>;

    fn flush(&self, handle: FileHandle) -> VfsResult<()>;

    /// Explicit durability request (`fsync(2)`/`fdatasync(2)`) for an open
    /// file. `datasync` asks for data-only durability (metadata may be
    /// skipped). Filesystems whose durability is managed elsewhere (e.g. a
    /// checkpoint layer) may treat this as a signal and return `Ok(())`.
    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()>;

    fn release(&self, handle: FileHandle) -> VfsResult<()>;
}

/// The slim contract a **read-only** control-plane feature implements: just
/// the read surface. The blanket [`VfsService`] impl below supplies
/// everything else uniformly — every mutation (`write`, `create`, `mkdir`,
/// `unlink`, `rmdir`, `truncate`) answers
/// [`ReadOnlyFilesystem`](VfsError::ReadOnlyFilesystem), `flush`/`fsync` are
/// no-ops (there is nothing to make durable), and writable/truncating/
/// appending opens are rejected *before* [`open`](Self::open) is called, so
/// implementations only ever see read-only requests.
///
/// Every `.vfs` feature (`cas`, `history`, `debug`) implements this instead
/// of the full trait.
pub trait ReadOnlyFeature: Send + Sync {
    /// See [`VfsService::lookup`].
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry>;
    /// See [`VfsService::getattr`].
    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr>;
    /// See [`VfsService::readdir`].
    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>>;
    /// Read-only open (writable modes were already rejected).
    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile>;
    /// See [`VfsService::read`].
    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>>;
    /// Drop a handle minted by [`open`](Self::open). Default: handles are
    /// stateless, nothing to do.
    fn release(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }
}

impl<T: ReadOnlyFeature> VfsService for T {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        ReadOnlyFeature::lookup(self, parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        ReadOnlyFeature::getattr(self, inode_id)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        ReadOnlyFeature::readdir(self, inode_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.mode != OpenMode::ReadOnly || request.truncate || request.append {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        ReadOnlyFeature::open(self, request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        ReadOnlyFeature::read(self, request)
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

    fn truncate(&self, _inode_id: InodeId, _size: u64) -> VfsResult<NodeAttr> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, _inode_id: InodeId, _handle: FileHandle, _datasync: bool) -> VfsResult<()> {
        // Read-only features have nothing to make durable.
        Ok(())
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        ReadOnlyFeature::release(self, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::VfsService;
    use std::sync::Arc;

    fn accepts_trait_object(_: Arc<dyn VfsService>) {}

    #[test]
    fn vfs_service_is_object_safe() {
        let _ = accepts_trait_object;
    }
}
