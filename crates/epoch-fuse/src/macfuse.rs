//! Mount on macOS via macFUSE's privileged `mount_macfuse` helper.
//!
//! ## Why this exists
//!
//! fuser's built-in macOS mount (`fuse_mount_compat25`, see fuser's `fuse2.rs`)
//! performs the `mount()` syscall **in-process**. macOS 26 (Tahoe) refuses a
//! third-party in-process mount with `EPERM` — even as root — so fuser's stock
//! path no longer works there. fuser has no helper-based mount for macOS; its
//! documented escape hatch is [`fuser::Session::from_fd`] ("wrap a fuse file
//! descriptor into a Session", added in fuser 0.15 for manual mounting).
//!
//! So we do the mount the way macFUSE's own libfuse does, and the way fuser
//! already does it for Linux's `fusermount`: spawn the setuid helper, which
//! performs the privileged `mount()` and passes the `/dev/macfuse` fd back over
//! a socketpair via `SCM_RIGHTS`. We then hand that fd to `Session::from_fd`.
//! This is the same fd-passing dance as fuser's `fuse_mount_fusermount`, just
//! pointed at `mount_macfuse` with macFUSE's `_FUSE_*` environment protocol.

use std::ffi::CString;
use std::io::{self, Error, ErrorKind, IoSliceMut};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::mount::{MntFlags, unmount};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, SockaddrStorage, recvmsg};

/// macFUSE's setuid mount helper. libfuse hard-codes this path.
const MOUNT_MACFUSE: &str = "/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse";

/// A live macFUSE mount. `mount_macfuse` runs detached (reparented to init,
/// exactly as under libfuse) and exits on its own once the mount goes away, so
/// [`MacfuseMount::unmount`] just needs to unmount the path.
pub struct MacfuseMount {
    mountpoint: CString,
}

impl MacfuseMount {
    /// Unmount the filesystem. As the mounting user a plain `unmount(2)` works.
    /// Best-effort: on Ctrl-C the process group is killed and the surrounding
    /// script unmounts.
    pub fn unmount(self) {
        let _ = unmount(self.mountpoint.as_c_str(), MntFlags::empty());
    }
}

/// Mount the macFUSE filesystem at `mountpoint` via the privileged helper and
/// return an owned `/dev/macfuse` fd to feed to [`fuser::Session::from_fd`].
///
/// `options` are passed as a single `-o opt1,opt2,...` argument (empty → none).
///
/// Crucially this does **not** wait for `mount_macfuse` to exit: the helper
/// stays alive managing the mount, and its `mount()` only finalizes once the
/// caller starts answering FUSE requests (`INIT`) on the returned fd. Waiting
/// here would deadlock.
pub fn mount_via_helper(mountpoint: &Path, options: &[&str]) -> io::Result<(OwnedFd, MacfuseMount)> {
    let mountpoint = mountpoint.canonicalize()?;
    let mountpoint_c = CString::new(mountpoint.as_os_str().as_encoded_bytes())
        .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;

    // The helper sends the device fd back over `child_socket`; we read it from
    // `recv_socket` (the other end of the pair).
    let (child_socket, recv_socket) = UnixStream::pair()?;

    let daemon_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_default();

    // Inherit stdio so any helper diagnostics reach the user directly — and so
    // there are no pipes we'd have to drain.
    let mut builder = Command::new(MOUNT_MACFUSE);
    if !options.is_empty() {
        builder.arg("-o").arg(options.join(","));
    }
    builder
        .arg(&mountpoint)
        // The env macFUSE's libfuse passes to mount_macfuse. `_FUSE_CALL_BY_LIB`
        // is the gate that tells the helper it's library-invoked: without it the
        // helper prints "not meant to be called directly" and bails.
        .env("_FUSE_CALL_BY_LIB", "1")
        .env("_FUSE_COMMFD", child_socket.as_raw_fd().to_string())
        .env("_FUSE_COMMVERS", "2")
        .env("_FUSE_DAEMON_PATH", daemon_path);

    // Replicate macFUSE's `fuse_mount_core` exactly: clear FD_CLOEXEC on the comm
    // socket, then DOUBLE-FORK so the actual `mount_macfuse` runs as a detached
    // orphan. `Command` single-forks, so we add the second fork in `pre_exec`:
    // the intermediate (`Command`'s child) exits immediately, leaving the spawned
    // `Child` reapable at once, while the grandchild goes on to exec the helper.
    let comm_fd = child_socket.as_raw_fd();
    // SAFETY: `pre_exec` runs post-fork; it only calls async-signal-safe fcntl/fork/_exit.
    unsafe {
        builder.pre_exec(move || {
            let fd = BorrowedFd::borrow_raw(comm_fd);
            let flags = FdFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFD)?);
            if flags.contains(FdFlag::FD_CLOEXEC) {
                fcntl(fd, FcntlArg::F_SETFD(flags & !FdFlag::FD_CLOEXEC))?;
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => Ok(()),              // grandchild: exec mount_macfuse
                _ => libc::_exit(0),      // intermediate: exit immediately
            }
        });
    }

    let mut intermediate = builder.spawn()?;
    drop(child_socket); // close our copy of the helper's socket end in the parent

    let recv = receive_device_fd(&recv_socket);
    // Reap the intermediate (it exited right after forking the grandchild).
    let _ = intermediate.wait();

    match recv {
        Ok(dev_fd) => Ok((dev_fd, MacfuseMount { mountpoint: mountpoint_c })),
        Err(e) => Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "mount_macfuse did not return a device fd ({e}); is the macFUSE \
                 kernel extension approved and loaded?"
            ),
        )),
    }
}

/// Receive the single `SCM_RIGHTS` file descriptor the helper passes back.
/// Mirrors fuser's own `receive_fusermount_message`.
fn receive_device_fd(socket: &UnixStream) -> io::Result<OwnedFd> {
    let mut data = [0u8];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut cmsg_buffer = nix::cmsg_space!(RawFd);

    let msg = loop {
        match recvmsg::<SockaddrStorage>(
            socket.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buffer),
            MsgFlags::empty(),
        ) {
            Ok(msg) => break msg,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
    };

    if msg.bytes == 0 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "mount_macfuse closed the socket without sending a device fd",
        ));
    }

    for cmsg in msg
        .cmsgs()
        .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?
    {
        if let ControlMessageOwned::ScmRights(fds) = cmsg
            && let Some(&fd) = fds.first()
        {
            if fd < 0 {
                return Err(ErrorKind::InvalidData.into());
            }
            // SAFETY: `fd` is a freshly received, owned descriptor.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }

    Err(Error::new(
        ErrorKind::InvalidData,
        "no SCM_RIGHTS message received from mount_macfuse",
    ))
}
