//! The generic CAS engine (D14): `encode → hash → zstd → backend.put`, reversed
//! and integrity-verified on read.
//!
//! `SyncGenericCAS<B, C, H>` / `AsyncGenericCAS<B, C, H>` implement
//! [`SyncCAS<V>`](crate::SyncCAS) / [`AsyncCAS<V>`](crate::AsyncCAS) over a byte
//! backend `B`, a [`Codec<V>`](crate::Codec) `C`, and a
//! [`ContentHasher`](crate::ContentHasher) `H` (default
//! [`Blake3Hasher`](crate::Blake3Hasher)), with `Id = CASHash`.

use std::marker::PhantomData;

use async_trait::async_trait;

use crate::backend::{AsyncByteBackend, SyncByteBackend};
use crate::cas::{AsyncCAS, SyncCAS};
use crate::codec::Codec;
use crate::compress::{compress, decompress};
use crate::error::CASError;
use crate::hash::CASHash;
use crate::hasher::{Blake3Hasher, ContentHasher};

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
/// and a [`ContentHasher`] `H` (default [`Blake3Hasher`]).
pub struct SyncGenericCAS<B, C, H = Blake3Hasher> {
    backend: B,
    _marker: PhantomData<fn() -> (C, H)>,
}

impl<B, C, H> SyncGenericCAS<B, C, H> {
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

impl<V, B, C, H> SyncCAS<V> for SyncGenericCAS<B, C, H>
where
    V: Send + Sync + 'static,
    B: SyncByteBackend,
    C: Codec<V>,
    H: ContentHasher,
{
    type Id = CASHash;

    fn put(&self, value: V) -> Result<CASHash, CASError> {
        let (id, compressed) = encode_for_store::<V, C, H>(&value)?;
        // Content addressing: re-putting identical content is a harmless
        // overwrite of identical bytes, so no existence check is required.
        self.backend.put(&id, compressed)?;
        Ok(id)
    }

    fn get(&self, id: &CASHash) -> Result<Option<V>, CASError> {
        match self.backend.get(id)? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C, H>(id, &compressed)?)),
        }
    }

    fn has(&self, id: &CASHash) -> Result<bool, CASError> {
        self.backend.has(id)
    }

    fn delete(&self, id: &CASHash) -> Result<(), CASError> {
        self.backend.delete(id)
    }
}

/// Asynchronous generic CAS over an [`AsyncByteBackend`] `B`, a [`Codec<V>`]
/// `C`, and a [`ContentHasher`] `H` (default [`Blake3Hasher`]).
pub struct AsyncGenericCAS<B, C, H = Blake3Hasher> {
    backend: B,
    _marker: PhantomData<fn() -> (C, H)>,
}

impl<B, C, H> AsyncGenericCAS<B, C, H> {
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
impl<V, B, C, H> AsyncCAS<V> for AsyncGenericCAS<B, C, H>
where
    V: Send + Sync + 'static,
    B: AsyncByteBackend,
    C: Codec<V>,
    H: ContentHasher,
{
    type Id = CASHash;

    async fn put(&self, value: V) -> Result<CASHash, CASError> {
        let (id, compressed) = encode_for_store::<V, C, H>(&value)?;
        self.backend.put(&id, compressed).await?;
        Ok(id)
    }

    async fn get(&self, id: &CASHash) -> Result<Option<V>, CASError> {
        match self.backend.get(id).await? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C, H>(id, &compressed)?)),
        }
    }

    async fn has(&self, id: &CASHash) -> Result<bool, CASError> {
        self.backend.has(id).await
    }

    async fn delete(&self, id: &CASHash) -> Result<(), CASError> {
        self.backend.delete(id).await
    }
}
