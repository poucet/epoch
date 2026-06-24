//! [`TreeEntry`] — one named child of a [`Tree`](crate::Tree).
//!
//! An entry is the tree analog of a git tree record: a `(name, mode, child link,
//! size)` tuple. The `name` is *raw* filename bytes, not a `String`: unix
//! filenames are an arbitrary byte sequence with only `/` and NUL excluded, and
//! are not guaranteed to be valid UTF-8, so forcing them through `String` would
//! be lossy. We keep them as [`Bytes`].
//!
//! `TreeEntry<L>` is generic over the **link** type `L` (see [`Link`]) — the
//! kind of child the entry points at. A *complete* tree uses
//! [`EntryId<B>`](crate::EntryId) (a blob or subtree, never a whiteout); a
//! *sparse overlay* tree uses [`Overlay<EntryId<B>>`](crate::Overlay) (which adds
//! the tombstone). `B` is the foreign blob-id type (see
//! [`BlobRef`](crate::BlobRef)): a tree references blobs by whatever id type the
//! file CAS produces, and only ever names its *own* subtrees via [`TreeId`].
//!
//! ## Symlinks need no special variant
//!
//! Following git, a symlink is *not* a distinct entry kind. It is a blob entry
//! whose [`mode`](TreeEntry::mode) is [`SYMLINK_MODE`] (`0o120000`) and whose
//! blob bytes *are the link target path*. Only the mode distinguishes a symlink
//! from a regular file. This keeps the object model small — there is exactly one
//! indirection kind (blob) and one container kind (tree).
//!
//! ## Tombstones (overlay whiteouts)
//!
//! A complete tree cannot hold a whiteout — [`tombstone`](TreeEntry::tombstone)
//! exists *only* on `TreeEntry<Overlay<E>>`, so an illegal "tombstone in a
//! non-overlay tree" layout will not type-check. In an overlay tree read against
//! a base: an **absent** name falls through to the lower layer, a tombstone
//! **hides** the lower entry (a deletion in the merged view), and a populated
//! entry **overrides** it. A tombstone addresses no object: its `id` is
//! [`Overlay::Tombstone`] and its [`mode`](TreeEntry::mode) and
//! [`size`](TreeEntry::size) are `0`.

use epoch_cas::Bytes;

use crate::id::{Link, Overlay, TreeId};

/// The file mode marking a blob entry as a symlink.
///
/// Matches the unix/git convention `0o120000`. A symlink's blob content is the
/// raw target path bytes; see the [module docs](self) for the rationale behind
/// modelling symlinks as moded blobs rather than a separate entry kind.
pub const SYMLINK_MODE: u32 = 0o120000;

/// One named child of a tree: a filename plus the child's link, mode and size.
///
/// `name` is raw filename bytes (not necessarily UTF-8). `mode` carries the
/// unix permission/type bits (e.g. `0o100644` for a regular file, `0o040000`
/// for a directory, [`SYMLINK_MODE`] for a symlink). `size` is the logical
/// content size in bytes — for directories (and tombstones) it is conventionally
/// `0`.
///
/// Generic over the link type `L` (see the [module docs](self)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry<L> {
    /// Raw filename bytes (unix names are not guaranteed UTF-8).
    pub name: Bytes,
    /// The child's link (blob / subtree, plus tombstone for overlay trees).
    pub id: L,
    /// Unix mode/permission bits (see [`SYMLINK_MODE`]); `0` for a tombstone.
    pub mode: u32,
    /// Logical content size in bytes (`0` by convention for directories and
    /// tombstones).
    pub size: u64,
}

impl<L: Link> TreeEntry<L> {
    /// Construct an entry pointing at a file blob.
    ///
    /// Pass [`SYMLINK_MODE`] as `mode` (with `size` = the target path length and
    /// the blob bytes = the target) to represent a symlink; see the
    /// [module docs](self). Works for any link type `L` (complete or overlay).
    pub fn file(name: impl Into<Bytes>, id: L::Blob, mode: u32, size: u64) -> TreeEntry<L> {
        TreeEntry {
            name: name.into(),
            id: L::from_blob(id),
            mode,
            size,
        }
    }

    /// Construct an entry pointing at a subtree. `size` is fixed to `0` — a
    /// directory has no meaningful content length.
    pub fn dir(name: impl Into<Bytes>, id: TreeId, mode: u32) -> TreeEntry<L> {
        TreeEntry {
            name: name.into(),
            id: L::from_tree(id),
            mode,
            size: 0,
        }
    }
}

impl<E> TreeEntry<Overlay<E>> {
    /// Construct a tombstone (whiteout) entry hiding `name` in an overlay layer.
    ///
    /// Defined *only* for an overlay link, so a complete tree
    /// (`TreeEntry<EntryId<B>>`) cannot construct one — the illegal layout does
    /// not type-check. A tombstone addresses no object, so both `mode` and
    /// `size` are fixed to `0`; see the [module docs](self).
    pub fn tombstone(name: impl Into<Bytes>) -> TreeEntry<Overlay<E>> {
        TreeEntry {
            name: name.into(),
            id: Overlay::Tombstone,
            mode: 0,
            size: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntryId, Link, LinkKind};
    use epoch_cas::CASHash;

    // CASHash is used as the blob-id type B in tests (it satisfies BlobRef).
    type Complete = EntryId<CASHash>;
    type Sparse = Overlay<EntryId<CASHash>>;

    fn sample_hash(seed: u8) -> CASHash {
        CASHash::from_bytes([seed; 32])
    }

    #[test]
    fn file_constructor_sets_blob_link() {
        let id = sample_hash(1);
        let e = TreeEntry::<Complete>::file(Bytes::from_static(b"readme.txt"), id, 0o100644, 42);
        assert_eq!(e.name, Bytes::from_static(b"readme.txt"));
        assert_eq!(e.id, EntryId::Blob(id));
        assert_eq!(e.mode, 0o100644);
        assert_eq!(e.size, 42);
        assert!(e.id.is_blob());
    }

    #[test]
    fn dir_constructor_forces_zero_size() {
        let id = TreeId::from_cas_hash(sample_hash(2));
        let e = TreeEntry::<Complete>::dir(Bytes::from_static(b"src"), id, 0o040000);
        assert_eq!(e.id, EntryId::Tree(id));
        assert_eq!(e.size, 0);
        assert!(e.id.is_tree());
    }

    #[test]
    fn overlay_file_wraps_in_populated() {
        let id = sample_hash(3);
        let e = TreeEntry::<Sparse>::file(Bytes::from_static(b"f"), id, 0o100644, 7);
        assert_eq!(e.id, Overlay::Populated(EntryId::Blob(id)));
        assert_eq!(e.id.kind(), LinkKind::Blob(id));
    }

    #[test]
    fn tombstone_constructor_is_zero_moded_whiteout() {
        let e = TreeEntry::<Sparse>::tombstone(Bytes::from_static(b"gone"));
        assert_eq!(e.name, Bytes::from_static(b"gone"));
        assert_eq!(e.id, Overlay::Tombstone);
        assert_eq!(e.mode, 0);
        assert_eq!(e.size, 0);
        assert!(e.id.is_tombstone());
        assert_eq!(Link::cas_hash(&e.id), None);
    }

    #[test]
    fn symlink_is_a_blob_with_symlink_mode() {
        // A symlink "link" -> "target/path" is just a blob entry whose mode is
        // SYMLINK_MODE; the blob's bytes (elsewhere in the file CAS) are the
        // target path.
        let target = b"target/path";
        let id = sample_hash(4);
        let e = TreeEntry::<Complete>::file(
            Bytes::from_static(b"link"),
            id,
            SYMLINK_MODE,
            target.len() as u64,
        );
        assert_eq!(e.mode, SYMLINK_MODE);
        assert!(e.id.is_blob());
        assert_eq!(e.size, target.len() as u64);
    }
}
