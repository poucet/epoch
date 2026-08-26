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

    fn flush(&self, handle: FileHandle) -> VfsResult<()>;

    /// Explicit durability request (`fsync(2)`/`fdatasync(2)`) for an open
    /// file. `datasync` asks for data-only durability (metadata may be
    /// skipped). Filesystems whose durability is managed elsewhere (e.g. a
    /// checkpoint layer) may treat this as a signal and return `Ok(())`.
    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()>;

    fn release(&self, handle: FileHandle) -> VfsResult<()>;
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
