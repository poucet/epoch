//! The codec boundary (D4): owns the canonical `value <-> bytes` mapping.
//!
//! The encoding is a domain invariant — it determines the BLAKE3 hash, which
//! determines CAS convergence — so each value type has exactly one codec.

use bytes::Bytes;

use crate::error::CASError;

/// Canonical encode/decode for a value type `V`.
///
/// `encode` must be deterministic: identical values must produce identical
/// bytes, since those bytes are what gets hashed.
pub trait Codec<V>: Send + Sync + 'static {
    /// Encode a value to its canonical byte form.
    fn encode(value: &V) -> Bytes;

    /// Decode bytes back into a value. Returns [`CASError::Codec`] on malformed
    /// input.
    fn decode(bytes: &[u8]) -> Result<V, CASError>;
}

/// The registry of **object-kind domain tags** (git-style): the first byte of
/// every canonical CAS encoding names what the object *is*, and is hashed into
/// the id. Ids of different kinds therefore live in disjoint hash domains — a
/// tree hash can never resolve as a blob, type sniffing is a one-byte peek at
/// the decompressed object, and no cross-type confusion can survive transport
/// between stores or machines.
///
/// The kernel assigns the bytes but attaches no semantics; each typed codec
/// (in its own crate) prepends and validates its tag.
pub mod object_kind {
    /// A tree object (`epoch-tree`'s `TreeCodec`).
    pub const TREE: u8 = b'T';
    /// A file manifest (`epoch-file-cas`'s `FileObjectCodec`).
    pub const FILE: u8 = b'F';
    /// A raw content chunk (`epoch-file-cas`'s chunk store).
    pub const CHUNK: u8 = b'C';
}

/// The trivial codec for raw [`Bytes`]: file content is already canonical, so
/// encode is a cheap refcount clone and decode is a copy of the slice.
pub struct IdentityCodec;

impl Codec<Bytes> for IdentityCodec {
    fn encode(value: &Bytes) -> Bytes {
        value.clone()
    }

    fn decode(bytes: &[u8]) -> Result<Bytes, CASError> {
        Ok(Bytes::copy_from_slice(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip() {
        let v = Bytes::from_static(b"payload");
        let enc = IdentityCodec::encode(&v);
        assert_eq!(enc, v);
        let dec = IdentityCodec::decode(&enc).unwrap();
        assert_eq!(dec, v);
    }
}
