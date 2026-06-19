use std::{ffi::OsStr, io, path::Path, sync::Arc};

use epoch_vfs_api::{
    CreateRequest, Entry, FileHandle as VfsFileHandle, NodeAttr, NodeId, NodeKind, OpenMode,
    OpenRequest, ReadRequest, VfsError, VfsService, WriteRequest,
};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    KernelConfig, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use futures_executor::block_on;

pub struct FuseSpikeFs {
    vfs: Arc<dyn VfsService>,
}

impl FuseSpikeFs {
    pub fn new(vfs: Arc<dyn VfsService>) -> Self {
        Self { vfs }
    }
}

pub fn mount_spike(
    mountpoint: impl AsRef<Path>,
    vfs: Arc<dyn VfsService>,
) -> Result<(), std::io::Error> {
    let mut config = Config::default();
    // `AutoUnmount` is intentionally omitted: fuser requires a non-`Owner` ACL
    // (allow_other/allow_root) when it is set, which on macFUSE additionally needs
    // allow_other to be permitted system-wide. An owner-only mount still unmounts
    // on clean exit via fuser's drop guard. To re-enable auto-unmount, also set
    // `config.acl = SessionACL::All`.
    config.mount_options = vec![
        MountOption::FSName("epoch-fuse".to_owned()),
        MountOption::DefaultPermissions,
    ];
    fuser::mount2(FuseSpikeFs::new(vfs), mountpoint, &config)
}

macro_rules! fuse_log {
    ($($arg:tt)*) => {
        eprintln!("[epoch-fuse:fuse] {}", format_args!($($arg)*));
    };
}

impl Filesystem for FuseSpikeFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        fuse_log!("init");
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        fuse_log!("lookup parent={} name={:?}", parent.0, name);
        match block_on(self.vfs.lookup(from_ino(parent), name)) {
            Ok(entry) => {
                fuse_log!(
                    "lookup ok node={} kind={:?} size={}",
                    entry.attr.node_id.0,
                    entry.attr.kind,
                    entry.attr.size
                );
                reply_entry(reply, entry);
            }
            Err(err) => reply_error_entry(reply, err),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        fuse_log!("getattr ino={}", ino.0);
        match block_on(self.vfs.getattr(from_ino(ino))) {
            Ok(attr) => {
                fuse_log!(
                    "getattr ok ino={} kind={:?} size={}",
                    attr.node_id.0,
                    attr.kind,
                    attr.size
                );
                let ttl = attr_ttl();
                let attr = to_file_attr(attr);
                reply.attr(&ttl, &attr);
            }
            Err(err) => reply_error_attr(reply, err),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        fuse_log!("open ino={} flags={flags:?}", ino.0);
        let request = OpenRequest {
            node_id: from_ino(ino),
            mode: to_open_mode(flags),
            truncate: false,
            append: false,
        };

        match block_on(self.vfs.open(request)) {
            Ok(opened) => {
                fuse_log!("open ok fh={}", opened.handle.0);
                reply.opened(to_fh(opened.handle), FopenFlags::empty());
            }
            Err(err) => reply_error_open(reply, err),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        fuse_log!(
            "read ino={} fh={} offset={} size={}",
            ino.0,
            fh.0,
            offset,
            size
        );
        let request = ReadRequest {
            node_id: from_ino(ino),
            handle: from_fh(fh),
            offset,
            size,
        };

        match block_on(self.vfs.read(request)) {
            Ok(data) => {
                fuse_log!("read ok bytes={}", data.len());
                reply.data(&data);
            }
            Err(err) => reply_error_data(reply, err),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        fuse_log!(
            "write ino={} fh={} offset={} bytes={}",
            ino.0,
            fh.0,
            offset,
            data.len()
        );
        let request = WriteRequest {
            node_id: from_ino(ino),
            handle: from_fh(fh),
            offset,
            data: data.to_vec(),
        };

        match block_on(self.vfs.write(request)) {
            Ok(bytes_written) => {
                fuse_log!("write ok bytes={bytes_written}");
                reply.written(bytes_written);
            }
            Err(err) => reply_error_write(reply, err),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        fuse_log!("flush fh={}", fh.0);
        match block_on(self.vfs.flush(from_fh(fh))) {
            Ok(()) => {
                fuse_log!("flush ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        fuse_log!("release fh={}", fh.0);
        match block_on(self.vfs.release(from_fh(fh))) {
            Ok(()) => {
                fuse_log!("release ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        fuse_log!("opendir ino={}", ino.0);
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        fuse_log!("readdir ino={} offset={offset}", ino.0);
        match block_on(self.vfs.readdir(from_ino(ino))) {
            Ok(entries) => {
                let total = entries.len();
                for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    fuse_log!(
                        "readdir add offset={} node={} name={:?} kind={:?}",
                        index + 1,
                        entry.node_id.0,
                        entry.name,
                        entry.kind
                    );
                    let full = reply.add(
                        to_ino(entry.node_id),
                        (index + 1) as u64,
                        to_file_type(entry.kind),
                        entry.name,
                    );
                    if full {
                        break;
                    }
                }
                fuse_log!("readdir ok total={total}");
                reply.ok();
            }
            Err(err) => reply_error_directory(reply, err),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        fuse_log!("releasedir fh={}", fh.0);
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        fuse_log!("create parent={} name={:?} mode={mode:o}", parent.0, name);
        let request = CreateRequest::file(from_ino(parent), name.to_owned(), mode);

        match block_on(self.vfs.create(request)) {
            Ok(created) => {
                let Some(opened) = created.opened else {
                    fuse_log!("create error missing opened handle");
                    reply.error(Errno::EIO);
                    return;
                };
                fuse_log!(
                    "create ok node={} fh={}",
                    created.entry.attr.node_id.0,
                    opened.handle.0
                );
                let ttl = created.entry.ttl;
                let attr = to_file_attr(created.entry.attr);
                reply.created(
                    &ttl,
                    &attr,
                    Generation(created.entry.generation),
                    to_fh(opened.handle),
                    FopenFlags::empty(),
                );
            }
            Err(err) => reply_error_create(reply, err),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        fuse_log!("mkdir parent={} name={:?} mode={mode:o}", parent.0, name);
        let request = CreateRequest::directory(from_ino(parent), name.to_owned(), mode);

        match block_on(self.vfs.mkdir(request)) {
            Ok(entry) => {
                fuse_log!("mkdir ok node={}", entry.attr.node_id.0);
                reply_entry(reply, entry);
            }
            Err(err) => reply_error_entry(reply, err),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        fuse_log!("unlink parent={} name={:?}", parent.0, name);
        match block_on(self.vfs.unlink(from_ino(parent), name)) {
            Ok(()) => {
                fuse_log!("unlink ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        fuse_log!("rmdir parent={} name={:?}", parent.0, name);
        match block_on(self.vfs.rmdir(from_ino(parent), name)) {
            Ok(()) => {
                fuse_log!("rmdir ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }
}

fn reply_entry(reply: ReplyEntry, entry: Entry) {
    let ttl = entry.ttl;
    let attr = to_file_attr(entry.attr);
    reply.entry(&ttl, &attr, Generation(entry.generation));
}

fn attr_ttl() -> std::time::Duration {
    std::time::Duration::from_secs(1)
}

fn log_errno(error: &VfsError) -> Errno {
    let errno = to_errno(error);
    fuse_log!("api error {error:?} -> errno {}", errno.code());
    errno
}

fn reply_error_attr(reply: ReplyAttr, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_create(reply: ReplyCreate, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_data(reply: ReplyData, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_directory(reply: ReplyDirectory, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_empty(reply: ReplyEmpty, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_entry(reply: ReplyEntry, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_open(reply: ReplyOpen, error: VfsError) {
    reply.error(log_errno(&error));
}

fn reply_error_write(reply: ReplyWrite, error: VfsError) {
    reply.error(log_errno(&error));
}

fn to_ino(node_id: NodeId) -> INodeNo {
    INodeNo(node_id.0)
}

fn from_ino(ino: INodeNo) -> NodeId {
    NodeId(ino.0)
}

fn to_fh(handle: VfsFileHandle) -> FileHandle {
    FileHandle(handle.0)
}

fn from_fh(handle: FileHandle) -> VfsFileHandle {
    VfsFileHandle(handle.0)
}

fn to_file_attr(attr: NodeAttr) -> FileAttr {
    FileAttr {
        ino: to_ino(attr.node_id),
        size: attr.size,
        blocks: attr.blocks,
        atime: attr.atime,
        mtime: attr.mtime,
        ctime: attr.ctime,
        crtime: attr.created,
        kind: to_file_type(attr.kind),
        perm: attr.perm,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev,
        blksize: attr.block_size,
        flags: attr.flags,
    }
}

fn to_file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::RegularFile => FileType::RegularFile,
        NodeKind::Symlink => FileType::Symlink,
    }
}

fn to_open_mode(flags: OpenFlags) -> OpenMode {
    match flags.acc_mode() {
        OpenAccMode::O_RDONLY => OpenMode::ReadOnly,
        OpenAccMode::O_WRONLY => OpenMode::WriteOnly,
        OpenAccMode::O_RDWR => OpenMode::ReadWrite,
    }
}

fn to_errno(error: &VfsError) -> Errno {
    match error {
        VfsError::NotFound => Errno::ENOENT,
        VfsError::AlreadyExists => Errno::EEXIST,
        VfsError::NotDirectory => Errno::ENOTDIR,
        VfsError::IsDirectory => Errno::EISDIR,
        VfsError::DirectoryNotEmpty => Errno::ENOTEMPTY,
        VfsError::PermissionDenied => Errno::EACCES,
        VfsError::ReadOnlyFilesystem => Errno::EROFS,
        VfsError::InvalidInput(_) => Errno::EINVAL,
        VfsError::Unsupported(_) => Errno::ENOSYS,
        VfsError::Io(_) | VfsError::Other(_) => Errno::EIO,
    }
}
