use std::{ffi::OsStr, io, path::Path, sync::Arc};

use epoch_vfs_api::{
    CreateRequest, Entry, FileHandle as VfsFileHandle, NodeAttr, InodeId, NodeKind, OpenMode,
    OpenRequest, ReadRequest, SetAttrRequest, VfsError, VfsService, WriteRequest,
};
use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};

pub struct FuseSpikeFs {
    vfs: Arc<dyn VfsService>,
}

impl FuseSpikeFs {
    pub fn new(vfs: Arc<dyn VfsService>) -> Self {
        Self { vfs }
    }
}

/// Mount the overlay and serve it until unmounted (blocks).
///
/// Everywhere except macOS, fuser's own mount works (pure-rust on Linux). On
/// macOS, fuser's path performs the `mount()` syscall in-process, which macOS 26
/// rejects with EPERM; [`mount_spike_macos`] routes through macFUSE's privileged
/// `mount_macfuse` helper instead. See `crate::macfuse`.
#[cfg(not(target_os = "macos"))]
pub fn mount_spike(
    mountpoint: impl AsRef<Path>,
    vfs: Arc<dyn VfsService>,
) -> Result<(), std::io::Error> {
    use fuser::{Config, MountOption};

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

/// macOS variant: mount via macFUSE's `mount_macfuse` helper, then drive the
/// returned `/dev/macfuse` fd with `Session::from_fd`.
#[cfg(target_os = "macos")]
pub fn mount_spike(
    mountpoint: impl AsRef<Path>,
    vfs: Arc<dyn VfsService>,
) -> Result<(), std::io::Error> {
    use fuser::{Config, Session, SessionACL};

    let mountpoint = mountpoint.as_ref();
    // Privileged mount through the helper; yields the kernel device fd.
    // `local` marks the volume as a local (not network/removable) disk. Without
    // it, macOS's privacy layer (TCC) treats the mount as a protected volume
    // class and silently answers EPERM to every open/opendir from apps the
    // user hasn't granted "Files and Folders" access — metadata ops pass, but
    // `ls`/`cat` fail with "Operation not permitted" and the request never
    // reaches the filesystem. `nobrowse` keeps Finder/Spotlight from indexing
    // the (frequently remounted) volume.
    // `noappledouble` refuses `._*` AppleDouble companions outright — without
    // it, macOS materializes them as real files (which the overlay would then
    // dutifully checkpoint) whenever xattrs can't be stored natively.
    let (dev_fd, mount) = crate::macfuse::mount_via_helper(
        mountpoint,
        &["fsname=epoch-fuse", "local", "nobrowse", "noappledouble"],
    )?;

    // Owner-only ACL (no allow_other), matching the non-macOS mount.
    let session = Session::from_fd(
        FuseSpikeFs::new(vfs),
        dev_fd,
        SessionACL::Owner,
        Config::default(),
    )?;

    // `run` is crate-private in fuser; `spawn` + `join` runs the loop on a
    // worker thread while we block here, giving the same foreground behaviour.
    let result = session.spawn()?.join();

    // Tear the mount down through the helper on clean exit. (On Ctrl-C the
    // process is killed outright; mount-mac.sh's trap unmounts in that case.)
    mount.unmount();
    result
}

macro_rules! fuse_log {
    ($($arg:tt)*) => {
        eprintln!("[epoch-fuse:fuse] {}", format_args!($($arg)*));
    };
}

impl Filesystem for FuseSpikeFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        fuse_log!("init");
        // Optimization G: Tune kernel readahead and max write buffers
        let _ = config.set_max_readahead(1024 * 1024);
        let _ = config.set_max_write(1024 * 1024);
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);

        // Add capability flags if supported by the kernel
        let caps = config.capabilities();
        let mut to_add = InitFlags::empty();
        to_add |= caps & InitFlags::FUSE_DO_READDIRPLUS;
        to_add |= caps & InitFlags::FUSE_READDIRPLUS_AUTO;
        to_add |= caps & InitFlags::FUSE_PARALLEL_DIROPS;
        to_add |= caps & InitFlags::FUSE_AUTO_INVAL_DATA;
        #[cfg(not(target_os = "macos"))]
        {
            to_add |= caps & InitFlags::FUSE_SPLICE_READ;
            to_add |= caps & InitFlags::FUSE_SPLICE_WRITE;
            to_add |= caps & InitFlags::FUSE_SPLICE_MOVE;
        }
        if !to_add.is_empty() {
            let _ = config.add_capabilities(to_add);
        }
        Ok(())
    }

    // macFUSE sends an `access` request and, if the filesystem returns ENOSYS
    // (fuser's default), falls back to its own attribute-based permission check —
    // which denies directory reads here with EPERM (opendir succeeds, then the
    // readdir is rejected before it ever reaches us). The overlay enforces its own
    // policy (reads pass, writes return EROFS), so grant access unconditionally.
    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        fuse_log!(
            "access ino={} mask={:?} req_uid={} req_gid={} pid={}",
            ino.0,
            mask,
            req.uid(),
            req.gid(),
            req.pid()
        );
        reply.ok();
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        fuse_log!("lookup parent={} name={:?}", parent.0, name);
        match self.vfs.lookup(from_ino(parent), name) {
            Ok(entry) => {
                fuse_log!(
                    "lookup ok node={} kind={:?} size={}",
                    entry.attr.inode_id.0,
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
        match self.vfs.getattr(from_ino(ino)) {
            Ok(attr) => {
                fuse_log!(
                    "getattr ok ino={} kind={:?} size={} perm={:o} uid={} gid={} nlink={}",
                    attr.inode_id.0,
                    attr.kind,
                    attr.size,
                    attr.perm,
                    attr.uid,
                    attr.gid,
                    attr.nlink
                );
                let ttl = attr_ttl(from_ino(ino));
                let attr = to_file_attr(attr);
                reply.attr(&ttl, &attr);
            }
            Err(err) => reply_error_attr(reply, err),
        }
    }

    // Truncation (`O_TRUNC`, `truncate(2)`) usually arrives here as a
    // size-setting request rather than as an open flag. `chmod` (mode) is
    // honored and persisted (it seals into the tree entry). atime/mtime are
    // accepted and ignored — not persisted; `make`-style mtime reasoning
    // inside an overlay mount is a documented known gap. uid/gid changes are
    // accepted and ignored: the overlay stamps the mounting user's ids.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        fuse_log!(
            "setattr ino={} size={size:?} mode={mode:?} uid={uid:?} gid={gid:?} atime={} mtime={}",
            ino.0,
            atime.is_some(),
            mtime.is_some()
        );
        let id = from_ino(ino);
        let result = (|| {
            if let Some(size) = size {
                self.vfs.truncate(id, size)?;
            }
            if mode.is_some() || atime.is_some() || mtime.is_some() {
                self.vfs.setattr(SetAttrRequest {
                    inode_id: id,
                    mode,
                    atime: atime.map(time_or_now),
                    mtime: mtime.map(time_or_now),
                })?;
            }
            self.vfs.getattr(id)
        })();
        match result {
            Ok(attr) => {
                fuse_log!("setattr ok ino={} size={} perm={:o}", attr.inode_id.0, attr.size, attr.perm);
                let ttl = attr_ttl(attr.inode_id);
                let attr = to_file_attr(attr);
                reply.attr(&ttl, &attr);
            }
            Err(err) => reply_error_attr(reply, err),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        fuse_log!("readlink ino={}", ino.0);
        match self.vfs.readlink(from_ino(ino)) {
            Ok(target) => {
                fuse_log!("readlink ok ino={} target={target:?}", ino.0);
                reply.data(target.as_encoded_bytes());
            }
            Err(err) => reply_error_data(reply, err),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        fuse_log!(
            "symlink parent={} name={link_name:?} target={target:?}",
            parent.0
        );
        match self
            .vfs
            .symlink(from_ino(parent), link_name, target.as_os_str())
        {
            Ok(entry) => {
                fuse_log!("symlink ok node={}", entry.attr.inode_id.0);
                reply_entry(reply, entry);
            }
            Err(err) => reply_error_entry(reply, err),
        }
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        fuse_log!("statfs ino={}", ino.0);
        match self.vfs.statfs(from_ino(ino)) {
            Ok(stats) => {
                fuse_log!(
                    "statfs ok ino={} blocks={} free={} files={}",
                    ino.0, stats.blocks, stats.blocks_avail, stats.files
                );
                reply.statfs(
                    stats.blocks,
                    stats.blocks_free,
                    stats.blocks_avail,
                    stats.files,
                    stats.files_free,
                    stats.block_size as u32,
                    stats.name_max,
                    stats.block_size as u32,
                );
            }
            Err(err) => reply.error(log_errno(&err)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        fuse_log!("open ino={} flags={flags:?}", ino.0);
        let request = OpenRequest {
            inode_id: from_ino(ino),
            mode: to_open_mode(flags),
            truncate: flags.0 & libc::O_TRUNC != 0,
            append: flags.0 & libc::O_APPEND != 0,
        };

        match self.vfs.open(request) {
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
            inode_id: from_ino(ino),
            handle: from_fh(fh),
            offset,
            size,
        };

        match self.vfs.read(request) {
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
            inode_id: from_ino(ino),
            handle: from_fh(fh),
            offset,
            data,
        };

        match self.vfs.write(request) {
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
        match self.vfs.flush(from_fh(fh)) {
            Ok(()) => {
                fuse_log!("flush ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        fuse_log!("fsync ino={} fh={} datasync={datasync}", ino.0, fh.0);
        match self.vfs.fsync(from_ino(ino), from_fh(fh), datasync) {
            Ok(()) => {
                fuse_log!("fsync ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        fuse_log!(
            "rename parent={} name={name:?} newparent={} newname={newname:?}",
            parent.0,
            newparent.0
        );
        match self
            .vfs
            .rename(from_ino(parent), name, from_ino(newparent), newname)
        {
            Ok(()) => {
                fuse_log!("rename ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    // Extended attributes are not stored (the tree format has no xattr slot
    // yet): get/set/remove answer ENOTSUP so callers degrade silently, and
    // the attribute list is served as genuinely empty.
    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: fuser::ReplyXattr) {
        reply.error(Errno::ENOTSUP);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: fuser::ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
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
        match self.vfs.release(from_fh(fh)) {
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
        match self.vfs.readdir(from_ino(ino)) {
            Ok(entries) => {
                let total = entries.len();
                for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    fuse_log!(
                        "readdir add offset={} node={} name={:?} kind={:?}",
                        index + 1,
                        entry.inode_id.0,
                        entry.name,
                        entry.kind
                    );
                    let full = reply.add(
                        to_ino(entry.inode_id),
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

    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        fuse_log!("readdirplus ino={} offset={offset}", ino.0);
        match self.vfs.readdir_plus(from_ino(ino)) {
            Ok(entries) => {
                let total = entries.len();
                for (index, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                    fuse_log!(
                        "readdirplus add offset={} node={} name={:?} kind={:?}",
                        index + 1,
                        entry.inode_id.0,
                        entry.name,
                        entry.kind
                    );
                    let file_attr = to_file_attr(entry.attr);
                    let ttl = attr_ttl(entry.inode_id);
                    let full = reply.add(
                        to_ino(entry.inode_id),
                        (index + 1) as u64,
                        entry.name,
                        &ttl,
                        &file_attr,
                        Generation(0),
                    );
                    if full {
                        break;
                    }
                }
                fuse_log!("readdirplus ok total={total}");
                reply.ok();
            }
            Err(err) => reply.error(log_errno(&err)),
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

        match self.vfs.create(request) {
            Ok(created) => {
                let Some(opened) = created.opened else {
                    fuse_log!("create error missing opened handle");
                    reply.error(Errno::EIO);
                    return;
                };
                fuse_log!(
                    "create ok node={} fh={}",
                    created.entry.attr.inode_id.0,
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

        match self.vfs.mkdir(request) {
            Ok(entry) => {
                fuse_log!("mkdir ok node={}", entry.attr.inode_id.0);
                reply_entry(reply, entry);
            }
            Err(err) => reply_error_entry(reply, err),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        fuse_log!("unlink parent={} name={:?}", parent.0, name);
        match self.vfs.unlink(from_ino(parent), name) {
            Ok(()) => {
                fuse_log!("unlink ok");
                reply.ok();
            }
            Err(err) => reply_error_empty(reply, err),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        fuse_log!("rmdir parent={} name={:?}", parent.0, name);
        match self.vfs.rmdir(from_ino(parent), name) {
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

fn attr_ttl(inode: InodeId) -> std::time::Duration {
    if inode.0 >= epoch_vfs::CP_BASE {
        std::time::Duration::from_secs(60)
    } else {
        std::time::Duration::from_secs(1)
    }
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

fn to_ino(inode_id: InodeId) -> INodeNo {
    INodeNo(inode_id.0)
}

fn from_ino(ino: INodeNo) -> InodeId {
    InodeId(ino.0)
}

fn to_fh(handle: VfsFileHandle) -> FileHandle {
    FileHandle(handle.0)
}

fn from_fh(handle: FileHandle) -> VfsFileHandle {
    VfsFileHandle(handle.0)
}

fn to_file_attr(attr: NodeAttr) -> FileAttr {
    FileAttr {
        ino: to_ino(attr.inode_id),
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

/// `TimeOrNow::Now` resolves at the adapter boundary (the overlay treats
/// atime/mtime as best-effort anyway).
fn time_or_now(t: fuser::TimeOrNow) -> std::time::SystemTime {
    match t {
        fuser::TimeOrNow::SpecificTime(time) => time,
        fuser::TimeOrNow::Now => std::time::SystemTime::now(),
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
        VfsError::CrossDevice => Errno::EXDEV,
        VfsError::InvalidInput(_) => Errno::EINVAL,
        VfsError::Unsupported(_) => Errno::ENOSYS,
        VfsError::Io(_) | VfsError::Other(_) => Errno::EIO,
    }
}
