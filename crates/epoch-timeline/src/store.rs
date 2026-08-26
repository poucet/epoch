//! [`TimelineStore`] — the engine-agnostic trait contract for timeline
//! persistence, and [`TimelineError`].

use crate::event::TimelineEvent;
use crate::id::EventId;

/// Errors from timeline operations, engine-agnostic.
#[derive(Debug, thiserror::Error)]
pub enum TimelineError {
    /// A referenced event is not present in the store.
    #[error("event not found: {0}")]
    Missing(EventId),
    /// An appended event references a parent the store does not hold.
    #[error("parent event not found: {0}")]
    MissingParent(EventId),
    /// A stored event's bytes failed to decode (or re-verify).
    #[error("event codec error: {0}")]
    Codec(String),
    /// The underlying storage engine failed.
    #[error("timeline storage error: {0}")]
    Storage(String),
}

impl TimelineError {
    /// Wrap an engine error as [`TimelineError::Storage`].
    pub fn storage(err: impl std::fmt::Display) -> TimelineError {
        TimelineError::Storage(err.to_string())
    }
}

/// The persistence contract for a timeline: an append-only set of immutable
/// events plus a small mutable map of **named heads**.
///
/// Engines (redb, sqlite, a flat log, …) implement the three storage
/// primitives — `append`/`get` for events, the head map — in their own crates;
/// everything lineage-shaped ([`at`](Self::at), [`log`](Self::log)) has a
/// default implementation as a first-parent chain walk, which an engine may
/// override with an index (e.g. a per-head time table) without changing
/// callers.
///
/// ## Contract
///
/// - Events are immutable and content-addressed: `append` must store the
///   event's canonical encoding under [`TimelineEvent::id`] and is an
///   idempotent no-op for an already-present event.
/// - Every parent of an appended event must already be present
///   ([`TimelineError::MissingParent`] otherwise) — so reachable lineage is
///   never dangling.
/// - Heads are the only mutable state. One head per writer (agent, mount);
///   forking is `set_head("other", existing_id)`.
pub trait TimelineStore: Send + Sync {
    /// Store `event`, returning its content-derived id. Idempotent; errors
    /// with [`TimelineError::MissingParent`] if a parent is absent.
    fn append(&self, event: &TimelineEvent) -> Result<EventId, TimelineError>;

    /// Fetch an event by id, or `None` if absent.
    fn get(&self, id: &EventId) -> Result<Option<TimelineEvent>, TimelineError>;

    /// Point head `name` at `id` (creating the head if new). The event must be
    /// present ([`TimelineError::Missing`] otherwise).
    fn set_head(&self, name: &str, id: EventId) -> Result<(), TimelineError>;

    /// The event head `name` points at, or `None` for an unknown head.
    fn get_head(&self, name: &str) -> Result<Option<EventId>, TimelineError>;

    /// All heads, as `(name, event id)` pairs in unspecified order.
    fn list_heads(&self) -> Result<Vec<(String, EventId)>, TimelineError>;

    /// The newest event with `timestamp_ms <= ts_ms` on `head`'s first-parent
    /// chain, or `None` if the head is unknown or the whole chain is newer
    /// than `ts_ms`.
    ///
    /// Relies on timestamps being non-increasing while walking parents (the
    /// checkpoint layer keeps them strictly increasing along a chain).
    fn at(&self, head: &str, ts_ms: u64) -> Result<Option<EventId>, TimelineError> {
        let mut cursor = self.get_head(head)?;
        while let Some(id) = cursor {
            let event = self.get(&id)?.ok_or(TimelineError::Missing(id))?;
            if event.timestamp_ms <= ts_ms {
                return Ok(Some(id));
            }
            cursor = event.parents.first().copied();
        }
        Ok(None)
    }

    /// Up to `limit` events from `head` backwards along the first-parent
    /// chain, newest first. An unknown head yields an empty log.
    fn log(
        &self,
        head: &str,
        limit: usize,
    ) -> Result<Vec<(EventId, TimelineEvent)>, TimelineError> {
        let mut out = Vec::new();
        let mut cursor = self.get_head(head)?;
        while let Some(id) = cursor {
            if out.len() == limit {
                break;
            }
            let event = self.get(&id)?.ok_or(TimelineError::Missing(id))?;
            cursor = event.parents.first().copied();
            out.push((id, event));
        }
        Ok(out)
    }
}
