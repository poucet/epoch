//! Sync ↔ async bridges (D14). Either world can consume the other, chosen at
//! composition time:
//!
//! - [`Blocking<A>`]: an [`AsyncCAS`] driven as a [`SyncCAS`] via a held
//!   `tokio::runtime::Handle` (`block_on`). Reaches a remote backend from the
//!   sync FUSE hot path.
//! - [`Asyncify<S>`]: a [`SyncCAS`] (in an `Arc`) exposed as an [`AsyncCAS`] via
//!   `spawn_blocking`. Lets async consumers (gRPC, background flush) drive a
//!   local sync CAS without stalling a runtime worker.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::runtime::Handle;

use crate::cas::{AsyncCAS, SyncCAS};
use crate::error::CASError;

/// Drives an [`AsyncCAS`] from synchronous code via `handle.block_on`.
///
/// Must NOT be constructed on a thread that is itself inside a tokio worker
/// (that would panic). It is meant for fuser threads, which are not runtime
/// threads.
pub struct Blocking<A> {
    inner: A,
    handle: Handle,
}

impl<A> Blocking<A> {
    /// Wrap `inner`, driving it on `handle`.
    pub fn new(inner: A, handle: Handle) -> Self {
        Self { inner, handle }
    }

    /// Borrow the wrapped async CAS.
    pub fn inner(&self) -> &A {
        &self.inner
    }
}

impl<V, A> SyncCAS<V> for Blocking<A>
where
    V: Send + Sync + 'static,
    A: AsyncCAS<V>,
    A::Id: Send + Sync + 'static,
{
    type Id = A::Id;

    fn put(&self, value: V) -> Result<Self::Id, CASError> {
        self.handle.block_on(self.inner.put(value))
    }

    fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError> {
        self.handle.block_on(self.inner.get(id))
    }

    fn has(&self, id: &Self::Id) -> Result<bool, CASError> {
        self.handle.block_on(self.inner.has(id))
    }

    fn delete(&self, id: &Self::Id) -> Result<(), CASError> {
        self.handle.block_on(self.inner.delete(id))
    }
}

/// Exposes a [`SyncCAS`] (held in an `Arc`) as an [`AsyncCAS`] via
/// `tokio::task::spawn_blocking`, so blocking work runs off the runtime's async
/// workers.
pub struct Asyncify<S> {
    inner: Arc<S>,
}

impl<S> Asyncify<S> {
    /// Wrap an `Arc<S>`.
    pub fn new(inner: Arc<S>) -> Self {
        Self { inner }
    }

    /// Clone the inner handle.
    pub fn inner(&self) -> Arc<S> {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl<V, S> AsyncCAS<V> for Asyncify<S>
where
    V: Send + Sync + 'static,
    S: SyncCAS<V>,
    // `Clone` is needed to move the id into the blocking closure; the kernel's
    // `Id = CASHash` is `Copy`, so this is free in practice.
    S::Id: Send + Sync + Clone + 'static,
{
    type Id = S::Id;

    async fn put(&self, value: V) -> Result<Self::Id, CASError> {
        let inner = Arc::clone(&self.inner);
        join(tokio::task::spawn_blocking(move || inner.put(value)).await)
    }

    async fn get(&self, id: &Self::Id) -> Result<Option<V>, CASError> {
        let inner = Arc::clone(&self.inner);
        let id = id.clone();
        join(tokio::task::spawn_blocking(move || inner.get(&id)).await)
    }

    async fn has(&self, id: &Self::Id) -> Result<bool, CASError> {
        let inner = Arc::clone(&self.inner);
        let id = id.clone();
        join(tokio::task::spawn_blocking(move || inner.has(&id)).await)
    }

    async fn delete(&self, id: &Self::Id) -> Result<(), CASError> {
        let inner = Arc::clone(&self.inner);
        let id = id.clone();
        join(tokio::task::spawn_blocking(move || inner.delete(&id)).await)
    }
}

/// Collapse a `spawn_blocking` `JoinError` into [`CASError::Shutdown`].
fn join<T>(res: Result<Result<T, CASError>, tokio::task::JoinError>) -> Result<T, CASError> {
    match res {
        Ok(inner) => inner,
        Err(_) => Err(CASError::Shutdown),
    }
}
