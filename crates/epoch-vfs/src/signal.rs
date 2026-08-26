//! [`SignalVfs`] — the decorator that feeds filesystem activity into the
//! checkpoint scheduler.
//!
//! Forwards every op to `inner` unchanged and, after the op succeeds, emits
//! the matching [`TriggerEvent`]: `write → Wrote(n)`, `flush → Flush`,
//! `release → Release`, `fsync → Fsync`. With `hard_fsync` enabled an `fsync`
//! becomes a synchronous barrier: the call blocks on
//! [`SchedulerHandle::checkpoint_now`] and only returns once the checkpoint is
//! durable (there is no deadlock: the checkpoint path — seal + promote — never
//! re-enters the VFS). Soft mode (the default) treats `fsync` as one more
//! debounce signal, per the soft-fsync contract.

use std::ffi::OsStr;
use std::sync::Arc;

use epoch_history::{SchedulerHandle, TriggerEvent};
use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, InodeId, NodeAttr, OpenRequest,
    OpenedFile, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
};

/// The event-emitting decorator. See the [module docs](self).
pub struct SignalVfs {
    inner: Arc<dyn VfsService>,
    scheduler: SchedulerHandle,
    hard_fsync: bool,
}

impl SignalVfs {
    /// Wrap `inner`, signaling `scheduler`; `hard_fsync` selects the
    /// synchronous fsync barrier.
    pub fn new(inner: Arc<dyn VfsService>, scheduler: SchedulerHandle, hard_fsync: bool) -> Self {
        Self {
            inner,
            scheduler,
            hard_fsync,
        }
    }
}

impl VfsService for SignalVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        self.inner.lookup(parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        self.inner.getattr(inode_id)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        self.inner.readdir(inode_id)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        self.inner.open(request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        self.inner.read(request)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        let written = self.inner.write(request)?;
        self.scheduler.signal(TriggerEvent::Wrote(written as u64));
        Ok(written)
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        self.inner.create(request)
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        self.inner.mkdir(request)
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.inner.unlink(parent, name)?;
        // A deletion is a durable-intent mutation with no write/flush pair.
        self.scheduler.signal(TriggerEvent::Wrote(0));
        self.scheduler.signal(TriggerEvent::Flush);
        Ok(())
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        self.inner.rmdir(parent, name)?;
        self.scheduler.signal(TriggerEvent::Wrote(0));
        self.scheduler.signal(TriggerEvent::Flush);
        Ok(())
    }

    fn flush(&self, handle: FileHandle) -> VfsResult<()> {
        self.inner.flush(handle)?;
        self.scheduler.signal(TriggerEvent::Flush);
        Ok(())
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        self.inner.fsync(inode_id, handle, datasync)?;
        if self.hard_fsync {
            self.scheduler
                .checkpoint_now()
                .map(|_| ())
                .map_err(|err| VfsError::Other(format!("hard fsync checkpoint: {err}")))
        } else {
            self.scheduler.signal(TriggerEvent::Fsync { hard: false });
            Ok(())
        }
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        self.inner.release(handle)?;
        self.scheduler.signal(TriggerEvent::Release);
        Ok(())
    }
}
