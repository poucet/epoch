//! Integration tests for [`TreeCas`]: typed round-trips, dedup, nesting,
//! genericity over the blob-id type, and `SyncCAS` trait interop.

use epoch_cas::{CASHash, MemoryByteBackend, SyncCAS};
use epoch_tree::{EntryId, Tree, TreeEntry, TreeId};
use epoch_tree_cas::TreeCas;

/// Most tests use [`CASHash`] itself as the blob-id type `B`: it satisfies
/// [`BlobRef`](epoch_tree::BlobRef) for free via the reflexive
/// `From<CASHash> for CASHash`, so it is the simplest concrete `B`.
fn hash(seed: u8) -> CASHash {
    CASHash::from_bytes([seed; 32])
}

/// A blob entry over the `CASHash` blob-id type.
fn blob(name: &'static [u8], seed: u8, mode: u32, size: u64) -> TreeEntry<CASHash> {
    TreeEntry::file(name, hash(seed), mode, size)
}

/// A subtree entry pointing at `child`.
fn dir(name: &'static [u8], child: TreeId, mode: u32) -> TreeEntry<CASHash> {
    TreeEntry::dir(name, child, mode)
}

/// A fresh, empty tree CAS over an in-memory backend keyed by `CASHash`.
fn cas() -> TreeCas<MemoryByteBackend, CASHash> {
    TreeCas::new(MemoryByteBackend::new())
}

#[test]
fn put_get_roundtrips_mixed_entries() {
    let store = cas();
    let tree = Tree::from_entries([
        blob(b"readme.txt", 1, 0o100644, 42),
        dir(b"src", TreeId::from_cas_hash(hash(2)), 0o040000),
        blob(b"zebra", 3, 0o100755, 7),
    ])
    .unwrap();

    let id = store.put(tree.clone()).unwrap();
    let got = store.get(&id).unwrap().expect("tree present after put");

    // Entries, their canonical order, and every field are preserved.
    assert_eq!(got, tree);
    let names: Vec<&[u8]> = got.iter().map(|e| e.name.as_ref()).collect();
    assert_eq!(names, vec![&b"readme.txt"[..], b"src", b"zebra"]);
}

#[test]
fn identical_tree_dedups_differing_tree_diverges() {
    let store = cas();
    let a = Tree::from_entries([blob(b"a", 1, 0o100644, 1)]).unwrap();
    // Same logical entries, built in a different insertion order, must converge.
    let a_reordered = Tree::from_entries([
        blob(b"b", 2, 0o100644, 2),
        blob(b"a", 1, 0o100644, 1),
    ])
    .unwrap();
    let a_full = Tree::from_entries([
        blob(b"a", 1, 0o100644, 1),
        blob(b"b", 2, 0o100644, 2),
    ])
    .unwrap();

    let id1 = store.put(a.clone()).unwrap();
    let id2 = store.put(a).unwrap();
    assert_eq!(id1, id2, "identical trees converge to the same TreeId");

    let id_reordered = store.put(a_reordered).unwrap();
    let id_full = store.put(a_full).unwrap();
    assert_eq!(
        id_reordered, id_full,
        "insertion order does not affect the TreeId"
    );

    // A differing tree gets a different id.
    let differ = Tree::from_entries([blob(b"a", 9, 0o100644, 1)]).unwrap();
    let id_differ = store.put(differ).unwrap();
    assert_ne!(id1, id_differ, "a differing tree yields a different TreeId");
}

#[test]
fn get_absent_is_none_has_and_delete_behave() {
    let store = cas();
    let absent = TreeId::from_cas_hash(hash(200));
    assert_eq!(store.get(&absent).unwrap(), None);
    assert!(!store.has(&absent).unwrap());

    let tree = Tree::from_entries([blob(b"x", 1, 0o100644, 0)]).unwrap();
    let id = store.put(tree).unwrap();
    assert!(store.has(&id).unwrap());

    store.delete(&id).unwrap();
    assert!(!store.has(&id).unwrap());
    assert_eq!(store.get(&id).unwrap(), None);

    // Delete is idempotent: removing an absent id is not an error.
    store.delete(&id).unwrap();
    store.delete(&absent).unwrap();
}

#[test]
fn nested_trees_roundtrip() {
    let store = cas();

    // Put a child tree, then reference its id from a parent.
    let child = Tree::from_entries([blob(b"leaf", 1, 0o100644, 3)]).unwrap();
    let child_id = store.put(child.clone()).unwrap();

    let parent = Tree::from_entries([
        blob(b"file", 2, 0o100644, 5),
        dir(b"child", child_id, 0o040000),
    ])
    .unwrap();
    let parent_id = store.put(parent.clone()).unwrap();

    let got_parent = store.get(&parent_id).unwrap().expect("parent present");
    assert_eq!(got_parent, parent);

    // The parent's "child" entry points at the child's TreeId; following it
    // returns the original child tree.
    let entry = got_parent.get(b"child").unwrap();
    assert_eq!(entry.id, EntryId::Tree(child_id));
    let followed = store.get(&child_id).unwrap().expect("child present");
    assert_eq!(followed, child);
}

/// A local, non-`CASHash` blob-id type to prove the store is generic over `B`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct MyBlobId(CASHash);

impl From<CASHash> for MyBlobId {
    fn from(h: CASHash) -> Self {
        MyBlobId(h)
    }
}

impl From<MyBlobId> for CASHash {
    fn from(id: MyBlobId) -> Self {
        id.0
    }
}

#[test]
fn generic_over_a_foreign_blob_id_type() {
    // The store works with a blob-id type that is *not* CASHash, proving the
    // genericity over B.
    let store: TreeCas<MemoryByteBackend, MyBlobId> = TreeCas::new(MemoryByteBackend::new());

    let tree: Tree<MyBlobId> = Tree::from_entries([
        TreeEntry::file(b"a".as_slice(), MyBlobId(hash(1)), 0o100644, 1),
        TreeEntry::dir(b"sub".as_slice(), TreeId::from_cas_hash(hash(2)), 0o040000),
    ])
    .unwrap();

    let id = store.put(tree.clone()).unwrap();
    let got = store.get(&id).unwrap().expect("tree present");
    assert_eq!(got, tree);
    assert_eq!(got.get(b"a").unwrap().id, EntryId::Blob(MyBlobId(hash(1))));
}

#[test]
fn sync_cas_trait_interop_with_tree_id() {
    // Drive the same ops through the SyncCAS<Tree<B>> impl and confirm the
    // associated Id is TreeId.
    let store = cas();
    let tree = Tree::from_entries([blob(b"only", 1, 0o100644, 4)]).unwrap();

    // Annotating the binding as TreeId proves `<TreeCas as SyncCAS>::Id == TreeId`.
    let id: TreeId = SyncCAS::put(&store, tree.clone()).unwrap();
    assert!(SyncCAS::has(&store, &id).unwrap());
    assert_eq!(SyncCAS::get(&store, &id).unwrap(), Some(tree));

    SyncCAS::delete(&store, &id).unwrap();
    assert_eq!(<TreeCas<_, CASHash> as SyncCAS<Tree<CASHash>>>::get(&store, &id).unwrap(), None);
}
