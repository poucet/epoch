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

/// The uid/gid stamped on synthesized attrs: the mounting process's effective
/// ids, so CAS-backed and synthetic nodes appear owned by the user who
/// mounted (instead of root). Passthrough attrs carry the host's real ids and
/// don't go through these helpers.
#[cfg(unix)]
fn process_ids() -> (u32, u32) {
    // SAFETY: geteuid/getegid are trivially safe (no arguments, no errors).
    unsafe { (libc::geteuid(), libc::getegid()) }
}

#[cfg(not(unix))]
fn process_ids() -> (u32, u32) {
    (0, 0)
}

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
            uid: process_ids().0,
            gid: process_ids().1,
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
            uid: process_ids().0,
            gid: process_ids().1,
            rdev: 0,
            block_size: 4096,
            flags: 0,
        }
    }

    /// Attrs of a symlink of `target_len` bytes. Permission bits are the
    /// conventional `0o777` (a symlink's mode is never consulted for access;
    /// the target's are).
    pub fn symlink(inode_id: InodeId, target_len: u64, now: SystemTime) -> Self {
        Self {
            inode_id,
            kind: NodeKind::Symlink,
            size: target_len,
            blocks: target_len.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            created: now,
            perm: 0o777,
            nlink: 1,
            uid: process_ids().0,
            gid: process_ids().1,
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

/// The attribute-mutation half of `setattr(2)` that epoch supports.
///
/// `size` changes are **not** here — they go through
/// [`VfsService::truncate`], because kernels deliver `O_TRUNC`/`truncate(2)`
/// as a size-only setattr and the trait already routes those. `mode` is the
/// chmod half: persisted (in the tree entry, so it survives checkpoint and
/// remount). `atime`/`mtime` are accepted for wire compatibility (`touch`,
/// `cp -p`, editors) but are **best-effort and not persisted** — a
/// `make`-style mtime comparison inside an overlay mount is a documented
/// known gap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetAttrRequest {
    /// The node to modify.
    pub inode_id: InodeId,
    /// New permission bits (`chmod`); full unix mode — only the low 12 bits
    /// (`0o7777`) are applied.
    pub mode: Option<u32>,
    /// New atime (accepted, not persisted).
    pub atime: Option<SystemTime>,
    /// New mtime (accepted, not persisted).
    pub mtime: Option<SystemTime>,
}

impl SetAttrRequest {
    /// A request touching only `inode_id` (every field `None`).
    pub fn new(inode_id: InodeId) -> Self {
        Self {
            inode_id,
            mode: None,
            atime: None,
            mtime: None,
        }
    }

    /// A mode-only (`chmod`) request.
    pub fn mode(inode_id: InodeId, mode: u32) -> Self {
        Self {
            mode: Some(mode),
            ..Self::new(inode_id)
        }
    }
}

/// Filesystem-wide capacity/usage statistics (`statfs(2)`/`statvfs(2)`).
///
/// For an overlay mount these describe the **lower** host filesystem: the
/// overlay itself is bounded by RAM (staging) and the state store, not by a
/// block device, so the lower's numbers are the honest answer installers and
/// `df` are looking for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatFs {
    /// Optimal transfer block size. Set from `f_frsize` (not `f_bsize`): the
    /// `f_blocks`/`f_bfree`/`f_bavail` counts are in `f_frsize` units on both
    /// Linux and macOS, so this keeps `blocks × block_size` == total bytes —
    /// which is what `df` prints.
    pub block_size: u64,
    /// Total data blocks (`f_blocks`).
    pub blocks: u64,
    /// Free blocks (`f_bfree`).
    pub blocks_free: u64,
    /// Free blocks available to unprivileged users (`f_bavail`).
    pub blocks_avail: u64,
    /// Total file nodes (`f_files`).
    pub files: u64,
    /// Free file nodes (`f_ffree`).
    pub files_free: u64,
    /// Maximum filename length (`f_namemax`).
    pub name_max: u32,
}

impl StatFs {
    /// The stats of the filesystem hosting `path`, via `statvfs(2)`.
    #[cfg(unix)]
    pub fn of_path(path: &std::path::Path) -> VfsResult<StatFs> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            VfsError::InvalidInput(format!("path contains an interior NUL: {path:?}"))
        })?;
        // SAFETY: `c_path` is a valid NUL-terminated CString that outlives the
        // call; `stat` points at a zeroed `statvfs` we own.
        let stat = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                return Err(VfsError::Io(std::io::Error::last_os_error().to_string()));
            }
            stat
        };
        Ok(StatFs {
            // f_frsize, not f_bsize: see the field's doc comment.
            block_size: stat.f_frsize as u64,
            blocks: stat.f_blocks as u64,
            blocks_free: stat.f_bfree as u64,
            blocks_avail: stat.f_bavail as u64,
            files: stat.f_files as u64,
            files_free: stat.f_ffree as u64,
            name_max: stat.f_namemax as u32,
        })
    }

    /// A synthetic answer for a filesystem with no host backing (a pure
    /// in-memory CAS tree): unbounded for practical purposes.
    pub fn synthetic() -> StatFs {
        StatFs {
            block_size: 4096,
            blocks: 1 << 28, // 1 TiB in 4 KiB blocks
            blocks_free: 1 << 28,
            blocks_avail: 1 << 28,
            files: 0,
            files_free: 1 << 30,
            name_max: 255,
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
    /// The operation would cross a boundary it cannot (e.g. a rename between
    /// the overlay and a routed pass-through subtree, or out of a fall-through
    /// directory). Maps to `EXDEV`, so callers fall back to copy + delete.
    CrossDevice,
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
            Self::CrossDevice => f.write_str("cross-device link"),
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

    /// Move `parent/name` to `new_parent/new_name` (POSIX `rename(2)`:
    /// atomically replaces an existing target file, or an existing *empty*
    /// target directory). Editors rely on this for atomic saves
    /// (write-temp-then-rename). Implementations that cannot move across an
    /// internal boundary return [`VfsError::CrossDevice`] so callers fall
    /// back to copy + delete.
    fn rename(
        &self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()>;

    /// Set the file's size (`truncate(2)` / `O_TRUNC`, or `ftruncate` growth),
    /// returning the updated attributes. Kernels commonly deliver `O_TRUNC` as
    /// a size-setting `setattr` rather than an open flag, so writable
    /// filesystems must implement this for shell redirection (`>`) to work.
    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr>;

    /// Create a symlink `name` under `parent` pointing at `target` (the raw
    /// target path bytes, stored verbatim — relative targets stay relative).
    /// The default answers [`Unsupported`](VfsError::Unsupported); writable
    /// filesystems override it.
    fn symlink(&self, parent: InodeId, name: &OsStr, target: &OsStr) -> VfsResult<Entry> {
        let _ = (parent, name, target);
        Err(VfsError::Unsupported("symlink"))
    }

    /// Read a symlink's target. The default answers
    /// [`Unsupported`](VfsError::Unsupported).
    fn readlink(&self, inode_id: InodeId) -> VfsResult<OsString> {
        let _ = inode_id;
        Err(VfsError::Unsupported("readlink"))
    }

    /// Apply the non-size attribute changes in `request` (see
    /// [`SetAttrRequest`] for what persists), returning the updated
    /// attributes. The default answers [`Unsupported`](VfsError::Unsupported).
    fn setattr(&self, request: SetAttrRequest) -> VfsResult<NodeAttr> {
        let _ = request;
        Err(VfsError::Unsupported("setattr"))
    }

    /// Filesystem-wide capacity statistics (`statfs(2)`). `inode_id` identifies
    /// which filesystem is asked (any live node of it). The default answers
    /// [`Unsupported`](VfsError::Unsupported).
    fn statfs(&self, inode_id: InodeId) -> VfsResult<StatFs> {
        let _ = inode_id;
        Err(VfsError::Unsupported("statfs"))
    }

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
    /// See [`VfsService::readlink`]. Default: no symlinks here (a feature that
    /// serves tree snapshots overrides it — see
    /// [`TreeBrowseFeature`](crate::submount) in `epoch-vfs`).
    fn readlink(&self, _inode_id: InodeId) -> VfsResult<OsString> {
        Err(VfsError::InvalidInput("not a symlink".into()))
    }
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

    fn readlink(&self, inode_id: InodeId) -> VfsResult<OsString> {
        ReadOnlyFeature::readlink(self, inode_id)
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

    fn rename(
        &self,
        _parent: InodeId,
        _name: &OsStr,
        _new_parent: InodeId,
        _new_name: &OsStr,
    ) -> VfsResult<()> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn truncate(&self, _inode_id: InodeId, _size: u64) -> VfsResult<NodeAttr> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn symlink(&self, _parent: InodeId, _name: &OsStr, _target: &OsStr) -> VfsResult<Entry> {
        Err(VfsError::ReadOnlyFilesystem)
    }

    fn setattr(&self, _request: SetAttrRequest) -> VfsResult<NodeAttr> {
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
