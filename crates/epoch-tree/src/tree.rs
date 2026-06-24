//! [`Tree`] — the content-addressed directory object.
//!
//! A `Tree` is an ordered *set* of [`TreeEntry`]s. Its canonical form is the
//! entries sorted ascending by `name` (byte-lexicographic), with names unique.
//! Canonical order is load-bearing: the [`TreeCodec`](crate::TreeCodec) emits
//! entries in this order, so two trees with the same logical entries — built in
//! any insertion order — encode to identical bytes and therefore hash to the
//! same [`TreeId`](crate::TreeId). Determinism is the whole point of the
//! content-addressed model.
//!
//! `Tree<L>` is generic over the **link** type `L` (see [`Link`](crate::Link)):
//! a *complete* tree is `Tree<EntryId<B>>` (no tombstones possible), a *sparse
//! overlay* tree is `Tree<Overlay<EntryId<B>>>`. The tree object itself imposes
//! no requirement on `L` for its in-memory operations (sort/lookup are by
//! `name`); only the codec needs the [`Link`](crate::Link) bound.
//!
//! The invariant (sorted, unique names) is established once at construction
//! ([`Tree::empty`] / [`Tree::from_entries`]) and thereafter relied upon by
//! [`get`](Tree::get) (binary search) and the codec (no re-sort needed).

use crate::entry::TreeEntry;
use crate::error::TreeError;

/// An ordered, name-unique set of [`TreeEntry`]s — the directory object.
///
/// The internal vector is always kept sorted ascending by entry `name`; see the
/// [module docs](self) for why canonical order matters. Generic over the link
/// type `L`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree<L> {
    /// Always sorted ascending by `name`, names unique. Never expose mutable
    /// access that could break this invariant.
    entries: Vec<TreeEntry<L>>,
}

impl<L> Tree<L> {
    /// The empty tree (no entries).
    pub fn empty() -> Tree<L> {
        Tree {
            entries: Vec::new(),
        }
    }

    /// Build a tree from entries in any order.
    ///
    /// Entries are sorted ascending by `name` to establish canonical form.
    /// Returns [`TreeError::DuplicateName`] if any name appears more than once.
    pub fn from_entries(
        entries: impl IntoIterator<Item = TreeEntry<L>>,
    ) -> Result<Tree<L>, TreeError> {
        let mut entries: Vec<TreeEntry<L>> = entries.into_iter().collect();
        // Sort first so equal names are adjacent; a stable comparison on the raw
        // name bytes gives byte-lexicographic order.
        entries.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()));
        // After sorting, any duplicate names sit next to each other.
        if let Some(dup) = entries
            .windows(2)
            .find(|w| w[0].name == w[1].name)
            .map(|w| w[0].name.to_vec())
        {
            return Err(TreeError::DuplicateName { name: dup });
        }
        Ok(Tree { entries })
    }

    /// Construct directly from entries already known to satisfy the invariant
    /// (sorted ascending by name, names unique). Crate-internal: the decoder
    /// validates order/uniqueness in its single pass, so re-sorting via
    /// [`from_entries`](Tree::from_entries) would be wasted work.
    pub(crate) fn from_validated(entries: Vec<TreeEntry<L>>) -> Tree<L> {
        debug_assert!(
            entries.windows(2).all(|w| w[0].name < w[1].name),
            "from_validated requires strictly ascending unique names"
        );
        Tree { entries }
    }

    /// The entries, in canonical (name-ascending) order.
    pub fn entries(&self) -> &[TreeEntry<L>] {
        &self.entries
    }

    /// Look up an entry by raw name via binary search (the vector is sorted).
    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry<L>> {
        self.entries
            .binary_search_by(|e| e.name.as_ref().cmp(name))
            .ok()
            .map(|i| &self.entries[i])
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tree has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate entries in canonical order.
    pub fn iter(&self) -> std::slice::Iter<'_, TreeEntry<L>> {
        self.entries.iter()
    }
}

impl<'a, L> IntoIterator for &'a Tree<L> {
    type Item = &'a TreeEntry<L>;
    type IntoIter = std::slice::Iter<'a, TreeEntry<L>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{EntryId, TreeId};
    use epoch_cas::{Bytes, CASHash};

    // Tests instantiate the generic with a complete-tree link (EntryId<CASHash>);
    // CASHash satisfies BlobRef for free via the reflexive `From<CASHash>`.
    type L = EntryId<CASHash>;

    fn blob(name: &'static [u8], seed: u8) -> TreeEntry<L> {
        TreeEntry::file(
            Bytes::from_static(name),
            CASHash::from_bytes([seed; 32]),
            0o100644,
            seed as u64,
        )
    }

    fn dir(name: &'static [u8], seed: u8) -> TreeEntry<L> {
        TreeEntry::dir(
            Bytes::from_static(name),
            TreeId::from_cas_hash(CASHash::from_bytes([seed; 32])),
            0o040000,
        )
    }

    #[test]
    fn empty_tree_is_empty() {
        let t = Tree::<L>::empty();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.entries(), &[]);
        assert!(t.get(b"anything").is_none());
    }

    #[test]
    fn from_entries_sorts_by_name() {
        let t = Tree::from_entries([blob(b"zebra", 1), dir(b"apple", 2), blob(b"mango", 3)]).unwrap();
        let names: Vec<&[u8]> = t.iter().map(|e| e.name.as_ref()).collect();
        assert_eq!(names, vec![&b"apple"[..], b"mango", b"zebra"]);
    }

    #[test]
    fn from_entries_rejects_duplicate_names() {
        let err = Tree::from_entries([blob(b"dup", 1), blob(b"dup", 2)]).unwrap_err();
        match err {
            TreeError::DuplicateName { name } => assert_eq!(name, b"dup"),
        }
    }

    #[test]
    fn get_finds_by_name() {
        let t = Tree::from_entries([blob(b"a", 1), dir(b"b", 2), blob(b"c", 3)]).unwrap();
        assert_eq!(t.get(b"b").unwrap().name, Bytes::from_static(b"b"));
        assert!(t.get(b"missing").is_none());
    }

    #[test]
    fn ref_into_iter_matches_iter() {
        let t = Tree::from_entries([blob(b"a", 1), blob(b"b", 2)]).unwrap();
        let via_iter: Vec<_> = t.iter().collect();
        let via_into: Vec<_> = (&t).into_iter().collect();
        assert_eq!(via_iter, via_into);
    }
}
