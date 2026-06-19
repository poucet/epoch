//! The generic CAS traits (D14): sync and async variants over a value type `V`
//! with an associated `Id`.
//!
//! Both exist on purpose — the sync variant lets the synchronous FUSE hot path
//! talk to a local CAS with zero runtime involvement; the async variant
//! supports remote/cloud backends.

use async_trait::async_trait;

use crate::error::CASError;

/// Synchronous content-addressed store over values of type `V`.
///
/// `put` returns the content-address of the stored value; identical values
/// always yield the same `Id` (that is the whole point of CAS).
pub trait SyncCAS<V>: Send + Sync + 'static {
    /// The content-address type. The kernel engine uses [`crate::Hash`].
    type Id;

    /// Store `value`, returning its content-address.
    fn put(&self, value: V) -> Result<Self::Id, CASError>;
    /// Fetch the value addressed by `id`, if present.
    fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError>;
    /// Whether `id` is present.
    fn has(&self, id: &Self::Id) -> Result<bool, CASError>;
    /// Remove `id` (GC sweep). No error if absent.
    fn delete(&self, id: &Self::Id) -> Result<(), CASError>;
}

/// The async twin of [`SyncCAS`] for remote/cloud backends.
#[async_trait]
pub trait AsyncCAS<V>: Send + Sync + 'static {
    /// The content-address type. The kernel engine uses [`crate::Hash`].
    type Id;

    /// Store `value`, returning its content-address.
    async fn put(&self, value: V) -> Result<Self::Id, CASError>;
    /// Fetch the value addressed by `id`, if present.
    async fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError>;
    /// Whether `id` is present.
    async fn has(&self, id: &Self::Id) -> Result<bool, CASError>;
    /// Remove `id` (GC sweep).
    async fn delete(&self, id: &Self::Id) -> Result<(), CASError>;
}
