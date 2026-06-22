//! Integration tests for `epoch-tree`: exercises the public API surface the way
//! a downstream crate would, end-to-end across modules.
//!
//! The tree object is generic over a foreign blob-id type `B: BlobRef`. These
//! tests instantiate that parameter with [`CASHash`] itself, which satisfies
//! [`BlobRef`] for free via the reflexive `From<CASHash> for CASHash` — so we
//! never need to define a fake blob-id newtype just to exercise the API.

use epoch_cas::{Bytes, CASError, CASHash, Codec};
use epoch_tree::{
    BlobRef, EntryId, Tree, TreeCodec, TreeEntry, TreeError, TreeId, SYMLINK_MODE,
    TREE_FORMAT_VERSION,
};

fn hash(seed: u8) -> CASHash {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = seed.wrapping_add(i as u8);
    }
    CASHash::from_bytes(bytes)
}

fn blob(name: &[u8], seed: u8, mode: u32, size: u64) -> TreeEntry<CASHash> {
    TreeEntry::file(Bytes::copy_from_slice(name), hash(seed), mode, size)
}

fn dir(name: &[u8], seed: u8) -> TreeEntry<CASHash> {
    TreeEntry::dir(Bytes::copy_from_slice(name), TreeId::from_cas_hash(hash(seed)), 0o040000)
}

#[test]
fn cas_hash_satisfies_blob_ref() {
    // Compile-time check that the chosen `B` meets the bound (and, by extension,
    // that the bound is blanket-implemented for any conforming foreign id).
    fn assert_blob_ref<B: BlobRef>() {}
    assert_blob_ref::<CASHash>();
}

#[test]
fn id_hex_roundtrip_and_formatting() {
    let h = hash(7);
    let tree_id = TreeId::from_cas_hash(h);

    // hex matches the underlying CASHash exactly.
    assert_eq!(tree_id.to_hex(), h.to_hex());

    // Display = bare hex; Debug = type-prefixed.
    assert_eq!(tree_id.to_string(), h.to_hex());
    assert_eq!(format!("{tree_id:?}"), format!("TreeId({})", h.to_hex()));

    // EntryId carries the tagged hash. The blob arm holds the foreign id (here
    // CASHash), the tree arm holds a TreeId.
    let blob_entry: EntryId<CASHash> = EntryId::Blob(h);
    assert!(blob_entry.is_blob());
    assert_eq!(blob_entry.cas_hash(), Some(h));

    let tree_entry: EntryId<CASHash> = EntryId::Tree(tree_id);
    assert!(tree_entry.is_tree());
    assert_eq!(tree_entry.cas_hash(), Some(h));

    // A tombstone addresses no object.
    let tomb: EntryId<CASHash> = EntryId::Tombstone;
    assert!(tomb.is_tombstone());
    assert_eq!(tomb.cas_hash(), None);
}

#[test]
fn tree_entry_constructors_and_symlink_as_blob() {
    let file = blob(b"main.rs", 1, 0o100644, 128);
    assert!(file.id.is_blob());
    assert_eq!(file.size, 128);

    let d = dir(b"src", 2);
    assert!(d.id.is_tree());
    assert_eq!(d.size, 0); // dir() forces size to 0.

    // A symlink is just a blob entry with SYMLINK_MODE; its blob bytes are the
    // target path (stored elsewhere in the file CAS).
    let target = b"../elsewhere";
    let link = TreeEntry::<CASHash>::file(
        Bytes::from_static(b"shortcut"),
        hash(3),
        SYMLINK_MODE,
        target.len() as u64,
    );
    assert_eq!(link.mode, SYMLINK_MODE);
    assert!(link.id.is_blob());
}

#[test]
fn tree_sorts_dedupes_and_looks_up() {
    // from_entries sorts by name regardless of insertion order.
    let t = Tree::from_entries([
        blob(b"zzz", 1, 0o100644, 1),
        dir(b"aaa", 2),
        blob(b"mmm", 3, 0o100644, 3),
    ])
    .unwrap();
    let names: Vec<&[u8]> = t.iter().map(|e| e.name.as_ref()).collect();
    assert_eq!(names, vec![&b"aaa"[..], b"mmm", b"zzz"]);

    // get() finds by raw name.
    assert!(t.get(b"mmm").is_some());
    assert!(t.get(b"absent").is_none());
    assert_eq!(t.len(), 3);
    assert!(!t.is_empty());

    // empty tree.
    assert!(Tree::<CASHash>::empty().is_empty());

    // duplicate names are rejected.
    let err = Tree::from_entries([blob(b"x", 1, 0o100644, 0), dir(b"x", 2)]).unwrap_err();
    match err {
        TreeError::DuplicateName { name } => assert_eq!(name, b"x"),
    }
}

#[test]
fn codec_roundtrips_empty_and_multi_entry() {
    // Empty tree.
    let empty = Tree::<CASHash>::empty();
    let enc = TreeCodec::<CASHash>::encode(&empty);
    assert_eq!(enc[0], TREE_FORMAT_VERSION);
    assert_eq!(TreeCodec::<CASHash>::decode(&enc).unwrap(), empty);

    // Mixed blob + tree entries, a non-UTF8 name, and a huge size.
    let t = Tree::from_entries([
        blob(b"first", 10, 0o100755, 1),
        dir(b"\xc3\x28\xff", 11), // invalid UTF-8 bytes as a name
        blob(b"huge", 12, 0o100644, u64::MAX),
    ])
    .unwrap();
    let enc = TreeCodec::<CASHash>::encode(&t);
    let dec = TreeCodec::<CASHash>::decode(&enc).unwrap();
    assert_eq!(dec, t);
    assert_eq!(TreeCodec::<CASHash>::encode(&dec), enc); // byte-stable round-trip
}

#[test]
fn codec_encode_is_deterministic_across_insertion_orders() {
    let order_a = Tree::from_entries([
        blob(b"a", 1, 0o100644, 1),
        dir(b"b", 2),
        blob(b"c", 3, 0o100644, 3),
        blob(b"\xfe", 4, 0o100644, 4),
    ])
    .unwrap();
    let order_b = Tree::from_entries([
        blob(b"\xfe", 4, 0o100644, 4),
        blob(b"c", 3, 0o100644, 3),
        blob(b"a", 1, 0o100644, 1),
        dir(b"b", 2),
    ])
    .unwrap();

    // Same logical tree, different build order => identical bytes => identical
    // hash. This determinism is the whole point.
    assert_eq!(order_a, order_b);
    assert_eq!(
        TreeCodec::<CASHash>::encode(&order_a),
        TreeCodec::<CASHash>::encode(&order_b)
    );
}

#[test]
fn codec_decode_rejects_malformed_input() {
    let t = Tree::from_entries([blob(b"a", 1, 0o100644, 0), blob(b"b", 2, 0o100644, 0)]).unwrap();
    let good = TreeCodec::<CASHash>::encode(&t).to_vec();

    // Bad version.
    let mut bad_version = good.clone();
    bad_version[0] = TREE_FORMAT_VERSION.wrapping_add(1);
    assert!(matches!(
        TreeCodec::<CASHash>::decode(&bad_version),
        Err(CASError::Codec(_))
    ));

    // Unknown kind tag (first tag byte after 1+4 header).
    let mut bad_tag = good.clone();
    bad_tag[5] = 2;
    assert!(matches!(
        TreeCodec::<CASHash>::decode(&bad_tag),
        Err(CASError::Codec(_))
    ));

    // Truncated buffer.
    assert!(matches!(
        TreeCodec::<CASHash>::decode(&good[..good.len() - 3]),
        Err(CASError::Codec(_))
    ));

    // Unsorted entries: hand-build descending order.
    let mut unsorted = Vec::new();
    unsorted.push(TREE_FORMAT_VERSION);
    unsorted.extend_from_slice(&2u32.to_le_bytes());
    for name in [b"z".as_slice(), b"a".as_slice()] {
        unsorted.push(0); // blob tag
        unsorted.extend_from_slice(&[1u8; 32]);
        unsorted.extend_from_slice(&0o100644u32.to_le_bytes());
        unsorted.extend_from_slice(&0u64.to_le_bytes());
        unsorted.extend_from_slice(&(name.len() as u32).to_le_bytes());
        unsorted.extend_from_slice(name);
    }
    assert!(matches!(
        TreeCodec::<CASHash>::decode(&unsorted),
        Err(CASError::Codec(_))
    ));

    // Duplicate names.
    let mut dup = Vec::new();
    dup.push(TREE_FORMAT_VERSION);
    dup.extend_from_slice(&2u32.to_le_bytes());
    for _ in 0..2 {
        dup.push(0);
        dup.extend_from_slice(&[1u8; 32]);
        dup.extend_from_slice(&0o100644u32.to_le_bytes());
        dup.extend_from_slice(&0u64.to_le_bytes());
        dup.extend_from_slice(&1u32.to_le_bytes());
        dup.push(b'q');
    }
    assert!(matches!(
        TreeCodec::<CASHash>::decode(&dup),
        Err(CASError::Codec(_))
    ));
}
