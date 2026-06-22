//! # epoch-tree — the content-addressed tree object (git-tree analog)
//!
//! This is the **definition** crate for the tree object and its own id newtype.
//! It is pure data + one canonical codec: it defines *what a tree is* and
//! *exactly how it serializes*, and nothing else. It depends only on the
//! id-agnostic kernel [`epoch_cas`] (plus `bytes`/`thiserror`) and knows nothing
//! of file-cas, tree-cas, inodes, hashers, or any runtime concern.
//!
//! ## Generic over the blob-id type
//!
//! A tree references two kinds of child: its own subtrees and file blobs. A
//! subtree is always addressed by *this crate's* concrete [`TreeId`]. A file
//! blob, however, is addressed by an id type that lives in a **separate** crate
//! (`epoch-file-cas`) — the tree must not depend on it. So the object model is
//! **generic over the foreign blob-id type `B`**: [`Tree<B>`], [`TreeEntry<B>`]
//! and [`EntryId<B>`] all carry `B`, and the only requirement on it is the
//! blanket-implemented [`BlobRef`] bound (a 32-byte, [`CASHash`](epoch_cas::CASHash)-convertible
//! id). The blob-id crate and this crate are tied together at a higher level,
//! neither depending on the other.
//!
//! ## Layers
//!
//! There are two layers in the stack and this crate sits squarely in the first:
//! the **content layer** (immutable, content-addressed objects — what this crate
//! defines) versus the **runtime layer** (mutable VFS/inode state, hashers,
//! storage engines — defined elsewhere). Keeping the object model in a tiny
//! dependency-light crate lets every higher layer agree on one canonical
//! encoding, which is what makes content-addresses converge.
//!
//! ## The object model
//!
//! - [`Tree`] — an ordered, name-unique set of [`TreeEntry`]s; the directory
//!   object. Its canonical [`TreeCodec`] encoding *defines* its hash.
//! - [`TreeEntry`] — one `(name, id, mode, size)` child record. Names are raw
//!   bytes (unix filenames are not guaranteed UTF-8).
//! - [`EntryId`] — the tagged child reference: `Blob(B)` for a foreign file
//!   blob, `Tree(`[`TreeId`]`)` for a subtree, or `Tombstone` for an overlay
//!   whiteout (see below).
//! - [`TreeId`] — a tree object's own content-address; a concrete newtype over
//!   the kernel's [`CASHash`](epoch_cas::CASHash).
//! - [`BlobRef`] — the bound a foreign blob-id type must meet to be referenced
//!   from a tree (so the file-cas crate satisfies it without depending here).
//!
//! **Symlinks** need no special entry kind: a symlink is a [blob](EntryId::Blob)
//! entry whose mode is [`SYMLINK_MODE`] and whose blob bytes are the link
//! target (git-style). See [`entry`] for the rationale.
//!
//! ## Trees as sparse overlay layers
//!
//! Besides being a complete directory listing, a [`Tree`] can serve as a
//! **sparse overlay layer** over a lower/base tree. The
//! [`Tombstone`](EntryId::Tombstone) entry kind is the whiteout marker for that
//! mode. Read against a base: an *absent* name falls through to the lower layer,
//! a tombstone *hides* the lower entry (deletion), and a blob/tree entry
//! *overrides* it. A tree with no tombstones, read with no base, is a plain
//! complete tree — the overlay model is fully backward compatible. See
//! [`entry`] for the detailed semantics and [`codec`] for the compact tombstone
//! wire encoding.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod entry;
pub mod error;
pub mod id;
pub mod tree;

pub use codec::{TreeCodec, TREE_FORMAT_VERSION};
pub use entry::{TreeEntry, SYMLINK_MODE};
pub use error::TreeError;
pub use id::{BlobRef, EntryId, TreeId};
pub use tree::Tree;
