//! `.vfs/cas` — the hash-addressed read-only portal, as a control-plane
//! feature.
//!
//! Intentionally **not listable** (content-addressed entries are addressable
//! by id, not enumerable — the "readdir blocked to prevent context-window
//! blowout" rule); `lookup` of any name under it yields a synthetic read-only
//! file whose contents echo the name, newline-terminated (a stub of the
//! by-hash object portal). Everything is served inside the feature's
//! [`FeatureSlot`] id slice.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use epoch_vfs_api::{
    DirEntry, Entry, FileHandle, InodeId, NodeAttr, NodeKind, OpenRequest, OpenedFile,
    ReadRequest, VfsError, VfsResult,
};

use crate::feature::{FeatureSlot, ReadOnlyFeature, control_dir_attr, cp_file_attr, slice_bytes};

/// The `.vfs/cas` feature. See the [module docs](self).
pub struct CasEchoDir {
    slot: FeatureSlot,
    state: RwLock<CasState>,
    next_id: AtomicU64,
}

/// Maps each looked-up `.vfs/cas/<id>` to and from its synthetic [`InodeId`].
#[derive(Default)]
struct CasState {
    id_to_cas: HashMap<InodeId, OsString>,
    cas_to_id: HashMap<OsString, InodeId>,
}

impl CasEchoDir {
    /// Build the feature in `slot`.
    pub fn new(slot: FeatureSlot) -> CasEchoDir {
        CasEchoDir {
            slot,
            state: RwLock::new(CasState::default()),
            // base is the root dir; dynamic ids and handles follow it.
            next_id: AtomicU64::new(slot.base + 1),
        }
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, CasState>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("cas dir read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, CasState>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("cas dir write lock poisoned".to_owned()))
    }

    /// Assign (or reuse) a synthetic node id for cas id `name`.
    fn cas_node_id(&self, name: &OsStr) -> VfsResult<InodeId> {
        if let Some(inode_id) = self.read_state()?.cas_to_id.get(name) {
            return Ok(*inode_id);
        }
        let mut state = self.write_state()?;
        // Re-check under the write lock to avoid a race minting two ids.
        if let Some(inode_id) = state.cas_to_id.get(name) {
            return Ok(*inode_id);
        }
        let inode_id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.cas_to_id.insert(name.to_owned(), inode_id);
        state.id_to_cas.insert(inode_id, name.to_owned());
        Ok(inode_id)
    }

    /// The cas id string a synthetic node id addresses, if any.
    fn cas_name(&self, inode_id: InodeId) -> VfsResult<Option<OsString>> {
        Ok(self.read_state()?.id_to_cas.get(&inode_id).cloned())
    }
}

/// Contents of a `.vfs/cas/<cas-id>` file: the id itself, newline-terminated.
fn cas_content(name: &OsStr) -> Vec<u8> {
    let mut bytes = os_str_bytes(name);
    bytes.push(b'\n');
    bytes
}

#[cfg(unix)]
fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

impl ReadOnlyFeature for CasEchoDir {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if parent == self.slot.root_id() {
            let inode_id = self.cas_node_id(name)?;
            let size = cas_content(name).len() as u64;
            return Ok(Entry::new(cp_file_attr(inode_id, size)));
        }
        // cas entries are files: they have no children.
        Err(VfsError::NotFound)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if inode_id == self.slot.root_id() {
            return Ok(control_dir_attr(inode_id));
        }
        let name = self.cas_name(inode_id)?.ok_or(VfsError::NotFound)?;
        Ok(cp_file_attr(inode_id, cas_content(&name).len() as u64))
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if inode_id == self.slot.root_id() {
            // Intentionally not listable: entries are addressable by id only.
            return Ok(vec![
                DirEntry::new(inode_id, OsString::from("."), NodeKind::Directory),
                DirEntry::new(self.slot.vfs_dir_id, OsString::from(".."), NodeKind::Directory),
            ]);
        }
        Err(VfsError::NotDirectory)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if request.inode_id == self.slot.root_id() {
            return Err(VfsError::IsDirectory);
        }
        self.cas_name(request.inode_id)?.ok_or(VfsError::NotFound)?;
        Ok(OpenedFile {
            handle: FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed)),
            direct_io: false,
            keep_cache: false,
        })
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if request.inode_id == self.slot.root_id() {
            return Err(VfsError::IsDirectory);
        }
        let name = self.cas_name(request.inode_id)?.ok_or(VfsError::NotFound)?;
        slice_bytes(&cas_content(&name), request.offset, request.size)
    }









}
