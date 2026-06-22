//! [`TreeEntry`] — one named child of a [`Tree`](crate::Tree).
//!
//! An entry is the tree analog of a git tree record: a `(name, mode, child id,
//! size)` tuple. The `name` is *raw* filename bytes, not a `String`: unix
//! filenames are an arbitrary byte sequence with only `/` and NUL excluded, and
//! are not guaranteed to be valid UTF-8, so forcing them through `String` would
//! be lossy. We keep them as [`Bytes`].
//!
//! `TreeEntry<B>` is generic over the foreign blob-id type `B` (see
//! [`BlobRef`](crate::BlobRef)): a tree references blobs by whatever id type the
//! file CAS produces, and only ever names its *own* subtrees via [`TreeId`].
//!
//! ## Symlinks need no special variant
//!
//! Following git, a symlink is *not* a distinct entry kind. It is a [blob]
//! entry whose [`mode`](TreeEntry::mode) is [`SYMLINK_MODE`] (`0o120000`) and
//! whose blob bytes *are the link target path*. The [`EntryId::Blob`] address
//! therefore points at a regular blob in the file CAS; only the mode
//! distinguishes a symlink from a regular file. This keeps the object model
//! small — there is exactly one indirection kind (blob) and one container kind
//! (tree).
//!
//! [blob]: EntryId::Blob
//!
//! ## Tombstones (overlay whiteouts)
//!
//! A tree is normally a complete directory listing, but it can also act as a
//! **sparse overlay layer** over a lower/base tree. A [tombstone] entry is the
//! whiteout marker for that mode. When a tree is read as an overlay over a base:
//!
//! - an **absent** name falls through to the lower layer;
//! - a [tombstone] entry **hides** the lower entry (a deletion in the merged
//!   view);
//! - a [blob]/[tree] entry **overrides** the lower entry at that name.
//!
//! A tombstone addresses no object: its [`id`](TreeEntry::id) is
//! [`EntryId::Tombstone`] and its [`mode`](TreeEntry::mode) and
//! [`size`](TreeEntry::size) are `0`. A tree with no tombstones, read with no
//! base, is a plain complete tree — the overlay model is backward compatible.
//!
//! [tree]: EntryId::Tree
//! [tombstone]: EntryId::Tombstone

use epoch_cas::Bytes;

use crate::id::{EntryId, TreeId};

/// The file mode marking a [blob](EntryId::Blob) entry as a symlink.
///
/// Matches the unix/git convention `0o120000`. A symlink's blob content is the
/// raw target path bytes; see the [module docs](self) for the rationale behind
/// modelling symlinks as moded blobs rather than a separate entry kind.
pub const SYMLINK_MODE: u32 = 0o120000;

/// One named child of a tree: a filename plus the child's id, mode and size.
///
/// `name` is raw filename bytes (not necessarily UTF-8). `mode` carries the
/// unix permission/type bits (e.g. `0o100644` for a regular file, `0o040000`
/// for a directory, [`SYMLINK_MODE`] for a symlink). `size` is the logical
/// content size in bytes — for directories it is conventionally `0`, since a
/// directory's "size" is not a meaningful content length.
///
/// Generic over the foreign blob-id type `B`; see the [module docs](self).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry<B> {
    /// Raw filename bytes (unix names are not guaranteed UTF-8).
    pub name: Bytes,
    /// The child object's id, tagged blob vs tree.
    pub id: EntryId<B>,
    /// Unix mode/permission bits (see [`SYMLINK_MODE`]).
    pub mode: u32,
    /// Logical content size in bytes (`0` by convention for directories).
    pub size: u64,
}

impl<B> TreeEntry<B> {
    /// Construct an entry pointing at a file blob.
    ///
    /// Pass [`SYMLINK_MODE`] as `mode` (with `size` = the target path length and
    /// the blob bytes = the target) to represent a symlink; see the
    /// [module docs](self).
    pub fn file(name: impl Into<Bytes>, id: B, mode: u32, size: u64) -> TreeEntry<B> {
        TreeEntry {
            name: name.into(),
            id: EntryId::Blob(id),
            mode,
            size,
        }
    }

    /// Construct an entry pointing at a subtree. `size` is fixed to `0` — a
    /// directory has no meaningful content length.
    pub fn dir(name: impl Into<Bytes>, id: TreeId, mode: u32) -> TreeEntry<B> {
        TreeEntry {
            name: name.into(),
            id: EntryId::Tree(id),
            mode,
            size: 0,
        }
    }

    /// Construct a tombstone (whiteout) entry hiding `name` in an overlay layer.
    ///
    /// A tombstone addresses no object, so both `mode` and `size` are fixed to
    /// `0`; see the [module docs](self) for the overlay semantics.
    pub fn tombstone(name: impl Into<Bytes>) -> TreeEntry<B> {
        TreeEntry {
            name: name.into(),
            id: EntryId::Tombstone,
            mode: 0,
            size: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_cas::CASHash;

    fn sample_hash(seed: u8) -> CASHash {
        CASHash::from_bytes([seed; 32])
    }

    #[test]
    fn file_constructor_sets_blob_id() {
        // CASHash is used as the blob-id type B in tests (it satisfies BlobRef).
        let id = sample_hash(1);
        let e = TreeEntry::<CASHash>::file(Bytes::from_static(b"readme.txt"), id, 0o100644, 42);
        assert_eq!(e.name, Bytes::from_static(b"readme.txt"));
        assert_eq!(e.id, EntryId::Blob(id));
        assert_eq!(e.mode, 0o100644);
        assert_eq!(e.size, 42);
        assert!(e.id.is_blob());
    }

    #[test]
    fn dir_constructor_forces_zero_size() {
        let id = TreeId::from_cas_hash(sample_hash(2));
        let e = TreeEntry::<CASHash>::dir(Bytes::from_static(b"src"), id, 0o040000);
        assert_eq!(e.id, EntryId::Tree(id));
        assert_eq!(e.size, 0);
        assert!(e.id.is_tree());
    }

    #[test]
    fn tombstone_constructor_is_zero_moded_whiteout() {
        let e = TreeEntry::<CASHash>::tombstone(Bytes::from_static(b"gone"));
        assert_eq!(e.name, Bytes::from_static(b"gone"));
        assert_eq!(e.id, EntryId::Tombstone);
        assert_eq!(e.mode, 0);
        assert_eq!(e.size, 0);
        assert!(e.id.is_tombstone());
        assert_eq!(e.id.cas_hash(), None);
    }

    #[test]
    fn symlink_is_a_blob_with_symlink_mode() {
        // A symlink "link" -> "target/path" is just a blob entry whose mode is
        // SYMLINK_MODE; the blob's bytes (elsewhere in the file CAS) are the
        // target path.
        let target = b"target/path";
        let id = sample_hash(3);
        let e = TreeEntry::<CASHash>::file(
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
