//! [`TreeCas`] — the typed content-addressed store for tree objects.
//!
//! A thin facade over the generic kernel engine
//! [`SyncGenericCAS`](epoch_cas::SyncGenericCAS), wired with
//! [`TreeCodec<B>`](epoch_tree::TreeCodec) so it stores [`Tree<B>`] values and
//! addresses them by [`TreeId`]. The engine speaks the raw kernel
//! [`CASHash`](epoch_cas::CASHash); this facade's *only* logic is wrapping that
//! hash into a [`TreeId`] on the way out and unwrapping it on the way in, so a
//! tree id can never be confused with a blob id (or a bare hash) at compile
//! time. Everything else delegates straight to `inner`.
//!
//! ## Type-parameter order: `TreeCas<Bk, B>`
//!
//! The backend comes first, mirroring
//! [`SyncGenericCAS<B, C, H>`](epoch_cas::SyncGenericCAS) where the byte backend
//! is the leading parameter; `B` is the foreign blob-id type the stored trees
//! reference (see [`BlobRef`](epoch_tree::BlobRef)). The codec is not a
//! parameter — it is fixed to [`TreeCodec<B>`](epoch_tree::TreeCodec), since the
//! whole point of this crate is the *typed* tree store.

use epoch_cas::{CASError, SyncByteBackend, SyncCAS, SyncGenericCAS};
use epoch_tree::{BlobRef, Tree, TreeCodec, TreeId};

/// A typed content-addressed store for [`Tree<B>`] objects.
///
/// `TreeCas<Bk, B>` is a thin facade over
/// [`SyncGenericCAS`](epoch_cas::SyncGenericCAS) parameterised with
/// [`TreeCodec<B>`](epoch_tree::TreeCodec). It is **sync only** (a trivial async
/// mirror over [`AsyncGenericCAS`](epoch_cas::AsyncGenericCAS) is left as future
/// work) and **generic over the foreign blob-id type `B`** — it stores and
/// returns whole [`Tree<B>`] values without caring which crate's blob id `B` is,
/// just as [`Tree<B>`] itself is.
///
/// Its id type is [`TreeId`]: identical trees converge to the same `TreeId`
/// because [`TreeCodec`](epoch_tree::TreeCodec)'s canonical encoding feeds the
/// kernel's content hash.
///
/// `B` carries the standard `Send + Sync` bounds the kernel engine requires of
/// any stored value; every real blob-id type (a 32-byte newtype) has them
/// trivially.
///
/// See the [module docs](self) for the `TreeCas<Bk, B>` parameter order.
pub struct TreeCas<Bk, B> {
    inner: SyncGenericCAS<Bk, TreeCodec<B>>,
}

impl<Bk, B> TreeCas<Bk, B>
where
    Bk: SyncByteBackend,
    B: BlobRef + Send + Sync + 'static,
{
    /// Build a tree CAS over `backend`.
    pub fn new(backend: Bk) -> Self {
        Self {
            inner: SyncGenericCAS::new(backend),
        }
    }

    /// Store `tree`, returning its [`TreeId`].
    ///
    /// Wraps the kernel [`CASHash`](epoch_cas::CASHash) the engine produces.
    pub fn put(&self, tree: Tree<B>) -> Result<TreeId, CASError> {
        let hash = self.inner.put(tree)?;
        Ok(TreeId::from_cas_hash(hash))
    }

    /// Fetch the [`Tree<B>`] addressed by `id`, if present.
    pub fn get(&self, id: &TreeId) -> Result<Option<Tree<B>>, CASError> {
        self.inner.get(id.as_cas_hash())
    }

    /// Whether `id` is present.
    pub fn has(&self, id: &TreeId) -> Result<bool, CASError> {
        self.inner.has(id.as_cas_hash())
    }

    /// Remove `id` (GC sweep). No error if absent.
    pub fn delete(&self, id: &TreeId) -> Result<(), CASError> {
        self.inner.delete(id.as_cas_hash())
    }

    /// Borrow the underlying backend (test/inspection helper).
    pub fn backend(&self) -> &Bk {
        self.inner.backend()
    }
}

/// A `TreeCas` is a first-class typed CAS with `Id = TreeId`, so it slots in
/// anywhere a [`SyncCAS<Tree<B>>`] is wanted while preventing a blob id (or a
/// bare hash) from being passed where a tree id is required. Delegates to the
/// inherent methods.
impl<Bk, B> SyncCAS<Tree<B>> for TreeCas<Bk, B>
where
    Bk: SyncByteBackend,
    B: BlobRef + Send + Sync + 'static,
{
    type Id = TreeId;

    fn put(&self, value: Tree<B>) -> Result<TreeId, CASError> {
        TreeCas::put(self, value)
    }

    fn get(&self, id: &TreeId) -> Result<Option<Tree<B>>, CASError> {
        TreeCas::get(self, id)
    }

    fn has(&self, id: &TreeId) -> Result<bool, CASError> {
        TreeCas::has(self, id)
    }

    fn delete(&self, id: &TreeId) -> Result<(), CASError> {
        TreeCas::delete(self, id)
    }
}
