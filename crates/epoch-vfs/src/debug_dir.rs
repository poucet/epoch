//! `.vfs/debug` — live filesystem statistics, as a control-plane feature.
//!
//! A listable read-only directory of three virtual files (`near-objects`,
//! `dirty-files`, `overlay`), each rendered fresh from the provider's
//! [`DebugSnapshot`] on every access (procfs-style: `getattr` sizes and
//! `read` bytes both track live state; a read racing a mutation may see a
//! size from a slightly older snapshot).

use std::{
    ffi::{OsStr, OsString},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use epoch_vfs_api::{
    DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind, OpenRequest, OpenedFile,
    ReadRequest, VfsError, VfsResult,
};

use crate::debug::DebugStats;
use crate::feature::{FeatureSlot, ReadOnlyFeature, control_dir_attr, cp_file_attr, slice_bytes};

/// The fixed debug files, as `(name, id offset from the slot base)` pairs —
/// the single source for lookup, readdir, and content routing.
const FILES: [(&str, u64); 3] = [("near-objects", 1), ("dirty-files", 2), ("overlay", 3)];

/// First mintable file-handle id offset (past the root and the fixed files).
const FIRST_HANDLE: u64 = 4;

/// The `.vfs/debug` feature. See the [module docs](self).
pub struct DebugDir {
    slot: FeatureSlot,
    provider: Arc<dyn DebugStats>,
    next_handle: AtomicU64,
}

impl DebugDir {
    /// Serve `provider`'s statistics from `slot`.
    pub fn new(slot: FeatureSlot, provider: Arc<dyn DebugStats>) -> DebugDir {
        DebugDir {
            slot,
            provider,
            next_handle: AtomicU64::new(slot.base + FIRST_HANDLE),
        }
    }

    fn file_id(&self, name: &OsStr) -> Option<InodeId> {
        FILES
            .iter()
            .find(|(file_name, _)| name == OsStr::new(file_name))
            .map(|&(_, offset)| InodeId(self.slot.base + offset))
    }

    fn is_file(&self, inode_id: InodeId) -> bool {
        FILES
            .iter()
            .any(|&(_, offset)| inode_id.0 == self.slot.base + offset)
    }

    /// Render the current contents of debug file `inode_id`.
    fn content(&self, inode_id: InodeId) -> VfsResult<Vec<u8>> {
        let snapshot = self.provider.debug_snapshot();
        let text = match inode_id.0.checked_sub(self.slot.base) {
            Some(1) => match snapshot.near_objects {
                Some(count) => format!("{count}\n"),
                // The backend cannot count its staging tier cheaply.
                None => "unknown\n".to_owned(),
            },
            Some(2) => format!("{}\n", snapshot.dirty_files),
            Some(3) => snapshot
                .overlay_paths
                .iter()
                .map(|path| format!("{path}\n"))
                .collect(),
            _ => return Err(VfsError::NotFound),
        };
        Ok(text.into_bytes())
    }
}

impl ReadOnlyFeature for DebugDir {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if parent == self.slot.root_id() {
            let inode_id = self.file_id(name).ok_or(VfsError::NotFound)?;
            let size = self.content(inode_id)?.len() as u64;
            return Ok(Entry::new(cp_file_attr(inode_id, size)));
        }
        Err(VfsError::NotFound)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == self.slot.root_id() {
            return Ok(control_dir_attr(inode_id));
        }
        if self.is_file(inode_id) {
            return Ok(cp_file_attr(inode_id, self.content(inode_id)?.len() as u64));
        }
        Err(VfsError::NotFound)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == self.slot.root_id() {
            let mut entries = vec![
                DirEntry::new(inode_id, OsString::from("."), NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, OsString::from(".."), NodeKind::Directory),
            ];
            entries.extend(FILES.iter().map(|&(name, offset)| {
                DirEntry::new(
                    InodeId(self.slot.base + offset),
                    OsString::from(name),
                    NodeKind::RegularFile,
                )
            }));
            return Ok(entries);
        }
        Err(VfsError::NotDirectory)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.inode_id == self.slot.root_id() {
            return Err(VfsError::IsDirectory);
        }
        if !self.is_file(request.inode_id) {
            return Err(VfsError::NotFound);
        }
        Ok(OpenedFile {
            handle: FileHandle(self.next_handle.fetch_add(1, Ordering::Relaxed)),
            direct_io: false,
            keep_cache: false,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if request.inode_id == self.slot.root_id() {
            return Err(VfsError::IsDirectory);
        }
        let content = self.content(request.inode_id)?;
        slice_bytes(&content, request.offset, request.size)
    }









}
