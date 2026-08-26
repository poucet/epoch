//! Tracing decorator that logs every [`VfsService`] call.
//!
//! [`LoggingVfs`] is a pure pass-through: it wraps an `inner: Arc<dyn
//! VfsService>`, emits an `eprintln!("[epoch-vfs] …")` line per call (and, for a
//! few operations, a follow-up line summarizing the result), then delegates and
//! returns the inner result unchanged. It owns no node/handle state of its own.

use std::{ffi::OsStr, sync::Arc};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, OpenRequest,
    OpenedFile, ReadRequest, VfsResult, VfsService, WriteRequest,
};

/// `VfsService` decorator that logs each call before delegating to `inner`.
pub struct LoggingVfs {
    inner: Arc<dyn VfsService>,
}

impl std::fmt::Debug for LoggingVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggingVfs").finish_non_exhaustive()
    }
}

impl LoggingVfs {
    /// Wrap `inner`, logging every call that passes through.
    pub fn new(inner: Arc<dyn VfsService>) -> Self {
        Self { inner }
    }
}

impl VfsService for LoggingVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        eprintln!("[epoch-vfs] lookup parent={} name={:?}", parent.0, name);
        let entry = self.inner.lookup(parent, name)?;
        eprintln!(
            "[epoch-vfs] lookup -> node={} kind={:?} size={}",
            entry.attr.inode_id.0, entry.attr.kind, entry.attr.size
        );
        Ok(entry)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        eprintln!("[epoch-vfs] getattr node={}", inode_id.0);
        let attr = self.inner.getattr(inode_id)?;
        eprintln!(
            "[epoch-vfs] getattr -> kind={:?} size={}",
            attr.kind, attr.size
        );
        Ok(attr)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        eprintln!("[epoch-vfs] readdir node={}", inode_id.0);
        let entries = self.inner.readdir(inode_id)?;
        eprintln!("[epoch-vfs] readdir -> {} entries", entries.len());
        Ok(entries)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        eprintln!(
            "[epoch-vfs] open node={} mode={:?} truncate={} append={}",
            request.inode_id.0, request.mode, request.truncate, request.append
        );
        let opened = self.inner.open(request)?;
        eprintln!("[epoch-vfs] open -> fh={}", opened.handle.0);
        Ok(opened)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        eprintln!(
            "[epoch-vfs] read node={} fh={} offset={} size={}",
            request.inode_id.0, request.handle.0, request.offset, request.size
        );
        let data = self.inner.read(request)?;
        eprintln!("[epoch-vfs] read -> {} bytes", data.len());
        Ok(data)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        eprintln!(
            "[epoch-vfs] write node={} fh={} offset={} bytes={}",
            request.inode_id.0,
            request.handle.0,
            request.offset,
            request.data.len()
        );
        self.inner.write(request)
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        eprintln!(
            "[epoch-vfs] create parent={} name={:?} kind={:?} mode={:o}",
            request.parent.0, request.name, request.kind, request.mode
        );
        self.inner.create(request)
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        eprintln!(
            "[epoch-vfs] mkdir parent={} name={:?} mode={:o}",
            request.parent.0, request.name, request.mode
        );
        self.inner.mkdir(request)
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        eprintln!("[epoch-vfs] unlink parent={} name={:?}", parent.0, name);
        self.inner.unlink(parent, name)
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        eprintln!("[epoch-vfs] rmdir parent={} name={:?}", parent.0, name);
        self.inner.rmdir(parent, name)
    }

    fn rename(
        &self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()> {
        eprintln!(
            "[epoch-vfs] rename parent={} name={:?} new_parent={} new_name={:?}",
            parent.0, name, new_parent.0, new_name
        );
        self.inner.rename(parent, name, new_parent, new_name)
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        eprintln!("[epoch-vfs] truncate node={} size={size}", inode_id.0);
        self.inner.truncate(inode_id, size)
    }

    fn flush(&self, handle: FileHandle) -> VfsResult<()> {
        eprintln!("[epoch-vfs] flush fh={}", handle.0);
        self.inner.flush(handle)
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        eprintln!(
            "[epoch-vfs] fsync ino={} fh={} datasync={datasync}",
            inode_id.0, handle.0
        );
        self.inner.fsync(inode_id, handle, datasync)
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        eprintln!("[epoch-vfs] release fh={}", handle.0);
        self.inner.release(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StubVfs;
    use epoch_vfs_api::{NodeKind, OpenMode, ROOT_INODE_ID};
    use std::ffi::OsString;

    #[test]
    fn results_pass_through_unchanged() {
        let stub = Arc::new(StubVfs::with_root_entries(&[("file.txt", b"payload")]));
        let logging = LoggingVfs::new(stub.clone());

        // lookup result matches the inner result exactly.
        let logged = logging
            .lookup(ROOT_INODE_ID, OsStr::new("file.txt"))
            .expect("logged lookup");
        let direct = stub
            .lookup(ROOT_INODE_ID, OsStr::new("file.txt"))
            .expect("direct lookup");
        assert_eq!(logged.attr.inode_id, direct.attr.inode_id);
        assert_eq!(logged.attr.kind, NodeKind::RegularFile);

        // readdir, getattr, open, read all pass the inner value back unchanged.
        let names: Vec<_> = logging
            .readdir(ROOT_INODE_ID)
            .expect("readdir")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&OsString::from("file.txt")));

        let attr = logging.getattr(logged.attr.inode_id).expect("getattr");
        assert_eq!(attr.size, b"payload".len() as u64);

        let opened = logging
            .open(OpenRequest {
                inode_id: logged.attr.inode_id,
                mode: OpenMode::ReadOnly,
                truncate: false,
                append: false,
            })
            .expect("open");
        let data = logging
            .read(ReadRequest {
                inode_id: logged.attr.inode_id,
                handle: opened.handle,
                offset: 0,
                size: 64,
            })
            .expect("read");
        assert_eq!(data, b"payload");

        // An error from the inner service is propagated verbatim.
        assert_eq!(
            logging.lookup(ROOT_INODE_ID, OsStr::new("missing")),
            stub.lookup(ROOT_INODE_ID, OsStr::new("missing"))
        );
    }
}
