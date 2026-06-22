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
//! [`TreeEntry`](crate::TreeEntry): it carries the child address *and* records
//! whether the child is a blob (`B`), a tree ([`TreeId`]), or a *tombstone*
//! (whiteout) — the kind a parent needs to interpret (or hide) its children.
//!
//! ## Overlay semantics
//!
//! A [`Tree`](crate::Tree) is normally a *complete* directory listing. It can
//! also act as a **sparse overlay layer** over a lower/base tree, in which case
//! its entries are read against that base:
//!
//! - an **absent** name falls through to the lower layer;
//! - a [`Tombstone`](EntryId::Tombstone) entry **hides** the lower entry
//!   (a whiteout) — the name is treated as deleted in the merged view;
//! - a [`Blob`](EntryId::Blob)/[`Tree`](EntryId::Tree) entry **overrides** the
//!   lower entry at that name.
//!
//! A tree that contains no tombstones, read with no base, is still a plain
//! complete tree — the overlay model is backward compatible.

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

/// A tree-entry child reference: the kind of object a name points at (or a
/// whiteout hiding it in an overlay).
///
/// For [`Blob`](EntryId::Blob)/[`Tree`](EntryId::Tree) the variant carries the
/// strongly-typed child address; the blob arm holds a foreign blob-id type `B`
/// (see [`BlobRef`]), the tree arm holds this crate's own [`TreeId`]. The third
/// kind, [`Tombstone`](EntryId::Tombstone), carries no address: it marks the
/// name as deleted relative to a lower overlay layer. See the
/// [module docs](self) for the full overlay semantics.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EntryId<B> {
    /// The child is a file blob, addressed by the foreign blob-id type `B`.
    Blob(B),
    /// The child is a subtree.
    Tree(TreeId),
    /// A whiteout: in an overlay this name is hidden (deleted) relative to the
    /// lower/base layer. It addresses no object and so has no content-hash.
    Tombstone,
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

    /// Whether this entry is a whiteout (overlay deletion marker).
    pub const fn is_tombstone(&self) -> bool {
        matches!(self, EntryId::Tombstone)
    }
}

impl<B: BlobRef> EntryId<B> {
    /// The underlying content-address, or `None` for a tombstone.
    ///
    /// A [`Blob`](EntryId::Blob) id converts into its [`CASHash`] (consuming the
    /// `Copy` value) and a [`Tree`](EntryId::Tree) id unwraps its own; a
    /// [`Tombstone`](EntryId::Tombstone) addresses no object and yields `None`.
    pub fn cas_hash(&self) -> Option<CASHash> {
        match self {
            EntryId::Blob(b) => Some((*b).into()),
            EntryId::Tree(t) => Some(t.into_cas_hash()),
            EntryId::Tombstone => None,
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
        let tomb: EntryId<CASHash> = EntryId::Tombstone;

        assert!(blob.is_blob());
        assert!(!blob.is_tree());
        assert!(!blob.is_tombstone());
        assert!(tree.is_tree());
        assert!(!tree.is_blob());
        assert!(!tree.is_tombstone());
        assert!(tomb.is_tombstone());
        assert!(!tomb.is_blob());
        assert!(!tomb.is_tree());

        assert_eq!(blob.cas_hash(), Some(bh));
        assert_eq!(tree.cas_hash(), Some(th));
        assert_eq!(tomb.cas_hash(), None);
    }
}
