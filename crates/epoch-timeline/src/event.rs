//! [`TimelineEvent`] — one checkpointed filesystem state, and the canonical
//! encoding that *defines* its [`EventId`].
//!
//! ## Byte layout (all integers little-endian, fixed width)
//!
//! ```text
//! [u8]        format version  (== TIMELINE_EVENT_FORMAT_VERSION, currently 1)
//! [u32 LE]    parent count
//! repeated:   [32 bytes] parent EventId raw hash bytes
//! [32 bytes]  root TreeId raw hash bytes
//! [u64 LE]    timestamp_ms (unix epoch milliseconds)
//! [u8]        meta present (0 = none, 1 = present)
//! if present: [u32 LE] meta length (n), [n bytes] UTF-8 meta
//! ```
//!
//! Determinism is non-negotiable: these bytes are what blake3 hashes to get the
//! [`EventId`], so two stores converge on ids iff they encode identically.
//! [`decode`](TimelineEvent::decode) is strictly validating (version, meta tag,
//! UTF-8 meta, no trailing bytes) so a decode → encode round-trip reproduces
//! the input byte-for-byte.

use epoch_cas::{Blake3Hasher, CASHash, ContentHasher};
use epoch_tree::TreeId;

use crate::id::EventId;
use crate::store::TimelineError;

/// The version byte stamped at the start of every encoded event. Bump only on
/// a breaking layout change.
pub const TIMELINE_EVENT_FORMAT_VERSION: u8 = 1;

/// Width of a raw hash field, in bytes.
const HASH_LEN: usize = 32;

/// One immutable record in the timeline DAG: a checkpointed CAS root, its
/// lineage, and when it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    /// Parent event(s). The first parent is the mainline: time queries walk the
    /// first-parent chain. Empty for a lineage's root event; more than one for
    /// a merge. Forks need nothing here — a fork is a second head pointing at
    /// an existing event.
    pub parents: Vec<EventId>,
    /// The checkpointed filesystem root. Its transitive closure must already be
    /// durable in the object store when this event becomes reachable (the
    /// ordering rule; see the crate docs).
    pub root: TreeId,
    /// Unix-epoch milliseconds. The checkpoint layer keeps this strictly
    /// increasing along a head's first-parent chain (clamping to `last + 1`
    /// across clock steps) so time queries can binary-branch on it.
    pub timestamp_ms: u64,
    /// Optional free-form annotation (author, trigger, message, …).
    pub meta: Option<String>,
}

impl TimelineEvent {
    /// The event's identity: blake3 over the canonical encoding.
    pub fn id(&self) -> EventId {
        EventId::from_cas_hash(Blake3Hasher::hash(&self.encode()))
    }

    /// The canonical byte encoding (see the [module docs](self) for the exact
    /// layout).
    pub fn encode(&self) -> Vec<u8> {
        let meta_len = self.meta.as_ref().map_or(0, |m| 4 + m.len());
        let mut buf =
            Vec::with_capacity(1 + 4 + self.parents.len() * HASH_LEN + HASH_LEN + 8 + 1 + meta_len);
        buf.push(TIMELINE_EVENT_FORMAT_VERSION);
        buf.extend_from_slice(&(self.parents.len() as u32).to_le_bytes());
        for parent in &self.parents {
            buf.extend_from_slice(parent.as_cas_hash().as_bytes());
        }
        buf.extend_from_slice(self.root.as_cas_hash().as_bytes());
        buf.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        match &self.meta {
            None => buf.push(0),
            Some(meta) => {
                buf.push(1);
                buf.extend_from_slice(&(meta.len() as u32).to_le_bytes());
                buf.extend_from_slice(meta.as_bytes());
            }
        }
        buf
    }

    /// Decode a canonical encoding, validating strictly.
    pub fn decode(bytes: &[u8]) -> Result<TimelineEvent, TimelineError> {
        let mut r = Reader { buf: bytes, pos: 0 };

        let version = r.u8()?;
        if version != TIMELINE_EVENT_FORMAT_VERSION {
            return Err(TimelineError::Codec(format!(
                "unsupported event format version: {version} (expected {TIMELINE_EVENT_FORMAT_VERSION})"
            )));
        }
        let parent_count = r.u32()? as usize;
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(EventId::from_cas_hash(CASHash::from_bytes(r.hash()?)));
        }
        let root = TreeId::from_cas_hash(CASHash::from_bytes(r.hash()?));
        let timestamp_ms = r.u64()?;
        let meta = match r.u8()? {
            0 => None,
            1 => {
                let len = r.u32()? as usize;
                let raw = r.take(len)?.to_vec();
                Some(String::from_utf8(raw).map_err(|_| {
                    TimelineError::Codec("event meta is not valid UTF-8".to_string())
                })?)
            }
            other => {
                return Err(TimelineError::Codec(format!(
                    "invalid meta-present tag: {other}"
                )));
            }
        };

        if r.pos != r.buf.len() {
            return Err(TimelineError::Codec(format!(
                "trailing {} byte(s) after event",
                r.buf.len() - r.pos
            )));
        }

        Ok(TimelineEvent {
            parents,
            root,
            timestamp_ms,
            meta,
        })
    }
}

/// A tiny bounds-checking cursor mapping any shortfall to a codec error.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], TimelineError> {
        let end = self.pos.checked_add(n).ok_or_else(|| {
            TimelineError::Codec("length overflow while decoding event".to_string())
        })?;
        if end > self.buf.len() {
            return Err(TimelineError::Codec(format!(
                "truncated event: needed {n} byte(s) at offset {}, have {}",
                self.pos,
                self.buf.len() - self.pos
            )));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, TimelineError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, TimelineError> {
        let b: [u8; 4] = self.take(4)?.try_into().expect("take yields 4 bytes");
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, TimelineError> {
        let b: [u8; 8] = self.take(8)?.try_into().expect("take yields 8 bytes");
        Ok(u64::from_le_bytes(b))
    }

    fn hash(&mut self) -> Result<[u8; HASH_LEN], TimelineError> {
        let b: [u8; HASH_LEN] = self
            .take(HASH_LEN)?
            .try_into()
            .expect("take yields HASH_LEN bytes");
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(seed: u8) -> TreeId {
        TreeId::from_cas_hash(CASHash::from_bytes([seed; 32]))
    }

    fn event(parents: Vec<EventId>, seed: u8, ts: u64, meta: Option<&str>) -> TimelineEvent {
        TimelineEvent {
            parents,
            root: tree(seed),
            timestamp_ms: ts,
            meta: meta.map(str::to_string),
        }
    }

    #[test]
    fn roundtrips_byte_stable() {
        let root = event(vec![], 1, 1000, None);
        let child = event(vec![root.id()], 2, 2000, Some("fsync"));
        let merge = event(vec![root.id(), child.id()], 3, 3000, Some(""));

        for e in [root, child, merge] {
            let enc = e.encode();
            let dec = TimelineEvent::decode(&enc).unwrap();
            assert_eq!(dec, e);
            assert_eq!(dec.encode(), enc, "re-encode is byte-identical");
        }
    }

    #[test]
    fn id_is_content_derived_and_sensitive_to_every_field() {
        let base = event(vec![], 1, 1000, None);
        assert_eq!(base.id(), base.clone().id(), "deterministic");

        let mut other = base.clone();
        other.timestamp_ms += 1;
        assert_ne!(base.id(), other.id(), "timestamp changes the id");

        let mut other = base.clone();
        other.root = tree(9);
        assert_ne!(base.id(), other.id(), "root changes the id");

        let mut other = base.clone();
        other.parents = vec![base.id()];
        assert_ne!(base.id(), other.id(), "parents change the id");

        let mut other = base.clone();
        other.meta = Some(String::new());
        assert_ne!(base.id(), other.id(), "empty meta differs from no meta");
    }

    #[test]
    fn decode_rejects_bad_version_trailing_and_truncation() {
        let enc = event(vec![], 1, 1, Some("m")).encode();

        let mut bad = enc.clone();
        bad[0] = 99;
        assert!(matches!(
            TimelineEvent::decode(&bad),
            Err(TimelineError::Codec(_))
        ));

        let mut trailing = enc.clone();
        trailing.push(0);
        assert!(matches!(
            TimelineEvent::decode(&trailing),
            Err(TimelineError::Codec(_))
        ));

        assert!(matches!(
            TimelineEvent::decode(&enc[..enc.len() - 1]),
            Err(TimelineError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_meta_tag_and_non_utf8_meta() {
        let mut enc = event(vec![], 1, 1, None).encode();
        *enc.last_mut().unwrap() = 2;
        assert!(matches!(
            TimelineEvent::decode(&enc),
            Err(TimelineError::Codec(_))
        ));

        let mut enc = event(vec![], 1, 1, Some("ab")).encode();
        let n = enc.len();
        enc[n - 2] = 0xff; // corrupt a meta byte into invalid UTF-8
        assert!(matches!(
            TimelineEvent::decode(&enc),
            Err(TimelineError::Codec(_))
        ));
    }
}
