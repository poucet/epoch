//! Tree ids and the per-entry **link** types, layered on the kernel's
//! [`CASHash`].
//!
//! `epoch-cas` is deliberately id-agnostic: it speaks only [`CASHash`], the raw
//! 32-byte content-address. This module gives those addresses *meaning* in the
//! tree domain:
//!
//! - [`TreeId`] — the id of a tree object (the content-address a
//!   [`TreeStore`](crate::TreeStore) produces for a [`Tree`](crate::Tree)'s
//!   canonical bytes). A concrete, zero-cost newtype around [`CASHash`].
//! - [`BlobRef`] — the *bound* a foreign blob-id type must satisfy to be
//!   referenced from a tree (file blob ids live in `epoch-file-cas`).
//!
//! ## Two link types, one guarantee
//!
//! A directory entry points at a *child* via a **link**. There are two link
//! types, and the split is load-bearing — it makes one class of corruption
//! *unrepresentable*:
//!
//! - [`EntryId<B>`](EntryId) — a **populated** link: a [`Blob`](EntryId::Blob)
//!   (`B`) or a [`Tree`](EntryId::Tree) ([`TreeId`]). It has **no** tombstone
//!   variant, so a tree whose entries are `EntryId<B>` is a *complete* directory
//!   listing that simply **cannot** contain a whiteout.
//! - [`Overlay<E>`](Overlay) — wraps a populated link `E`, adding the
//!   [`Tombstone`](Overlay::Tombstone) whiteout. A tree whose entries are
//!   `Overlay<EntryId<B>>` is a **sparse overlay layer**: an *absent* name falls
//!   through to the lower/base layer, a [`Tombstone`](Overlay::Tombstone)
//!   *hides* the lower entry, and a populated link *overrides* it.
//!
//! [`Link`] is the trait both implement: it classifies a link for encoding
//! ([`kind`](Link::kind) → [`LinkKind`]) and reconstructs one while decoding
//! ([`from_blob`](Link::from_blob) / [`from_tree`](Link::from_tree) /
//! [`tombstone`](Link::tombstone)). Crucially [`tombstone`](Link::tombstone)
//! returns `None` for [`EntryId`], so the codec rejects a whiteout in a complete
//! tree — the type-level guarantee is enforced end to end. A single generic body
//! (codec, hydrate) serves both link types through this trait.

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

/// `From<CASHash>` / `Into<CASHash>`: a [`TreeId`] is a transparent skin over a
/// content-address, so it satisfies the engine's typed-`Id` bound
/// (`Copy + From<CASHash> + Into<CASHash>`) and plugs straight into
/// [`SyncGenericCAS`](epoch_cas::SyncGenericCAS) as its id — that is what
/// [`TreeStore`](crate::TreeStore) is. Mirrors the const [`from_cas_hash`](TreeId::from_cas_hash)
/// / [`into_cas_hash`](TreeId::into_cas_hash) helpers.
impl From<CASHash> for TreeId {
    fn from(hash: CASHash) -> Self {
        TreeId(hash)
    }
}

impl From<TreeId> for CASHash {
    fn from(id: TreeId) -> Self {
        id.0
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

/// A **populated** tree-entry link: the child a present name points at.
///
/// Either a file [`Blob`](EntryId::Blob) (addressed by the foreign blob-id type
/// `B`, see [`BlobRef`]) or a subtree [`Tree`](EntryId::Tree) (this crate's own
/// [`TreeId`]). It has **no whiteout variant** by construction — a tree built
/// over `EntryId<B>` is a complete listing that cannot hold a tombstone. For the
/// overlay (sparse) form, wrap it in [`Overlay`].
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

/// An **overlay** tree-entry link: a populated link `E`, or a whiteout.
///
/// This is the link type for a *sparse overlay layer*. A
/// [`Populated`](Overlay::Populated) entry overrides the lower layer at that
/// name; a [`Tombstone`](Overlay::Tombstone) hides it (a deletion in the merged
/// view); an *absent* name falls through. `E` is the populated link it wraps —
/// in practice [`EntryId<B>`](EntryId).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Overlay<E> {
    /// A present child — overrides the lower layer at this name.
    Populated(E),
    /// A whiteout: hides the lower entry of this name (overlay deletion marker).
    /// It addresses no object and so has no content-hash.
    Tombstone,
}

impl<E> Overlay<E> {
    /// Whether this is a whiteout (overlay deletion marker).
    pub const fn is_tombstone(&self) -> bool {
        matches!(self, Overlay::Tombstone)
    }

    /// The wrapped populated link, or `None` for a tombstone.
    pub const fn as_populated(&self) -> Option<&E> {
        match self {
            Overlay::Populated(e) => Some(e),
            Overlay::Tombstone => None,
        }
    }
}

/// The classification of a [`Link`] for the wire format: a present child carries
/// its raw [`CASHash`], a tombstone carries nothing.
///
/// This is the bridge that lets one codec/hydrate body serve both link types —
/// it speaks the kernel [`CASHash`], not a typed blob/tree id, so it is agnostic
/// to which link produced it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkKind {
    /// A file blob child, by its raw content-address.
    Blob(CASHash),
    /// A subtree child, by its raw content-address.
    Tree(CASHash),
    /// A whiteout (overlay deletion marker) — addresses no object.
    Tombstone,
}

/// The shared interface over the two link types ([`EntryId`] and [`Overlay`]).
///
/// It lets a single generic body (the [`codec`](crate::codec), the working
/// tree's hydrate) classify a link for encoding ([`kind`](Link::kind)) and
/// reconstruct one while decoding ([`from_blob`](Link::from_blob) /
/// [`from_tree`](Link::from_tree) / [`tombstone`](Link::tombstone)) without
/// caring which concrete link type it is — or which foreign blob-id type
/// ([`Blob`](Link::Blob)) backs it.
///
/// [`tombstone`](Link::tombstone) is the type-level guarantee in action: a
/// complete-tree link ([`EntryId`]) returns `None`, so a decoder reconstructing
/// into a complete tree rejects a whiteout tag rather than admitting an illegal
/// layout.
pub trait Link: Sized {
    /// The foreign blob-id type this link addresses blobs by (see [`BlobRef`]).
    type Blob: BlobRef;

    /// Classify this link for the wire format.
    fn kind(&self) -> LinkKind;

    /// Reconstruct a populated *blob* link.
    fn from_blob(blob: Self::Blob) -> Self;

    /// Reconstruct a populated *tree* link.
    fn from_tree(tree: TreeId) -> Self;

    /// Reconstruct a *tombstone* link, or `None` if this link type cannot be a
    /// tombstone (a complete-tree link). The codec turns `None` into a decode
    /// error for a whiteout tag.
    fn tombstone() -> Option<Self>;

    /// The link's underlying content-address, or `None` for a tombstone.
    fn cas_hash(&self) -> Option<CASHash> {
        match self.kind() {
            LinkKind::Blob(h) | LinkKind::Tree(h) => Some(h),
            LinkKind::Tombstone => None,
        }
    }

    /// Whether this link is a whiteout.
    fn is_tombstone(&self) -> bool {
        matches!(self.kind(), LinkKind::Tombstone)
    }
}

impl<B: BlobRef> Link for EntryId<B> {
    type Blob = B;

    fn kind(&self) -> LinkKind {
        match self {
            EntryId::Blob(b) => LinkKind::Blob((*b).into()),
            EntryId::Tree(t) => LinkKind::Tree(t.into_cas_hash()),
        }
    }

    fn from_blob(blob: B) -> Self {
        EntryId::Blob(blob)
    }

    fn from_tree(tree: TreeId) -> Self {
        EntryId::Tree(tree)
    }

    fn tombstone() -> Option<Self> {
        // A complete-tree link has no whiteout — this is the unrepresentability.
        None
    }
}

impl<E: Link> Link for Overlay<E> {
    type Blob = E::Blob;

    fn kind(&self) -> LinkKind {
        match self {
            Overlay::Populated(e) => e.kind(),
            Overlay::Tombstone => LinkKind::Tombstone,
        }
    }

    fn from_blob(blob: E::Blob) -> Self {
        Overlay::Populated(E::from_blob(blob))
    }

    fn from_tree(tree: TreeId) -> Self {
        Overlay::Populated(E::from_tree(tree))
    }

    fn tombstone() -> Option<Self> {
        Some(Overlay::Tombstone)
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

        assert!(blob.is_blob() && !blob.is_tree());
        assert!(tree.is_tree() && !tree.is_blob());

        assert_eq!(blob.kind(), LinkKind::Blob(bh));
        assert_eq!(tree.kind(), LinkKind::Tree(th));
        assert_eq!(blob.cas_hash(), Some(bh));
        assert_eq!(tree.cas_hash(), Some(th));
        assert!(!blob.is_tombstone() && !tree.is_tombstone());

        // A complete-tree link can never be a tombstone.
        assert!(<EntryId<CASHash> as Link>::tombstone().is_none());
    }

    #[test]
    fn overlay_wraps_populated_and_adds_tombstone() {
        let bh = sample_hash(6);
        let pop: Overlay<EntryId<CASHash>> = Overlay::Populated(EntryId::Blob(bh));
        let tomb: Overlay<EntryId<CASHash>> = Overlay::Tombstone;

        assert!(!pop.is_tombstone());
        assert_eq!(pop.as_populated(), Some(&EntryId::Blob(bh)));
        assert_eq!(pop.kind(), LinkKind::Blob(bh));
        assert_eq!(pop.cas_hash(), Some(bh));

        assert!(tomb.is_tombstone());
        assert_eq!(tomb.as_populated(), None);
        assert_eq!(tomb.kind(), LinkKind::Tombstone);
        assert_eq!(Link::cas_hash(&tomb), None);

        // The overlay link CAN reconstruct a tombstone (unlike EntryId).
        assert!(<Overlay<EntryId<CASHash>> as Link>::tombstone().is_some());

        // Reconstruction wraps populated links.
        assert_eq!(
            <Overlay<EntryId<CASHash>> as Link>::from_blob(bh),
            Overlay::Populated(EntryId::Blob(bh))
        );
    }
}
