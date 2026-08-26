//! Shared test fixtures for the decorator unit tests.
//!
//! This module is compiled only under `#[cfg(test)]`. It provides:
//!
//! - [`TempDir`] — a unique scratch directory under the system temp dir that is
//!   removed on drop (avoids a `tempfile` dev-dependency).
//! - [`StubVfs`] — a minimal in-memory [`VfsService`] used as the `inner` of the
//!   [`ControlPlaneVfs`](crate::ControlPlaneVfs) and
//!   [`LoggingVfs`](crate::LoggingVfs) tests so they don't touch the filesystem.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, Entry, FileHandle, NodeAttr, InodeId, NodeKind, OpenedFile,
    OpenRequest, ROOT_INODE_ID, ReadRequest, VfsError, VfsResult, VfsService, WriteRequest,
};

/// A unique scratch directory under the system temp dir, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh, process- and call-unique directory tagged with `label`.
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "epoch-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// The path of the scratch directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A minimal in-memory [`VfsService`] holding a flat set of root-level files.
///
/// Only the read path needed by the decorator tests is implemented; mutations
/// return [`VfsError::ReadOnlyFilesystem`].
#[derive(Debug)]
pub struct StubVfs {
    files: HashMap<OsString, Vec<u8>>,
    ids: RwLock<StubIds>,
    next: AtomicU64,
}

#[derive(Debug, Default)]
struct StubIds {
    name_to_id: HashMap<OsString, InodeId>,
    id_to_name: HashMap<InodeId, OsString>,
}

impl StubVfs {
    /// Build a stub whose root directory contains the given `(name, content)`
    /// regular files.
    pub fn with_root_entries(entries: &[(&str, &[u8])]) -> Self {
        let files = entries
            .iter()
            .map(|(name, content)| (OsString::from(*name), content.to_vec()))
            .collect();
        Self {
            files,
            ids: RwLock::new(StubIds::default()),
            // Start low so ids stay well below the control plane's high range.
            next: AtomicU64::new(ROOT_INODE_ID.0 + 1),
        }
    }

    fn id_for(&self, name: &OsStr) -> InodeId {
        {
            let ids = self.ids.read().expect("stub ids read");
            if let Some(id) = ids.name_to_id.get(name) {
                return *id;
            }
        }
        let mut ids = self.ids.write().expect("stub ids write");
        if let Some(id) = ids.name_to_id.get(name) {
            return *id;
        }
        let id = InodeId(self.next.fetch_add(1, Ordering::Relaxed));
        ids.name_to_id.insert(name.to_owned(), id);
        ids.id_to_name.insert(id, name.to_owned());
        id
    }

    fn name_for(&self, inode_id: InodeId) -> Option<OsString> {
        self.ids
            .read()
            .expect("stub ids read")
            .id_to_name
            .get(&inode_id)
            .cloned()
    }

    fn file_attr(&self, inode_id: InodeId, content: &[u8]) -> NodeAttr {
        NodeAttr::regular_file(inode_id, content.len() as u64, SystemTime::now())
    }
}

impl VfsService for StubVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if parent != ROOT_INODE_ID {
            return Err(VfsError::NotFound);
        }
        let content = self.files.get(name).ok_or(VfsError::NotFound)?;
        Ok(Entry::new(self.file_attr(self.id_for(name), content)))
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == ROOT_INODE_ID {
            return Ok(NodeAttr::directory(inode_id, SystemTime::now()));
        }
        let name = self.name_for(inode_id).ok_or(VfsError::NotFound)?;
        let content = self.files.get(&name).ok_or(VfsError::NotFound)?;
        Ok(self.file_attr(inode_id, content))
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id != ROOT_INODE_ID {
            return Err(VfsError::NotDirectory);
        }
        let mut entries = vec![
            DirEntry::new(ROOT_INODE_ID, OsString::from("."), NodeKind::Directory),
            DirEntry::new(ROOT_INODE_ID, OsString::from(".."), NodeKind::Directory),
        ];
        for name in self.files.keys() {
            entries.push(DirEntry::new(
                self.id_for(name),
                name.clone(),
                NodeKind::RegularFile,
            ));
        }
        Ok(entries)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        let name = self.name_for(request.inode_id).ok_or(VfsError::NotFound)?;
        if !self.files.contains_key(&name) {
            return Err(VfsError::NotFound);
        }
        Ok(OpenedFile {
            handle: FileHandle(request.inode_id.0),
            direct_io: false,
            keep_cache: false,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        let name = self.name_for(request.inode_id).ok_or(VfsError::NotFound)?;
        let content = self.files.get(&name).ok_or(VfsError::NotFound)?;
        let start = request.offset as usize;
        if start >= content.len() {
            return Ok(Vec::new());
        }
        let end = (start + request.size as usize).min(content.len());
        Ok(content[start..end].to_vec())
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

    fn flush(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }

    fn fsync(&self, _inode_id: InodeId, _handle: FileHandle, _datasync: bool) -> VfsResult<()> {
        Ok(())
    }

    fn release(&self, _handle: FileHandle) -> VfsResult<()> {
        Ok(())
    }
}
