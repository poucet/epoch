//! [`HistoryError`] — the checkpoint layer's error type.

use epoch_cas::CASError;
use epoch_file_cas::BlobId;
use epoch_timeline::TimelineError;
use epoch_tree::TreeId;

/// Errors from closure walks and checkpoint commits.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// An object-store operation failed.
    #[error(transparent)]
    Cas(#[from] CASError),
    /// A timeline-store operation failed.
    #[error(transparent)]
    Timeline(#[from] TimelineError),
    /// A reachable tree object is absent from the store — the root being
    /// committed references data that was never written (or was lost).
    #[error("tree object missing from store: {0}")]
    MissingTree(TreeId),
    /// A reachable blob manifest is absent from the store.
    #[error("blob manifest missing from store: {0:?}")]
    MissingBlob(BlobId),
}
