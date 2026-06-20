//! The pluggable content-hash function: maps canonical bytes to a [`CASHash`].
//!
//! Extracted behind a trait so the digest algorithm is a per-CAS choice rather
//! than baked into the address type. The kernel defaults to [`Blake3Hasher`];
//! any deterministic digest that fits in 32 bytes can be swapped in through the
//! `H` type parameter of [`SyncGenericCAS`](crate::SyncGenericCAS) /
//! [`AsyncGenericCAS`](crate::AsyncGenericCAS).

use crate::hash::CASHash;

/// Maps a canonical byte encoding to its content-address [`CASHash`].
///
/// Stateless by contract — an associated fn, like [`Codec`](crate::Codec): a
/// content hash takes no key or seed. Implementors must be deterministic, since
/// the same bytes must always yield the same [`CASHash`]; otherwise content
/// addressing and the read-side integrity check both break.
pub trait ContentHasher: Send + Sync + 'static {
    /// The content-address of `bytes`.
    fn hash(bytes: &[u8]) -> CASHash;
}

/// The kernel's default hasher: a 32-byte BLAKE3 digest.
pub struct Blake3Hasher;

impl ContentHasher for Blake3Hasher {
    fn hash(bytes: &[u8]) -> CASHash {
        CASHash::from_bytes(*blake3::hash(bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_is_deterministic() {
        assert_eq!(Blake3Hasher::hash(b"hello"), Blake3Hasher::hash(b"hello"));
        assert_ne!(Blake3Hasher::hash(b"hello"), Blake3Hasher::hash(b"world"));
    }

    #[test]
    fn matches_known_blake3_vector() {
        // BLAKE3 of the empty input.
        let h = Blake3Hasher::hash(b"");
        assert_eq!(
            h.to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
