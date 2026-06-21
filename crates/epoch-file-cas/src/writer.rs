//! [`FileWriter`] — the offset-aware write path.
//!
//! A `FileWriter` is a mutable, growable view of a file's bytes that a caller
//! mutates with offset-addressed [`write_at`](FileWriter::write_at) /
//! [`truncate`](FileWriter::truncate) calls and reads back with
//! [`read_at`](FileWriter::read_at) (read-your-writes). It is obtained from
//! [`FileCas::edit`](crate::FileCas::edit) and sealed by
//! [`FileCas::commit`](crate::FileCas::commit).
//!
//! ## Why a separate write path (the design rationale, stated loudly)
//!
//! Hashing and chunking are **not** on the per-write hot path. Each
//! `write_at` is a plain in-memory buffer mutation — zero CAS work, zero
//! hashing. The file is hashed and stored exactly once, at `commit` time, which
//! "seals" the accumulated edits into an immutable [`BlobId`](crate::BlobId).
//! This is what makes the writer suitable for the synchronous FUSE write path,
//! where a single logical file write can be many small `write_at` calls.
//!
//! ## v1 materialization, and the optimization it leaves room for
//!
//! v1 is deliberately simple: [`edit`](crate::FileCas::edit) materializes the
//! base file **fully** into a contiguous `Vec<u8>` (empty when there is no
//! base), and edits mutate that buffer. The cost is that committing a one-byte
//! change to a huge file re-stores the whole file.
//!
//! The future optimization is a sparse overlay over the base plus incremental
//! chunk reuse, so a small write to a huge file re-stores roughly one chunk.
//! That pairs with content-defined chunking and slots in **behind this same
//! public API** — `write_at`/`truncate`/`read_at`/`commit` do not change. The
//! inode/HEAD layer (a future crate) holds a `FileWriter` open per dirty file
//! and commits on `fsync`/`close`/`snapshot`.

use epoch_cas::Bytes;

/// A mutable, growable, offset-addressed buffer of a file's bytes.
///
/// Mutations are pure in-memory work; the contents become a content-addressed
/// blob only when handed to [`FileCas::commit`](crate::FileCas::commit).
pub struct FileWriter {
    /// The full materialized file contents. v1 holds the whole file here; a
    /// future sparse overlay would replace this field without changing the API.
    pub(crate) buf: Vec<u8>,
}

impl FileWriter {
    /// Build a writer over already-materialized bytes.
    ///
    /// Used by [`FileCas::edit`](crate::FileCas::edit): the base file (or an
    /// empty `Vec` for a fresh file) is passed straight in.
    pub(crate) fn from_buf(buf: Vec<u8>) -> FileWriter {
        FileWriter { buf }
    }

    /// Overwrite `data` starting at byte offset `off`, growing the file as
    /// needed.
    ///
    /// If `off` is past the current end, the gap `[len, off)` is zero-filled
    /// (a sparse-style hole, materialized as zeroes in v1) before `data` is
    /// written. Writing within the existing length overwrites in place.
    pub fn write_at(&mut self, off: u64, data: &[u8]) {
        let off = off as usize;
        let end = off + data.len();
        // Grow + zero-fill so that both the gap before `off` (a hole) and the
        // region `data` lands in exist before we copy.
        if end > self.buf.len() {
            self.buf.resize(end, 0);
        }
        self.buf[off..end].copy_from_slice(data);
    }

    /// Set the file length to exactly `len`: grow with zero-fill, or shrink by
    /// dropping the tail.
    pub fn truncate(&mut self, len: u64) {
        self.buf.resize(len as usize, 0);
    }

    /// Read up to `len` bytes starting at `off` from the writer's *current*
    /// contents (read-your-writes), clamped to the buffer — a read past the end
    /// returns only what exists, and a read fully past the end is empty.
    pub fn read_at(&self, off: u64, len: u32) -> Bytes {
        let off = off as usize;
        if off >= self.buf.len() {
            return Bytes::new();
        }
        let end = (off + len as usize).min(self.buf.len());
        Bytes::copy_from_slice(&self.buf[off..end])
    }

    /// The current logical length of the file in bytes.
    pub fn len(&self) -> u64 {
        self.buf.len() as u64
    }

    /// Whether the file is currently empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_at_zero_then_read_your_writes() {
        let mut w = FileWriter::from_buf(Vec::new());
        assert!(w.is_empty());
        w.write_at(0, b"hello");
        assert_eq!(w.len(), 5);
        assert_eq!(w.read_at(0, 5), Bytes::from_static(b"hello"));
    }

    #[test]
    fn write_at_gap_zero_fills_the_hole() {
        let mut w = FileWriter::from_buf(Vec::new());
        w.write_at(4, b"AB");
        assert_eq!(w.len(), 6);
        assert_eq!(w.read_at(0, 6), Bytes::from(vec![0, 0, 0, 0, b'A', b'B']));
    }

    #[test]
    fn write_at_overwrites_middle_in_place() {
        let mut w = FileWriter::from_buf(b"aaaaaaaa".to_vec());
        w.write_at(2, b"XY");
        assert_eq!(w.read_at(0, 8), Bytes::from_static(b"aaXYaaaa"));
        assert_eq!(w.len(), 8);
    }

    #[test]
    fn truncate_grows_with_zero_fill() {
        let mut w = FileWriter::from_buf(b"abc".to_vec());
        w.truncate(5);
        assert_eq!(w.read_at(0, 5), Bytes::from(vec![b'a', b'b', b'c', 0, 0]));
    }

    #[test]
    fn truncate_shrinks() {
        let mut w = FileWriter::from_buf(b"abcdef".to_vec());
        w.truncate(3);
        assert_eq!(w.len(), 3);
        assert_eq!(w.read_at(0, 8), Bytes::from_static(b"abc"));
    }

    #[test]
    fn read_at_past_eof_is_empty_and_clamped() {
        let w = FileWriter::from_buf(b"abc".to_vec());
        assert_eq!(w.read_at(10, 5), Bytes::new());
        // Partial overlap clamps to available bytes.
        assert_eq!(w.read_at(1, 100), Bytes::from_static(b"bc"));
    }
}
