//! [`TreeCodec`] — the canonical `Tree <-> bytes` mapping that *defines* the
//! tree hash.
//!
//! This encoding is a domain invariant: the bytes it produces are what an
//! `epoch-tree-cas` hashes to get a [`TreeId`](crate::TreeId), so two trees
//! converge in storage iff they encode identically. Determinism is therefore
//! non-negotiable, and the format is specified exactly below.
//!
//! ## Byte layout (all integers little-endian, fixed width)
//!
//! ```text
//! [u8]        format version  (== TREE_FORMAT_VERSION, currently 1)
//! [u32 LE]    entry count
//! repeated, in canonical name-ascending order:
//!     [u8]      kind tag       (0 = Blob, 1 = Tree; 2 reserved, never emitted)
//!     [32 bytes] child CASHash raw bytes
//!     [u32 LE]  mode
//!     [u64 LE]  size
//!     [u32 LE]  name length (n)
//!     [n bytes] raw name bytes
//! ```
//!
//! Fixed-width little-endian was chosen over a varint scheme for being trivially
//! unambiguous: there is exactly one byte sequence for any given tree, with no
//! encoder discretion. Because [`Tree`] always stores entries in canonical order
//! and the encoder emits them in stored order, the same logical tree always
//! yields the same bytes regardless of how it was built.
//!
//! The blob arm stores `B`'s underlying [`CASHash`] bytes, so the layout is
//! identical no matter which foreign blob-id type `B` is in play; the codec is
//! generic over `B: BlobRef` purely to convert that id to/from its raw hash.
//!
//! ## Decode is validating
//!
//! [`decode`](TreeCodec::decode) rejects anything that is not canonical:
//! unknown/wrong version, unknown kind tag, truncated/over-long input, and
//! names that are not *strictly* ascending (which also catches duplicates). The
//! strict-ascending check guarantees a decode → encode round-trip reproduces the
//! input byte-for-byte.

use core::marker::PhantomData;

use epoch_cas::{Bytes, CASError, CASHash, Codec};

use crate::entry::TreeEntry;
use crate::id::{BlobRef, EntryId, TreeId};
use crate::tree::Tree;

/// The version byte stamped at the start of every encoded tree. Bump only on a
/// breaking layout change.
pub const TREE_FORMAT_VERSION: u8 = 1;

/// Kind tag for a [blob](EntryId::Blob) child.
const KIND_BLOB: u8 = 0;
/// Kind tag for a [tree](EntryId::Tree) child.
const KIND_TREE: u8 = 1;

/// Width of the raw [`CASHash`] field, in bytes.
const HASH_LEN: usize = 32;

/// The canonical codec for [`Tree<B>`]. See the [module docs](self) for the
/// exact byte layout.
///
/// The `PhantomData<fn() -> B>` marker ties the codec to its tree's blob-id type
/// without owning a `B`; using the `fn() -> B` form keeps `TreeCodec<B>`
/// unconditionally `Send + Sync + 'static` (as [`Codec`] requires) regardless of
/// `B`'s own auto-traits.
pub struct TreeCodec<B>(PhantomData<fn() -> B>);

impl<B: BlobRef + 'static> Codec<Tree<B>> for TreeCodec<B> {
    fn encode(value: &Tree<B>) -> Bytes {
        let entries = value.entries();

        // Pre-size the buffer so encoding is a single allocation: header
        // (1 + 4) plus, per entry, the fixed fields (1 + 32 + 4 + 8 + 4) and
        // the variable-length name.
        let fixed_per_entry = 1 + HASH_LEN + 4 + 8 + 4;
        let names_len: usize = entries.iter().map(|e| e.name.len()).sum();
        let capacity = 1 + 4 + entries.len() * fixed_per_entry + names_len;

        let mut buf = Vec::with_capacity(capacity);
        buf.push(TREE_FORMAT_VERSION);
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        for entry in entries {
            let (tag, hash): (u8, CASHash) = match &entry.id {
                EntryId::Blob(b) => (KIND_BLOB, Into::<CASHash>::into(*b)),
                EntryId::Tree(id) => (KIND_TREE, id.into_cas_hash()),
            };
            buf.push(tag);
            buf.extend_from_slice(hash.as_bytes());
            buf.extend_from_slice(&entry.mode.to_le_bytes());
            buf.extend_from_slice(&entry.size.to_le_bytes());
            buf.extend_from_slice(&(entry.name.len() as u32).to_le_bytes());
            buf.extend_from_slice(&entry.name);
        }

        Bytes::from(buf)
    }

    fn decode(bytes: &[u8]) -> Result<Tree<B>, CASError> {
        let mut r = Reader::new(bytes);

        let version = r.u8()?;
        if version != TREE_FORMAT_VERSION {
            return Err(codec_err(format!(
                "unsupported tree format version: {version} (expected {TREE_FORMAT_VERSION})"
            )));
        }
        let count = r.u32()? as usize;

        let mut entries: Vec<TreeEntry<B>> = Vec::with_capacity(count);
        // Track the previous name to enforce strict-ascending order in one pass
        // (this subsumes the uniqueness and canonical-order checks).
        let mut prev_name: Option<Vec<u8>> = None;

        for _ in 0..count {
            let tag = r.u8()?;
            let hash = CASHash::from_bytes(r.hash()?);
            let mode = r.u32()?;
            let size = r.u64()?;
            let name_len = r.u32()? as usize;
            let name = r.bytes(name_len)?;

            let id = match tag {
                KIND_BLOB => EntryId::Blob(B::from(hash)),
                KIND_TREE => EntryId::Tree(TreeId::from_cas_hash(hash)),
                other => return Err(codec_err(format!("unknown entry kind tag: {other}"))),
            };

            if let Some(prev) = &prev_name {
                match name.as_ref().cmp(prev.as_slice()) {
                    std::cmp::Ordering::Greater => {}
                    std::cmp::Ordering::Equal => {
                        return Err(codec_err(format!("duplicate entry name: {name:?}")));
                    }
                    std::cmp::Ordering::Less => {
                        return Err(codec_err(format!(
                            "entries not in strictly ascending name order at {name:?}"
                        )));
                    }
                }
            }
            prev_name = Some(name.to_vec());

            entries.push(TreeEntry {
                name,
                id,
                mode,
                size,
            });
        }

        if !r.is_empty() {
            return Err(codec_err(format!(
                "trailing {} byte(s) after {count} entries",
                r.remaining()
            )));
        }

        // Order/uniqueness were validated above, so the invariant already holds;
        // construct directly rather than re-sorting via `from_entries`.
        Ok(Tree::from_validated(entries))
    }
}

/// Build a [`CASError::Codec`] from a message.
fn codec_err(msg: String) -> CASError {
    CASError::Codec(msg)
}

/// A tiny cursor over the input that bounds-checks every read and maps any
/// shortfall to a [`CASError::Codec`] truncation error.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Borrow the next `n` bytes, advancing the cursor, or error on shortfall.
    fn take(&mut self, n: usize) -> Result<&'a [u8], CASError> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            codec_err("length overflow while decoding tree".to_string())
        })?;
        if end > self.buf.len() {
            return Err(codec_err(format!(
                "truncated tree: needed {n} byte(s) at offset {}, have {}",
                self.pos,
                self.remaining()
            )));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CASError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CASError> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("take yields 4 bytes");
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, CASError> {
        let b: [u8; 8] = self.take(8)?.try_into().expect("take yields 8 bytes");
        Ok(u64::from_le_bytes(b))
    }

    fn hash(&mut self) -> Result<[u8; HASH_LEN], CASError> {
        let b: [u8; HASH_LEN] =
            self.take(HASH_LEN)?.try_into().expect("take yields HASH_LEN bytes");
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<Bytes, CASError> {
        Ok(Bytes::copy_from_slice(self.take(n)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_cas::CASHash;

    // Tests use CASHash itself as the blob-id type B (it satisfies BlobRef for
    // free), so `TreeCodec::<CASHash>` is the codec under test throughout.
    fn blob_entry(name: &[u8], seed: u8, mode: u32, size: u64) -> TreeEntry<CASHash> {
        TreeEntry::file(
            Bytes::copy_from_slice(name),
            CASHash::from_bytes([seed; 32]),
            mode,
            size,
        )
    }

    fn tree_entry(name: &[u8], seed: u8, mode: u32) -> TreeEntry<CASHash> {
        TreeEntry::dir(
            Bytes::copy_from_slice(name),
            TreeId::from_cas_hash(CASHash::from_bytes([seed; 32])),
            mode,
        )
    }

    #[test]
    fn empty_tree_roundtrips() {
        let t = Tree::<CASHash>::empty();
        let enc = TreeCodec::<CASHash>::encode(&t);
        // version + count only.
        assert_eq!(enc.len(), 1 + 4);
        let dec = TreeCodec::<CASHash>::decode(&enc).unwrap();
        assert_eq!(dec, t);
    }

    #[test]
    fn multi_entry_roundtrips_with_mixed_kinds_and_nonutf8_names() {
        let t = Tree::from_entries([
            blob_entry(b"alpha", 1, 0o100644, 10),
            tree_entry(b"\xff\x00\xfe", 2, 0o040000),
            blob_entry(b"big", 3, 0o100755, u64::MAX),
        ])
        .unwrap();

        let enc = TreeCodec::<CASHash>::encode(&t);
        let dec = TreeCodec::<CASHash>::decode(&enc).unwrap();
        assert_eq!(dec, t);
        // Round-trip is byte-stable: re-encoding the decoded value matches.
        assert_eq!(TreeCodec::<CASHash>::encode(&dec), enc);
    }

    #[test]
    fn encode_is_deterministic_across_insertion_orders() {
        let a = Tree::from_entries([
            blob_entry(b"a", 1, 0o100644, 1),
            tree_entry(b"b", 2, 0o040000),
            blob_entry(b"c", 3, 0o100644, 3),
        ])
        .unwrap();
        let b = Tree::from_entries([
            blob_entry(b"c", 3, 0o100644, 3),
            blob_entry(b"a", 1, 0o100644, 1),
            tree_entry(b"b", 2, 0o040000),
        ])
        .unwrap();
        assert_eq!(TreeCodec::<CASHash>::encode(&a), TreeCodec::<CASHash>::encode(&b));
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut enc = TreeCodec::<CASHash>::encode(&Tree::empty()).to_vec();
        enc[0] = 99;
        assert!(matches!(
            TreeCodec::<CASHash>::decode(&enc),
            Err(CASError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_kind_tag() {
        let t = Tree::from_entries([blob_entry(b"x", 1, 0o100644, 0)]).unwrap();
        let mut enc = TreeCodec::<CASHash>::encode(&t).to_vec();
        // The kind tag sits right after the 1-byte version + 4-byte count.
        enc[5] = 2;
        assert!(matches!(
            TreeCodec::<CASHash>::decode(&enc),
            Err(CASError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_buffer() {
        let t = Tree::from_entries([blob_entry(b"x", 1, 0o100644, 0)]).unwrap();
        let enc = TreeCodec::<CASHash>::encode(&t).to_vec();
        let truncated = &enc[..enc.len() - 1];
        assert!(matches!(
            TreeCodec::<CASHash>::decode(truncated),
            Err(CASError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut enc = TreeCodec::<CASHash>::encode(&Tree::empty()).to_vec();
        enc.push(0);
        assert!(matches!(
            TreeCodec::<CASHash>::decode(&enc),
            Err(CASError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_unsorted_entries() {
        // Hand-craft a buffer with two entries in descending name order.
        let mut buf = Vec::new();
        buf.push(TREE_FORMAT_VERSION);
        buf.extend_from_slice(&2u32.to_le_bytes());
        for name in [b"b".as_slice(), b"a".as_slice()] {
            buf.push(KIND_BLOB);
            buf.extend_from_slice(&[7u8; HASH_LEN]);
            buf.extend_from_slice(&0o100644u32.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
            buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
            buf.extend_from_slice(name);
        }
        assert!(matches!(
            TreeCodec::<CASHash>::decode(&buf),
            Err(CASError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_duplicate_names() {
        let mut buf = Vec::new();
        buf.push(TREE_FORMAT_VERSION);
        buf.extend_from_slice(&2u32.to_le_bytes());
        for _ in 0..2 {
            buf.push(KIND_BLOB);
            buf.extend_from_slice(&[7u8; HASH_LEN]);
            buf.extend_from_slice(&0o100644u32.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.push(b'a');
        }
        assert!(matches!(
            TreeCodec::<CASHash>::decode(&buf),
            Err(CASError::Codec(_))
        ));
    }
}
