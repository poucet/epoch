//! [`RouterVfs`] — `.epochignore`-driven pass-through subtrees.
//!
//! The overlay captures every write into the CAS. Some subtrees should never
//! be captured (`node_modules`, `.git`, `.jj`, package-manager caches, …):
//! their reads *and* writes should go straight to the lower host directory and
//! stay invisible to checkpoints. This decorator implements that routing:
//!
//! - `overlay` — the copy-on-write stack (in the durable mount:
//!   `SignalVfs(CasVfs)`, so routed traffic never emits checkpoint trigger
//!   signals).
//! - `passthrough` — a [`PassthroughVfs::writable`](crate::PassthroughVfs::writable)
//!   over the same lower directory, taking direct reads and writes.
//! - `rules` — a compiled [`Gitignore`] loaded from `<lower>/.epochignore`
//!   (gitignore syntax, rooted at the lower directory). A path that matches —
//!   with [`Gitignore::matched_path_or_any_parents`] semantics, so a match on
//!   a directory routes its whole subtree — is served by `passthrough`.
//!
//! ## Id partition
//!
//! Routed nodes get synthetic ids in `[ROUTE_BASE, 0x8000_0000_0000_0000)` —
//! *below* [`CP_BASE`](crate::control_plane::CP_BASE), because the router sits
//! inside [`ControlPlaneVfs`](crate::ControlPlaneVfs) (which delegates
//! everything below `CP_BASE` down to it). Synthetic ids and handles map
//! bidirectionally to passthrough-local ones, exactly like
//! [`HistoryDir`](crate::HistoryDir) does for snapshot-local ids. Everything
//! outside the router range delegates to `overlay`; the router assumes the
//! overlay mints its session ids well below `ROUTE_BASE` (it starts at 1).
//!
//! ## Path tracking
//!
//! Rules match on mount-relative paths, so the router records the path of
//! every overlay node it sees traverse `lookup`/`readdir`/`create`/`mkdir`
//! (root = `""`). A `lookup` from an overlay directory evaluates
//! `parent_path/name` against the rules; on a match the same path is resolved
//! in the passthrough (component by component from its root) and the remapped
//! entry is returned. Directory-only patterns (`node_modules/`) route only
//! when the lower node actually is a directory.
//!
//! ## v1 caveats (documented, not solved)
//!
//! - **Shadowing**: pre-existing overlay-captured content under a
//!   newly-ignored path is shadowed by the lower while the lower node exists;
//!   if the path is missing in the lower, the overlay content shows through
//!   unrouted (rule matches only route to nodes the lower can resolve).
//! - **History**: `.vfs/history` snapshots show ignored paths as plain
//!   read-only fall-through of the *current* lower — routed content is never
//!   part of a checkpoint delta.
//! - **No live reload**: rules load once at mount time; editing
//!   `.epochignore` takes effect on the next mount.
//! - **Creation needs a lower parent**: `create`/`mkdir`/`unlink`/`rmdir` of a
//!   rule-matching path from an overlay directory route to the host only when
//!   the parent directory exists in the lower; otherwise the op falls back to
//!   the overlay.
//! - Stale path records of deleted overlay nodes are not pruned; they are
//!   refreshed by the next lookup of the name.

use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use epoch_vfs_api::{
    CreateRequest, CreatedNode, DirEntry, DirEntryPlus, Entry, FileHandle, InodeId, NodeAttr, NodeKind,
    OpenRequest, OpenedFile, ROOT_INODE_ID, ReadRequest, SetAttrRequest, StatFs, VfsError,
    VfsResult, VfsService, WriteRequest,
};

/// Name of the rules file in the lower directory root.
pub const EPOCHIGNORE_FILE: &str = ".epochignore";

/// First [`InodeId`]/[`FileHandle`] value owned by the router.
///
/// The router's range ends at [`CP_BASE`](crate::control_plane::CP_BASE)
/// (`0x8000_0000_0000_0000`), which the enclosing control plane owns.
pub const ROUTE_BASE: u64 = 0x4000_0000_0000_0000;

/// One past the router's id range ([`CP_BASE`](crate::control_plane::CP_BASE)).
const ROUTE_END: u64 = crate::control_plane::CP_BASE;

/// Returns `true` if `id` belongs to the router's synthetic range.
fn is_routed(id: u64) -> bool {
    (ROUTE_BASE..ROUTE_END).contains(&id)
}

/// The `.epochignore` routing decorator. See the [module docs](self).
pub struct RouterVfs {
    overlay: Arc<dyn VfsService>,
    passthrough: Arc<dyn VfsService>,
    rules: Gitignore,
    state: RwLock<RouterState>,
    next_id: AtomicU64,
}

/// Synthetic ↔ passthrough-local id/handle maps plus overlay path records.
#[derive(Default)]
struct RouterState {
    /// Synthetic id → passthrough-local id.
    to_local: HashMap<InodeId, InodeId>,
    /// Passthrough-local id → synthetic id (for minting idempotence).
    from_local: HashMap<InodeId, InodeId>,
    /// Synthetic handle → passthrough-local handle.
    handles: HashMap<FileHandle, FileHandle>,
    /// Overlay inode → mount-relative path (root = `""`), for rule matching.
    overlay_paths: HashMap<InodeId, PathBuf>,
    /// Mount-relative path → lower InodeId cache to avoid re-walking components from root.
    lower_inodes: HashMap<PathBuf, InodeId>,
}

impl RouterVfs {
    /// Route `rules`-matching paths of `overlay` to `passthrough`.
    ///
    /// `passthrough` should be a
    /// [`PassthroughVfs::writable`](crate::PassthroughVfs::writable) over the
    /// same lower directory the overlay falls through to; `rules` typically
    /// comes from [`load_rules`](Self::load_rules).
    pub fn new(
        overlay: Arc<dyn VfsService>,
        passthrough: Arc<dyn VfsService>,
        rules: Gitignore,
    ) -> Self {
        Self {
            overlay,
            passthrough,
            rules,
            state: RwLock::new(RouterState {
                overlay_paths: HashMap::from([(ROOT_INODE_ID, PathBuf::new())]),
                lower_inodes: HashMap::from([(PathBuf::new(), ROOT_INODE_ID)]),
                ..RouterState::default()
            }),
            next_id: AtomicU64::new(ROUTE_BASE),
        }
    }

    /// Compile `<lower_dir>/.epochignore` into a [`Gitignore`] rooted at
    /// `lower_dir`. A missing file compiles to an empty rule set (routes
    /// nothing).
    pub fn load_rules(lower_dir: &Path) -> VfsResult<Gitignore> {
        let mut builder = GitignoreBuilder::new(lower_dir);
        let file = lower_dir.join(EPOCHIGNORE_FILE);
        if file.is_file()
            && let Some(err) = builder.add(&file)
        {
            return Err(VfsError::InvalidInput(format!("{EPOCHIGNORE_FILE}: {err}")));
        }
        builder
            .build()
            .map_err(|err| VfsError::InvalidInput(format!("{EPOCHIGNORE_FILE}: {err}")))
    }

    fn read_state(&self) -> VfsResult<std::sync::RwLockReadGuard<'_, RouterState>> {
        self.state
            .read()
            .map_err(|_| VfsError::Other("router read lock poisoned".to_owned()))
    }

    fn write_state(&self) -> VfsResult<std::sync::RwLockWriteGuard<'_, RouterState>> {
        self.state
            .write()
            .map_err(|_| VfsError::Other("router write lock poisoned".to_owned()))
    }

    /// The stable synthetic id for a passthrough-local id (minted on first use).
    fn synthetic_id(&self, local: InodeId) -> VfsResult<InodeId> {
        if let Some(id) = self.read_state()?.from_local.get(&local) {
            return Ok(*id);
        }
        let mut state = self.write_state()?;
        // Re-check under the write lock to avoid a race minting two ids.
        if let Some(id) = state.from_local.get(&local) {
            return Ok(*id);
        }
        let id = InodeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        state.from_local.insert(local, id);
        state.to_local.insert(id, local);
        Ok(id)
    }

    /// The passthrough-local id a synthetic id stands for.
    fn local_of(&self, id: InodeId) -> VfsResult<InodeId> {
        self.read_state()?
            .to_local
            .get(&id)
            .copied()
            .ok_or(VfsError::NotFound)
    }

    /// The passthrough-local handle a synthetic handle stands for.
    fn local_handle(&self, handle: FileHandle) -> VfsResult<FileHandle> {
        self.read_state()?
            .handles
            .get(&handle)
            .copied()
            .ok_or(VfsError::NotFound)
    }

    /// Rewrite a passthrough-local attr into the synthetic id space.
    fn remap_attr(&self, mut attr: NodeAttr) -> VfsResult<NodeAttr> {
        attr.inode_id = self.synthetic_id(attr.inode_id)?;
        Ok(attr)
    }

    /// Rewrite a passthrough-local entry into the synthetic id space.
    fn remap_entry(&self, entry: Entry) -> VfsResult<Entry> {
        Ok(Entry {
            attr: self.remap_attr(entry.attr)?,
            ..entry
        })
    }

    /// Mint a synthetic handle for `local` and wrap the opened file.
    fn remap_opened(&self, opened: OpenedFile) -> VfsResult<OpenedFile> {
        let handle = FileHandle(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.write_state()?.handles.insert(handle, opened.handle);
        Ok(OpenedFile { handle, ..opened })
    }

    /// The recorded mount-relative path of an overlay node, if known.
    fn overlay_path(&self, inode_id: InodeId) -> VfsResult<Option<PathBuf>> {
        Ok(self.read_state()?.overlay_paths.get(&inode_id).cloned())
    }

    /// Record the mount-relative path of an overlay node.
    fn record_path(&self, inode_id: InodeId, path: PathBuf) -> VfsResult<()> {
        self.write_state()?.overlay_paths.insert(inode_id, path);
        Ok(())
    }

    /// Record a batch of mount-relative paths in a single write lock acquisition.
    fn record_paths_batch(&self, paths: impl IntoIterator<Item = (InodeId, PathBuf)>) -> VfsResult<()> {
        let mut state = self.write_state()?;
        state.overlay_paths.extend(paths);
        Ok(())
    }

    /// Does `path` (with the given kind) match the routing rules, itself or
    /// via any parent directory?
    fn path_matches(&self, path: &Path, is_dir: bool) -> bool {
        self.rules.matched_path_or_any_parents(path, is_dir).is_ignore()
    }

    /// Resolve a mount-relative path in the passthrough, walking components
    /// from its root via repeated `lookup`, caching resolved lower InodeIds.
    fn resolve_lower(&self, path: &Path) -> VfsResult<Entry> {
        {
            let state = self.read_state()?;
            if let Some(&id) = state.lower_inodes.get(path) {
                return self.passthrough.getattr(id).map(Entry::new);
            }
        }
        let mut node = self.passthrough.getattr(ROOT_INODE_ID).map(Entry::new)?;
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(VfsError::InvalidInput(format!(
                    "non-normal path component in {path:?}"
                )));
            };
            node = self.passthrough.lookup(node.attr.inode_id, name)?;
        }
        self.write_state()?
            .lower_inodes
            .insert(path.to_path_buf(), node.attr.inode_id);
        Ok(node)
    }

    /// If `path` routes to the lower, resolve it there and return the
    /// **passthrough-local** entry. `None` when the rules don't match or the
    /// lower cannot resolve the path (an overlay-only artifact stays with the
    /// overlay — see the module-docs shadowing caveat).
    fn route_for(&self, path: &Path) -> VfsResult<Option<Entry>> {
        // Cheap pre-check with both kinds; the strict check below re-evaluates
        // with the lower node's actual kind (so `dir/`-only patterns never
        // route a same-named lower file).
        if !self.path_matches(path, false) && !self.path_matches(path, true) {
            return Ok(None);
        }
        match self.resolve_lower(path) {
            Ok(entry) => {
                let is_dir = entry.attr.kind == NodeKind::Directory;
                if self.path_matches(path, is_dir) {
                    Ok(Some(entry))
                } else {
                    Ok(None)
                }
            }
            Err(VfsError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// The passthrough-local id of an overlay directory's counterpart in the
    /// lower, for routing a mutation (`create`/`mkdir`/`unlink`/`rmdir`) whose
    /// target path matches the rules.
    fn lower_dir_id(&self, parent_path: &Path) -> VfsResult<InodeId> {
        Ok(self.resolve_lower(parent_path)?.attr.inode_id)
    }

    /// For a mutation named `name` under an overlay parent: the parent's path
    /// and the target's mount-relative path, when the parent's path is known.
    fn overlay_child_path(&self, parent: InodeId, name: &OsStr) -> VfsResult<Option<(PathBuf, PathBuf)>> {
        let Some(parent_path) = self.overlay_path(parent)? else {
            return Ok(None);
        };
        let child = parent_path.join(name);
        Ok(Some((parent_path, child)))
    }
}

impl VfsService for RouterVfs {
    fn lookup(&self, parent: InodeId, name: &OsStr) -> VfsResult<Entry> {
        if self.rules.is_empty() {
            return self.overlay.lookup(parent, name);
        }
        if is_routed(parent.0) {
            let local = self.local_of(parent)?;
            let entry = self.passthrough.lookup(local, name)?;
            return self.remap_entry(entry);
        }
        if name != OsStr::new(".") && name != OsStr::new("..")
            && let Some((_, child)) = self.overlay_child_path(parent, name)?
        {
            if let Some(local_entry) = self.route_for(&child)? {
                return self.remap_entry(local_entry);
            }
            let entry = self.overlay.lookup(parent, name)?;
            self.record_path(entry.attr.inode_id, child)?;
            return Ok(entry);
        }
        self.overlay.lookup(parent, name)
    }

    fn getattr(&self, inode_id: InodeId) -> VfsResult<NodeAttr> {
        if self.rules.is_empty() {
            return self.overlay.getattr(inode_id);
        }
        if is_routed(inode_id.0) {
            let attr = self.passthrough.getattr(self.local_of(inode_id)?)?;
            return self.remap_attr(attr);
        }
        self.overlay.getattr(inode_id)
    }

    fn readdir(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntry>> {
        if self.rules.is_empty() {
            return self.overlay.readdir(inode_id);
        }
        if is_routed(inode_id.0) {
            let mut entries = self.passthrough.readdir(self.local_of(inode_id)?)?;
            for entry in &mut entries {
                entry.inode_id = self.synthetic_id(entry.inode_id)?;
            }
            return Ok(entries);
        }
        let mut entries = self.overlay.readdir(inode_id)?;
        if let Some(dir_path) = self.overlay_path(inode_id)? {
            let mut new_paths = Vec::new();
            for entry in &mut entries {
                if entry.name == OsStr::new(".") || entry.name == OsStr::new("..") {
                    continue;
                }
                let child = dir_path.join(&entry.name);
                if let Some(local_entry) = self.route_for(&child)? {
                    entry.inode_id = self.synthetic_id(local_entry.attr.inode_id)?;
                    entry.kind = local_entry.attr.kind;
                } else {
                    new_paths.push((entry.inode_id, child));
                }
            }
            if !new_paths.is_empty() {
                self.record_paths_batch(new_paths)?;
            }
        }
        Ok(entries)
    }

    fn readdir_plus(&self, inode_id: InodeId) -> VfsResult<Vec<DirEntryPlus>> {
        if self.rules.is_empty() {
            return self.overlay.readdir_plus(inode_id);
        }
        if is_routed(inode_id.0) {
            let mut entries = self.passthrough.readdir_plus(self.local_of(inode_id)?)?;
            for entry in &mut entries {
                entry.inode_id = self.synthetic_id(entry.inode_id)?;
                entry.attr = self.remap_attr(entry.attr.clone())?;
            }
            return Ok(entries);
        }
        let mut entries = self.overlay.readdir_plus(inode_id)?;
        if let Some(dir_path) = self.overlay_path(inode_id)? {
            let mut new_paths = Vec::new();
            for entry in &mut entries {
                if entry.name == OsStr::new(".") || entry.name == OsStr::new("..") {
                    continue;
                }
                let child = dir_path.join(&entry.name);
                if let Some(local_entry) = self.route_for(&child)? {
                    entry.inode_id = self.synthetic_id(local_entry.attr.inode_id)?;
                    entry.kind = local_entry.attr.kind;
                    entry.attr = self.remap_attr(local_entry.attr)?;
                } else {
                    new_paths.push((entry.inode_id, child));
                }
            }
            if !new_paths.is_empty() {
                self.record_paths_batch(new_paths)?;
            }
        }
        Ok(entries)
    }

    fn open(&self, request: OpenRequest) -> VfsResult<OpenedFile> {
        if is_routed(request.inode_id.0) {
            let opened = self.passthrough.open(OpenRequest {
                inode_id: self.local_of(request.inode_id)?,
                ..request
            })?;
            return self.remap_opened(opened);
        }
        self.overlay.open(request)
    }

    fn read(&self, request: ReadRequest) -> VfsResult<Vec<u8>> {
        if is_routed(request.inode_id.0) {
            return self.passthrough.read(ReadRequest {
                inode_id: self.local_of(request.inode_id)?,
                handle: self.local_handle(request.handle)?,
                ..request
            });
        }
        self.overlay.read(request)
    }

    fn write(&self, request: WriteRequest<'_>) -> VfsResult<u32> {
        if is_routed(request.inode_id.0) {
            return self.passthrough.write(WriteRequest {
                inode_id: self.local_of(request.inode_id)?,
                handle: self.local_handle(request.handle)?,
                ..request
            });
        }
        self.overlay.write(request)
    }

    fn create(&self, request: CreateRequest) -> VfsResult<CreatedNode> {
        if self.rules.is_empty() {
            return self.overlay.create(request);
        }
        if is_routed(request.parent.0) {
            let created = self.passthrough.create(CreateRequest {
                parent: self.local_of(request.parent)?,
                ..request
            })?;
            return Ok(CreatedNode {
                entry: self.remap_entry(created.entry)?,
                opened: created.opened.map(|o| self.remap_opened(o)).transpose()?,
            });
        }
        if let Some((parent_path, child)) = self.overlay_child_path(request.parent, &request.name)?
            && self.path_matches(&child, request.kind == NodeKind::Directory)
            && let Ok(local_parent) = self.lower_dir_id(&parent_path)
        {
            let created = self.passthrough.create(CreateRequest {
                parent: local_parent,
                ..request
            })?;
            return Ok(CreatedNode {
                entry: self.remap_entry(created.entry)?,
                opened: created.opened.map(|o| self.remap_opened(o)).transpose()?,
            });
        }
        let created = self.overlay.create(request.clone())?;
        if let Some((_, child)) = self.overlay_child_path(request.parent, &request.name)? {
            self.record_path(created.entry.attr.inode_id, child)?;
        }
        Ok(created)
    }

    fn mkdir(&self, request: CreateRequest) -> VfsResult<Entry> {
        if self.rules.is_empty() {
            return self.overlay.mkdir(request);
        }
        if is_routed(request.parent.0) {
            let entry = self.passthrough.mkdir(CreateRequest {
                parent: self.local_of(request.parent)?,
                ..request
            })?;
            return self.remap_entry(entry);
        }
        if let Some((parent_path, child)) = self.overlay_child_path(request.parent, &request.name)?
            && self.path_matches(&child, request.kind == NodeKind::Directory)
            && let Ok(local_parent) = self.lower_dir_id(&parent_path)
        {
            let entry = self.passthrough.mkdir(CreateRequest {
                parent: local_parent,
                ..request
            })?;
            return self.remap_entry(entry);
        }
        let entry = self.overlay.mkdir(request.clone())?;
        if let Some((_, child)) = self.overlay_child_path(request.parent, &request.name)? {
            self.record_path(entry.attr.inode_id, child)?;
        }
        Ok(entry)
    }

    fn unlink(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        if self.rules.is_empty() {
            return self.overlay.unlink(parent, name);
        }
        if is_routed(parent.0) {
            return self.passthrough.unlink(self.local_of(parent)?, name);
        }
        if let Some((parent_path, child)) = self.overlay_child_path(parent, name)?
            && self.route_for(&child)?.is_some()
            && let Ok(local_parent) = self.lower_dir_id(&parent_path)
        {
            return self.passthrough.unlink(local_parent, name);
        }
        self.overlay.unlink(parent, name)
    }

    fn rmdir(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        if self.rules.is_empty() {
            return self.overlay.rmdir(parent, name);
        }
        if is_routed(parent.0) {
            return self.passthrough.rmdir(self.local_of(parent)?, name);
        }
        if let Some((parent_path, child)) = self.overlay_child_path(parent, name)?
            && self.route_for(&child)?.is_some()
            && let Ok(local_parent) = self.lower_dir_id(&parent_path)
        {
            return self.passthrough.rmdir(local_parent, name);
        }
        self.overlay.rmdir(parent, name)
    }

    fn rename(
        &self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()> {
        if self.rules.is_empty() {
            return self.overlay.rename(parent, name, new_parent, new_name);
        }
        match (is_routed(parent.0), is_routed(new_parent.0)) {
            // Entirely inside a routed subtree: the host handles it.
            (true, true) => self.passthrough.rename(
                self.local_of(parent)?,
                name,
                self.local_of(new_parent)?,
                new_name,
            ),
            // Crossing the overlay ↔ host boundary cannot be a rename; EXDEV
            // makes callers fall back to copy + delete, which routes each
            // side correctly.
            (true, false) | (false, true) => Err(VfsError::CrossDevice),
            (false, false) => {
                // Both parents are overlay-side, but either *name* may still
                // match the routing rules (e.g. `mv build node_modules`).
                let src_matches = match self.overlay_child_path(parent, name)? {
                    Some((_, child)) => self.route_for(&child)?.is_some(),
                    None => false,
                };
                let dst_matches = match self.overlay_child_path(new_parent, new_name)? {
                    Some((_, child)) => {
                        self.path_matches(&child, false) || self.path_matches(&child, true)
                    }
                    None => false,
                };
                if src_matches || dst_matches {
                    return Err(VfsError::CrossDevice);
                }
                self.overlay.rename(parent, name, new_parent, new_name)
            }
        }
    }

    fn truncate(&self, inode_id: InodeId, size: u64) -> VfsResult<NodeAttr> {
        if is_routed(inode_id.0) {
            let attr = self.passthrough.truncate(self.local_of(inode_id)?, size)?;
            return self.remap_attr(attr);
        }
        self.overlay.truncate(inode_id, size)
    }

    fn symlink(&self, parent: InodeId, name: &OsStr, target: &OsStr) -> VfsResult<Entry> {
        if is_routed(parent.0) {
            let entry = self.passthrough.symlink(self.local_of(parent)?, name, target)?;
            return self.remap_entry(entry);
        }
        if let Some((parent_path, child)) = self.overlay_child_path(parent, name)?
            && self.path_matches(&child, false)
            && let Ok(local_parent) = self.lower_dir_id(&parent_path)
        {
            let entry = self.passthrough.symlink(local_parent, name, target)?;
            return self.remap_entry(entry);
        }
        let entry = self.overlay.symlink(parent, name, target)?;
        if let Some((_, child)) = self.overlay_child_path(parent, name)? {
            self.record_path(entry.attr.inode_id, child)?;
        }
        Ok(entry)
    }

    fn readlink(&self, inode_id: InodeId) -> VfsResult<std::ffi::OsString> {
        if is_routed(inode_id.0) {
            return self.passthrough.readlink(self.local_of(inode_id)?);
        }
        self.overlay.readlink(inode_id)
    }

    fn setattr(&self, request: SetAttrRequest) -> VfsResult<NodeAttr> {
        if is_routed(request.inode_id.0) {
            let attr = self.passthrough.setattr(SetAttrRequest {
                inode_id: self.local_of(request.inode_id)?,
                ..request
            })?;
            return self.remap_attr(attr);
        }
        self.overlay.setattr(request)
    }

    fn statfs(&self, inode_id: InodeId) -> VfsResult<StatFs> {
        if is_routed(inode_id.0) {
            return self.passthrough.statfs(self.local_of(inode_id)?);
        }
        self.overlay.statfs(inode_id)
    }

    fn flush(&self, handle: FileHandle) -> VfsResult<()> {
        if is_routed(handle.0) {
            return self.passthrough.flush(self.local_handle(handle)?);
        }
        self.overlay.flush(handle)
    }

    fn fsync(&self, inode_id: InodeId, handle: FileHandle, datasync: bool) -> VfsResult<()> {
        if is_routed(inode_id.0) {
            return self.passthrough.fsync(
                self.local_of(inode_id)?,
                self.local_handle(handle)?,
                datasync,
            );
        }
        self.overlay.fsync(inode_id, handle, datasync)
    }

    fn release(&self, handle: FileHandle) -> VfsResult<()> {
        if is_routed(handle.0) {
            let removed = self.write_state()?.handles.remove(&handle);
            return match removed {
                Some(local) => self.passthrough.release(local),
                None => Ok(()),
            };
        }
        self.overlay.release(handle)
    }
}
