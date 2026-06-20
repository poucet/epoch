//! The generic CAS engine (D14): `encode → hash → zstd → backend.put`, reversed
//! and integrity-verified on read.
//!
//! `SyncGenericCAS<B, C>` / `AsyncGenericCAS<B, C>` implement
//! [`SyncCAS<V>`](crate::SyncCAS) / [`AsyncCAS<V>`](crate::AsyncCAS) over a byte
//! backend `B` and a [`Codec<V>`](crate::Codec) `C`, with `Id = CASHash`.

use std::marker::PhantomData;

use async_trait::async_trait;

use crate::backend::{AsyncByteBackend, SyncByteBackend};
use crate::cas::{AsyncCAS, SyncCAS};
use crate::codec::Codec;
use crate::compress::{compress, decompress};
use crate::error::CASError;
use crate::hash::CASHash;

/// Encode a value and derive its content-address + the compressed bytes ready
/// for the backend.
///
/// The id is `BLAKE3(canonical uncompressed encoding)`, so it is stable
/// regardless of compression level.
fn encode_for_store<V, C: Codec<V>>(value: &V) -> Result<(CASHash, bytes::Bytes), CASError> {
    let canonical = C::encode(value);
    let id = CASHash::of(&canonical);
    let compressed = compress(&canonical)?;
    Ok((id, compressed))
}

/// Decompress, verify the recomputed hash matches `id`, then decode.
fn decode_from_store<V, C: Codec<V>>(
    id: &CASHash,
    compressed: &[u8],
) -> Result<V, CASError> {
    let canonical = decompress(compressed)?;
    let actual = CASHash::of(&canonical);
    if actual != *id {
        return Err(CASError::IntegrityMismatch {
            expected: *id,
            actual,
        });
    }
    C::decode(&canonical)
}

/// Synchronous generic CAS over a [`SyncByteBackend`] `B` and [`Codec<V>`] `C`.
pub struct SyncGenericCAS<B, C> {
    backend: B,
    _codec: PhantomData<fn() -> C>,
}

impl<B, C> SyncGenericCAS<B, C> {
    /// Build a CAS over `backend`.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            _codec: PhantomData,
        }
    }

    /// Borrow the underlying backend (test/inspection helper).
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

impl<V, B, C> SyncCAS<V> for SyncGenericCAS<B, C>
where
    V: Send + Sync + 'static,
    B: SyncByteBackend,
    C: Codec<V>,
{
    type Id = CASHash;

    fn put(&self, value: V) -> Result<CASHash, CASError> {
        let (id, compressed) = encode_for_store::<V, C>(&value)?;
        // Content addressing: re-putting identical content is a harmless
        // overwrite of identical bytes, so no existence check is required.
        self.backend.put(&id, compressed)?;
        Ok(id)
    }

    fn get(&self, id: &CASHash) -> Result<Option<V>, CASError> {
        match self.backend.get(id)? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C>(id, &compressed)?)),
        }
    }

    fn has(&self, id: &CASHash) -> Result<bool, CASError> {
        self.backend.has(id)
    }

    fn delete(&self, id: &CASHash) -> Result<(), CASError> {
        self.backend.delete(id)
    }
}

/// Asynchronous generic CAS over an [`AsyncByteBackend`] `B` and [`Codec<V>`] `C`.
pub struct AsyncGenericCAS<B, C> {
    backend: B,
    _codec: PhantomData<fn() -> C>,
}

impl<B, C> AsyncGenericCAS<B, C> {
    /// Build a CAS over `backend`.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            _codec: PhantomData,
        }
    }

    /// Borrow the underlying backend (test/inspection helper).
    pub fn backend(&self) -> &B {
        &self.backend
    }
}

#[async_trait]
impl<V, B, C> AsyncCAS<V> for AsyncGenericCAS<B, C>
where
    V: Send + Sync + 'static,
    B: AsyncByteBackend,
    C: Codec<V>,
{
    type Id = CASHash;

    async fn put(&self, value: V) -> Result<CASHash, CASError> {
        let (id, compressed) = encode_for_store::<V, C>(&value)?;
        self.backend.put(&id, compressed).await?;
        Ok(id)
    }

    async fn get(&self, id: &CASHash) -> Result<Option<V>, CASError> {
        match self.backend.get(id).await? {
            None => Ok(None),
            Some(compressed) => Ok(Some(decode_from_store::<V, C>(id, &compressed)?)),
        }
    }

    async fn has(&self, id: &CASHash) -> Result<bool, CASError> {
        self.backend.has(id).await
    }

    async fn delete(&self, id: &CASHash) -> Result<(), CASError> {
        self.backend.delete(id).await
    }
}
