//! Store-side **derived views** over a state dir — the machinery behind the
//! `epoch diff` / `epoch export` / `epoch audit` verbs.
//!
//! Everything here is a *read-only, offline* computation over two stores a
//! state dir already holds: the object store ([`TreeStore`] + [`FileCas`] over
//! one byte backend) and the [`TimelineStore`]. No mount is needed — only
//! checkpointed (durable) state is visible, which is exactly the contract the
//! verbs document.
//!
//! Three views:
//!
//! - [`diff_roots`] — the semantic diff between two sealed roots (or between a
//!   root and the empty base, interpreting its sparse delta against the lower
//!   directory): `write` / `delete` / `mkdir` / `rename` / `restore` entries.
//!   **Rename pairs are reconstructed**: a path whose content vanishes at A and
//!   reappears byte-identical at B is one `rename A→B`, not delete + create —
//!   reviewers read "renamed", not two unrelated ops.
//! - [`materialize`] — write the merged snapshot (overlay delta over the
//!   lower, tombstones honored) to a destination directory.
//! - [`audit_log`] — the schema'd write log: walk a head's first-parent chain
//!   and diff each consecutive pair, attaching the event's timestamp and
//!   [`Attribution`]. Claims and limits are documented on the function.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use epoch_cas::{Blake3Hasher, CASHash, ContentHasher, SyncByteBackend, SyncCAS};
use epoch_file_cas::{BlobId, FileCas};
use epoch_timeline::{Attribution, EventId, TimelineStore};
use epoch_tree::{EntryId, Overlay, Tree, TreeId, TreeStore};

/// The link type of a sealed worktree root: a sparse overlay layer whose
/// populated entries address file blobs ([`BlobId`]) or subtrees ([`TreeId`]),
/// plus tombstone whiteouts.
type Delta = Overlay<EntryId<BlobId>>;

/// Errors from the derived views.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    /// An object-store operation failed.
    #[error(transparent)]
    Cas(#[from] epoch_cas::CASError),
    /// A timeline-store operation failed.
    #[error(transparent)]
    Timeline(#[from] epoch_timeline::TimelineError),
    /// A reachable tree object is absent from the store.
    #[error("tree object missing from store: {0}")]
    MissingTree(TreeId),
    /// A reachable blob's content is absent from the store.
    #[error("blob content missing from store: {0:?}")]
    MissingBlob(BlobId),
    /// A filesystem operation on the lower or destination directory failed.
    #[error("i/o at {path}: {err}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying error.
        err: std::io::Error,
    },
    /// The named head does not exist in the timeline.
    #[error("unknown head: {0}")]
    UnknownHead(String),
}

impl InspectError {
    fn io(path: &Path) -> impl Fn(std::io::Error) -> InspectError + '_ {
        move |err| InspectError::Io {
            path: path.to_path_buf(),
            err,
        }
    }
}

/// One entry of a semantic tree diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEntry {
    /// A file was created or overwritten: the merged view holds `blob` at
    /// `path`. (Without a lower to compare against, create-vs-modify is not
    /// distinguished — both are "the overlay wrote this path".)
    Write {
        /// Path from the mount root.
        path: PathBuf,
        /// Content-address of the new content (resolvable via `.vfs/cas`).
        blob: BlobId,
        /// Logical size in bytes.
        size: u64,
        /// Unix mode bits (`0o120000` marks a symlink whose blob is the target).
        mode: u32,
    },
    /// A path disappeared from the merged view: an upper entry was dropped, or
    /// a tombstone now hides the lower entry.
    Delete {
        /// Path from the mount root.
        path: PathBuf,
        /// Content the path held, when known (an upper blob; `None` for a
        /// tombstoned lower path — the lower's content is not the overlay's).
        previous: Option<BlobId>,
        /// Whether the deleted path was a directory (contents implied).
        dir: bool,
    },
    /// A directory was created (absent from the base and from the lower).
    Mkdir {
        /// Path from the mount root.
        path: PathBuf,
    },
    /// A reconstructed rename: identical content vanished at `from` and
    /// appeared at `to`.
    Rename {
        /// Source path (vanished).
        from: PathBuf,
        /// Destination path (appeared).
        to: PathBuf,
        /// The moved content, when it is overlay-owned.
        blob: Option<BlobId>,
        /// Whether a directory moved (matched by identical subtree id).
        dir: bool,
    },
    /// A path reverted to its lower state: the overlay entry (or tombstone)
    /// was lifted and the lower entry shows through again. Only distinguishable
    /// from [`Delete`](DiffEntry::Delete) when the lower is known.
    Restore {
        /// Path from the mount root.
        path: PathBuf,
        /// Whether the restored path is a directory.
        dir: bool,
    },
}

impl DiffEntry {
    /// The op name used in the JSON schema.
    pub fn op(&self) -> &'static str {
        match self {
            DiffEntry::Write { .. } => "write",
            DiffEntry::Delete { .. } => "delete",
            DiffEntry::Mkdir { .. } => "mkdir",
            DiffEntry::Rename { .. } => "rename",
            DiffEntry::Restore { .. } => "restore",
        }
    }

    /// The path to sort and display by (`from` for renames).
    pub fn path(&self) -> &Path {
        match self {
            DiffEntry::Write { path, .. }
            | DiffEntry::Delete { path, .. }
            | DiffEntry::Mkdir { path }
            | DiffEntry::Restore { path, .. }
            | DiffEntry::Rename { from: path, .. } => path,
        }
    }
}

/// The semantic diff of `new` against `old` (or against the empty base when
/// `old` is `None`, i.e. "what the overlay delta changes relative to the
/// lower").
///
/// `lower`, when given, is the overlaid host directory and sharpens three
/// classifications:
///
/// - a new subtree that already exists in the lower is not reported as
///   `mkdir` (it merely gained overlay entries);
/// - a dropped upper entry whose path still exists in the lower is a
///   `restore`, not a `delete`;
/// - rename reconstruction can pair a tombstoned **lower** file with its
///   re-created content (by hashing bytes on both sides) instead of only
///   pairing overlay-owned blobs by id.
pub fn diff_roots<B>(
    trees: &TreeStore<B, Delta>,
    files: &FileCas<B>,
    old: Option<&TreeId>,
    new: &TreeId,
    lower: Option<&Path>,
) -> Result<Vec<DiffEntry>, InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
{
    let mut raw = Vec::new();
    diff_dir(trees, Path::new(""), old.copied(), Some(*new), lower, &mut raw)?;
    let mut flat = Vec::new();
    flatten(raw, &mut flat);
    finalize(files, flat, lower)
}

/// One entry of the audit log: the changes one checkpoint event introduced,
/// with its timestamp and mount attribution.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    /// The event this record derives from.
    pub event: EventId,
    /// The event's (checkpoint-resolution) timestamp.
    pub timestamp_ms: u64,
    /// Mount attribution parsed from the event's `meta`, when present.
    pub attribution: Option<Attribution>,
    /// The semantic diff of this event's root against its first parent's.
    pub entries: Vec<DiffEntry>,
}

/// Derive the audit log of `head`: for each event on the first-parent chain
/// (oldest first), the diff against its parent.
///
/// ## Claims (these exact ones, no more)
///
/// - **Complete for writes and deletes of paths under the mountpoint** that
///   were captured into checkpoints — every mutation the overlay captured is
///   reflected in some event's diff.
/// - **Timestamp resolution is the checkpoint debounce**, not per-op: several
///   ops share one event's timestamp, and an op that happened and was undone
///   within one debounce window leaves no trace.
/// - **Reads are not recorded at all.** Fall-through reads never touch the
///   overlay; this log says nothing about them.
/// - **The overlay is not a sandbox**: this audit claims are scoped to
///   accidents and to review. A child inside a mount can write the host
///   directly (or unmount and write the lower) without leaving a trace here.
pub fn audit_log<B, T>(
    timeline: &T,
    trees: &TreeStore<B, Delta>,
    files: &FileCas<B>,
    head: &str,
    lower: Option<&Path>,
) -> Result<Vec<AuditRecord>, InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
    T: TimelineStore + ?Sized,
{
    if timeline.get_head(head)?.is_none() {
        return Err(InspectError::UnknownHead(head.to_string()));
    }
    let mut chain = timeline.log(head, usize::MAX)?; // newest first
    chain.reverse(); // chronological
    let mut records = Vec::with_capacity(chain.len());
    let mut prev: Option<TreeId> = None;
    for (id, event) in chain {
        let entries = diff_roots(trees, files, prev.as_ref(), &event.root, lower)?;
        records.push(AuditRecord {
            event: id,
            timestamp_ms: event.timestamp_ms,
            attribution: event.meta.as_deref().and_then(Attribution::parse),
            entries,
        });
        prev = Some(event.root);
    }
    Ok(records)
}

/// Counters returned by [`materialize`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaterializeStats {
    /// Regular files written.
    pub files: u64,
    /// Symlinks created.
    pub symlinks: u64,
    /// Directories created.
    pub dirs: u64,
    /// Total file bytes written (symlink targets excluded).
    pub bytes: u64,
}

/// Materialize the merged snapshot of `root` (overlay delta over `lower`,
/// tombstones honored) into `dest`, which is created and must be empty.
///
/// Overlay files are written from the CAS with their recorded permission
/// bits; symlink entries (mode `0o120000`) are recreated as symlinks. Names
/// absent from the delta are copied through from `lower` — files by
/// `fs::copy` (permissions preserved), symlinks recreated, directories
/// recursed. Mirroring the in-mount merged view, a lower `.vfs` entry at the
/// root is skipped (the control plane shadows it under a mount).
pub fn materialize<B>(
    trees: &TreeStore<B, Delta>,
    files: &FileCas<B>,
    root: &TreeId,
    lower: Option<&Path>,
    dest: &Path,
) -> Result<MaterializeStats, InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
{
    if dest.exists() && fs::read_dir(dest).map_err(InspectError::io(dest))?.next().is_some() {
        return Err(InspectError::Io {
            path: dest.to_path_buf(),
            err: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "destination exists and is not empty",
            ),
        });
    }
    let mut stats = MaterializeStats::default();
    fs::create_dir_all(dest).map_err(InspectError::io(dest))?;
    materialize_dir(trees, files, *root, lower, dest, true, &mut stats)?;
    Ok(stats)
}

// --- diff: raw walk ----------------------------------------------------------

/// A pre-reconstruction diff entry. Rename pairs are found over the flattened
/// form of these in [`finalize`].
enum Raw {
    /// Populated file in the new tree (new or changed blob).
    Write { path: PathBuf, blob: BlobId, size: u64, mode: u32 },
    /// The new tree tombstones this name (hides the lower entry).
    Delete { path: PathBuf, previous: Option<BlobId>, dir: bool },
    /// A tombstone was lifted: the lower entry shows through again.
    Restore { path: PathBuf, dir: bool },
    /// An upper file entry vanished outright: a delete of overlay-only content,
    /// a revert to the lower, or the source of a rename — decided in finalize.
    Vanished { path: PathBuf, previous: BlobId },
    /// A populated subtree in the new tree with no old counterpart; `contents`
    /// is the recursively-walked delta of its interior (all creates).
    DirNew { path: PathBuf, tree: TreeId, contents: Vec<Raw> },
    /// A populated subtree in the old tree with no new counterpart (dropped or
    /// tombstoned). `tombstoned` records which: a tombstoned dir can never be a
    /// restore (the lower entry is hidden too).
    DirGone { path: PathBuf, tree: TreeId, tombstoned: bool },
}

/// A flattener target for [`Raw`]: [`Raw::DirNew`] becomes a marker plus its
/// contents inline, so rename matching is global across directory boundaries.
enum Flat {
    Write { path: PathBuf, blob: BlobId, size: u64, mode: u32 },
    Delete { path: PathBuf, previous: Option<BlobId>, dir: bool },
    Restore { path: PathBuf, dir: bool },
    Vanished { path: PathBuf, previous: BlobId },
    /// Marker for a new subtree; `contents` is the index range of its
    /// (already flattened) interior entries, dropped if the dir is renamed.
    DirNew { path: PathBuf, tree: TreeId, contents: std::ops::Range<usize> },
    DirGone { path: PathBuf, tree: TreeId, tombstoned: bool },
}

/// How a tree entry (or its absence) sits on one side of the diff.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Absent,
    Tombstone,
    Blob(BlobId, u64, u32),
    Tree(TreeId),
}

fn side(entry: Option<&epoch_tree::TreeEntry<Delta>>) -> Side {
    match entry {
        None => Side::Absent,
        Some(e) => match e.id {
            Overlay::Tombstone => Side::Tombstone,
            Overlay::Populated(EntryId::Blob(b)) => Side::Blob(b, e.size, e.mode),
            Overlay::Populated(EntryId::Tree(t)) => Side::Tree(t),
        },
    }
}

/// Recursive merge-walk of two sorted entry lists, appending raw entries.
/// `old`/`new` are the subtree ids of the two sides at `prefix` (`None` = no
/// overlay entries there); `lower_dir` is `prefix` resolved against the lower.
#[allow(clippy::too_many_arguments)]
fn diff_dir<B>(
    trees: &TreeStore<B, Delta>,
    prefix: &Path,
    old: Option<TreeId>,
    new: Option<TreeId>,
    lower_dir: Option<&Path>,
    out: &mut Vec<Raw>,
) -> Result<(), InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
{
    let load = |id: TreeId| -> Result<Tree<Delta>, InspectError> {
        trees
            .get(&id)?
            .ok_or(InspectError::MissingTree(id))
    };
    let old_tree = old.map(&load).transpose()?;
    let new_tree = new.map(&load).transpose()?;

    // Merge-join the two name-sorted entry lists.
    let mut oi = old_tree.as_ref().map(|t| t.entries()).unwrap_or(&[]).iter().peekable();
    let mut ni = new_tree.as_ref().map(|t| t.entries()).unwrap_or(&[]).iter().peekable();
    loop {
        let name: &[u8] = match (oi.peek(), ni.peek()) {
            (None, None) => break,
            (Some(o), None) => &o.name,
            (None, Some(n)) => &n.name,
            (Some(o), Some(n)) => o.name.as_ref().min(n.name.as_ref()),
        };
        let o = match oi.peek() {
            Some(e) if e.name.as_ref() == name => oi.next(),
            _ => None,
        };
        let n = match ni.peek() {
            Some(e) if e.name.as_ref() == name => ni.next(),
            _ => None,
        };

        let child = prefix.join(OsStr::from_bytes(name));
        let child_lower = lower_dir.map(|d| d.join(OsStr::from_bytes(name)));
        match (side(o), side(n)) {
            (Side::Absent, Side::Absent) => unreachable!("name came from one side"),
            // New or changed file content.
            (Side::Absent, Side::Blob(b, sz, m))
            | (Side::Tombstone, Side::Blob(b, sz, m)) => {
                out.push(Raw::Write { path: child, blob: b, size: sz, mode: m });
            }
            (Side::Blob(a, _, _), Side::Blob(b, sz, m)) => {
                if a != b {
                    out.push(Raw::Write { path: child, blob: b, size: sz, mode: m });
                }
            }
            // A fresh whiteout: hides the lower entry (previous content is the
            // lower's, not the overlay's).
            (Side::Absent, Side::Tombstone) => {
                out.push(Raw::Delete { path: child, previous: None, dir: false });
            }
            (Side::Blob(b, _, _), Side::Tombstone) => {
                out.push(Raw::Delete { path: child, previous: Some(b), dir: false });
            }
            (Side::Tree(t), Side::Tombstone) => {
                out.push(Raw::DirGone { path: child, tree: t, tombstoned: true });
            }
            // Tombstone lifted: the lower shows through again.
            (Side::Tombstone, Side::Absent) => {
                out.push(Raw::Restore { path: child, dir: false });
            }
            // Upper entry dropped: delete / restore / rename-source — finalize.
            (Side::Blob(b, _, _), Side::Absent) => {
                out.push(Raw::Vanished { path: child, previous: b });
            }
            (Side::Tree(t), Side::Absent) => {
                out.push(Raw::DirGone { path: child, tree: t, tombstoned: false });
            }
            // New subtree: record it and walk its interior as all-creates.
            (Side::Absent | Side::Tombstone, Side::Tree(t)) => {
                let mut contents = Vec::new();
                diff_dir(trees, &child, None, Some(t), child_lower.as_deref(), &mut contents)?;
                out.push(Raw::DirNew { path: child, tree: t, contents });
            }
            // Type changes: the old side goes away, the new side appears.
            (Side::Blob(b, _, _), Side::Tree(t)) => {
                out.push(Raw::Vanished { path: child.clone(), previous: b });
                let mut contents = Vec::new();
                diff_dir(trees, &child, None, Some(t), child_lower.as_deref(), &mut contents)?;
                out.push(Raw::DirNew { path: child, tree: t, contents });
            }
            (Side::Tree(t), Side::Blob(b, sz, m)) => {
                out.push(Raw::DirGone { path: child.clone(), tree: t, tombstoned: false });
                out.push(Raw::Write { path: child, blob: b, size: sz, mode: m });
            }
            // Recurse where both sides have (different) subtrees.
            (Side::Tree(a), Side::Tree(b)) => {
                if a != b {
                    diff_dir(trees, &child, Some(a), Some(b), child_lower.as_deref(), out)?;
                }
            }
            (Side::Tombstone, Side::Tombstone) => {}
        }
    }
    Ok(())
}

fn flatten(raw: Vec<Raw>, out: &mut Vec<Flat>) {
    for entry in raw {
        match entry {
            Raw::Write { path, blob, size, mode } => {
                out.push(Flat::Write { path, blob, size, mode })
            }
            Raw::Delete { path, previous, dir } => {
                out.push(Flat::Delete { path, previous, dir })
            }
            Raw::Restore { path, dir } => out.push(Flat::Restore { path, dir }),
            Raw::Vanished { path, previous } => {
                out.push(Flat::Vanished { path, previous })
            }
            Raw::DirGone { path, tree, tombstoned } => {
                out.push(Flat::DirGone { path, tree, tombstoned })
            }
            Raw::DirNew { path, tree, contents } => {
                let marker = out.len();
                out.push(Flat::DirNew { path: path.clone(), tree, contents: marker..marker });
                flatten(contents, out);
                let end = out.len();
                out[marker] = Flat::DirNew { path, tree, contents: marker + 1..end };
            }
        }
    }
}

// --- diff: rename reconstruction + classification ----------------------------

/// A byte-content key for cross-domain rename matching (lower file ↔ overlay
/// blob): blake3 over the file's bytes, size-prefixed by construction.
fn content_key(bytes: &[u8]) -> CASHash {
    Blake3Hasher::hash(bytes)
}

fn finalize<B>(
    files: &FileCas<B>,
    flat: Vec<Flat>,
    lower: Option<&Path>,
) -> Result<Vec<DiffEntry>, InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
{
    let mut slots: Vec<Option<Flat>> = flat.into_iter().map(Some).collect();
    let mut out: Vec<DiffEntry> = Vec::new();

    // 1. Directory renames: identical subtree id vanished here, appeared there.
    let dir_news: Vec<usize> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Some(Flat::DirNew { .. }) => Some(i),
            _ => None,
        })
        .collect();
    for i in 0..slots.len() {
        let (gone_path, gone_tree, gone_tombstoned) = match &slots[i] {
            Some(Flat::DirGone { path, tree, tombstoned }) => {
                (path.clone(), *tree, *tombstoned)
            }
            _ => continue,
        };
        let matched = dir_news.iter().find(|&&j| {
            matches!(&slots[j], Some(Flat::DirNew { tree, .. }) if *tree == gone_tree)
        });
        if let Some(&j) = matched {
            if let Some(Flat::DirNew { path, contents, .. }) = slots[j].take() {
                for k in contents {
                    slots[k] = None; // interior moved wholesale with the dir
                }
                out.push(DiffEntry::Rename {
                    from: gone_path,
                    to: path,
                    blob: None,
                    dir: true,
                });
            }
            slots[i] = None;
        } else {
            // Unmatched: tombstoned ⇒ delete; otherwise lower decides.
            let entry = if gone_tombstoned {
                DiffEntry::Delete { path: gone_path, previous: None, dir: true }
            } else if lower.is_some_and(|l| l.join(&gone_path).is_dir()) {
                DiffEntry::Restore { path: gone_path, dir: true }
            } else {
                DiffEntry::Delete { path: gone_path, previous: None, dir: true }
            };
            out.push(entry);
            slots[i] = None;
        }
    }

    // 2a. File renames, overlay-owned: identical blob id on both sides.
    let write_idx_by_blob: HashMap<BlobId, usize> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Some(Flat::Write { blob, .. }) => Some((*blob, i)),
            _ => None,
        })
        .collect();
    for i in 0..slots.len() {
        let (from, previous) = match &slots[i] {
            Some(Flat::Delete { path, previous: Some(b), dir: false })
            | Some(Flat::Vanished { path, previous: b }) => (path.clone(), *b),
            _ => continue,
        };
        if let Some(&j) = write_idx_by_blob.get(&previous)
            && slots[j].is_some()
        {
            if let Some(Flat::Write { path, blob, .. }) = slots[j].take() {
                out.push(DiffEntry::Rename { from, to: path, blob: Some(blob), dir: false });
            }
            slots[i] = None;
        }
    }

    // 2b. File renames across the lower boundary (only possible when the lower
    // is known): hash bytes on both sides and pair tombstoned-lower deletes
    // with re-created writes.
    if let Some(lower) = lower {
        let blob_key = |files: &FileCas<B>, blob: BlobId| -> Result<Option<CASHash>, InspectError> {
            Ok(files.get(&blob)?.map(|b| content_key(&b)))
        };
        for i in 0..slots.len() {
            let (from, key) = match &slots[i] {
                Some(Flat::Delete { path, previous: None, dir: false }) => {
                    let lower_path = lower.join(path);
                    match fs::read(&lower_path) {
                        Ok(bytes) => (path.clone(), content_key(&bytes)),
                        Err(_) => continue, // lower drifted; leave as a plain delete
                    }
                }
                Some(Flat::Delete { path, previous: Some(b), dir: false })
                | Some(Flat::Vanished { path, previous: b }) => {
                    match blob_key(files, *b)? {
                        Some(k) => (path.clone(), k),
                        None => return Err(InspectError::MissingBlob(*b)),
                    }
                }
                _ => continue,
            };
            let mut matched: Option<usize> = None;
            for (j, slot) in slots.iter().enumerate() {
                if let Some(Flat::Write { blob, .. }) = slot
                    && blob_key(files, *blob)? == Some(key)
                {
                    matched = Some(j);
                    break;
                }
            }
            if let Some(j) = matched {
                if let Some(Flat::Write { path, blob, .. }) = slots[j].take() {
                    out.push(DiffEntry::Rename { from, to: path, blob: Some(blob), dir: false });
                }
                slots[i] = None;
            }
        }
    }

    // 3. Everything left is a plain create/delete/restore.
    for slot in &mut slots {
        let entry = match slot.take() {
            None => continue,
            Some(Flat::Write { path, blob, size, mode }) => {
                DiffEntry::Write { path, blob, size, mode }
            }
            Some(Flat::Delete { path, previous, dir }) => DiffEntry::Delete { path, previous, dir },
            Some(Flat::Restore { path, dir }) => {
                // Without a known lower a lifted tombstone *is* a restore; with
                // one, only if the path actually exists there (drift ⇒ no-op).
                if lower.is_some_and(|l| !l.join(&path).exists()) {
                    continue;
                }
                let dir = dir || lower.is_some_and(|l| l.join(&path).is_dir());
                DiffEntry::Restore { path, dir }
            }
            Some(Flat::Vanished { path, previous }) => {
                if lower.is_some_and(|l| l.join(&path).exists()) {
                    DiffEntry::Restore { path, dir: false }
                } else {
                    DiffEntry::Delete { path, previous: Some(previous), dir: false }
                }
            }
            Some(Flat::DirNew { path, .. }) => {
                if lower.is_some_and(|l| l.join(&path).is_dir()) {
                    continue; // pre-existing lower dir that gained overlay entries
                }
                DiffEntry::Mkdir { path }
            }
            Some(Flat::DirGone { .. }) => unreachable!("resolved in step 1"),
        };
        out.push(entry);
    }

    out.sort_by(|a, b| a.path().cmp(b.path()).then(a.op().cmp(b.op())));
    Ok(out)
}

// --- materialize -------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn materialize_dir<B>(
    trees: &TreeStore<B, Delta>,
    files: &FileCas<B>,
    tree_id: TreeId,
    lower_dir: Option<&Path>,
    dest_dir: &Path,
    at_root: bool,
    stats: &mut MaterializeStats,
) -> Result<(), InspectError>
where
    B: SyncByteBackend<CASHash> + Clone,
{
    let tree = trees
        .get(&tree_id)?
        .ok_or(InspectError::MissingTree(tree_id))?;

    // Overlay entries first; collect the name set so lower fall-through skips
    // overridden and tombstoned names.
    let mut overlay_names: Vec<&[u8]> = Vec::with_capacity(tree.len());
    for entry in tree.entries() {
        let name = OsStr::from_bytes(&entry.name);
        let dest = dest_dir.join(name);
        overlay_names.push(&entry.name);
        match entry.id {
            Overlay::Tombstone => {} // hidden: simply never copied through
            Overlay::Populated(EntryId::Blob(blob)) => {
                let bytes = files.get(&blob)?.ok_or(InspectError::MissingBlob(blob))?;
                if entry.mode & 0o170000 == epoch_tree::SYMLINK_MODE {
                    // A symlink's blob bytes are the raw target path.
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(OsStr::from_bytes(&bytes), &dest)
                        .map_err(InspectError::io(&dest))?;
                    stats.symlinks += 1;
                } else {
                    fs::write(&dest, &bytes).map_err(InspectError::io(&dest))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = if entry.mode == 0 { 0o644 } else { entry.mode & 0o7777 };
                        fs::set_permissions(&dest, fs::Permissions::from_mode(mode))
                            .map_err(InspectError::io(&dest))?;
                    }
                    stats.files += 1;
                    stats.bytes += bytes.len() as u64;
                }
            }
            Overlay::Populated(EntryId::Tree(sub)) => {
                fs::create_dir(&dest).map_err(InspectError::io(&dest))?;
                stats.dirs += 1;
                let child_lower = lower_dir.map(|d| d.join(name));
                materialize_dir(trees, files, sub, child_lower.as_deref(), &dest, false, stats)?;
            }
        }
    }

    // Lower fall-through: copy every lower name the overlay does not mention.
    if let Some(lower_dir) = lower_dir {
        let mut entries: Vec<_> = match fs::read_dir(lower_dir) {
            Ok(rd) => rd.collect::<Result<_, _>>().map_err(InspectError::io(lower_dir))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(InspectError::io(lower_dir)(e)),
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            if at_root && name == ".vfs" {
                continue; // the control plane shadows a lower `.vfs` in-mount
            }
            if overlay_names.iter().any(|n| *n == name.as_bytes()) {
                continue; // overridden or tombstoned
            }
            let dest = dest_dir.join(&name);
            let ty = entry.file_type().map_err(InspectError::io(&entry.path()))?;
            if ty.is_dir() {
                fs::create_dir(&dest).map_err(InspectError::io(&dest))?;
                stats.dirs += 1;
                copy_lower_dir(&entry.path(), &dest, stats)?;
            } else if ty.is_symlink() {
                let target = fs::read_link(entry.path()).map_err(InspectError::io(&entry.path()))?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &dest).map_err(InspectError::io(&dest))?;
                stats.symlinks += 1;
            } else if ty.is_file() {
                let meta = entry.metadata().map_err(InspectError::io(&entry.path()))?;
                fs::copy(entry.path(), &dest).map_err(InspectError::io(&dest))?;
                stats.files += 1;
                stats.bytes += meta.len();
            }
        }
    }
    Ok(())
}

/// Copy a lower directory subtree wholesale (no overlay involvement).
fn copy_lower_dir(src: &Path, dest: &Path, stats: &mut MaterializeStats) -> Result<(), InspectError> {
    let mut entries: Vec<_> = fs::read_dir(src)
        .map_err(InspectError::io(src))?
        .collect::<Result<_, _>>()
        .map_err(InspectError::io(src))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let dest_child = dest.join(entry.file_name());
        let ty = entry.file_type().map_err(InspectError::io(&entry.path()))?;
        if ty.is_dir() {
            fs::create_dir(&dest_child).map_err(InspectError::io(&dest_child))?;
            stats.dirs += 1;
            copy_lower_dir(&entry.path(), &dest_child, stats)?;
        } else if ty.is_symlink() {
            let target = fs::read_link(entry.path()).map_err(InspectError::io(&entry.path()))?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &dest_child).map_err(InspectError::io(&dest_child))?;
            stats.symlinks += 1;
        } else if ty.is_file() {
            let meta = entry.metadata().map_err(InspectError::io(&entry.path()))?;
            fs::copy(entry.path(), &dest_child).map_err(InspectError::io(&dest_child))?;
            stats.files += 1;
            stats.bytes += meta.len();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::sync::Arc;

    use epoch_cas::{Bytes, MemoryByteBackend};
    use epoch_timeline::MemoryTimelineStore;
    use epoch_vfs_api::ROOT_INODE_ID;
    use epoch_worktree::WorkingTree;

    use super::*;
    use crate::History;

    type Mem = MemoryByteBackend;

    fn stores() -> (TreeStore<Mem, Delta>, FileCas<Mem>) {
        let backend = MemoryByteBackend::new();
        (TreeStore::new(backend.clone()), FileCas::new(backend))
    }

    fn put_file(files: &FileCas<Mem>, content: &[u8]) -> BlobId {
        files.put(Bytes::copy_from_slice(content)).unwrap()
    }

    fn put_tree(trees: &TreeStore<Mem, Delta>, entries: Vec<epoch_tree::TreeEntry<Delta>>) -> TreeId {
        trees.put(Tree::from_entries(entries).unwrap()).unwrap()
    }

    fn file_entry(name: &[u8], blob: BlobId, size: u64) -> epoch_tree::TreeEntry<Delta> {
        epoch_tree::TreeEntry::file(Bytes::copy_from_slice(name), blob, 0o100644, size)
    }

    fn dir_entry(name: &[u8], tree: TreeId) -> epoch_tree::TreeEntry<Delta> {
        epoch_tree::TreeEntry::dir(Bytes::copy_from_slice(name), tree, 0o040755)
    }

    fn tomb_entry(name: &[u8]) -> epoch_tree::TreeEntry<Delta> {
        epoch_tree::TreeEntry::tombstone(Bytes::copy_from_slice(name))
    }

    /// Minimal unique temp dir (no tempfile dependency in the workspace).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            // nanos alone collide on coarse clocks when tests run in parallel.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "epoch-inspect-{tag}-{}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn diff_delta_against_empty_base_reports_writes_and_mkdirs() {
        let (trees, files) = stores();
        let hello = put_file(&files, b"hello");
        let inner = put_file(&files, b"inner-bytes");
        let sub = put_tree(&trees, vec![file_entry(b"inner", inner, 11)]);
        let root = put_tree(
            &trees,
            vec![
                file_entry(b"a.txt", hello, 5),
                dir_entry(b"d", sub),
                tomb_entry(b"gone.txt"),
            ],
        );

        let diff = diff_roots(&trees, &files, None, &root, None).unwrap();
        let ops: Vec<(&str, &Path)> = diff.iter().map(|e| (e.op(), e.path())).collect();
        assert_eq!(
            ops,
            vec![
                ("write", Path::new("a.txt")),
                ("mkdir", Path::new("d")),
                ("write", Path::new("d/inner")),
                ("delete", Path::new("gone.txt")),
            ]
        );
        match &diff[0] {
            DiffEntry::Write { blob, size, .. } => {
                assert_eq!(*blob, hello);
                assert_eq!(*size, 5);
            }
            other => panic!("expected write, got {other:?}"),
        }
    }

    #[test]
    fn pair_diff_reconstructs_file_rename_by_blob_id() {
        let (trees, files) = stores();
        let content = put_file(&files, b"same content");
        let old = put_tree(&trees, vec![file_entry(b"old.txt", content, 12)]);
        let new = put_tree(&trees, vec![file_entry(b"new.txt", content, 12)]);

        let diff = diff_roots(&trees, &files, Some(&old), &new, None).unwrap();
        assert_eq!(
            diff,
            vec![DiffEntry::Rename {
                from: PathBuf::from("old.txt"),
                to: PathBuf::from("new.txt"),
                blob: Some(content),
                dir: false,
            }]
        );
    }

    #[test]
    fn pair_diff_reconstructs_dir_rename_by_tree_id() {
        let (trees, files) = stores();
        let blob = put_file(&files, b"x");
        let sub = put_tree(&trees, vec![file_entry(b"f", blob, 1)]);
        let old = put_tree(&trees, vec![dir_entry(b"src", sub)]);
        let new = put_tree(&trees, vec![dir_entry(b"dst", sub)]);

        let diff = diff_roots(&trees, &files, Some(&old), &new, None).unwrap();
        assert_eq!(
            diff,
            vec![DiffEntry::Rename {
                from: PathBuf::from("src"),
                to: PathBuf::from("dst"),
                blob: None,
                dir: true,
            }],
            "identical subtree id = one dir rename, no interior entries"
        );
    }

    #[test]
    fn rename_across_the_lower_boundary_matches_on_content_bytes() {
        // A lower file renamed via the mount: old side absent + new tombstone
        // at the vacated name; new populated blob at the target (copied up
        // before the move). With the lower known, content hashing pairs them.
        let tmp = TempDir::new("lower-rename");
        fs::write(tmp.path().join("victim.txt"), b"victim bytes").unwrap();

        let (trees, files) = stores();
        let moved = put_file(&files, b"victim bytes");
        let old = put_tree(&trees, vec![]);
        let new = put_tree(
            &trees,
            vec![
                tomb_entry(b"victim.txt"),
                file_entry(b"moved.txt", moved, 12),
            ],
        );

        let diff = diff_roots(&trees, &files, Some(&old), &new, Some(tmp.path())).unwrap();
        assert_eq!(
            diff,
            vec![DiffEntry::Rename {
                from: PathBuf::from("victim.txt"),
                to: PathBuf::from("moved.txt"),
                blob: Some(moved),
                dir: false,
            }]
        );

        // Without the lower, the same pair cannot be reconstructed.
        let blind = diff_roots(&trees, &files, Some(&old), &new, None).unwrap();
        let ops: Vec<&str> = blind.iter().map(|e| e.op()).collect();
        assert_eq!(ops, vec!["write", "delete"], "sorted by path: moved.txt < victim.txt");
    }

    #[test]
    fn mkdir_suppressed_and_vanished_becomes_restore_when_lower_known() {
        let tmp = TempDir::new("lower-classify");
        fs::create_dir(tmp.path().join("pre")).unwrap();
        fs::write(tmp.path().join("reverted.txt"), b"lower original").unwrap();

        let (trees, files) = stores();
        let written = put_file(&files, b"new file in pre");
        let upper_blob = put_file(&files, b"upper edit");
        let sub = put_tree(&trees, vec![file_entry(b"fresh.txt", written, 15)]);
        // old: `reverted.txt` was edited (upper entry); new: entry dropped.
        let old = put_tree(&trees, vec![file_entry(b"reverted.txt", upper_blob, 10)]);
        let new = put_tree(&trees, vec![dir_entry(b"pre", sub)]);

        // Pair diff old → new, lower known.
        let diff = diff_roots(&trees, &files, Some(&old), &new, Some(tmp.path())).unwrap();
        let ops: Vec<(&str, &Path)> = diff.iter().map(|e| (e.op(), e.path())).collect();
        assert_eq!(
            ops,
            vec![
                ("write", Path::new("pre/fresh.txt")),
                ("restore", Path::new("reverted.txt")),
            ],
            "pre/ exists in the lower: no mkdir; dropped upper edit over an existing lower file is a restore"
        );

        // Delta-vs-base diff of `new` alone: same mkdir suppression.
        let delta = diff_roots(&trees, &files, None, &new, Some(tmp.path())).unwrap();
        let ops: Vec<(&str, &Path)> = delta.iter().map(|e| (e.op(), e.path())).collect();
        assert_eq!(ops, vec![("write", Path::new("pre/fresh.txt"))]);
    }

    #[test]
    fn end_to_end_session_audit_through_working_tree_and_history() {
        let tier_backend = MemoryByteBackend::new();
        let tier = epoch_cas::TieredByteBackend::new(tier_backend.clone(), tier_backend.clone());
        let timeline = Arc::new(MemoryTimelineStore::new());
        let history = History::new(tier.clone(), timeline.clone(), "main");
        let mut wt = WorkingTree::new(tier.clone());

        // Event 1: create a.txt + d/inner.
        let a = wt.create(ROOT_INODE_ID, OsStr::new("a.txt"), 0o644).unwrap();
        wt.write_at(a, 0, b"alpha").unwrap();
        let d = wt.mkdir(ROOT_INODE_ID, OsStr::new("d"), 0o755).unwrap();
        let inner = wt.create(d, OsStr::new("inner"), 0o644).unwrap();
        wt.write_at(inner, 0, b"in").unwrap();
        let root1 = wt.seal().unwrap();
        let attr = Attribution {
            mount: "m-test".into(),
            head: "main".into(),
            user: Some("lane-1".into()),
            host: None,
            lower: None,
        };
        history
            .commit_root_at(root1, 1_000, Some(attr.to_meta_json()))
            .unwrap()
            .unwrap();

        // Event 2: rename a.txt → b.txt (upper relink, same blob), delete inner.
        wt.rename(ROOT_INODE_ID, OsStr::new("a.txt"), ROOT_INODE_ID, OsStr::new("b.txt"))
            .unwrap();
        wt.unlink(d, OsStr::new("inner")).unwrap();
        let root2 = wt.seal().unwrap();
        history
            .commit_root_at(root2, 2_000, Some(attr.to_meta_json()))
            .unwrap()
            .unwrap();

        let trees: TreeStore<Mem, Delta> = TreeStore::new(tier_backend.clone());
        let files = FileCas::new(tier_backend);
        let log = audit_log(timeline.as_ref(), &trees, &files, "main", None).unwrap();

        assert_eq!(log.len(), 2);
        assert_eq!(log[0].timestamp_ms, 1_000);
        assert_eq!(log[0].attribution.as_ref().unwrap().mount, "m-test");
        assert_eq!(log[0].attribution.as_ref().unwrap().user.as_deref(), Some("lane-1"));
        let ops0: Vec<(&str, &Path)> = log[0].entries.iter().map(|e| (e.op(), e.path())).collect();
        assert_eq!(
            ops0,
            vec![
                ("write", Path::new("a.txt")),
                ("mkdir", Path::new("d")),
                ("write", Path::new("d/inner")),
            ]
        );

        let ops1: Vec<(&str, &Path)> = log[1].entries.iter().map(|e| (e.op(), e.path())).collect();
        assert_eq!(
            ops1,
            vec![
                ("rename", Path::new("a.txt")),
                ("delete", Path::new("d/inner")),
            ],
            "second event: the upper rename reconstructs; the upper-only delete is a plain delete"
        );
        match &log[1].entries[0] {
            DiffEntry::Rename { from, to, blob: Some(_), dir: false } => {
                assert_eq!(from, Path::new("a.txt"));
                assert_eq!(to, Path::new("b.txt"));
            }
            other => panic!("expected rename, got {other:?}"),
        }

        // Unknown head is an error, not an empty log.
        assert!(matches!(
            audit_log(timeline.as_ref(), &trees, &files, "nope", None),
            Err(InspectError::UnknownHead(_))
        ));
    }

    #[test]
    fn materialize_merges_overlay_over_lower_honoring_tombstones() {
        let tmp = TempDir::new("materialize");
        let lower = tmp.path().join("lower");
        fs::create_dir_all(lower.join("sub")).unwrap();
        fs::write(lower.join("keep.txt"), b"keep me").unwrap();
        fs::write(lower.join("hidden.txt"), b"tombstoned").unwrap();
        fs::write(lower.join("overridden.txt"), b"lower version").unwrap();
        fs::write(lower.join("sub/nested.txt"), b"nested").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("keep.txt", lower.join("link")).unwrap();
        fs::write(lower.join(".vfs"), b"shadowed in-mount").unwrap();

        let (trees, files) = stores();
        let fresh = put_file(&files, b"overlay version");
        let brand_new = put_file(&files, b"brand new");
        let root = put_tree(
            &trees,
            vec![
                tomb_entry(b"hidden.txt"),
                file_entry(b"overridden.txt", fresh, 15),
                file_entry(b"new.txt", brand_new, 9),
            ],
        );

        let dest = tmp.path().join("out");
        let stats = materialize(&trees, &files, &root, Some(&lower), &dest).unwrap();

        assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"keep me");
        assert_eq!(fs::read(dest.join("overridden.txt")).unwrap(), b"overlay version");
        assert_eq!(fs::read(dest.join("new.txt")).unwrap(), b"brand new");
        assert!(!dest.join("hidden.txt").exists(), "tombstone honored");
        assert_eq!(fs::read(dest.join("sub/nested.txt")).unwrap(), b"nested");
        assert!(!dest.join(".vfs").exists(), "root .vfs shadowed as in-mount");
        #[cfg(unix)]
        {
            let target = fs::read_link(dest.join("link")).unwrap();
            assert_eq!(target, Path::new("keep.txt"));
        }
        assert_eq!(stats.files, 4); // keep, overridden, new, sub/nested
        assert_eq!(stats.dirs, 1); // sub

        // A non-empty destination is refused.
        assert!(matches!(
            materialize(&trees, &files, &root, Some(&lower), &dest),
            Err(InspectError::Io { .. })
        ));
    }
}
