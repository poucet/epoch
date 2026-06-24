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
//! logic left, so it collapsed into this one-line alias. Use it through the
//! [`SyncCAS<Tree<B>>`](epoch_cas::SyncCAS) trait:
//!
//! ```
//! use epoch_cas::{CASHash, MemoryByteBackend, SyncCAS};
//! use epoch_tree::{Tree, TreeStore};
//!
//! // `B` is any blob-id type; `CASHash` itself satisfies the `BlobRef` bound.
//! let store: TreeStore<MemoryByteBackend, CASHash> = TreeStore::new(MemoryByteBackend::new());
//! let id = store.put(Tree::<CASHash>::empty()).unwrap();
//! let got: Option<Tree<CASHash>> = store.get(&id).unwrap();
//! assert!(got.is_some());
//! ```

use epoch_cas::{Blake3Hasher, SyncGenericCAS};

use crate::{TreeCodec, TreeId};

/// A typed content-addressed store for [`Tree<B>`](crate::Tree) objects:
/// [`SyncGenericCAS`](epoch_cas::SyncGenericCAS) fixed to [`TreeCodec<B>`](crate::TreeCodec),
/// the default [`Blake3Hasher`](epoch_cas::Blake3Hasher), and [`TreeId`] as its
/// public id. `put` returns a [`TreeId`]; `get`/`has`/`delete` take one;
/// identical trees converge to the same id. Construct with `TreeStore::new(backend)`.
pub type TreeStore<Bk, B> = SyncGenericCAS<Bk, TreeCodec<B>, Blake3Hasher, TreeId>;
