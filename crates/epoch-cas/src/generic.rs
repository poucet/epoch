//! The generic CAS engine (D14): `encode → hash → zstd → backend.put`, reversed
//! and integrity-verified on read.
//!
//! `SyncGenericCAS<B, C, H, Id>` / `AsyncGenericCAS<B, C, H, Id>` implement
//! [`SyncCAS<V>`](crate::SyncCAS) / [`AsyncCAS<V>`](crate::AsyncCAS) over a byte
//! backend `B`, a [`Codec<V>`](crate::Codec) `C`, and a
//! [`ContentHasher`](crate::ContentHasher) `H` (default
//! [`Blake3Hasher`](crate::Blake3Hasher)).
//!
//! The engine always addresses the backend by the kernel [`CASHash`], but its
//! *public* id type is a fourth parameter `Id` (default [`CASHash`]). Any newtype
//! that is `Copy + From<CASHash> + Into<CASHash>` — e.g. a `TreeId` or `BlobId` —
//! can be plugged in so `put` returns, and `get`/`has`/`delete` accept, that typed
//! id instead of a bare hash, with no bespoke wrapper crate. The id is purely a
//! skin over the hash; the stored bytes and content-address are unchanged.

use std::marker::PhantomData;

use async_trait::async_trait;

use crate::backend::{AsyncByteBackend, SyncByteBackend};
use crate::cas::{AsyncCAS, SyncCAS};
use crate::codec::Codec;
use crate::compress::{compress, decompress};
use crate::error::CASError;
use crate::hash::CASHash;
use crate::hasher::{Blake3Hasher, ContentHasher};

/// Zero-sized variance marker for the engine's phantom parameters.
///
/// The engine stores *bytes*, never a `C`/`H`/`Id` value, so it owns none of
/// them; this `fn() -> …` marker makes it covariant in all three while staying
/// `Send`/`Sync`/`'static` regardless of what they are.
type Variance<C, H, Id> = PhantomData<fn() -> (C, H, Id)>;

/// Encode a value and derive its content-address + the compressed bytes ready
/// for the backend.
///
/// The id is `H::hash(canonical uncompressed encoding)`, so it is stable
/// regardless of compression level.
fn encode_for_store<V, C, H>(value: &V) -> Result<(CASHash, bytes::Bytes), CASError>
where
    C: Codec<V>,
    H: ContentHasher,
{
    let canonical = C::encode(value);
    let id = H::hash(&canonical);
    let compressed = compress(&canonical)?;
    Ok((id, compressed))
}

/// Decompress, verify the recomputed hash matches `id`, then decode.
fn decode_from_store<V, C, H>(id: &CASHash, compressed: &[u8]) -> Result<V, CASError>
where
    C: Codec<V>,
    H: ContentHasher,
{
    let canonical = decompress(compressed)?;
    let actual = H::hash(&canonical);
    if actual != *id {
        return Err(CASError::IntegrityMismatch {
            expected: *id,
            actual,
        });
    }
    C::decode(&canonical)
}

/// Synchronous generic CAS over a [`SyncByteBackend`] `B`, a [`Codec<V>`] `C`,
/// a [`ContentHasher`] `H` (default [`Blake3Hasher`]), and a public id type
/// `Id` (default [`CASHash`]); see the [module docs](self) for `Id`.
pub struct SyncGenericCAS<B, C, H = Blake3Hasher, Id = CASHash> {
    backend: B,
    _marker: Variance<C, H, Id>,
}

impl<B, C, H, Id> SyncGenericCAS<B, C, H, Id> {
    /// Build a CAS over `backend`.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            _marker: PhantomData,
        }
    }

    /// Borrow the underlying backend (test/inspection helper).
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<V, B, C, H, Id> SyncCAS<V> for SyncGenericCAS<B, C, H, Id>
where
    V: Send + Sync + 'static,
    B: SyncByteBackend,
    C: Codec<V>,
    H: ContentHasher,
    Id: Copy + From<CASHash> + Into<CASHash> + 'static,
{
    type Id = Id;

    fn put(&self, value: V) -> Result<Id, CASError> {
        let (hash, compressed) = encode_for_store::<V, C, H>(&value)?;
        // Content addressing: re-putting identical content is a harmless
        // overwrite of identical bytes, so no existence check is required.
        self.backend.put(&hash, compressed)?;
        Ok(Id::from(hash))
    }

    fn get(&self, id: &Id) -> Result<Option<V>, CASError> {
        let hash: CASHash = (*id).into();
        match self.backend.get(&hash)? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C, H>(&hash, &compressed)?)),
        }
    }

    fn has(&self, id: &Id) -> Result<bool, CASError> {
        let hash: CASHash = (*id).into();
        self.backend.has(&hash)
    }

    fn delete(&self, id: &Id) -> Result<(), CASError> {
        let hash: CASHash = (*id).into();
        self.backend.delete(&hash)
    }
}

/// Asynchronous generic CAS over an [`AsyncByteBackend`] `B`, a [`Codec<V>`]
/// `C`, a [`ContentHasher`] `H` (default [`Blake3Hasher`]), and a public id type
/// `Id` (default [`CASHash`]); see the [module docs](self) for `Id`.
pub struct AsyncGenericCAS<B, C, H = Blake3Hasher, Id = CASHash> {
    backend: B,
    _marker: Variance<C, H, Id>,
}

impl<B, C, H, Id> AsyncGenericCAS<B, C, H, Id> {
    /// Build a CAS over `backend`.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            _marker: PhantomData,
        }
    }

    /// Borrow the underlying backend (test/inspection helper).
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

#[async_trait]
impl<V, B, C, H, Id> AsyncCAS<V> for AsyncGenericCAS<B, C, H, Id>
where
    V: Send + Sync + 'static,
    B: AsyncByteBackend,
    C: Codec<V>,
    H: ContentHasher,
    Id: Copy + From<CASHash> + Into<CASHash> + Send + Sync + 'static,
{
    type Id = Id;

    async fn put(&self, value: V) -> Result<Id, CASError> {
        let (hash, compressed) = encode_for_store::<V, C, H>(&value)?;
        self.backend.put(&hash, compressed).await?;
        Ok(Id::from(hash))
    }

    async fn get(&self, id: &Id) -> Result<Option<V>, CASError> {
        let hash: CASHash = (*id).into();
        match self.backend.get(&hash).await? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C, H>(&hash, &compressed)?)),
        }
    }

    async fn has(&self, id: &Id) -> Result<bool, CASError> {
        let hash: CASHash = (*id).into();
        self.backend.has(&hash).await
    }

    async fn delete(&self, id: &Id) -> Result<(), CASError> {
        let hash: CASHash = (*id).into();
        self.backend.delete(&hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MemoryByteBackend;
    use crate::codec::IdentityCodec;
    use bytes::Bytes;

    /// A typed id newtype, standing in for a downstream `TreeId`/`BlobId` that
    /// plugs into the engine via `From<CASHash>` / `Into<CASHash>`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct TestId(CASHash);
    impl From<CASHash> for TestId {
        fn from(h: CASHash) -> Self {
            Self(h)
        }
    }
    impl From<TestId> for CASHash {
        fn from(id: TestId) -> Self {
            id.0
        }
    }

    type TypedCas = SyncGenericCAS<MemoryByteBackend, IdentityCodec, Blake3Hasher, TestId>;
    type PlainCas = SyncGenericCAS<MemoryByteBackend, IdentityCodec>;

    #[test]
    fn typed_id_put_get_has_delete_roundtrip() {
        let cas = TypedCas::new(MemoryByteBackend::new());

        // `put` hands back the *typed* id, not a bare hash.
        let id: TestId = cas.put(Bytes::from_static(b"hello")).unwrap();

        assert_eq!(cas.get(&id).unwrap(), Some(Bytes::from_static(b"hello")));
        assert!(<TypedCas as SyncCAS<Bytes>>::has(&cas, &id).unwrap());

        <TypedCas as SyncCAS<Bytes>>::delete(&cas, &id).unwrap();
        assert!(!<TypedCas as SyncCAS<Bytes>>::has(&cas, &id).unwrap());
        assert_eq!(cas.get(&id).unwrap(), None);
    }

    #[test]
    fn typed_id_is_the_skinned_hash() {
        // The typed id wraps exactly the kernel hash: a default (CASHash-id) CAS
        // over the *same* backend resolves content stored through the typed CAS,
        // addressed by the unwrapped hash.
        let backend = MemoryByteBackend::new();
        let typed = TypedCas::new(backend.clone());
        let plain = PlainCas::new(backend);

        let id: TestId = typed.put(Bytes::from_static(b"shared")).unwrap();
        let hash: CASHash = id.into();
        assert_eq!(plain.get(&hash).unwrap(), Some(Bytes::from_static(b"shared")));

        // The default still defaults its id to `CASHash`.
        let plain_id = plain.put(Bytes::from_static(b"x")).unwrap();
        let _assert_default_id: CASHash = plain_id;
    }
}
