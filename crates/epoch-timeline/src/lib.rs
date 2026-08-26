//! # epoch-timeline — the lineage DAG of checkpointed filesystem states
//!
//! A timeline is an append-only DAG of immutable [`TimelineEvent`]s, each
//! recording a checkpointed CAS root ([`TreeId`](epoch_tree::TreeId)) with its
//! parent event(s) and timestamp, plus a small mutable set of **named heads**
//! (`name → EventId`). Lineage lives in the *records*: forking is nothing more
//! than pointing a second head at an existing event and appending from there —
//! multiple agents can branch off the same base CAS with no engine support.
//!
//! ## Decoupling (the design rule)
//!
//! The timeline is deliberately **orthogonal to CAS storage**: this crate
//! defines only the trait contract ([`TimelineStore`]) and the canonical event
//! encoding; storage engines (redb, sqlite, a flat log, …) implement the trait
//! in their own crates and are invisible to callers. Events are *not* CAS
//! objects — but [`EventId`] is the blake3 hash of the canonical event
//! encoding, so ids are engine-independent, verifiable, and two stores can
//! exchange events without renumbering.
//!
//! ## Consistency with the object store
//!
//! There is no cross-store atomicity; the checkpoint layer upholds an ordering
//! rule instead: *the root's closure is durable before the event is appended,
//! and the event before the head advances*. A crash in between leaves orphaned
//! durable objects (harmless; retention/GC's problem) — never a reachable event
//! pointing at missing data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod event;
mod id;
mod memory;
mod store;

pub use event::{TimelineEvent, TIMELINE_EVENT_FORMAT_VERSION};
pub use id::EventId;
pub use memory::MemoryTimelineStore;
pub use store::{TimelineError, TimelineStore};
