//! [`EventId`] — the content-address of a canonical [`TimelineEvent`] encoding.

use std::fmt;
use std::str::FromStr;

use epoch_cas::{CASHash, ParseHashError};

/// The identity of a [`TimelineEvent`](crate::TimelineEvent): the blake3 hash
/// of its canonical encoding.
///
/// Content-derived ids make the timeline engine-independent: any two
/// [`TimelineStore`](crate::TimelineStore) implementations agree on every id,
/// so events can be exchanged, merged, or re-indexed without renumbering, and a
/// fetched event can be verified against the id that named it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId(CASHash);

impl EventId {
    /// Wrap a raw content hash as an event id.
    pub fn from_cas_hash(hash: CASHash) -> EventId {
        EventId(hash)
    }

    /// The underlying content hash.
    pub fn as_cas_hash(&self) -> &CASHash {
        &self.0
    }

    /// Unwrap into the underlying content hash.
    pub fn into_cas_hash(self) -> CASHash {
        self.0
    }

    /// Lowercase hex form (64 chars).
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }
}

impl From<CASHash> for EventId {
    fn from(hash: CASHash) -> EventId {
        EventId(hash)
    }
}

impl From<EventId> for CASHash {
    fn from(id: EventId) -> CASHash {
        id.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EventId {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<EventId, ParseHashError> {
        Ok(EventId(s.parse()?))
    }
}
