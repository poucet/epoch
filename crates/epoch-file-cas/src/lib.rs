//! # epoch-file-cas — content-addressed storage for file content
//!
//! This crate is the file layer over the generic [`epoch-cas`](epoch_cas)
//! kernel. Where the kernel is id-agnostic and value-agnostic, this crate knows
//! one thing: **files**. It owns the domain id [`BlobId`] and an offset-aware
//! read/write path, and it depends only on `epoch-cas` (never on `epoch-tree` —
//! it is the lower layer).
//!
//! ## What's here
//!
//! - [`BlobId`] — the content-address of a stored file blob's manifest. Owned
//!   *here*, with two-way [`From`] conversions to/from the kernel's
//!   [`CASHash`](epoch_cas::CASHash) so a higher-level crate (`epoch-tree`) can
//!   use it as its blob-id type without this crate depending on that one.
//! - [`FileObject`] / [`ChunkRef`] — the on-disk manifest a `BlobId` addresses:
//!   either the bytes [`Inline`](FileObject::Inline) (small files) or an ordered
//!   list of chunk references ([`Chunked`](FileObject::Chunked)). The
//!   representation is an internal detail; the public API is just bytes.
//! - [`FileObjectCodec`] — the deterministic, tagged `FileObject <-> bytes`
//!   mapping that feeds the manifest's content hash.
//! - [`FileCas`] — the store: whole-file `put`/`get`, offset-aware
//!   [`read_at`](FileCas::read_at), `size`/`has`/`delete`, and the
//!   [`edit`](FileCas::edit)/[`commit`](FileCas::commit) write path. It also
//!   implements [`SyncCAS<Bytes>`](epoch_cas::SyncCAS) for generic interop.
//! - [`FileWriter`] — the offset-aware write buffer: per-write mutations do no
//!   hashing/CAS work; [`commit`](FileCas::commit) seals once.
//!
//! ## Representation-agnostic, with one chunking seam
//!
//! Inline vs. chunked is decided in a single place inside [`FileCas`] (the
//! `store_content` seam, with the `INLINE_MAX` / `CHUNK_SIZE` constants). v1
//! uses fixed-size splitting; adopting content-defined chunking (FastCDC) later
//! is a local change there — the `FileObject` format already supports N chunks
//! and the public API does not move.
//!
//! ## Sync now, async later
//!
//! The surface is synchronous, oriented at the FUSE hot path where a local CAS
//! must answer with no runtime involvement. An async twin mirroring the kernel's
//! [`AsyncCAS`](epoch_cas::AsyncCAS) is a future addition.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cas;
mod id;
mod object;
mod writer;

pub use cas::FileCas;
pub use id::BlobId;
pub use object::{ChunkRef, FileObject, FileObjectCodec};
pub use writer::FileWriter;
