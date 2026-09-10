//! [`FileObject`] — the on-disk object a [`BlobId`](crate::BlobId) addresses,
//! plus its canonical [`FileObjectCodec`].
//!
//! A `FileObject` is the *manifest* of a file's content. It is representation-
//! agnostic: a small file is stored whole and inline; a large file is stored as
//! an ordered list of [`ChunkRef`]s, each pointing at a chunk held separately in
//! the CAS. Reads ([`FileCas::get`](crate::FileCas::get) /
//! [`read_at`](crate::FileCas::read_at)) walk this manifest; the public API never
//! exposes which representation was chosen.
//!
//! The codec is **deterministic and tagged** — those bytes are what get hashed
//! into the `BlobId`, so two files with identical content must encode to
//! identical bytes (D4 convergence), and the decoder rejects anything malformed.

use epoch_cas::{Bytes, CASError, CASHash, Codec, object_kind};

/// A pointer to one stored chunk of a chunked file: the chunk's own CAS id plus
/// its logical (uncompressed) length.
///
/// The length is carried alongside the id so [`read_at`](crate::FileCas::read_at)
/// can map a byte offset to the right chunk(s) *without* fetching any chunk
/// bytes first. It is a `u32`: the chunking policy keeps every chunk within
/// `u32::MAX`, so 4 bytes suffice and the manifest stays compact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkRef {
    /// The CAS id of the chunk's raw bytes (stored via the chunk store).
    pub id: CASHash,
    /// The chunk's logical byte length.
    pub len: u32,
}

/// The manifest of a file's content: either the bytes inline, or an ordered list
/// of chunk references.
///
/// Choosing between the two is the chunking policy's job (see
/// [`FileCas`](crate::FileCas)); both decode back to the same logical byte
/// stream, so callers treat a file as just its bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileObject {
    /// The whole file, stored inline in the manifest (small files).
    Inline(Bytes),
    /// The file as an ordered sequence of chunks held separately in the CAS.
    Chunked {
        /// The chunks, in file order; concatenating them yields the file.
        chunks: Vec<ChunkRef>,
    },
}

impl FileObject {
    /// The file's logical size in bytes: the inline byte length, or the sum of
    /// the chunk lengths.
    ///
    /// Returns `u64` (not `usize`): a file can exceed a 32-bit address space
    /// even though each individual chunk is bounded to `u32`.
    pub fn logical_len(&self) -> u64 {
        match self {
            FileObject::Inline(bytes) => bytes.len() as u64,
            FileObject::Chunked { chunks } => chunks.iter().map(|c| c.len as u64).sum(),
        }
    }
}

/// Tag bytes for the wire format. A leading tag keeps the encoding self-
/// describing and lets the decoder reject anything that is neither variant.
const TAG_INLINE: u8 = 0;
const TAG_CHUNKED: u8 = 1;

/// The canonical `FileObject <-> bytes` mapping.
///
/// # Byte layout (little-endian, deterministic)
///
/// ```text
/// u8 tag
///   tag = 0 (Inline):  u64 len, then `len` content bytes
///   tag = 1 (Chunked): u32 count, then `count` × { 32-byte CASHash, u32 len }
/// ```
///
/// `decode` is strict: it rejects an unknown tag, any truncation, and any
/// trailing bytes — all as [`CASError::Codec`]. Strictness matters because a
/// `FileObject`'s bytes are content-addressed, so there must be exactly one
/// valid byte string per value.
pub struct FileObjectCodec;

impl FileObjectCodec {
    /// Per-chunk encoded size: 32-byte hash + 4-byte length.
    const CHUNK_ENCODED_LEN: usize = 32 + 4;
}

impl Codec<FileObject> for FileObjectCodec {
    fn encode(value: &FileObject) -> Bytes {
        match value {
            FileObject::Inline(content) => {
                let mut out = Vec::with_capacity(1 + 8 + content.len());
                out.push(object_kind::FILE);
                out.push(TAG_INLINE);
                out.extend_from_slice(&(content.len() as u64).to_le_bytes());
                out.extend_from_slice(content);
                Bytes::from(out)
            }
            FileObject::Chunked { chunks } => {
                let mut out =
                    Vec::with_capacity(1 + 4 + chunks.len() * FileObjectCodec::CHUNK_ENCODED_LEN);
                out.push(object_kind::FILE);
                out.push(TAG_CHUNKED);
                out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
                for chunk in chunks {
                    out.extend_from_slice(chunk.id.as_bytes());
                    out.extend_from_slice(&chunk.len.to_le_bytes());
                }
                Bytes::from(out)
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<FileObject, CASError> {
        let mut reader = Reader::new(bytes);
        let kind = reader.u8()?;
        if kind != object_kind::FILE {
            return Err(CASError::Codec(format!(
                "not a file manifest (kind tag {kind:#04x})"
            )));
        }
        let tag = reader.u8()?;
        let object = match tag {
            TAG_INLINE => {
                let len = reader.u64()? as usize;
                let content = reader.take(len)?;
                FileObject::Inline(Bytes::copy_from_slice(content))
            }
            TAG_CHUNKED => {
                let count = reader.u32()? as usize;
                if count > reader.remaining() / FileObjectCodec::CHUNK_ENCODED_LEN {
                    return Err(CASError::Codec(format!(
                        "truncated FileObject: needed at least {} bytes for {count} chunks, have {}",
                        count.saturating_mul(FileObjectCodec::CHUNK_ENCODED_LEN),
                        reader.remaining()
                    )));
                }
                let mut chunks = Vec::with_capacity(count);
                for _ in 0..count {
                    let id = CASHash::from_bytes(reader.array32()?);
                    let len = reader.u32()?;
                    chunks.push(ChunkRef { id, len });
                }
                FileObject::Chunked { chunks }
            }
            other => {
                return Err(CASError::Codec(format!("unknown FileObject tag: {other}")));
            }
        };
        reader.finish()?;
        Ok(object)
    }
}

/// A tiny cursor over the encoded bytes that turns every short read into a
/// [`CASError::Codec`] rather than a panic — DRY across the fixed-width fields.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CASError> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            CASError::Codec("FileObject length overflows address space".to_string())
        })?;
        if end > self.bytes.len() {
            return Err(CASError::Codec(format!(
                "truncated FileObject: need {n} bytes at offset {}, have {}",
                self.pos,
                self.bytes.len() - self.pos
            )));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CASError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CASError> {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(buf))
    }

    fn u64(&mut self) -> Result<u64, CASError> {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(buf))
    }

    fn array32(&mut self) -> Result<[u8; 32], CASError> {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(self.take(32)?);
        Ok(buf)
    }

    /// Reject trailing bytes: a content-addressed encoding must be exact.
    fn finish(self) -> Result<(), CASError> {
        if self.pos != self.bytes.len() {
            return Err(CASError::Codec(format!(
                "trailing bytes after FileObject: {} unread",
                self.bytes.len() - self.pos
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(seed: u8, len: u32) -> ChunkRef {
        ChunkRef {
            id: CASHash::from_bytes([seed; 32]),
            len,
        }
    }

    #[test]
    fn logical_len_inline_and_chunked() {
        assert_eq!(FileObject::Inline(Bytes::from_static(b"hello")).logical_len(), 5);
        let obj = FileObject::Chunked {
            chunks: vec![chunk(1, 10), chunk(2, 25)],
        };
        assert_eq!(obj.logical_len(), 35);
    }

    #[test]
    fn inline_roundtrip() {
        let obj = FileObject::Inline(Bytes::from_static(b"payload"));
        let enc = FileObjectCodec::encode(&obj);
        assert_eq!(FileObjectCodec::decode(&enc).unwrap(), obj);
    }

    #[test]
    fn chunked_roundtrip() {
        let obj = FileObject::Chunked {
            chunks: vec![chunk(7, 100), chunk(8, 200), chunk(9, 3)],
        };
        let enc = FileObjectCodec::encode(&obj);
        assert_eq!(FileObjectCodec::decode(&enc).unwrap(), obj);
    }

    #[test]
    fn encode_is_deterministic() {
        let obj = FileObject::Chunked {
            chunks: vec![chunk(1, 5), chunk(2, 6)],
        };
        assert_eq!(FileObjectCodec::encode(&obj), FileObjectCodec::encode(&obj));
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        let err = FileObjectCodec::decode(&[object_kind::FILE, 9u8, 0, 0]).unwrap_err();
        assert!(matches!(err, CASError::Codec(_)));
    }

    #[test]
    fn decode_rejects_truncation() {
        // Inline tag claiming 10 bytes but supplying none.
        let mut bytes = vec![object_kind::FILE, TAG_INLINE];
        bytes.extend_from_slice(&10u64.to_le_bytes());
        let err = FileObjectCodec::decode(&bytes).unwrap_err();
        assert!(matches!(err, CASError::Codec(_)));
    }

    #[test]
    fn decode_rejects_chunk_count_exceeding_buffer() {
        let mut bytes = vec![object_kind::FILE, TAG_CHUNKED];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = FileObjectCodec::decode(&bytes).unwrap_err();
        assert!(matches!(err, CASError::Codec(_)));
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut enc = FileObjectCodec::encode(&FileObject::Inline(Bytes::from_static(b"x"))).to_vec();
        enc.push(0xff);
        let err = FileObjectCodec::decode(&enc).unwrap_err();
        assert!(matches!(err, CASError::Codec(_)));
    }

    #[test]
    fn decode_rejects_empty() {
        let err = FileObjectCodec::decode(&[]).unwrap_err();
        assert!(matches!(err, CASError::Codec(_)));
    }
}
