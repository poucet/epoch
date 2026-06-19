//! `BatchingCAS<S>` (D8): a write-coalescing [`AsyncCAS`] decorator.
//!
//! Because puts are content-addressed, a `put` can compute `Id = Hash::of(...)`
//! **immediately** and hand the actual byte persistence to a bounded queue
//! drained by a background flush task. Reads, `has`, and `delete` pass
//! straight through to the inner store.
//!
//! Reading back a just-enqueued-but-not-yet-flushed value is the engine's job
//! (read-your-writes is served by the volatile HEAD, D8), not this decorator's
//! — so a `get` here only sees what the inner store has already persisted.

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::cas::AsyncCAS;
use crate::codec::Codec;
use crate::error::CASError;
use crate::hash::Hash;

/// A unit of work on the flush queue.
///
/// `value: Some(v)` persists `v`; `value: None` is a barrier — it persists
/// nothing but its ack still fires in FIFO order, so awaiting it proves every
/// prior write is durable. The id is computed synchronously before enqueue, so
/// callers already have it.
struct WriteJob<V> {
    value: Option<V>,
    /// Fires once this job is processed. Dropped if the flusher shuts down
    /// first (`recv` then yields [`CASError::Shutdown`], the D9 contract).
    done: oneshot::Sender<Result<(), CASError>>,
}

/// Write-coalescing decorator over an [`AsyncCAS<V>`] whose `Id = Hash`.
///
/// Generic over the same [`Codec`] the inner store uses, so it can derive the
/// content-address without delegating the (slow) persistence.
pub struct BatchingCAS<S, V, C> {
    inner: Arc<S>,
    tx: mpsc::Sender<WriteJob<V>>,
    /// Joined on [`BatchingCAS::shutdown`] to drain in-flight writes.
    flusher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    _codec: PhantomData<fn() -> C>,
}

impl<S, V, C> BatchingCAS<S, V, C>
where
    S: AsyncCAS<V, Id = Hash>,
    V: Send + Sync + 'static,
    C: Codec<V>,
{
    /// Wrap `inner` with a bounded queue of `capacity` pending writes and spawn
    /// the background flush task on the current tokio runtime.
    pub fn new(inner: Arc<S>, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<WriteJob<V>>(capacity.max(1));
        let flush_inner = Arc::clone(&inner);
        let flusher = tokio::spawn(async move {
            // Drain jobs until the channel closes (all senders dropped). Each
            // job is persisted in order; the result is reported to its waiter.
            while let Some(job) = rx.recv().await {
                let res = match job.value {
                    Some(value) => flush_inner.put(value).await.map(|_| ()),
                    None => Ok(()), // barrier: nothing to persist
                };
                // Receiver may have gone away (caller didn't wait); ignore.
                let _ = job.done.send(res);
            }
        });
        Self {
            inner,
            tx,
            flusher: Mutex::new(Some(flusher)),
            _codec: PhantomData,
        }
    }

    /// Borrow the wrapped store.
    pub fn inner(&self) -> &Arc<S> {
        &self.inner
    }

    /// Drain every already-enqueued write, then stop the flush task.
    ///
    /// Order matters: we await a [`flush`](Self::flush) first so in-flight
    /// writes become durable, *then* abort the (now-idle) flusher. Idempotent —
    /// a second call is a no-op.
    pub async fn shutdown(&self) {
        // Best-effort drain; if the flusher already died, the barrier returns
        // Shutdown and there is nothing left to wait for.
        let _ = self.flush().await;
        let handle = self.flusher.lock().await.take();
        if let Some(handle) = handle {
            // The queue is drained, so the flusher is parked on `recv`. We hold
            // `self.tx`, so it will never observe channel closure on its own —
            // abort it and join to reclaim the task.
            handle.abort();
            let _ = handle.await;
        }
    }
}

#[async_trait]
impl<S, V, C> AsyncCAS<V> for BatchingCAS<S, V, C>
where
    S: AsyncCAS<V, Id = Hash>,
    V: Send + Sync + 'static,
    C: Codec<V>,
{
    type Id = Hash;

    async fn put(&self, value: V) -> Result<Hash, CASError> {
        // Compute the content-address now — this is the whole point: the caller
        // gets the id immediately, persistence rides the queue.
        let id = Hash::of(&C::encode(&value));
        let (done, _ack) = oneshot::channel();
        let job = WriteJob {
            value: Some(value),
            done,
        };
        // Bounded send applies backpressure; a closed channel means the flusher
        // is gone → shutting down.
        self.tx
            .send(job)
            .await
            .map_err(|_| CASError::Shutdown)?;
        Ok(id)
    }

    async fn get(&self, id: &Hash) -> Result<Option<V>, CASError> {
        self.inner.get(id).await
    }

    async fn has(&self, id: &Hash) -> Result<bool, CASError> {
        self.inner.has(id).await
    }

    async fn delete(&self, id: &Hash) -> Result<(), CASError> {
        self.inner.delete(id).await
    }
}

impl<S, V, C> BatchingCAS<S, V, C>
where
    S: AsyncCAS<V, Id = Hash>,
    V: Send + Sync + 'static,
    C: Codec<V>,
{
    /// Block until every write enqueued *before this call* has been persisted
    /// by the flush task.
    ///
    /// Enqueues a barrier (a value-less job) and awaits its ack: because the
    /// flusher drains the FIFO queue in order, once the barrier's ack fires all
    /// prior writes are durable.
    pub async fn flush(&self) -> Result<(), CASError> {
        let (done, ack) = oneshot::channel();
        let job = WriteJob { value: None, done };
        self.tx.send(job).await.map_err(|_| CASError::Shutdown)?;
        // A dropped ack (flusher shut down before processing) surfaces as
        // Shutdown — the D9 contract.
        ack.await.map_err(|_| CASError::Shutdown)?
    }
}
