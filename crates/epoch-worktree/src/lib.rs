    //! # epoch-worktree — the mutable copy-on-write overlay
//!
//! `WorkingTree` is the live, mutable view that sits *above* the immutable
//! content-addressed object store (`epoch-file-cas` + `epoch-tree`) and *below*
//! a VFS/FUSE adapter. It is the home of the two
//! things the object layer deliberately lacks:
//!
//! - **mutable identity** — stable [`InodeId`] inodes that survive edits, so a
//!   file keeps its identity while its content (and thus its `BlobId`) changes;
//! - **the read/write bridge** — [`from_root`](WorkingTree::from_root) turns a
//!   [`TreeId`] into a tree of inodes, and [`seal`](WorkingTree::seal) walks the
//!   dirty inodes back down into fresh immutable objects, returning a new root
//!   [`TreeId`].
//!
//! ## ONE service, always an overlay
//!
//! `WorkingTree` is *the* copy-on-write overlay. It is a single type that holds
//! an **upper spine** (the actively-mutated inodes), **tombstones** (whiteouts
//! that hide lower entries), and an optional **lower** [`VfsService`] it falls
//! through to for everything it has not itself materialized. There is **one
//! code path**: the tree always seals an overlay-link tree (a [`TreeStore`] of
//! [`Overlay`]-link objects). The lower is simply optional:
//!
//! - `lower = Some(..)` — a *sparse* copy-on-write layer over a lower service.
//!   [`overlay`](WorkingTree::overlay) starts an empty layer;
//!   [`from_overlay`](WorkingTree::from_overlay) rehydrates a persisted delta on
//!   top of a lower. Only the actively-mutated spine is stored; every untouched
//!   name falls through to the lower.
//! - `lower = None` — an **empty lower**: nothing falls through, so no tombstone
//!   is ever created and the sealed tree is self-contained — a complete,
//!   standalone snapshot. [`new`](WorkingTree::new) /
//!   [`from_root`](WorkingTree::from_root) build this. (A tombstone-free overlay
//!   tree is byte-identical to a complete tree, so these seal to the same
//!   [`TreeId`]s a dedicated "pure" mode would.)
//!
//! The fully-flattened, *provably*-no-fall-through artifact (an `OpaqueTree`)
//! belongs to the future compactor, not here.
//!
//! The overlay **owns the [`InodeId`] space** (allocated from a monotonic
//! counter). Lower-service node ids live only *inside* the overlay's nodes
//! ([`LowerFile`](Body::LowerFile) and [`Dir::lower`](Body::Dir)); whenever an
//! attribute or dir-entry is derived from the lower, its `inode_id` is **remapped**
//! to the overlay's id, so a lower id is never leaked to a caller and a node's
//! identity is stable across repeated lookups (and matches `readdir`).
//!
//! It is intentionally **FUSE-independent** (it speaks the `epoch-vfs-api` types
//! but knows nothing of the kernel protocol) so it can be unit-tested on its own.
//!
//! ## First-cut scope (v1)
//!
//! - Inodes are **session-scoped**: allocated from a monotonic counter, stable
//!   for the life of this `WorkingTree`, not persisted and not preserved across
//!   history. Hardlinks, a durable inode table, and per-inode uid/gid are future
//!   work.
//! - [`from_root`](WorkingTree::from_root) / [`from_overlay`](WorkingTree::from_overlay)
//!   hydrate **lazily**: only the root is recorded, and each directory's children
//!   fault in from the CAS one spine level per access (see [`DirChildren`]). Lower
//!   entries are likewise materialized on first lookup/readdir.
//! - [`read_at`](WorkingTree::read_at) on a fall-through file opens/reads/releases
//!   the lower per call in v1; a per-handle cache is a later optimization.
//! - [`seal`](WorkingTree::seal) is **sparse**: only the materialized spine and
//!   tombstones are emitted; fall-through directories that produced nothing emit
//!   nothing, so untouched siblings are absent from the delta.
//! - **Symlinks are first-class overlay objects** (git-style): an upper symlink
//!   holds its target path bytes in memory and seals as a blob entry whose mode
//!   is [`SYMLINK_MODE`] (`0o120000`) — see `epoch-tree`'s entry docs. A
//!   lower (fall-through) symlink resolves its target from the lower on
//!   `readlink`. Mode bits (`chmod`) persist: they live in the tree entry's
//!   `mode` field, applied on copy-up and honored in synthesized attrs.
//!
//! ## Timestamps: in-memory only, never content-addressed
//!
//! `mtime`/`atime` live **only on the live inode**, never in the CAS — not in
//! the blob object, not in the tree object (the codec stays time-free). This is
//! a structural invariant, not a convention: two entries with identical content
//! and different mtimes seal to the *same* blob and tree ids, so dedup and
//! content addressing cannot be broken by a timestamp. (A time-in-tree design
//! would make every write re-hash its whole ancestry and destroy sharing across
//! checkpoints and forks.)
//!
//! The semantics, per node:
//!
//! - **explicit `setattr`** (`utimes(2)` / `FATTR_ATIME`/`FATTR_MTIME`) sets the
//!   in-memory times exactly, like `chmod` sets the mode. A fall-through node is
//!   copied up first so the times have an overlay home; the lower is never
//!   mutated.
//! - **writes and truncates** bump `mtime` (and `atime`) to the wall-clock now;
//!   creates initialize all three times to now.
//! - **copy-up carries the lower's times**: a fall-through file materialized on
//!   first write keeps the lower's `mtime`/`atime` until the write itself bumps
//!   them (the POSIX `cp` expectation).
//! - **reads never touch `atime`** — the mount behaves as if mounted `noatime`
//!   for implicit updates. Only an explicit `setattr` sets `atime`. (Updating
//!   atime per read would also arm the checkpoint trigger per read and destroy
//!   the debounce model.)
//! - **`ctime`** is the metadata-change instant (chmod/utimes/rename); it is
//!   live but *never persisted* anywhere — a fresh session reports the zeroed
//!   default.
//!
//! What that costs, stated plainly: times do **not** survive unmount → remount
//! (a remounted overlay shows the zeroed defaults), and `.vfs/history/*` /
//! `.vfs/cas` snapshots — which rehydrate from the time-free CAS tree — report
//! zeroed times too. Those are *content* views. `make`-style freshness works
//! within a mount session (a build does not span remounts); it does not resume
//! across one.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use epoch_cas::{Bytes, CASError, SyncByteBackend, SyncCAS};
use epoch_file_cas::{BlobId, FileCas, FileWriter};
use epoch_tree::{EntryId, EntryTime, Link, LinkKind, Overlay, SYMLINK_MODE, Tree, TreeEntry, TreeStore};
use epoch_vfs_api::{
    DirEntry, NodeAttr, InodeId, NodeKind, OpenMode, OpenRequest, ReadRequest, StatFs, VfsError,
    VfsResult, VfsService, ROOT_INODE_ID,
};

/// Re-exported so consumers of [`WorkingTree`] can name the sealed-tree id type
/// — returned by [`seal`](WorkingTree::seal), taken by
/// [`from_root`](WorkingTree::from_root) — without also depending on `epoch-tree`.
pub use epoch_tree::TreeId;

/// A mutable, in-memory copy-on-write overlay backed by content-addressed
/// storage.
///
/// Generic over two byte backends: `TBk` for tree/spine objects and `FBk` for
/// file content, defaulting to the same type so a single backend can serve both.
/// Both stores are built once at construction. Directories always seal as
/// [`Overlay`]-link tree objects (one code path); `lower = None` just means an
/// empty lower, which yields a self-contained, tombstone-free tree. See the
/// [crate docs](crate) for the layer's role and v1 scope.
pub struct WorkingTree<TBk, FBk = TBk> {
    /// Tree/spine objects, sealed as overlay-link trees. Built once.
    trees: TreeStore<TBk, OverlayLink>,
    /// Upper file content, addressed by [`BlobId`]. Built once.
    files: FileCas<FBk>,
    /// The overlay base, or `None` for an *empty lower*: nothing falls through,
    /// so no tombstone is ever created and the tree is a standalone snapshot.
    lower: Option<Arc<dyn VfsService>>,
    inodes: HashMap<InodeId, Inode>,
    /// Next inode number to hand out (monotonic; starts just past the root).
    next: u64,
}

/// The unix file-type bits mask (`S_IFMT`) — the part of a mode that says
/// dir / regular / symlink, preserved verbatim across a chmod.
const TYPE_BITS: u32 = 0o170000;

/// One node in the live tree: its mode bits, session timestamps, and
/// kind-specific body.
struct Inode {
    /// Unix mode bits as supplied by the caller / read from the sealed entry or
    /// the lower's attrs, preserved verbatim across seal/hydrate.
    mode: u32,
    /// Modification time — in-memory only (see the crate docs). Bumped by
    /// writes/truncates, set exactly by `setattr`, carried from the lower on
    /// copy-up, zeroed (the epoch) after a rehydrate.
    mtime: EntryTime,
    /// Access time — in-memory only. Updated **only** by an explicit `setattr`
    /// (the mount is effectively `noatime` for implicit updates); reads never
    /// touch it.
    atime: EntryTime,
    /// Metadata-change time — in-memory only and *not* settable via `setattr`:
    /// `chmod`/`utimes`/rename stamp it with the wall-clock now.
    ctime: EntryTime,
    body: Body,
}

impl Inode {
    /// A fresh inode with all session timestamps zeroed (used for nodes whose
    /// times are supplied by the caller / lower immediately after).
    fn bare(mode: u32, body: Body) -> Inode {
        Inode {
            mode,
            mtime: EntryTime::EPOCH,
            atime: EntryTime::EPOCH,
            ctime: EntryTime::EPOCH,
            body,
        }
    }

    /// A fresh inode with all session timestamps set to the wall-clock now
    /// (creates: file, directory, symlink).
    fn new(mode: u32, body: Body) -> Inode {
        let now = EntryTime::from_system_time(SystemTime::now());
        Inode {
            mode,
            mtime: now,
            atime: now,
            ctime: now,
            body,
        }
    }

    /// A fresh inode whose session timestamps are copied from a lower entry's
    /// attrs (fall-through materialization / copy-up): the overlay reports the
    /// lower's times until the node is written or explicitly re-stamped.
    fn with_times(mode: u32, body: Body, attr: &NodeAttr) -> Inode {
        let mut inode = Inode::bare(mode, body);
        inode.mtime = EntryTime::from_system_time(attr.mtime);
        inode.atime = EntryTime::from_system_time(attr.atime);
        inode
    }
}

enum Body {
    /// A directory in the upper spine. `children` is the upper child set,
    /// faulted in lazily: [`Sealed`](DirChildren::Sealed) until first accessed,
    /// then [`Open`](DirChildren::Open) — a live map of resolved children (upper
    /// values, tombstones, and cached fall-throughs). `lower` is the
    /// corresponding lower-service directory [`InodeId`] this dir overlays
    /// (`None` if the dir has no lower counterpart, e.g. a freshly `mkdir`'d
    /// dir).
    Dir {
        children: DirChildren,
        lower: Option<InodeId>,
        /// Set by `chmod` on this directory: forces the dir to seal an entry
        /// for itself even when it would otherwise be an invisible
        /// fall-through (no upper entries), so the new mode lands in the
        /// delta.
        mode_override: bool,
    },
    /// An upper-materialized regular file: `Clean(BlobId)` or `Dirty(FileWriter)`.
    File(FileBody),
    /// A fall-through regular file served from the lower service. The wrapped id
    /// is the file's *lower* [`InodeId`]; the overlay never exposes it directly.
    LowerFile(InodeId),
    /// An upper symlink: the target path bytes, verbatim. Symlink targets are
    /// immutable (a retarget is unlink + create), so there is no dirty/clean
    /// split; the bytes seal into the CAS as a blob on `seal`.
    Symlink(Bytes),
    /// A fall-through symlink served from the lower service (like
    /// [`LowerFile`](Body::LowerFile)); `readlink` delegates to the lower.
    LowerSymlink(InodeId),
    /// A whiteout: hides the lower entry of this name (overlay deletion marker).
    Tombstone,
}

enum FileBody {
    /// Content is sealed in the CAS under this id.
    Clean(BlobId),
    /// Content is being edited; the writer holds the live bytes until `seal`.
    Dirty(FileWriter),
}

/// A directory's upper children: lazily faulted in from the CAS.
///
/// A committed directory starts [`Sealed`](Self::Sealed) — just the [`TreeId`]
/// of its tree object, not yet read. The first access that needs the child set
/// ([`resolve_dir`](WorkingTree::resolve_dir)) loads *that one* tree object,
/// materializes its immediate children as inodes (sub-directories landing
/// `Sealed` again, so spines fault in one level at a time), and flips to
/// [`Open`](Self::Open). This keeps load cost proportional to the paths actually
/// touched and lets [`seal`](WorkingTree::seal) re-emit a never-touched subtree
/// by its original id without walking it.
enum DirChildren {
    /// Not yet faulted in: the tree-object id to load on first access.
    Sealed(TreeId),
    /// Faulted in / being edited: the live `name → inode` map.
    Open(BTreeMap<OsString, InodeId>),
}

impl DirChildren {
    /// The live child map. The caller must have run
    /// [`resolve_dir`](WorkingTree::resolve_dir) first; a still-`Sealed` dir here
    /// is an internal invariant violation.
    fn open(&self) -> VfsResult<&BTreeMap<OsString, InodeId>> {
        match self {
            DirChildren::Open(map) => Ok(map),
            DirChildren::Sealed(_) => Err(VfsError::Other("directory not resolved".into())),
        }
    }

    /// The live child map, mutably (see [`open`](Self::open)).
    fn open_mut(&mut self) -> VfsResult<&mut BTreeMap<OsString, InodeId>> {
        match self {
            DirChildren::Open(map) => Ok(map),
            DirChildren::Sealed(_) => Err(VfsError::Other("directory not resolved".into())),
        }
    }
}

/// One materialized overlay node, as reported by
/// [`overlay_entries`](WorkingTree::overlay_entries): the node's absolute
/// `path` (from the working tree's root, `/`-separated) and what `kind` of
/// delta it is.
///
/// The [`Display`](std::fmt::Display) rendering is the human-readable one-line
/// form used by debug listings: a plain path for a file, a trailing `/` for a
/// directory, and ` (tombstone)` / ` (sealed subtree)` annotations for the
/// non-plain kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEntry {
    /// Absolute path from the working tree's root (leading `/`, no trailing
    /// slash — [`Display`](std::fmt::Display) adds one for directories). Names
    /// are rendered lossily where they are not valid UTF-8.
    pub path: String,
    /// What kind of materialized node the path is.
    pub kind: OverlayEntryKind,
}

/// The kind of a materialized overlay node (see [`OverlayEntry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayEntryKind {
    /// An upper-materialized regular file (created or copied up; clean or dirty).
    File,
    /// An upper-materialized symlink (created in the overlay or copied up).
    Symlink,
    /// A directory created in the upper (no lower counterpart).
    Dir,
    /// A whiteout hiding the lower entry of this name.
    Tombstone,
    /// A still-[`Sealed`](DirChildren::Sealed) directory: part of the delta but
    /// never faulted in this session, reported **without** decoding its subtree.
    SealedDir,
}

impl std::fmt::Display for OverlayEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            OverlayEntryKind::File => write!(f, "{}", self.path),
            OverlayEntryKind::Symlink => write!(f, "{} (symlink)", self.path),
            OverlayEntryKind::Dir => write!(f, "{}/", self.path),
            OverlayEntryKind::Tombstone => write!(f, "{} (tombstone)", self.path),
            OverlayEntryKind::SealedDir => write!(f, "{}/ (sealed subtree)", self.path),
        }
    }
}

/// The link sealed and hydrated for every tree: a populated entry (blob or
/// subtree) or a [`Tombstone`](Overlay::Tombstone) whiteout. With an empty lower
/// no tombstone is ever produced, so the tree is byte-identical to a complete
/// (tombstone-free) tree.
type OverlayLink = Overlay<EntryId<BlobId>>;

/// How a directory child seals — computed before mutating `self` so the read
/// borrow on the inode map is released before any recursive seal / file commit.
#[derive(Clone, Copy)]
enum ChildKind {
    Dir,
    File,
    Symlink,
    Tombstone,
    Lower,
}

/// Map a kernel CAS error into a VFS I/O error.
fn io(err: CASError) -> VfsError {
    VfsError::Io(err.to_string())
}

/// Convenience constructors that use a **single** backend for both the tree and
/// file stores (the common case). For independent backends — e.g. a fast tree
/// tier over a separate blob tier — see [`with_stores`](WorkingTree::with_stores)
/// and [`from_root_with_stores`](WorkingTree::from_root_with_stores).
impl<Bk: SyncByteBackend + Clone> WorkingTree<Bk, Bk> {
    /// Create an empty standalone working tree (`lower = None`): a single root
    /// directory (mode `0o755`), no base layer.
    pub fn new(backend: Bk) -> Self {
        Self::with_root(backend.clone(), backend, None, 0o040755, None)
    }

    /// Hydrate a standalone working tree from a sealed root [`TreeId`].
    ///
    /// Hydration is lazy: only the root is recorded here (as
    /// [`Sealed`](DirChildren::Sealed)); each directory's children fault in from
    /// the CAS on first access (see [`DirChildren`]). The `backend` must be able
    /// to read the objects `root` references — pass a clone of the backend the
    /// tree was sealed into.
    pub fn from_root(backend: Bk, root: TreeId) -> VfsResult<Self> {
        let mut wt = Self::new(backend);
        wt.mark_root_sealed(root);
        Ok(wt)
    }

    /// Create an empty **overlay** over `lower`: the root falls through to the
    /// lower's root ([`ROOT_INODE_ID`]). The root's mode is taken from the
    /// lower's root attrs (fallback `0o755`).
    pub fn overlay(backend: Bk, lower: Arc<dyn VfsService>) -> Self {
        let mode = lower
            .getattr(ROOT_INODE_ID)
            .map(|a| 0o040000 | (a.perm as u32 & 0o7777))
            .unwrap_or(0o040755);
        Self::with_root(backend.clone(), backend, Some(lower), mode, Some(ROOT_INODE_ID))
    }

    /// Rehydrate a persisted delta layer on top of `lower`.
    ///
    /// `delta` is a sparse overlay tree (as produced by [`seal`](Self::seal)):
    /// on fault-in, a `Blob` entry becomes an upper [`File`](Body::File) (clean),
    /// a `Tree` becomes an upper [`Dir`](Body::Dir), and a `Tombstone` becomes a
    /// [`Tombstone`](Body::Tombstone) whiteout. Hydration is lazy — only the root
    /// is recorded here (as [`Sealed`](DirChildren::Sealed)); deeper directories
    /// fault in on first access, each resolving its `lower` counterpart one step
    /// down. Names absent from the delta fall through to the lower.
    pub fn from_overlay(backend: Bk, lower: Arc<dyn VfsService>, delta: TreeId) -> VfsResult<Self> {
        let mut wt = Self::overlay(backend, lower);
        // The root's lower counterpart (the lower's root) is wired by `overlay`.
        wt.mark_root_sealed(delta);
        Ok(wt)
    }
}

impl<TBk: SyncByteBackend + Clone, FBk: SyncByteBackend + Clone> WorkingTree<TBk, FBk> {
    /// Create an empty standalone working tree with **independent** tree and file
    /// backends (e.g. a fast tree tier and a separate blob tier).
    pub fn with_stores(tree_backend: TBk, file_backend: FBk) -> Self {
        Self::with_root(tree_backend, file_backend, None, 0o040755, None)
    }

    /// Hydrate a standalone working tree from a sealed root with independent tree
    /// and file backends. Lazy, like [`from_root`](Self::from_root).
    pub fn from_root_with_stores(
        tree_backend: TBk,
        file_backend: FBk,
        root: TreeId,
    ) -> VfsResult<Self> {
        let mut wt = Self::with_stores(tree_backend, file_backend);
        wt.mark_root_sealed(root);
        Ok(wt)
    }

    /// Shared constructor: install a single root directory, building both stores.
    fn with_root(
        tree_backend: TBk,
        file_backend: FBk,
        lower: Option<Arc<dyn VfsService>>,
        mode: u32,
        root_lower: Option<InodeId>,
    ) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INODE_ID,
            Inode::bare(
                mode,
                Body::Dir {
                    children: DirChildren::Open(BTreeMap::new()),
                    lower: root_lower,
                    mode_override: false,
                },
            ),
        );
        Self {
            trees: TreeStore::new(tree_backend),
            files: FileCas::new(file_backend),
            lower,
            inodes,
            next: ROOT_INODE_ID.0 + 1,
        }
    }

    /// Allocate a fresh inode number.
    fn alloc(&mut self) -> InodeId {
        let id = InodeId(self.next);
        self.next += 1;
        id
    }

    /// The root directory's inode id ([`ROOT_INODE_ID`]).
    pub fn root(&self) -> InodeId {
        ROOT_INODE_ID
    }

    // --- store / hydration ---------------------------------------------------

    /// Record the root as [`Sealed`](DirChildren::Sealed) so its children fault
    /// in from `tid` on first access. Used by [`from_root`](Self::from_root) and
    /// [`from_overlay`](Self::from_overlay).
    fn mark_root_sealed(&mut self, tid: TreeId) {
        if let Some(Inode { body: Body::Dir { children, .. }, .. }) =
            self.inodes.get_mut(&ROOT_INODE_ID)
        {
            *children = DirChildren::Sealed(tid);
        }
    }

    /// Fault `dir`'s upper children in from the CAS if it is still
    /// [`Sealed`](DirChildren::Sealed); a no-op if already
    /// [`Open`](DirChildren::Open) or not a directory.
    ///
    /// Loads *that one* tree object and materializes its **immediate** children as
    /// inodes — sub-directories landing [`Sealed`](DirChildren::Sealed) again, so
    /// the spine faults in one level per access. A [`LinkKind::Tombstone`] becomes
    /// a [`Tombstone`](Body::Tombstone) whiteout (it only occurs with a non-empty
    /// lower); each child directory's lower counterpart is resolved one step down
    /// from `dir_lower`. Every reader or mutator of a directory's child map calls
    /// this first.
    fn resolve_dir(&mut self, dir: InodeId) -> VfsResult<()> {
        let (tree_id, dir_lower) = match &self.inodes.get(&dir).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children: DirChildren::Sealed(tid), lower, .. } => (*tid, *lower),
            // Already faulted in (or not a directory) — nothing to do.
            _ => return Ok(()),
        };

        // The borrow of `self.trees` ends when `get` returns the owned tree.
        let tree: Tree<OverlayLink> =
            self.trees.get(&tree_id).map_err(io)?.ok_or(VfsError::NotFound)?;
        // Snapshot (name, kind, mode) so we don't borrow `tree` across the
        // self-mutation below.
        let entries: Vec<(OsString, LinkKind, u32)> = tree
            .entries()
            .iter()
            .map(|e| (OsString::from_vec(e.name.to_vec()), e.id.kind(), e.mode))
            .collect();

        let mut children = BTreeMap::new();
        for (name, kind, mode) in entries {
            let child = self.alloc();
            match kind {
                LinkKind::Blob(h) => {
                    let blob = BlobId::from(h);
                    if mode & TYPE_BITS == SYMLINK_MODE {
                        // A symlink is a blob whose bytes are the target path
                        // (git-style); fault the target in now (it is small).
                        let target =
                            self.files.get(&blob).map_err(io)?.ok_or(VfsError::NotFound)?;
                        self.inodes
                            .insert(child, Inode::bare(mode, Body::Symlink(target)));
                    } else {
                        self.inodes.insert(
                            child,
                            Inode::bare(mode, Body::File(FileBody::Clean(blob))),
                        );
                    }
                }
                LinkKind::Tree(h) => {
                    // Resolve this child dir's lower counterpart one step down;
                    // leave its own children Sealed (faulted in on later access).
                    let child_lower = self.lower_child_dir(dir_lower, &name);
                    self.inodes.insert(
                        child,
                        Inode::bare(
                            mode,
                            Body::Dir {
                                children: DirChildren::Sealed(TreeId::from_cas_hash(h)),
                                lower: child_lower,
                                mode_override: false,
                            },
                        ),
                    );
                }
                LinkKind::Tombstone => {
                    self.inodes.insert(child, Inode::bare(mode, Body::Tombstone));
                }
            }
            children.insert(name, child);
        }

        if let Some(Inode { body: Body::Dir { children: slot, .. }, .. }) = self.inodes.get_mut(&dir)
        {
            *slot = DirChildren::Open(children);
        }
        Ok(())
    }

    /// Resolve the lower-service [`InodeId`] of the *directory* child `name` under
    /// the lower dir `parent_lower`, or `None` if there is no lower, no such
    /// child, or the child is not a directory.
    fn lower_child_dir(&self, parent_lower: Option<InodeId>, name: &OsStr) -> Option<InodeId> {
        let lower = self.lower.as_ref()?;
        let pl = parent_lower?;
        let entry = lower.lookup(pl, name).ok()?;
        if entry.attr.kind == NodeKind::Directory {
            Some(entry.attr.inode_id)
        } else {
            None
        }
    }

    // --- resolve / lookup ----------------------------------------------------

    /// Resolve `name` within directory `parent`, materializing a lower entry on
    /// first sight so its overlay id is stable across repeated lookups (and
    /// matches `readdir`).
    pub fn lookup(&mut self, parent: InodeId, name: &OsStr) -> VfsResult<InodeId> {
        self.resolve_dir(parent)?;
        let (cached, parent_lower) = match self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { ref children, lower, .. } => (children.open()?.get(name).copied(), lower),
            _ => return Err(VfsError::NotDirectory),
        };

        if let Some(id) = cached {
            // A tombstone resolves to NotFound (the lower entry is hidden).
            return match self.inodes.get(&id).map(|n| &n.body) {
                Some(Body::Tombstone) => Err(VfsError::NotFound),
                _ => Ok(id),
            };
        }

        let ld = parent_lower.ok_or(VfsError::NotFound)?;
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
        let entry = lower.lookup(ld, name)?;
        let id = self.materialize_lower(&entry.attr);
        self.link(parent, name, id)?;
        Ok(id)
    }

    /// Build an overlay node from a lower entry's attrs and insert it, returning
    /// its (newly allocated) overlay [`InodeId`]. A directory becomes a fall-
    /// through `Dir` wired to its lower id; a regular file becomes a
    /// `LowerFile`; a symlink becomes a `LowerSymlink` (its target is read from
    /// the lower on `readlink`).
    /// Build an overlay node from a lower entry's attrs and insert it, returning
    /// its (newly allocated) overlay [`InodeId`]. A directory becomes a fall-
    /// through `Dir` wired to its lower id; a regular file becomes a
    /// `LowerFile`; a symlink becomes a `LowerSymlink` (its target is read from
    /// the lower on `readlink`). The node's session times start as the lower's
    /// own, so a fall-through `stat` reports real lower times and a later
    /// copy-up preserves them (the POSIX `cp` expectation).
    fn materialize_lower(&mut self, attr: &NodeAttr) -> InodeId {
        let id = self.alloc();
        let mode_perm = attr.perm as u32 & 0o7777;
        let body = match attr.kind {
            NodeKind::Directory => Body::Dir {
                children: DirChildren::Open(BTreeMap::new()),
                lower: Some(attr.inode_id),
                mode_override: false,
            },
            NodeKind::RegularFile => Body::LowerFile(attr.inode_id),
            NodeKind::Symlink => Body::LowerSymlink(attr.inode_id),
        };
        let mode = match attr.kind {
            NodeKind::Directory => 0o040000 | mode_perm,
            NodeKind::RegularFile => 0o100000 | mode_perm,
            NodeKind::Symlink => SYMLINK_MODE | mode_perm,
        };
        let inode = Inode::with_times(mode, body, attr);
        self.inodes.insert(id, inode);
        id
    }

    // --- getattr / readdir ---------------------------------------------------

    /// Attributes of `node` (kind, mode, size, times).
    ///
    /// Times are the session values (see the crate docs): explicit `setattr` /
    /// write-bump for upper nodes, the lower's own for a fall-through node
    /// (re-read live from the lower), zeroed after a rehydrate. `ctime` is the
    /// in-memory metadata-change instant. Reads never modify `atime`.
    pub fn getattr(&self, node: InodeId) -> VfsResult<NodeAttr> {
        let inode = self.inodes.get(&node).ok_or(VfsError::NotFound)?;
        let mut attr = match &inode.body {
            Body::Dir { .. } => NodeAttr::directory(node, UNIX_EPOCH),
            Body::File(fb) => NodeAttr::regular_file(node, self.file_size(fb)?, UNIX_EPOCH),
            Body::Symlink(target) => NodeAttr::symlink(node, target.len() as u64, UNIX_EPOCH),
            Body::LowerFile(ld) | Body::LowerSymlink(ld) => {
                let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
                let mut a = lower.getattr(*ld)?;
                a.inode_id = node;
                return Ok(a);
            }
            Body::Tombstone => return Err(VfsError::NotFound),
        };
        attr.perm = (inode.mode & 0o7777) as u16;
        attr.mtime = inode.mtime.to_system_time();
        attr.atime = inode.atime.to_system_time();
        attr.ctime = inode.ctime.to_system_time();
        Ok(attr)
    }

    /// List the children of directory `node`: cached children (skipping
    /// tombstones) merged with the lower's children whose names are not already
    /// overridden/tombstoned/cached. Lower entries are materialized+cached so a
    /// later `lookup` returns the same id; every entry's `inode_id` is the
    /// overlay id.
    pub fn readdir(&mut self, node: InodeId) -> VfsResult<Vec<DirEntry>> {
        self.resolve_dir(node)?;
        let (cached, lower_id) = match self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { ref children, lower, .. } => (
                children.open()?.iter().map(|(n, &i)| (n.clone(), i)).collect::<Vec<_>>(),
                lower,
            ),
            _ => return Err(VfsError::NotDirectory),
        };

        let mut out = Vec::new();
        for (name, id) in &cached {
            let kind = match &self.inodes.get(id).ok_or(VfsError::NotFound)?.body {
                Body::Dir { .. } => NodeKind::Directory,
                Body::File(_) | Body::LowerFile(_) => NodeKind::RegularFile,
                Body::Symlink(_) | Body::LowerSymlink(_) => NodeKind::Symlink,
                Body::Tombstone => continue,
            };
            out.push(DirEntry::new(*id, name.clone(), kind));
        }

        if let Some(ld) = lower_id {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
            for de in lower.readdir(ld)? {
                // The `VfsService::readdir` contract includes synthesized `.`/`..`
                // entries (see `PassthroughVfs`); they are not mergeable content.
                if de.name == "." || de.name == ".." {
                    continue;
                }
                if cached.iter().any(|(n, _)| n == &de.name) {
                    // Already overridden / tombstoned / cached — skip.
                    continue;
                }
                // Materialize+cache the lower child via its full attrs so its id
                // is stable and matches a later lookup.
                let attr = lower.getattr(de.inode_id)?;
                let id = self.materialize_lower(&attr);
                self.link(node, &de.name, id)?;
                out.push(DirEntry::new(id, de.name, attr.kind));
            }
        }
        Ok(out)
    }

    // --- read / write --------------------------------------------------------

    /// Read up to `len` bytes of file `node` at offset `off` (clamped to EOF).
    /// A fall-through file is served from the lower (open/read/release per call
    /// in v1).
    pub fn read_at(&self, node: InodeId, off: u64, len: u32) -> VfsResult<Vec<u8>> {
        match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => Err(VfsError::IsDirectory),
            Body::Symlink(_) | Body::LowerSymlink(_) => {
                Err(VfsError::InvalidInput("cannot read a symlink; use readlink".into()))
            }
            Body::File(FileBody::Clean(blob)) => {
                Ok(self.files.read_at(blob, off, len).map_err(io)?.to_vec())
            }
            Body::File(FileBody::Dirty(writer)) => Ok(writer.read_at(off, len).to_vec()),
            Body::LowerFile(ld) => self.read_lower(*ld, off, len),
            Body::Tombstone => Err(VfsError::NotFound),
        }
    }

    /// Read a range from a lower file: open read-only, read, release.
    fn read_lower(&self, ld: InodeId, off: u64, len: u32) -> VfsResult<Vec<u8>> {
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        let opened = lower.open(OpenRequest {
            inode_id: ld,
            mode: OpenMode::ReadOnly,
            truncate: false,
            append: false,
        })?;
        let res = lower.read(ReadRequest {
            inode_id: ld,
            handle: opened.handle,
            offset: off,
            size: len,
        });
        // Release regardless of the read outcome.
        let _ = lower.release(opened.handle);
        res
    }

    /// Read the entire lower file (used by copy-up).
    fn read_lower_all(&self, ld: InodeId) -> VfsResult<Vec<u8>> {
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        let size = lower.getattr(ld)?.size;
        let opened = lower.open(OpenRequest {
            inode_id: ld,
            mode: OpenMode::ReadOnly,
            truncate: false,
            append: false,
        })?;
        let mut buf = Vec::with_capacity(size as usize);
        let mut off = 0u64;
        loop {
            // Read in bounded chunks so a giant lower file does not need a single
            // u32-sized request.
            let want = (size - off).min(1 << 20) as u32;
            if want == 0 {
                break;
            }
            let chunk = lower.read(ReadRequest {
                inode_id: ld,
                handle: opened.handle,
                offset: off,
                size: want,
            });
            match chunk {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        break;
                    }
                    off += bytes.len() as u64;
                    buf.extend_from_slice(&bytes);
                }
                Err(e) => {
                    let _ = lower.release(opened.handle);
                    return Err(e);
                }
            }
        }
        let _ = lower.release(opened.handle);
        Ok(buf)
    }

    /// Write `data` into file `node` at offset `off`, returning bytes written.
    ///
    /// - A dirty upper file is written in place.
    /// - A clean upper file is re-opened for editing (its content materialized
    ///   into a [`FileWriter`]) on first write.
    /// - A **fall-through** file is **copied up**: its whole lower content is
    ///   read into a fresh writer, the inode flips to [`File`](Body::File)
    ///   keeping the **same [`InodeId`]**, and the write is applied. The lower is
    ///   never mutated.
    ///
    /// The write bumps the node's session `mtime` (and `atime`) to now; a
    /// copied-up lower file keeps the lower's times only until the first write.
    pub fn write_at(&mut self, node: InodeId, off: u64, data: &[u8]) -> VfsResult<u32> {
        self.make_dirty(node)?;
        self.writer_mut(node)?.write_at(off, data);
        self.bump_mtime(node);
        Ok(data.len() as u32)
    }

    /// Truncate file `node` to exactly `len` bytes (POSIX `ftruncate`, and the
    /// `O_TRUNC` open flag): shrinks, or zero-extends if `len` exceeds the
    /// current size.
    ///
    /// Like [`write_at`](Self::write_at) this materializes on first mutation — a
    /// clean upper file is reopened for editing, a fall-through file is copied up
    /// (same [`InodeId`]; the lower is never touched). Truncating to `len == 0`
    /// installs a fresh empty writer directly, so neither the clean blob nor the
    /// lower content is read.
    pub fn truncate(&mut self, node: InodeId, len: u64) -> VfsResult<()> {
        if len == 0 {
            // Fast path: an empty result needs none of the existing content, so
            // skip the clean-blob load / lower copy-up entirely.
            match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
                Body::Dir { .. } => return Err(VfsError::IsDirectory),
                Body::Symlink(_) | Body::LowerSymlink(_) => {
                    return Err(VfsError::InvalidInput("cannot truncate a symlink".into()));
                }
                Body::Tombstone => return Err(VfsError::NotFound),
                Body::File(_) | Body::LowerFile(_) => {}
            }
            let writer = self.files.edit(None).map_err(io)?;
            if let Some(inode) = self.inodes.get_mut(&node) {
                inode.body = Body::File(FileBody::Dirty(writer));
            }
            self.bump_mtime(node);
            return Ok(());
        }
        self.make_dirty(node)?;
        self.writer_mut(node)?.truncate(len);
        self.bump_mtime(node);
        Ok(())
    }

    /// Ensure file `node` is backed by a live [`FileWriter`]
    /// ([`Dirty`](FileBody::Dirty)), materializing it on first mutation: a clean
    /// file is reopened for editing; a fall-through [`LowerFile`](Body::LowerFile)
    /// is copied up from the lower (keeping the same [`InodeId`]; the lower is
    /// never mutated). Errors if `node` is a directory or a tombstone.
    fn make_dirty(&mut self, node: InodeId) -> VfsResult<()> {
        // Decide the materialization step without holding a borrow across the
        // FileCas / lower calls.
        enum Step {
            Ready,
            EditClean(BlobId),
            CopyUp(InodeId),
        }
        let step = match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => return Err(VfsError::IsDirectory),
            Body::Symlink(_) | Body::LowerSymlink(_) => {
                return Err(VfsError::InvalidInput("cannot write a symlink".into()));
            }
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::File(FileBody::Dirty(_)) => Step::Ready,
            Body::File(FileBody::Clean(b)) => Step::EditClean(*b),
            Body::LowerFile(ld) => Step::CopyUp(*ld),
        };
        match step {
            Step::Ready => {}
            Step::EditClean(b) => {
                let writer = self.files.edit(Some(b)).map_err(io)?;
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::File(FileBody::Dirty(writer));
                }
            }
            Step::CopyUp(ld) => {
                // Read the whole lower file, then seed a fresh writer with it.
                let bytes = self.read_lower_all(ld)?;
                let mut writer = self.files.edit(None).map_err(io)?;
                writer.write_at(0, &bytes);
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::File(FileBody::Dirty(writer));
                }
            }
        }
        Ok(())
    }

    /// Borrow `node`'s live writer. Call only after
    /// [`make_dirty`](Self::make_dirty); any other body is a bug.
    fn writer_mut(&mut self, node: InodeId) -> VfsResult<&mut FileWriter> {
        match &mut self.inodes.get_mut(&node).ok_or(VfsError::NotFound)?.body {
            Body::File(FileBody::Dirty(writer)) => Ok(writer),
            _ => Err(VfsError::Io("file body changed unexpectedly".into())),
        }
    }

    // --- create / mkdir ------------------------------------------------------

    /// Create an empty regular file `name` under `parent` (upper only).
    ///
    /// Recreating over a tombstone (a previously-deleted lower name) replaces
    /// the whiteout.
    pub fn create(&mut self, parent: InodeId, name: &OsStr, mode: u32) -> VfsResult<InodeId> {
        self.resolve_dir(parent)?;
        self.ensure_creatable(parent, name)?;
        let writer = self.files.edit(None).map_err(io)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode::new(mode, Body::File(FileBody::Dirty(writer))),
        );
        self.link(parent, name, id)?;
        Ok(id)
    }

    /// Create an empty subdirectory `name` under `parent` (upper only, `lower =
    /// None`). Recreating over a tombstone replaces the whiteout.
    pub fn mkdir(&mut self, parent: InodeId, name: &OsStr, mode: u32) -> VfsResult<InodeId> {
        self.resolve_dir(parent)?;
        self.ensure_creatable(parent, name)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode::new(
                mode,
                Body::Dir {
                    children: DirChildren::Open(BTreeMap::new()),
                    lower: None,
                    mode_override: false,
                },
            ),
        );
        self.link(parent, name, id)?;
        Ok(id)
    }

    // --- symlink / readlink --------------------------------------------------

    /// Create a symlink `name` under `parent` pointing at `target` (upper
    /// only). The target is stored verbatim — a relative target stays
    /// relative — and seals as a [`SYMLINK_MODE`]-moded blob (git-style), so
    /// checkpoints and history snapshots reproduce the link exactly.
    /// Recreating over a tombstone replaces the whiteout.
    pub fn symlink(&mut self, parent: InodeId, name: &OsStr, target: &OsStr) -> VfsResult<InodeId> {
        self.resolve_dir(parent)?;
        self.ensure_creatable(parent, name)?;
        let id = self.alloc();
        self.inodes.insert(
            id,
            Inode::new(
                SYMLINK_MODE | 0o777,
                Body::Symlink(Bytes::copy_from_slice(target.as_bytes())),
            ),
        );
        self.link(parent, name, id)?;
        Ok(id)
    }

    /// Read a symlink's target: the stored bytes for an upper link, the
    /// lower's own `readlink` for a fall-through one.
    pub fn readlink(&self, node: InodeId) -> VfsResult<OsString> {
        match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Symlink(target) => Ok(OsString::from_vec(target.to_vec())),
            Body::LowerSymlink(ld) => {
                let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
                lower.readlink(*ld)
            }
            Body::Tombstone => Err(VfsError::NotFound),
            _ => Err(VfsError::InvalidInput("not a symlink".into())),
        }
    }

    // --- setattr (mode) ------------------------------------------------------

    /// Apply `chmod` to `node`: set the permission bits, preserving the type
    /// bits. The mode persists — it seals into the node's tree entry.
    ///
    /// A fall-through node is **copied up** first (content for a file, the
    /// target for a symlink, a forced self-entry for a directory) so the new
    /// mode has an overlay home; the lower is never mutated.
    pub fn set_mode(&mut self, node: InodeId, mode: u32) -> VfsResult<()> {
        // Decide the copy-up step first so the borrow ends before mutation.
        enum Step {
            Ready,
            CopyUpFile,
            CopyUpSymlink(InodeId),
            Dir,
        }
        let step = match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::LowerFile(_) => Step::CopyUpFile,
            Body::LowerSymlink(ld) => Step::CopyUpSymlink(*ld),
            Body::Dir { .. } => Step::Dir,
            Body::File(_) | Body::Symlink(_) => Step::Ready,
        };
        match step {
            Step::Ready => {}
            Step::CopyUpFile => self.make_dirty(node)?,
            Step::CopyUpSymlink(ld) => {
                let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
                let target = lower.readlink(ld)?;
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::Symlink(Bytes::copy_from_slice(target.as_bytes()));
                }
            }
            Step::Dir => {
                // Fault a sealed dir in so its (re-)seal carries the new mode.
                self.resolve_dir(node)?;
            }
        }
        let inode = self.inodes.get_mut(&node).ok_or(VfsError::NotFound)?;
        inode.mode = (inode.mode & TYPE_BITS) | (mode & 0o7777);
        inode.ctime = EntryTime::from_system_time(SystemTime::now());
        if let Body::Dir { mode_override, .. } = &mut inode.body {
            // A fall-through dir with no upper entries would seal to nothing;
            // force it to emit so the mode lands in the delta.
            *mode_override = true;
        }
        Ok(())
    }

    // --- setattr (times) -----------------------------------------------------

    /// Apply `utimes(2)` to `node`: set the session `mtime` and/or `atime`
    /// exactly (a `None` half is left alone — the `UTIME_OMIT` case). Times are
    /// in-memory only (see the crate docs); `ctime` is stamped with now.
    ///
    /// Like [`set_mode`](Self::set_mode), a fall-through node is **copied up**
    /// first so the times have an overlay home: a file's content moves into the
    /// CAS, a symlink's target is re-homed, a directory force-emits its own
    /// entry. The lower is never mutated.
    pub fn set_times(
        &mut self,
        node: InodeId,
        mtime: Option<SystemTime>,
        atime: Option<SystemTime>,
    ) -> VfsResult<()> {
        if mtime.is_none() && atime.is_none() {
            return Ok(());
        }
        // Copy up a fall-through node first (same shapes as `set_mode`).
        enum Step {
            Ready,
            CopyUpFile,
            CopyUpSymlink(InodeId),
            Dir,
        }
        let step = match &self.inodes.get(&node).ok_or(VfsError::NotFound)?.body {
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::LowerFile(_) => Step::CopyUpFile,
            Body::LowerSymlink(ld) => Step::CopyUpSymlink(*ld),
            Body::Dir { .. } => Step::Dir,
            Body::File(_) | Body::Symlink(_) => Step::Ready,
        };
        match step {
            Step::Ready => {}
            Step::CopyUpFile => self.make_dirty(node)?,
            Step::CopyUpSymlink(ld) => {
                let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
                let target = lower.readlink(ld)?;
                if let Some(inode) = self.inodes.get_mut(&node) {
                    inode.body = Body::Symlink(Bytes::copy_from_slice(target.as_bytes()));
                }
            }
            Step::Dir => self.resolve_dir(node)?,
        }
        let inode = self.inodes.get_mut(&node).ok_or(VfsError::NotFound)?;
        if let Some(m) = mtime {
            inode.mtime = EntryTime::from_system_time(m);
        }
        if let Some(a) = atime {
            inode.atime = EntryTime::from_system_time(a);
        }
        inode.ctime = EntryTime::from_system_time(SystemTime::now());
        if let Body::Dir { mode_override, .. } = &mut inode.body {
            // A utimes'd fall-through dir must seal a self-entry too, or the
            // times die with the session's invisibility in the delta.
            *mode_override = true;
        }
        Ok(())
    }

    /// Bump `node`'s session `mtime` (and `atime`) to the wall-clock now. The
    /// write/truncate stamp; reads never call this.
    fn bump_mtime(&mut self, node: InodeId) {
        if let Some(inode) = self.inodes.get_mut(&node) {
            let now = EntryTime::from_system_time(SystemTime::now());
            inode.mtime = now;
            inode.atime = now;
        }
    }

    // --- statfs --------------------------------------------------------------

    /// Filesystem-wide statistics: the lower's own numbers when there is one
    /// (the overlay is bounded by RAM and the state store, not a block device,
    /// so the lower is the honest answer `df` / installers are looking for), a
    /// synthetic unbounded answer otherwise.
    pub fn statfs(&self) -> VfsResult<StatFs> {
        match &self.lower {
            Some(lower) => lower.statfs(ROOT_INODE_ID),
            None => Ok(StatFs::synthetic()),
        }
    }

    // --- unlink / rmdir ------------------------------------------------------

    /// Remove the regular file `name` from `parent`.
    ///
    /// If the name also exists in the lower a **tombstone** (whiteout) is left in
    /// its place; a pure-upper name is simply dropped.
    pub fn unlink(&mut self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir { .. } => return Err(VfsError::IsDirectory),
            Body::File(_) | Body::LowerFile(_) | Body::Symlink(_) | Body::LowerSymlink(_) => {}
            Body::Tombstone => return Err(VfsError::NotFound),
        }
        self.remove_child(parent, name, id)
    }

    /// Remove the empty directory `name` from `parent`.
    ///
    /// "Empty" means no non-tombstone upper children **and** no un-tombstoned
    /// lower children. If the name also exists in the lower a tombstone is left;
    /// a pure-upper directory is simply dropped.
    pub fn rmdir(&mut self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        let id = self.lookup(parent, name)?;
        match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::File(_) | Body::LowerFile(_) | Body::Symlink(_) | Body::LowerSymlink(_) => {
                return Err(VfsError::NotDirectory);
            }
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::Dir { .. } => {}
        }
        self.resolve_dir(id)?;
        if !self.dir_is_empty(id)? {
            return Err(VfsError::DirectoryNotEmpty);
        }
        self.remove_child(parent, name, id)
    }

    /// Whether directory `id` has no visible children (no non-tombstone upper
    /// child, and no lower child not shadowed by a tombstone).
    fn dir_is_empty(&self, id: InodeId) -> VfsResult<bool> {
        let (children, lower) = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, lower, .. } => (children.open()?, *lower),
            _ => return Err(VfsError::NotDirectory),
        };
        // Any non-tombstone upper child means non-empty.
        for cid in children.values() {
            if !matches!(self.inodes.get(cid).map(|n| &n.body), Some(Body::Tombstone)) {
                return Ok(false);
            }
        }
        // Any lower child whose name is not tombstoned in the upper means
        // non-empty.
        if let Some(ld) = lower {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
            for de in lower.readdir(ld)? {
                // `.`/`..` are synthesized by the lower service, not content —
                // they must not count toward non-emptiness.
                if de.name == "." || de.name == ".." {
                    continue;
                }
                let shadowed = children
                    .get(&de.name)
                    .map(|cid| matches!(self.inodes.get(cid).map(|n| &n.body), Some(Body::Tombstone)))
                    .unwrap_or(false);
                if !shadowed {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Detach `name`→`id` from `parent`: leave a tombstone if the name also
    /// exists in the lower, otherwise drop it. Frees the removed inode unless it
    /// is replaced by a tombstone (a tombstone is its own fresh node).
    fn remove_child(&mut self, parent: InodeId, name: &OsStr, id: InodeId) -> VfsResult<()> {
        if self.name_in_lower(parent, name)? {
            // Whiteout: replace the resolved child with a tombstone, reusing its
            // slot. We free the old inode and install a tombstone in `children`.
            self.inodes.remove(&id);
            let tomb = self.alloc();
            self.inodes.insert(tomb, Inode::bare(0, Body::Tombstone));
            self.link(parent, name, tomb)?;
        } else {
            self.unlink_child(parent, name)?;
            self.inodes.remove(&id);
        }
        Ok(())
    }

    /// Whether `name` resolves to an entry in the lower of `parent` (a
    /// `LowerFile`/fall-through dir already cached counts, as does a fresh lower
    /// lookup).
    fn name_in_lower(&self, parent: InodeId, name: &OsStr) -> VfsResult<bool> {
        let parent_lower = match &self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { lower, .. } => *lower,
            _ => return Err(VfsError::NotDirectory),
        };
        let ld = match parent_lower {
            None => return Ok(false),
            Some(ld) => ld,
        };
        let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
        Ok(lower.lookup(ld, name).is_ok())
    }

    // --- rename --------------------------------------------------------------

    /// Move `parent/name` to `new_parent/new_name` (POSIX `rename(2)`).
    ///
    /// - An existing target **file** is replaced; an existing **empty** target
    ///   directory is replaced; other combinations error per POSIX.
    /// - A fall-through **file** is copied up first — its content becomes
    ///   CAS-owned and therefore location-independent. Upper files (clean or
    ///   dirty) and fully-materialized directories re-link as-is: blobs and
    ///   sealed subtrees are content-addressed, so moving them is free.
    /// - A fall-through **directory** (one that still merges lower content)
    ///   cannot move: overlay fall-through is positional, so the merge would
    ///   silently detach at the new location. Errors
    ///   [`VfsError::CrossDevice`], and callers (e.g. `mv`) fall back to
    ///   copy + delete — which routes through the normal copy-up paths and
    ///   does the right thing.
    /// - Tombstone bookkeeping mirrors [`unlink`](Self::unlink): the vacated
    ///   name is whited-out when it also exists in the lower.
    pub fn rename(
        &mut self,
        parent: InodeId,
        name: &OsStr,
        new_parent: InodeId,
        new_name: &OsStr,
    ) -> VfsResult<()> {
        self.resolve_dir(parent)?;
        self.resolve_dir(new_parent)?;
        let src = self.lookup(parent, name)?;
        let src_is_dir = match &self.inodes.get(&src).ok_or(VfsError::NotFound)?.body {
            Body::Tombstone => return Err(VfsError::NotFound),
            Body::Dir { lower, .. } => {
                if lower.is_some() {
                    // Fall-through dirs are positional; see the doc comment.
                    return Err(VfsError::CrossDevice);
                }
                true
            }
            Body::File(_) | Body::LowerFile(_) | Body::Symlink(_) | Body::LowerSymlink(_) => false,
        };

        // Renaming a node onto itself is a no-op.
        if parent == new_parent && name == new_name {
            return Ok(());
        }
        // A directory must not move into its own subtree (POSIX EINVAL).
        if src_is_dir && self.is_in_subtree(src, new_parent)? {
            return Err(VfsError::InvalidInput(
                "cannot move a directory into its own subtree".to_owned(),
            ));
        }
        // A fall-through file becomes upper-owned before it moves.
        if !src_is_dir
            && matches!(
                self.inodes.get(&src).map(|n| &n.body),
                Some(Body::LowerFile(_))
            )
        {
            self.make_dirty(src)?;
        }
        // A fall-through symlink likewise: its target is read from the lower
        // and becomes an upper link (a `make_dirty`-style content copy-up
        // would follow the link, so it is not reusable here).
        if !src_is_dir
            && let Some(Body::LowerSymlink(ld)) = self.inodes.get(&src).map(|n| &n.body)
        {
            let ld = *ld;
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?.clone();
            let target = lower.readlink(ld)?;
            if let Some(inode) = self.inodes.get_mut(&src) {
                inode.body = Body::Symlink(Bytes::copy_from_slice(target.as_bytes()));
            }
        }

        // POSIX replace semantics for an existing target.
        match self.lookup(new_parent, new_name) {
            Err(VfsError::NotFound) => {}
            Err(e) => return Err(e),
            Ok(dst) => {
                let dst_is_dir = match &self.inodes.get(&dst).ok_or(VfsError::NotFound)?.body {
                    Body::Tombstone => false, // a whiteout: nothing visible to replace
                    Body::Dir { .. } => true,
                    Body::File(_) | Body::LowerFile(_) | Body::Symlink(_) | Body::LowerSymlink(_) => {
                        false
                    }
                };
                match (src_is_dir, dst_is_dir) {
                    (true, false) => return Err(VfsError::NotDirectory),
                    (false, true) => return Err(VfsError::IsDirectory),
                    (true, true) => {
                        self.resolve_dir(dst)?;
                        if !self.dir_is_empty(dst)? {
                            return Err(VfsError::DirectoryNotEmpty);
                        }
                        self.remove_child(new_parent, new_name, dst)?;
                    }
                    (false, false) => {
                        if dst != src {
                            self.remove_child(new_parent, new_name, dst)?;
                        }
                    }
                }
            }
        }

        // Vacate the old name (whiteout if it also exists in the lower),
        // keeping the moved inode alive.
        if self.name_in_lower(parent, name)? {
            let tomb = self.alloc();
            self.inodes.insert(tomb, Inode::bare(0, Body::Tombstone));
            self.link(parent, name, tomb)?;
        } else {
            self.unlink_child(parent, name)?;
        }

        // POSIX: rename is a metadata change — stamp the source's ctime (the
        // name changed; the content, mtime and atime did not).
        if let Some(inode) = self.inodes.get_mut(&src) {
            inode.ctime = EntryTime::from_system_time(SystemTime::now());
        }
        // Attach at the new location, freeing a leftover whiteout slot if the
        // target map still holds one (created by `remove_child` above, or by
        // an earlier deletion of a lower name).
        let leftover = match &self.inodes.get(&new_parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, .. } => children.open()?.get(new_name).copied(),
            _ => None,
        };
        if let Some(existing) = leftover
            && matches!(self.inodes.get(&existing).map(|n| &n.body), Some(Body::Tombstone))
        {
            self.inodes.remove(&existing);
        }
        self.link(new_parent, new_name, src)
    }

    /// Whether `candidate` equals `root` or lies anywhere below it in the
    /// **hydrated** upper spine. Sufficient for the rename cycle check: a
    /// hydrated node's ancestors are necessarily hydrated too (children are
    /// only ever reached through their parent), so if `candidate` descends
    /// from `root` the whole path between them is walkable.
    fn is_in_subtree(&self, root: InodeId, candidate: InodeId) -> VfsResult<bool> {
        if root == candidate {
            return Ok(true);
        }
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if let Some(Inode { body: Body::Dir { children: DirChildren::Open(map), .. }, .. }) =
                self.inodes.get(&dir)
            {
                for child in map.values() {
                    if *child == candidate {
                        return Ok(true);
                    }
                    stack.push(*child);
                }
            }
        }
        Ok(false)
    }

    // --- seal ----------------------------------------------------------------

    /// Seal the current state into immutable objects, returning the new root
    /// [`TreeId`].
    ///
    /// Always seals a *sparse* overlay delta: only the materialized spine and
    /// tombstones are emitted; a fall-through directory (no upper entries, has a
    /// lower) emits nothing, so untouched siblings are absent. With an empty lower
    /// nothing falls through and no tombstone exists, so the result is a complete,
    /// self-contained tree. The root always emits (even when empty).
    pub fn seal(&mut self) -> VfsResult<TreeId> {
        match self.seal_dir(ROOT_INODE_ID, true)? {
            Some(id) => Ok(id),
            // Root with no entries and a lower: still emit an empty tree.
            None => self.trees.put(Tree::<OverlayLink>::empty()).map_err(io),
        }
    }

    /// Seal directory `dir` into a [`Tree<OverlayLink>`] in `self.trees`. Returns
    /// `None` iff this is a fall-through dir (produced no entries AND has a lower)
    /// and `force_emit` is false — such a dir is absent from the delta.
    fn seal_dir(&mut self, dir: InodeId, force_emit: bool) -> VfsResult<Option<TreeId>> {
        // A never-touched sealed subtree re-emits by id without walking
        // (structural sharing of clean subtrees comes for free). A chmod'd
        // dir is the exception: it must fault in and re-seal so the new mode
        // lands in the tree entry.
        let reseal = match &self.inodes.get(&dir).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children: DirChildren::Sealed(tid), mode_override, .. } => {
                if !mode_override {
                    return Ok(Some(*tid));
                }
                true
            }
            _ => false,
        };
        if reseal {
            self.resolve_dir(dir)?;
        }
        let (children, has_lower, mode_overridden) = match &self.inodes.get(&dir).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children: DirChildren::Open(map), lower, mode_override } => (
                map.iter().map(|(n, &i)| (n.clone(), i)).collect::<Vec<_>>(),
                lower.is_some(),
                *mode_override,
            ),
            _ => return Err(VfsError::NotDirectory),
        };

        let mut entries: Vec<TreeEntry<OverlayLink>> = Vec::with_capacity(children.len());
        for (name, id) in children {
            let mode = self.inodes.get(&id).ok_or(VfsError::NotFound)?.mode;
            let name_bytes = Bytes::copy_from_slice(name.as_bytes());
            // Classify first so the inode-map borrow ends before recursion.
            let kind = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                Body::Dir { .. } => ChildKind::Dir,
                Body::File(_) => ChildKind::File,
                Body::Symlink(_) => ChildKind::Symlink,
                Body::Tombstone => ChildKind::Tombstone,
                Body::LowerFile(_) | Body::LowerSymlink(_) => ChildKind::Lower,
            };
            match kind {
                ChildKind::Dir => {
                    if let Some(sub) = self.seal_dir(id, false)? {
                        entries.push(TreeEntry::dir(name_bytes, sub, mode));
                    }
                    // None ⇒ fall-through dir; skip (falls through).
                }
                ChildKind::File => {
                    let (blob, size) = self.seal_file(id)?;
                    entries.push(TreeEntry::file(name_bytes, blob, mode, size));
                }
                ChildKind::Symlink => {
                    // Git-style: the target path bytes are the blob content,
                    // and the mode carries the symlink type bits.
                    let target = match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                        Body::Symlink(target) => target.clone(),
                        _ => return Err(VfsError::Io("seal: symlink body changed".into())),
                    };
                    let size = target.len() as u64;
                    let blob = self.files.put(target).map_err(io)?;
                    entries.push(TreeEntry::file(name_bytes, blob, mode, size));
                }
                ChildKind::Tombstone => {
                    entries.push(TreeEntry {
                        name: name_bytes,
                        id: Overlay::Tombstone,
                        mode: 0,
                        size: 0,
                        mtime: EntryTime::EPOCH,
                        atime: EntryTime::EPOCH,
                    });
                }
                ChildKind::Lower => {
                    // A fall-through file never copied up is absent (falls through).
                }
            }
        }

        // A fall-through dir (no entries, has a lower) emits nothing unless forced
        // (the root) — or chmod'd (`mode_override`: the new mode needs an entry
        // to live in). A new dir (lower = None) emits even when empty.
        if entries.is_empty() && has_lower && !force_emit && !mode_overridden {
            return Ok(None);
        }

        let tree = Tree::from_entries(entries).map_err(|e| VfsError::Other(e.to_string()))?;
        Ok(Some(self.trees.put(tree).map_err(io)?))
    }

    /// Seal a single upper file inode, returning its blob id and logical size. A
    /// dirty writer is committed and the inode flipped to clean; a clean file is
    /// left as-is.
    fn seal_file(&mut self, id: InodeId) -> VfsResult<(BlobId, u64)> {
        // Take ownership of a dirty writer (if any) without holding a borrow
        // across `self.files.commit`. Swap in a Tombstone sentinel that is
        // overwritten immediately below.
        let dirty: Option<FileWriter> = {
            let inode = self.inodes.get_mut(&id).ok_or(VfsError::NotFound)?;
            match &inode.body {
                Body::File(FileBody::Clean(_)) => None,
                Body::File(FileBody::Dirty(_)) => {
                    let taken = std::mem::replace(&mut inode.body, Body::Tombstone);
                    match taken {
                        Body::File(FileBody::Dirty(w)) => Some(w),
                        _ => None,
                    }
                }
                _ => return Err(VfsError::IsDirectory),
            }
        };

        let blob = match dirty {
            None => match &self.inodes.get(&id).ok_or(VfsError::NotFound)?.body {
                Body::File(FileBody::Clean(b)) => *b,
                _ => return Err(VfsError::Io("seal_file: file vanished".into())),
            },
            Some(writer) => {
                let b = self.files.commit(writer).map_err(io)?;
                if let Some(inode) = self.inodes.get_mut(&id) {
                    inode.body = Body::File(FileBody::Clean(b));
                }
                b
            }
        };

        let size = self.files.size(&blob).map_err(io)?.unwrap_or(0);
        Ok((blob, size))
    }

    // --- debug introspection --------------------------------------------------

    /// The number of files with a live, un-sealed [`FileWriter`] (inodes whose
    /// body is [`Dirty`](FileBody::Dirty)) — i.e. content written since the last
    /// [`seal`](Self::seal) (which commits every writer and flips it clean).
    /// Read-only: never faults anything in.
    pub fn dirty_file_count(&self) -> usize {
        self.inodes
            .values()
            .filter(|inode| matches!(inode.body, Body::File(FileBody::Dirty(_))))
            .count()
    }

    /// List every node materialized in the overlay — the delta vs the lower:
    /// upper files (created or copied up), upper-created directories,
    /// tombstones, and still-sealed subtrees.
    ///
    /// Read-only and **hydration-free**: it walks only the
    /// already-[`Open`](DirChildren::Open) upper spine. A directory still
    /// [`Sealed`](DirChildren::Sealed) is reported as a single
    /// [`SealedDir`](OverlayEntryKind::SealedDir) entry without loading its tree
    /// object. Cached fall-through nodes ([`LowerFile`](Body::LowerFile) and
    /// hydrated fall-through dirs) are *not* delta and are not listed — a
    /// fall-through dir is only descended into. Entries come back in
    /// depth-first, name-sorted order.
    pub fn overlay_entries(&self) -> Vec<OverlayEntry> {
        let mut out = Vec::new();
        // A root never faulted in this session is itself a sealed subtree.
        if let Some(Inode { body: Body::Dir { children: DirChildren::Sealed(_), .. }, .. }) =
            self.inodes.get(&ROOT_INODE_ID)
        {
            out.push(OverlayEntry { path: String::new(), kind: OverlayEntryKind::SealedDir });
            return out;
        }
        self.collect_overlay(ROOT_INODE_ID, "", &mut out);
        out
    }

    /// Depth-first walk behind [`overlay_entries`](Self::overlay_entries):
    /// append `dir`'s materialized children (prefixed with `prefix`) to `out`.
    fn collect_overlay(&self, dir: InodeId, prefix: &str, out: &mut Vec<OverlayEntry>) {
        let children = match self.inodes.get(&dir).map(|inode| &inode.body) {
            Some(Body::Dir { children: DirChildren::Open(map), .. }) => map,
            _ => return,
        };
        for (name, child) in children {
            let path = format!("{prefix}/{}", name.to_string_lossy());
            match self.inodes.get(child).map(|inode| &inode.body) {
                Some(Body::File(_)) => {
                    out.push(OverlayEntry { path, kind: OverlayEntryKind::File });
                }
                Some(Body::Symlink(_)) => {
                    out.push(OverlayEntry { path, kind: OverlayEntryKind::Symlink });
                }
                Some(Body::Tombstone) => {
                    out.push(OverlayEntry { path, kind: OverlayEntryKind::Tombstone });
                }
                Some(Body::Dir { children: DirChildren::Sealed(_), .. }) => {
                    out.push(OverlayEntry { path, kind: OverlayEntryKind::SealedDir });
                }
                Some(Body::Dir { children: DirChildren::Open(_), lower, .. }) => {
                    // An upper-created dir is delta; a hydrated fall-through dir
                    // is only a route to deeper delta.
                    if lower.is_none() {
                        out.push(OverlayEntry {
                            path: path.clone(),
                            kind: OverlayEntryKind::Dir,
                        });
                    }
                    self.collect_overlay(*child, &path, out);
                }
                // Cached fall-throughs (files and symlinks) are not delta; a
                // dangling id is a bug we do not surface from a debug listing.
                Some(Body::LowerFile(_) | Body::LowerSymlink(_)) | None => {}
            }
        }
    }

    // --- small helpers -------------------------------------------------------

    fn file_size(&self, body: &FileBody) -> VfsResult<u64> {
        match body {
            FileBody::Clean(b) => Ok(self.files.size(b).map_err(io)?.unwrap_or(0)),
            FileBody::Dirty(w) => Ok(w.len()),
        }
    }

    /// Error unless `parent` is a directory where `name` can be created: it must
    /// not already resolve to a live (non-tombstone) entry — neither in the
    /// upper nor (un-tombstoned) in the lower. A tombstone is replaceable.
    fn ensure_creatable(&self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        let (cached, parent_lower) = match &self.inodes.get(&parent).ok_or(VfsError::NotFound)?.body {
            Body::Dir { children, lower, .. } => (children.open()?.get(name).copied(), *lower),
            _ => return Err(VfsError::NotDirectory),
        };
        if let Some(id) = cached {
            return match self.inodes.get(&id).map(|n| &n.body) {
                // A tombstone is a deleted lower name; creating over it is fine.
                Some(Body::Tombstone) => Ok(()),
                _ => Err(VfsError::AlreadyExists),
            };
        }
        // Not cached upper: a live lower name would collide.
        if let Some(ld) = parent_lower {
            let lower = self.lower.as_ref().ok_or(VfsError::NotFound)?;
            if lower.lookup(ld, name).is_ok() {
                return Err(VfsError::AlreadyExists);
            }
        }
        Ok(())
    }

    /// Insert `name → id` into `parent`'s child map. The caller must have
    /// [`resolve_dir`](Self::resolve_dir)'d `parent` first.
    fn link(&mut self, parent: InodeId, name: &OsStr, id: InodeId) -> VfsResult<()> {
        if let Some(Inode { body: Body::Dir { children, .. }, .. }) = self.inodes.get_mut(&parent) {
            children.open_mut()?.insert(name.to_os_string(), id);
        }
        Ok(())
    }

    /// Remove `name` from `parent`'s child map. The caller must have
    /// [`resolve_dir`](Self::resolve_dir)'d `parent` first.
    fn unlink_child(&mut self, parent: InodeId, name: &OsStr) -> VfsResult<()> {
        if let Some(Inode { body: Body::Dir { children, .. }, .. }) = self.inodes.get_mut(&parent) {
            children.open_mut()?.remove(name);
        }
        Ok(())
    }
}
