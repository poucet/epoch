//! [`FileCas`] — content-addressed storage for whole files, with an offset-aware
//! read/write path and a single, swappable chunking seam.
//!
//! `FileCas` composes two generic kernel stores over one shared byte backend:
//!
//! - a **chunk store** ([`IdentityCodec`]): raw chunk bytes, keyed by their own
//!   [`CASHash`]; this is where dedup happens, since identical chunks collapse
//!   to one entry.
//! - a **manifest store** ([`FileObjectCodec`]): the
//!   [`FileObject`](crate::FileObject) describing a file. A
//!   [`BlobId`](crate::BlobId) is the CAS id of a stored manifest.
//!
//! Storing a file ([`put`](FileCas::put) / [`commit`](FileCas::commit)) runs the
//! bytes through [`store_content`](FileCas::store_content) to produce a
//! `FileObject`, stores any chunks, stores the manifest, and returns the
//! manifest's id wrapped as a `BlobId`. Reads load the manifest and reassemble.

use epoch_cas::{
    Bytes, CASError, CASHash, IdentityCodec, MemoryByteBackend, SyncByteBackend, SyncCAS,
    SyncGenericCAS,
};

use crate::id::BlobId;
use crate::object::{ChunkRef, FileObject, FileObjectCodec};
use crate::writer::FileWriter;

/// THE SINGLE CHUNKING SEAM — files at or below this size are stored whole and
/// inline in the manifest; larger files are split into chunks.
///
/// Inlining small files avoids a second CAS round-trip (and a second stored
/// entry) for the common case of tiny files.
const INLINE_MAX: usize = 64 * 1024;

/// THE SINGLE CHUNKING SEAM — v1 fixed-size chunk length for files above
/// [`INLINE_MAX`].
///
/// Fixed-size splitting is the *entire* v1 chunking policy. It is intentionally
/// the only thing to change to adopt content-defined chunking (e.g. FastCDC)
/// later: the [`FileObject`](crate::FileObject) format already carries an
/// arbitrary number of chunks, and the public `FileCas` API is independent of
/// how [`store_content`](FileCas::store_content) draws chunk boundaries — so a
/// smarter splitter slots in here with no API change.
///
/// Kept at 1 MiB, comfortably within `u32`, so every [`ChunkRef::len`] fits.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Content-addressed storage for file content.
///
/// Cheaply parameterized over any [`SyncByteBackend`] `B` (the in-memory
/// [`MemoryByteBackend`] for tests, a persistent backend in production). The
/// chunk and manifest stores share one cloned backend, so chunk and manifest
/// entries live in the same keyspace (their keys are content hashes and never
/// collide in practice).
pub struct FileCas<B> {
    /// Raw chunk bytes, content-addressed (dedup happens here).
    chunks: SyncGenericCAS<B, IdentityCodec>,
    /// `FileObject` manifests; a [`BlobId`] addresses an entry here.
    manifests: SyncGenericCAS<B, FileObjectCodec>,
}

impl<B: SyncByteBackend + Clone> FileCas<B> {
    /// Build a `FileCas` over `backend`. The chunk and manifest stores each get
    /// a clone of the backend, sharing the same underlying storage.
    pub fn new(backend: B) -> FileCas<B> {
        FileCas {
            chunks: SyncGenericCAS::new(backend.clone()),
            manifests: SyncGenericCAS::new(backend),
        }
    }

    /// Store `content` as a new file, returning its [`BlobId`].
    ///
    /// Identical content always yields an identical `BlobId` (content
    /// addressing); re-putting is a harmless overwrite.
    pub fn put(&self, content: Bytes) -> Result<BlobId, CASError> {
        let object = self.store_content(&content)?;
        self.put_manifest(object)
    }

    /// Fetch the whole file addressed by `id`, reassembled, or `None` if no such
    /// manifest exists.
    pub fn get(&self, id: &BlobId) -> Result<Option<Bytes>, CASError> {
        let object = match self.manifest(id)? {
            None => return Ok(None),
            Some(object) => object,
        };
        match object {
            FileObject::Inline(content) => Ok(Some(content)),
            FileObject::Chunked { chunks } => {
                let mut out = Vec::with_capacity(
                    chunks.iter().map(|c| c.len as usize).sum::<usize>(),
                );
                for chunk in &chunks {
                    out.extend_from_slice(&self.load_chunk(chunk)?);
                }
                Ok(Some(Bytes::from(out)))
            }
        }
    }

    /// Read the overlap of `[off, off + len)` with the file's `[0, size)`,
    /// fetching only the chunk(s) that overlap.
    ///
    /// Returns empty when `off` is at or past EOF, and clamps a partial overlap
    /// to the bytes that exist. Errors with [`CASError::NotFound`] if the
    /// manifest is missing (a present-but-empty read range is *not* an error).
    pub fn read_at(&self, id: &BlobId, off: u64, len: u32) -> Result<Bytes, CASError> {
        let object = self.manifest(id)?.ok_or(CASError::NotFound)?;
        let size = object.logical_len();
        // Clamp the request to what the file actually holds.
        if off >= size {
            return Ok(Bytes::new());
        }
        let end = off.saturating_add(len as u64).min(size);
        let want = (end - off) as usize;

        match object {
            FileObject::Inline(content) => {
                Ok(content.slice(off as usize..off as usize + want))
            }
            FileObject::Chunked { chunks } => {
                let mut out = Vec::with_capacity(want);
                // Walk chunks by cumulative offset, fetching only those that
                // overlap `[off, end)`.
                let mut chunk_start: u64 = 0;
                for chunk in &chunks {
                    let chunk_end = chunk_start + chunk.len as u64;
                    if chunk_end <= off {
                        // Entirely before the window — skip without fetching.
                        chunk_start = chunk_end;
                        continue;
                    }
                    if chunk_start >= end {
                        // Past the window — nothing more to read.
                        break;
                    }
                    let bytes = self.load_chunk(chunk)?;
                    // Intersect `[off, end)` with this chunk's span.
                    let from = off.saturating_sub(chunk_start) as usize;
                    let to = (end.min(chunk_end) - chunk_start) as usize;
                    out.extend_from_slice(&bytes[from..to]);
                    chunk_start = chunk_end;
                }
                Ok(Bytes::from(out))
            }
        }
    }

    /// The logical size of the file addressed by `id`, or `None` if absent.
    pub fn size(&self, id: &BlobId) -> Result<Option<u64>, CASError> {
        Ok(self.manifest(id)?.map(|o| o.logical_len()))
    }

    /// Whether a manifest for `id` is present.
    pub fn has(&self, id: &BlobId) -> Result<bool, CASError> {
        self.manifests.has(id.as_cas_hash())
    }

    /// Remove the **manifest** for `id`. No error if absent.
    ///
    /// This deletes only the manifest, *not* the chunks it referenced: chunks
    /// are content-addressed and may be shared (deduped) with other files, so
    /// blindly deleting them here could corrupt unrelated blobs. Reclaiming
    /// unreferenced chunks is the job of a separate mark-and-sweep GC pass that
    /// has the full set of live manifests in view.
    pub fn delete(&self, id: &BlobId) -> Result<(), CASError> {
        self.manifests.delete(id.as_cas_hash())
    }

    /// Open a [`FileWriter`] for editing.
    ///
    /// With `base = None` the writer starts empty. With `base = Some(id)` the
    /// base file is materialized fully into the writer's buffer (v1 strategy;
    /// see [`FileWriter`] for the sparse-overlay optimization this leaves room
    /// for). Errors with [`CASError::NotFound`] if `base` names a missing blob.
    pub fn edit(&self, base: Option<BlobId>) -> Result<FileWriter, CASError> {
        let buf = match base {
            None => Vec::new(),
            Some(id) => self.get(&id)?.ok_or(CASError::NotFound)?.to_vec(),
        };
        Ok(FileWriter::from_buf(buf))
    }

    /// Seal a [`FileWriter`] into an immutable blob, returning its [`BlobId`].
    ///
    /// This is where the one-and-only hashing/chunking happens for the edit
    /// session (see [`FileWriter`]'s hot-path rationale).
    pub fn commit(&self, writer: FileWriter) -> Result<BlobId, CASError> {
        let object = self.store_content(&writer.buf)?;
        self.put_manifest(object)
    }

    /// THE SINGLE CHUNKING SEAM (implementation).
    ///
    /// Maps raw content to a [`FileObject`], storing chunks as a side effect:
    ///
    /// - `len <= INLINE_MAX` → [`FileObject::Inline`] holding a copy of the
    ///   bytes (no chunk store writes).
    /// - otherwise → split into [`CHUNK_SIZE`]-byte pieces, `put` each into the
    ///   chunk store, and collect the resulting [`ChunkRef`]s into
    ///   [`FileObject::Chunked`].
    ///
    /// Everything about *how a file is broken up* lives here and nowhere else;
    /// swapping fixed-size splitting for content-defined chunking is a local
    /// change to this function.
    fn store_content(&self, content: &[u8]) -> Result<FileObject, CASError> {
        if content.len() <= INLINE_MAX {
            return Ok(FileObject::Inline(Bytes::copy_from_slice(content)));
        }
        let mut chunks = Vec::with_capacity(content.len().div_ceil(CHUNK_SIZE));
        for piece in content.chunks(CHUNK_SIZE) {
            let id = self.chunks.put(Bytes::copy_from_slice(piece))?;
            // CHUNK_SIZE fits in u32, so `piece.len()` always does too.
            chunks.push(ChunkRef {
                id,
                len: piece.len() as u32,
            });
        }
        Ok(FileObject::Chunked { chunks })
    }

    /// Store a manifest and return its id wrapped as a [`BlobId`].
    fn put_manifest(&self, object: FileObject) -> Result<BlobId, CASError> {
        let hash = self.manifests.put(object)?;
        Ok(BlobId::from_cas_hash(hash))
    }

    /// Load the [`FileObject`] manifest addressed by `id`, or `None` if absent.
    ///
    /// This is the structural view of a blob: callers that need to enumerate
    /// the CAS objects a blob is made of (e.g. a checkpoint closure walk
    /// promoting chunks and the manifest to durable storage) read the
    /// [`ChunkRef`]s from here rather than reassembling content.
    pub fn manifest(&self, id: &BlobId) -> Result<Option<FileObject>, CASError> {
        self.manifests.get(id.as_cas_hash())
    }

    /// Fetch a referenced chunk's raw bytes, treating an absent chunk as
    /// corruption: a live manifest must not point at a missing chunk.
    fn load_chunk(&self, chunk: &ChunkRef) -> Result<Bytes, CASError> {
        self.fetch_chunk(&chunk.id)
    }

    /// Fetch raw chunk bytes by CAS id, erroring [`CASError::NotFound`] when
    /// absent (a dangling chunk reference).
    fn fetch_chunk(&self, id: &CASHash) -> Result<Bytes, CASError> {
        self.chunks.get(id)?.ok_or(CASError::NotFound)
    }
}

/// Convenience constructor for the in-memory backend, mirroring the kernel's
/// reference setup for tests and ephemeral use.
impl FileCas<MemoryByteBackend> {
    /// A `FileCas` over a fresh in-memory backend.
    pub fn in_memory() -> FileCas<MemoryByteBackend> {
        FileCas::new(MemoryByteBackend::new())
    }
}

/// Interop with the generic kernel trait: a `FileCas` is itself a
/// `SyncCAS<Bytes>` whose id is [`BlobId`], so it drops into anything generic
/// over [`SyncCAS`]. `get`/`put`/`has`/`delete` mean the whole-file operations;
/// the offset-aware [`read_at`](FileCas::read_at) and the
/// [`edit`](FileCas::edit)/[`commit`](FileCas::commit) write path are the
/// richer, file-specific surface beyond the trait.
impl<B: SyncByteBackend + Clone> SyncCAS<Bytes> for FileCas<B> {
    type Id = BlobId;

    fn put(&self, value: Bytes) -> Result<BlobId, CASError> {
        FileCas::put(self, value)
    }

    fn get(&self, id: &BlobId) -> Result<Option<Bytes>, CASError> {
        FileCas::get(self, id)
    }

    fn has(&self, id: &BlobId) -> Result<bool, CASError> {
        FileCas::has(self, id)
    }

    fn delete(&self, id: &BlobId) -> Result<(), CASError> {
        FileCas::delete(self, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file just over `INLINE_MAX` so it is guaranteed to be chunked, with a
    /// non-repeating-ish pattern so chunk boundaries are detectable in reads.
    fn big_content(len: usize) -> Bytes {
        let mut v = Vec::with_capacity(len);
        for i in 0..len {
            v.push((i % 251) as u8);
        }
        Bytes::from(v)
    }

    #[test]
    fn put_get_small_inline_roundtrip() {
        let cas = FileCas::in_memory();
        let content = Bytes::from_static(b"a small file");
        let id = cas.put(content.clone()).unwrap();
        assert_eq!(cas.get(&id).unwrap(), Some(content));
    }

    #[test]
    fn put_get_large_multichunk_roundtrip() {
        let cas = FileCas::in_memory();
        // 2.5 MiB -> 3 chunks.
        let content = big_content(CHUNK_SIZE * 2 + CHUNK_SIZE / 2);
        let id = cas.put(content.clone()).unwrap();
        assert_eq!(cas.get(&id).unwrap(), Some(content));
    }

    #[test]
    fn identical_content_dedups_to_same_blob_id() {
        let cas = FileCas::in_memory();
        let content = big_content(CHUNK_SIZE * 2);
        let a = cas.put(content.clone()).unwrap();
        let b = cas.put(content).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn small_is_inline_large_is_chunked_with_expected_count() {
        let cas = FileCas::in_memory();

        let small = cas.store_content(&[1u8; INLINE_MAX]).unwrap();
        assert!(matches!(small, FileObject::Inline(_)));

        // INLINE_MAX + 1 forces chunking; with one byte over a single inline
        // limit it is still a single 64KiB+1 chunk (< CHUNK_SIZE).
        let just_over = cas.store_content(&vec![1u8; INLINE_MAX + 1]).unwrap();
        match just_over {
            FileObject::Chunked { chunks } => assert_eq!(chunks.len(), 1),
            other => panic!("expected chunked, got {other:?}"),
        }

        // 2.5 MiB -> ceil(2.5) = 3 chunks.
        let big = cas
            .store_content(&big_content(CHUNK_SIZE * 2 + CHUNK_SIZE / 2))
            .unwrap();
        match big {
            FileObject::Chunked { chunks } => assert_eq!(chunks.len(), 3),
            other => panic!("expected chunked, got {other:?}"),
        }
    }

    #[test]
    fn read_at_full_partial_boundary_and_eof() {
        let cas = FileCas::in_memory();
        let content = big_content(CHUNK_SIZE * 2 + 1234);
        let id = cas.put(content.clone()).unwrap();

        // Full read via a generous length clamps to size.
        let full = cas.read_at(&id, 0, u32::MAX).unwrap();
        assert_eq!(full, content);

        // Partial read at an offset within the first chunk.
        let mid = cas.read_at(&id, 100, 50).unwrap();
        assert_eq!(mid, content.slice(100..150));

        // Read spanning the first/second chunk boundary.
        let span = cas
            .read_at(&id, (CHUNK_SIZE - 10) as u64, 20)
            .unwrap();
        assert_eq!(span, content.slice(CHUNK_SIZE - 10..CHUNK_SIZE + 10));

        // Read past EOF -> empty.
        let past = cas.read_at(&id, content.len() as u64 + 5, 16).unwrap();
        assert!(past.is_empty());

        // Read straddling EOF -> clamped to the tail.
        let tail = cas
            .read_at(&id, content.len() as u64 - 4, 100)
            .unwrap();
        assert_eq!(tail, content.slice(content.len() - 4..));
    }

    #[test]
    fn read_at_on_inline_clamps_too() {
        let cas = FileCas::in_memory();
        let content = Bytes::from_static(b"0123456789");
        let id = cas.put(content).unwrap();
        assert_eq!(cas.read_at(&id, 3, 4).unwrap(), Bytes::from_static(b"3456"));
        assert_eq!(cas.read_at(&id, 8, 100).unwrap(), Bytes::from_static(b"89"));
        assert!(cas.read_at(&id, 10, 5).unwrap().is_empty());
    }

    #[test]
    fn size_reports_logical_len() {
        let cas = FileCas::in_memory();
        let small = cas.put(Bytes::from_static(b"abc")).unwrap();
        assert_eq!(cas.size(&small).unwrap(), Some(3));

        let big_len = CHUNK_SIZE * 2 + 7;
        let big = cas.put(big_content(big_len)).unwrap();
        assert_eq!(cas.size(&big).unwrap(), Some(big_len as u64));

        let missing = BlobId::from_cas_hash(CASHash::from_bytes([0xab; 32]));
        assert_eq!(cas.size(&missing).unwrap(), None);
    }

    #[test]
    fn manifest_exposes_structure() {
        let cas = FileCas::in_memory();

        let small = cas.put(Bytes::from_static(b"tiny")).unwrap();
        assert!(matches!(
            cas.manifest(&small).unwrap(),
            Some(FileObject::Inline(_))
        ));

        let big = cas.put(big_content(CHUNK_SIZE * 2 + 1)).unwrap();
        match cas.manifest(&big).unwrap() {
            Some(FileObject::Chunked { chunks }) => {
                assert_eq!(chunks.len(), 3);
                // Every referenced chunk is fetchable by its id.
                for c in &chunks {
                    assert!(cas.fetch_chunk(&c.id).is_ok());
                }
            }
            other => panic!("expected chunked manifest, got {other:?}"),
        }

        let missing = BlobId::from_cas_hash(CASHash::from_bytes([0x77; 32]));
        assert_eq!(cas.manifest(&missing).unwrap(), None);
    }

    #[test]
    fn has_and_delete_manifest() {
        let cas = FileCas::in_memory();
        let id = cas.put(Bytes::from_static(b"x")).unwrap();
        assert!(cas.has(&id).unwrap());
        cas.delete(&id).unwrap();
        assert!(!cas.has(&id).unwrap());
        assert_eq!(cas.get(&id).unwrap(), None);
        // Deleting again is harmless.
        cas.delete(&id).unwrap();
    }

    #[test]
    fn edit_empty_write_commit_roundtrip() {
        let cas = FileCas::in_memory();
        let mut w = cas.edit(None).unwrap();
        w.write_at(0, b"hello ");
        // Write at a gap offset -> zero-filled hole.
        w.write_at(10, b"world");
        // Overwrite a middle region.
        w.write_at(0, b"HELLO");
        // Read-your-writes via writer.read_at.
        assert_eq!(w.read_at(0, 5), Bytes::from_static(b"HELLO"));
        let expected = w.read_at(0, w.len() as u32);

        let id = cas.commit(w).unwrap();
        assert_eq!(cas.get(&id).unwrap(), Some(expected));
    }

    #[test]
    fn truncate_grow_and_shrink_then_commit() {
        let cas = FileCas::in_memory();

        let mut grow = cas.edit(None).unwrap();
        grow.write_at(0, b"abc");
        grow.truncate(5);
        let grown = cas.commit(grow).unwrap();
        assert_eq!(
            cas.get(&grown).unwrap(),
            Some(Bytes::from(vec![b'a', b'b', b'c', 0, 0]))
        );

        let mut shrink = cas.edit(Some(grown)).unwrap();
        shrink.truncate(2);
        let shrunk = cas.commit(shrink).unwrap();
        assert_eq!(cas.get(&shrunk).unwrap(), Some(Bytes::from_static(b"ab")));
    }

    #[test]
    fn edit_existing_base_overwrite_yields_new_blob_both_readable() {
        let cas = FileCas::in_memory();
        let original = big_content(CHUNK_SIZE + 500);
        let old = cas.put(original.clone()).unwrap();

        let mut w = cas.edit(Some(old)).unwrap();
        w.write_at(100, b"PATCHED");
        let new = cas.commit(w).unwrap();

        assert_ne!(new, old);
        // Old blob is untouched and still readable.
        assert_eq!(cas.get(&old).unwrap(), Some(original.clone()));
        // New blob has the patch applied.
        let mut patched = original.to_vec();
        patched[100..107].copy_from_slice(b"PATCHED");
        assert_eq!(cas.get(&new).unwrap(), Some(Bytes::from(patched)));
    }

    #[test]
    fn edit_missing_base_errors() {
        let cas = FileCas::in_memory();
        let missing = BlobId::from_cas_hash(CASHash::from_bytes([0x11; 32]));
        assert!(matches!(cas.edit(Some(missing)), Err(CASError::NotFound)));
    }

    #[test]
    fn sync_cas_trait_interop() {
        let cas = FileCas::in_memory();
        // Drive it purely through the generic trait surface.
        fn roundtrip<C: SyncCAS<Bytes, Id = BlobId>>(c: &C, b: Bytes) -> Bytes {
            let id = c.put(b).unwrap();
            assert!(c.has(&id).unwrap());
            c.get(&id).unwrap().unwrap()
        }
        let content = Bytes::from_static(b"trait interop");
        assert_eq!(roundtrip(&cas, content.clone()), content);
    }
}
