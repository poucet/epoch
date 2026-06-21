//! The tree's own id newtype and the blob-id bound, layered on the kernel's
//! [`CASHash`].
//!
//! `epoch-cas` is deliberately id-agnostic: it speaks only [`CASHash`], the raw
//! 32-byte content-address. This module gives those addresses *meaning* in the
//! tree domain:
//!
//! - [`TreeId`] — the id of a tree object (the content-address an
//!   `epoch-tree-cas` produces for a [`Tree`](crate::Tree)'s canonical bytes).
//!   It is a concrete, zero-cost newtype around [`CASHash`]; a tree object's
//!   address is always *this* crate's own type.
//! - [`BlobRef`] — the *bound* a foreign blob-id type must satisfy to be
//!   referenced from a tree. A tree no longer owns its blob-id type: file blob
//!   ids live in `epoch-file-cas`, and a [`Tree`](crate::Tree) is generic over
//!   whatever 32-byte, [`CASHash`]-convertible id that crate (or any other)
//!   provides. The two are wired together at a higher level.
//!
//! [`EntryId<B>`](EntryId) is the tagged union actually stored in a
//! [`TreeEntry`](crate::TreeEntry): it both carries the child address *and*
//! records whether the child is a blob (`B`) or a tree ([`TreeId`]) — the one
//! bit a parent needs to interpret its children.

use std::fmt;

use epoch_cas::CASHash;

/// The bound a blob-id type must satisfy to be referenced from a tree.
///
/// Blanket-implemented: any 32-byte id newtype convertible to/from [`CASHash`]
/// qualifies, with no dependency on this crate. This lets a *foreign* blob-id
/// type (e.g. `epoch-file-cas`'s `BlobId`) be used as a tree's `B` parameter
/// without that crate ever naming `epoch-tree` — it only needs the standard
/// bounds plus `From`/`Into<CASHash>`, which it would have anyway.
pub trait BlobRef:
    Copy + Eq + core::hash::Hash + core::fmt::Debug + From<CASHash> + Into<CASHash>
{
}

impl<T> BlobRef for T where
    T: Copy + Eq + core::hash::Hash + core::fmt::Debug + From<CASHash> + Into<CASHash>
{
}

/// The id of a tree object — the content-address of a [`Tree`](crate::Tree)'s
/// canonical encoding.
///
/// A `repr`-transparent wrapper around [`CASHash`]; the distinct type exists so
/// a tree id is never confused with a blob id (or a bare hash) at compile time.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TreeId(CASHash);

impl TreeId {
    /// Wrap a raw content-address as a tree id (no hashing performed).
    pub const fn from_cas_hash(hash: CASHash) -> TreeId {
        TreeId(hash)
    }

    /// Borrow the underlying content-address.
    pub const fn as_cas_hash(&self) -> &CASHash {
        &self.0
    }

    /// Unwrap into the underlying content-address.
    pub const fn into_cas_hash(self) -> CASHash {
        self.0
    }

    /// Lowercase hex of the underlying address (64 chars).
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

/// `Display` is the bare hex, matching [`CASHash`]'s `Display`.
impl fmt::Display for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// `Debug` names the id type so logs disambiguate tree ids from blob/raw ids.
impl fmt::Debug for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TreeId({})", self.0.to_hex())
    }
}

/// A tree-entry child reference: a content-address tagged with the kind of
/// object it points at.
///
/// A parent tree needs exactly one bit beyond the raw address — *is this child a
/// blob or another tree?* — to know how to interpret it. `EntryId` carries that
/// bit alongside the strongly-typed id. The blob arm holds a foreign blob-id
/// type `B` (see [`BlobRef`]); the tree arm holds this crate's own [`TreeId`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryId<B> {
    /// The child is a file blob, addressed by the foreign blob-id type `B`.
    Blob(B),
    /// The child is a subtree.
    Tree(TreeId),
}

impl<B> EntryId<B> {
    /// Whether this entry points at a file blob.
    pub const fn is_blob(&self) -> bool {
        matches!(self, EntryId::Blob(_))
    }

    /// Whether this entry points at a subtree.
    pub const fn is_tree(&self) -> bool {
        matches!(self, EntryId::Tree(_))
    }
}

impl<B: BlobRef> EntryId<B> {
    /// The underlying content-address regardless of kind.
    ///
    /// Returned by value: a blob id converts into its [`CASHash`] (consuming the
    /// `Copy` value), and a tree id unwraps its own.
    pub fn as_cas_hash(&self) -> CASHash {
        match self {
            EntryId::Blob(b) => (*b).into(),
            EntryId::Tree(t) => t.into_cas_hash(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash(seed: u8) -> CASHash {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        CASHash::from_bytes(bytes)
    }

    #[test]
    fn tree_id_roundtrips_through_cas_hash() {
        let h = sample_hash(2);
        let id = TreeId::from_cas_hash(h);
        assert_eq!(id.as_cas_hash(), &h);
        assert_eq!(id.into_cas_hash(), h);
        assert_eq!(id.to_hex(), h.to_hex());
    }

    #[test]
    fn display_is_bare_hex_debug_is_prefixed() {
        let h = sample_hash(3);
        let tree = TreeId::from_cas_hash(h);

        assert_eq!(tree.to_string(), h.to_hex());
        assert_eq!(format!("{tree:?}"), format!("TreeId({})", h.to_hex()));
    }

    #[test]
    fn entry_id_kind_and_hash() {
        // Instantiate the blob arm with CASHash itself, which satisfies BlobRef
        // for free (reflexive `From<CASHash> for CASHash`).
        let bh = sample_hash(4);
        let th = sample_hash(5);
        let blob: EntryId<CASHash> = EntryId::Blob(bh);
        let tree: EntryId<CASHash> = EntryId::Tree(TreeId::from_cas_hash(th));

        assert!(blob.is_blob());
        assert!(!blob.is_tree());
        assert!(tree.is_tree());
        assert!(!tree.is_blob());

        assert_eq!(blob.as_cas_hash(), bh);
        assert_eq!(tree.as_cas_hash(), th);
    }
}
