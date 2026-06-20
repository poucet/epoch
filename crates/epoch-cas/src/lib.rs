//! # epoch-cas — the generic, id-agnostic Content-Addressed Storage kernel
//!
//! This crate is the foundation of the `epoch` storage stack.
//! It is **id-agnostic**: it is keyed by its own content-address [`CASHash`] and
//! depends on nothing internal. Domain id newtypes (`BlobId`/`TreeId`) are
//! layered on top in later crates.
//!
//! ## What's here
//!
//! - [`CASHash`] + [`CASError`] — the kernel's content-address and error model.
//! - [`Codec`] (+ [`IdentityCodec`]) — the canonical `value <-> bytes` mapping
//!   that determines convergence (D4).
//! - [`ContentHasher`] (+ [`Blake3Hasher`]) — the pluggable `bytes -> CASHash`
//!   digest the engine content-addresses with.
//! - [`SyncByteBackend`] / [`AsyncByteBackend`] — an opaque content-keyed KV
//!   with zero CAS semantics, plus the in-memory [`MemoryByteBackend`].
//! - [`SyncCAS`] / [`AsyncCAS`] — the generic CAS traits.
//! - [`SyncGenericCAS`] / [`AsyncGenericCAS`] — the engine: encode → hash →
//!   zstd → backend, integrity-verified on read.
//! - [`BatchingCAS`] — the write-coalescing decorator (D8).
//! - [`Blocking`] / [`Asyncify`] — sync ↔ async bridges.
//!
//! Notably absent (by design): `CachingCAS`/`moka` (lives in `epoch-cas-moka`),
//! `redb` (in `epoch-cas-redb`), and any domain id type.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod backend;
mod batching;
mod bridge;
mod cas;
mod codec;
mod compress;
mod error;
mod generic;
mod hash;
mod hasher;

pub use backend::{
    AsyncByteBackend, AsyncMemoryByteBackend, MemoryByteBackend, SyncByteBackend,
};
pub use batching::BatchingCAS;
pub use bridge::{Asyncify, Blocking};
pub use cas::{AsyncCAS, SyncCAS};
pub use codec::{Codec, IdentityCodec};
pub use compress::{compress, decompress};
pub use error::CASError;
pub use generic::{AsyncGenericCAS, SyncGenericCAS};
pub use hash::{CASHash, ParseHashError};
pub use hasher::{Blake3Hasher, ContentHasher};

// Re-export `bytes::Bytes` so downstream crates use the exact same type the
// kernel signatures speak (D5).
pub use bytes::Bytes;
