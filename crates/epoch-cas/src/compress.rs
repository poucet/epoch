//! zstd compression of the canonical (hashed) encoding before it hits the
//! backend. The id is the hash of the *uncompressed* encoding, so compression
//! level never affects convergence.

use bytes::Bytes;

use crate::error::CASError;

/// Default zstd level. 3 is zstd's own default — a good speed/ratio balance.
const ZSTD_LEVEL: i32 = 3;

/// Compress canonical bytes for storage.
pub fn compress(bytes: &[u8]) -> Result<Bytes, CASError> {
    zstd::stream::encode_all(bytes, ZSTD_LEVEL)
        .map(Bytes::from)
        .map_err(|e| CASError::Compression(e.to_string()))
}

/// Decompress stored bytes back to the canonical encoding.
pub fn decompress(bytes: &[u8]) -> Result<Bytes, CASError> {
    zstd::stream::decode_all(bytes)
        .map(Bytes::from)
        .map_err(|e| CASError::Compression(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let c = compress(&original).unwrap();
        let d = decompress(&c).unwrap();
        assert_eq!(&d[..], &original[..]);
    }

    #[test]
    fn decompress_rejects_garbage() {
        assert!(matches!(
            decompress(b"not zstd"),
            Err(CASError::Compression(_))
        ));
    }
}
