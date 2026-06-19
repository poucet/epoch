//! Async VFS contracts that stay independent from FUSE, CAS, timeline stores,
//! daemon process state, and concrete runtimes.

use std::{
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime},
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type VfsResult<T> = Result<T, VfsError>;

pub const ROOT_NODE_ID: NodeId = NodeId(1);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(pub u64);

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
    pub node_id: NodeId,
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
    pub fn directory(node_id: NodeId, now: SystemTime) -> Self {
        Self {
            node_id,
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

    pub fn regular_file(node_id: NodeId, size: u64, now: SystemTime) -> Self {
        Self {
            node_id,
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
    pub node_id: NodeId,
    pub name: OsString,
    pub kind: NodeKind,
}

impl DirEntry {
    pub fn new(node_id: NodeId, name: impl Into<OsString>, kind: NodeKind) -> Self {
        Self {
            node_id,
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
    pub node_id: NodeId,
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
    pub node_id: NodeId,
    pub handle: FileHandle,
    pub offset: u64,
    pub size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteRequest {
    pub node_id: NodeId,
    pub handle: FileHandle,
    pub offset: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateRequest {
    pub parent: NodeId,
    pub name: OsString,
    pub kind: NodeKind,
    pub mode: u32,
}

impl CreateRequest {
    pub fn file(parent: NodeId, name: impl Into<OsString>, mode: u32) -> Self {
        Self {
            parent,
            name: name.into(),
            kind: NodeKind::RegularFile,
            mode,
        }
    }

    pub fn directory(parent: NodeId, name: impl Into<OsString>, mode: u32) -> Self {
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
    fn lookup<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<Entry>>;

    fn getattr<'a>(&'a self, node_id: NodeId) -> BoxFuture<'a, VfsResult<NodeAttr>>;

    fn readdir<'a>(&'a self, node_id: NodeId) -> BoxFuture<'a, VfsResult<Vec<DirEntry>>>;

    fn open<'a>(&'a self, request: OpenRequest) -> BoxFuture<'a, VfsResult<OpenedFile>>;

    fn read<'a>(&'a self, request: ReadRequest) -> BoxFuture<'a, VfsResult<Vec<u8>>>;

    fn write<'a>(&'a self, request: WriteRequest) -> BoxFuture<'a, VfsResult<u32>>;

    fn create<'a>(&'a self, request: CreateRequest) -> BoxFuture<'a, VfsResult<CreatedNode>>;

    fn mkdir<'a>(&'a self, request: CreateRequest) -> BoxFuture<'a, VfsResult<Entry>>;

    fn unlink<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<()>>;

    fn rmdir<'a>(&'a self, parent: NodeId, name: &'a OsStr) -> BoxFuture<'a, VfsResult<()>>;

    fn flush<'a>(&'a self, handle: FileHandle) -> BoxFuture<'a, VfsResult<()>>;

    fn release<'a>(&'a self, handle: FileHandle) -> BoxFuture<'a, VfsResult<()>>;
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
