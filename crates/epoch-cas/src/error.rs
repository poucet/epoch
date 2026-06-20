//! The kernel error model (D6): semantic variants callers branch on, plus an
//! opaque `Backend` escape hatch so the kernel never depends on a concrete
//! storage crate.

use crate::hash::CASHash;

/// Errors produced anywhere in the CAS kernel.
///
/// `Send + Sync + 'static`, so it crosses thread boundaries and lives behind
/// `Arc<dyn _>` decorator stacks.
#[derive(Debug, thiserror::Error)]
pub enum CASError {
    /// The requested id is not present in the backend.
    #[error("content not found")]
    NotFound,

    /// The bytes fetched for an id did not re-hash to that id — the stored
    /// content is corrupt or was tampered with.
    #[error("integrity mismatch: expected {expected}, got {actual}")]
    IntegrityMismatch {
        /// The id that was requested (and re-hashed against).
        expected: CASHash,
        /// The id the fetched bytes actually hashed to.
        actual: CASHash,
    },

    /// A background worker / channel was dropped — the component is shutting
    /// down and the operation cannot complete.
    #[error("CAS is shutting down")]
    Shutdown,

    /// A codec failed to encode or (more usually) decode a value.
    #[error("codec error: {0}")]
    Codec(String),

    /// zstd compression or decompression failed.
    #[error("compression error: {0}")]
    Compression(String),

    /// An opaque error from an underlying byte backend (redb, S3, …). The
    /// kernel never names the concrete backend type — backends convert with
    /// [`CASError::backend`].
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl CASError {
    /// Wrap an arbitrary backend error into the opaque [`CASError::Backend`]
    /// variant. Orphan-safe: the kernel needs no dependency on the backend.
    pub fn backend<E>(err: E) -> CASError
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        CASError::Backend(Box::new(err))
    }
}

// A static assertion that the error type satisfies the cross-thread bound.
const _: fn() = || {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<CASError>();
};
