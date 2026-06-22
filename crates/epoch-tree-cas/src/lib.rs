//! # epoch-tree-cas — the typed content-addressed store for tree objects
//!
//! This crate is a **thin facade** over the generic CAS engine
//! [`SyncGenericCAS`](epoch_cas::SyncGenericCAS), wired with
//! [`TreeCodec`](epoch_tree::TreeCodec) so it stores whole
//! [`Tree<B>`](epoch_tree::Tree) objects and addresses them by
//! [`TreeId`](epoch_tree::TreeId). It owns no storage, hashing, or encoding of
//! its own — all of that lives in [`epoch_cas`] (the engine) and
//! [`epoch_tree`] (the object model + canonical codec). The only thing this
//! crate adds is the *typing*: a [`TreeCas`] is a CAS whose `Id` is a
//! [`TreeId`], so a tree id can never be confused with a blob id (or a bare
//! [`CASHash`](epoch_cas::CASHash)) at compile time.
//!
//! ## Generic over the blob-id type
//!
//! Just like [`Tree<B>`](epoch_tree::Tree), [`TreeCas`] is **generic over the
//! foreign blob-id type `B`** (see [`BlobRef`](epoch_tree::BlobRef)). The store
//! is therefore agnostic of *which* crate's blob id its trees reference —
//! `epoch-file-cas`'s `BlobId`, a [`CASHash`](epoch_cas::CASHash) directly, or
//! any other 32-byte, hash-convertible newtype. The two sides are tied together
//! at a higher level, neither depending on the other.
//!
//! ## Convergence
//!
//! Identical trees converge to the same [`TreeId`](epoch_tree::TreeId): the
//! engine content-addresses the bytes produced by
//! [`TreeCodec`](epoch_tree::TreeCodec)'s canonical encoding, and that encoding
//! is deterministic regardless of how a tree was built (entries are stored in
//! canonical name-ascending order). Two trees with the same logical entries
//! hash to the same id; any difference yields a different id.
//!
//! ## Sync only
//!
//! Only the synchronous [`TreeCas`] exists today. A trivial async mirror over
//! [`AsyncGenericCAS`](epoch_cas::AsyncGenericCAS) is left as future work, in
//! the same spirit as `epoch-file-cas`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cas;

pub use cas::TreeCas;

// Re-export the tree object model from `epoch-tree` so callers can name the
// stored value and id types without a second dependency. `TreeId` is this
// store's id type; the rest let a caller build the `Tree<B>` values it stores.
pub use epoch_tree::{BlobRef, EntryId, Tree, TreeEntry, TreeId};
