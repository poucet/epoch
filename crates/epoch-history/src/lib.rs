//! # epoch-history — the checkpoint layer tying the object tiers to a timeline
//!
//! This crate turns a sealed [`WorkingTree`](epoch_worktree) root into a
//! durable, time-addressable filesystem state:
//!
//! 1. [`pending_closure`] enumerates, in post-order, every CAS object reachable
//!    from a root [`TreeId`](epoch_tree::TreeId) that is not yet durable —
//!    trees, blob manifests, and chunks — pruning whole subtrees the durable
//!    tier already holds.
//! 2. [`History::commit_root`] promotes that closure into the durable tier in
//!    ordered batches, appends a [`TimelineEvent`](epoch_timeline::TimelineEvent)
//!    for the root, and advances the mount's head.
//!
//! ## The ordering rule (crash safety without cross-store atomicity)
//!
//! Object promotion, the event append, and the head advance are three separate
//! durable writes, strictly in that order — and the closure batches themselves
//! are flushed in post-order (children before the tree/manifest that references
//! them). At any crash point the durable tier is a *superset* of every
//! reachable state: an interrupted checkpoint leaves orphaned objects (a
//! retention/GC concern, self-healing on the next checkpoint since promotion
//! prunes on `far.has`), never a reachable event or tree pointing at missing
//! data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod closure;
mod error;
mod history;
pub mod inspect;
mod policy;
mod scheduler;

pub use closure::pending_closure;
pub use error::HistoryError;
pub use history::History;
pub use inspect::{
    AuditRecord, DiffEntry, InspectError, MaterializeStats, audit_log, diff_roots, materialize,
};
pub use policy::{CheckpointPolicy, Decision, TriggerEvent};
pub use scheduler::{CheckpointFn, Scheduler, SchedulerHandle};
