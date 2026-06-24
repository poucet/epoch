//! [`TreeStore`] — the typed content-addressed store for tree objects.
//!
//! This is *not* a bespoke wrapper type: it is the kernel engine
//! [`SyncGenericCAS`](epoch_cas::SyncGenericCAS) specialised to the tree
//! [`codec`](crate::codec) and addressed by [`TreeId`] via the engine's typed
//! `Id` parameter. Storing a tree is therefore plain `encode → hash → zstd →
//! put`, with the resulting [`CASHash`](epoch_cas::CASHash) skinned as a
//! [`TreeId`] on the way out.
//!
//! Earlier this lived in a separate `epoch-tree-cas` crate whose entire job was
//! that id-wrapping; once the engine learned a typed id, the wrapper had no
//! logic left, so it collapsed into this one-line alias.
//!
//! It is generic over the **link** type `L` (see [`Link`](crate::Link)): use
//! [`EntryId<B>`](crate::EntryId) for a complete tree store and
//! [`Overlay<EntryId<B>>`](crate::Overlay) for a sparse-overlay tree store. Both
//! address by the same [`TreeId`]. Use it through the
//! [`SyncCAS<Tree<L>>`](epoch_cas::SyncCAS) trait:
//!
//! ```
//! use epoch_cas::{CASHash, MemoryByteBackend, SyncCAS};
//! use epoch_tree::{EntryId, Tree, TreeStore};
//!
//! // A complete-tree store; CASHash itself satisfies the `BlobRef` bound on B.
//! let store: TreeStore<MemoryByteBackend, EntryId<CASHash>> =
//!     TreeStore::new(MemoryByteBackend::new());
//! let id = store.put(Tree::<EntryId<CASHash>>::empty()).unwrap();
//! let got: Option<Tree<EntryId<CASHash>>> = store.get(&id).unwrap();
//! assert!(got.is_some());
//! ```

use epoch_cas::{Blake3Hasher, SyncGenericCAS};

use crate::{TreeCodec, TreeId};

/// A typed content-addressed store for [`Tree<L>`](crate::Tree) objects:
/// [`SyncGenericCAS`](epoch_cas::SyncGenericCAS) fixed to [`TreeCodec<L>`](crate::TreeCodec),
/// the default [`Blake3Hasher`](epoch_cas::Blake3Hasher), and [`TreeId`] as its
/// public id. `put` returns a [`TreeId`]; `get`/`has`/`delete` take one;
/// identical trees converge to the same id. Construct with `TreeStore::new(backend)`.
///
/// `L` is the [`Link`](crate::Link) type — [`EntryId<B>`](crate::EntryId) for a
/// complete tree, [`Overlay<EntryId<B>>`](crate::Overlay) for an overlay tree.
pub type TreeStore<Bk, L> = SyncGenericCAS<Bk, TreeCodec<L>, Blake3Hasher, TreeId>;
