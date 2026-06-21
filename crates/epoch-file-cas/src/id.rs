//! [`BlobId`] — the domain id for a stored file blob.
//!
//! A `BlobId` is the content-address of a stored [`FileObject`](crate::FileObject)
//! manifest: feed a whole file through [`FileCas`](crate::FileCas) and the
//! `BlobId` you get back addresses the manifest that describes it (inline bytes,
//! or the ordered chunk list). It is a thin newtype around the kernel's
//! [`CASHash`] — a domain-meaningful name in signatures, with the kernel id one
//! cheap unwrap away.
//!
//! ## Why this id lives here, not in `epoch-tree`
//!
//! `BlobId` is **owned by this crate**, deliberately. A higher-level crate
//! (`epoch-tree`) wants to use `BlobId` as the blob-id type its directory trees
//! point at, but `epoch-file-cas` must not depend on `epoch-tree` — the file CAS
//! is the lower layer. The two-way [`From`] conversions with [`CASHash`] are the
//! seam that makes this work: `epoch-tree` can convert a `BlobId` to/from a bare
//! `CASHash` at its own boundary without either crate naming the other.

use std::fmt;

use epoch_cas::CASHash;

/// The content-address of a stored file blob's manifest.
///
/// Two identical files always produce the same `BlobId` (the whole point of
/// content addressing). Cheap to copy (it is 32 bytes) and usable as a map key
/// (`Hash`/`Eq`) or in a sorted index (`Ord`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobId(CASHash);

impl BlobId {
    /// Wrap a kernel [`CASHash`] as a `BlobId` (no hashing performed).
    pub const fn from_cas_hash(hash: CASHash) -> BlobId {
        BlobId(hash)
    }

    /// Borrow the underlying [`CASHash`].
    pub const fn as_cas_hash(&self) -> &CASHash {
        &self.0
    }

    /// Consume into the underlying [`CASHash`].
    pub const fn into_cas_hash(self) -> CASHash {
        self.0
    }

    /// Lowercase hex representation (64 chars), delegating to the inner hash.
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

/// The lower-layer → higher-layer seam: `epoch-tree` builds a `BlobId` from a
/// bare [`CASHash`] without depending on this crate's constructor.
impl From<CASHash> for BlobId {
    fn from(hash: CASHash) -> BlobId {
        BlobId(hash)
    }
}

/// The reverse seam: hand a `BlobId` back to anything that speaks raw
/// [`CASHash`] (e.g. a tree's serializer) without it naming `BlobId`.
impl From<BlobId> for CASHash {
    fn from(id: BlobId) -> CASHash {
        id.0
    }
}

/// Bare hex, like [`CASHash`]'s `Display` — readable in logs and paths.
impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_hex())
    }
}

/// Tagged hex so the domain type is unmistakable in debug output.
impl fmt::Debug for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobId({})", self.0.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash() -> CASHash {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        CASHash::from_bytes(bytes)
    }

    #[test]
    fn wrap_and_unwrap_roundtrip() {
        let h = sample_hash();
        let id = BlobId::from_cas_hash(h);
        assert_eq!(*id.as_cas_hash(), h);
        assert_eq!(id.into_cas_hash(), h);
    }

    #[test]
    fn from_conversions_are_two_way() {
        let h = sample_hash();
        let id: BlobId = h.into();
        let back: CASHash = id.into();
        assert_eq!(h, back);
    }

    #[test]
    fn display_is_bare_hex_debug_is_tagged() {
        let id = BlobId::from_cas_hash(sample_hash());
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(id.to_string(), hex);
        assert_eq!(format!("{id:?}"), format!("BlobId({hex})"));
    }
}
