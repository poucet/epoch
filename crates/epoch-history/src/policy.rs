//! [`CheckpointPolicy`] — the pure trigger state machine.
//!
//! Decides *when* to checkpoint from a stream of filesystem events and a
//! clock; it performs no IO and holds no locks, so it is table-testable. The
//! shape is **debounced-flush with a max-wait cap and a dirty-bytes
//! backstop** (deliberately ext4-`commit=N`-like, per the soft-fsync
//! contract):
//!
//! - Durability *signals* ([`Flush`](TriggerEvent::Flush),
//!   soft [`Fsync`](TriggerEvent::Fsync), [`Release`](TriggerEvent::Release))
//!   (re)arm a short debounce window, coalescing a burst of saves into one
//!   checkpoint…
//! - …but the deadline never slips past `first dirty + max_wait`, so a
//!   continuous stream of signals cannot starve the checkpoint.
//! - [`Wrote`](TriggerEvent::Wrote) accumulates dirty bytes; crossing
//!   `dirty_limit` forces [`Decision::Now`] even if the workload never
//!   flushes — bounding staged RAM.
//! - A **hard** fsync forces [`Decision::Now`]: the caller wants a synchronous
//!   durability barrier.
//!
//! Signals with nothing dirty are ignored: with no writes since the last
//! checkpoint there is nothing to make durable.

use std::time::{Duration, Instant};

/// A filesystem occurrence the policy reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    /// A file handle was flushed (`close(2)` path).
    Flush,
    /// An explicit `fsync(2)`; `hard` requests a synchronous barrier.
    Fsync {
        /// Whether the mount is configured to honor fsync synchronously.
        hard: bool,
    },
    /// A file handle was released.
    Release,
    /// `n` bytes were written into staging.
    Wrote(u64),
}

/// What the scheduler should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing pending.
    Idle,
    /// Checkpoint at (or after) this deadline unless re-decided first.
    At(Instant),
    /// Checkpoint immediately.
    Now,
}

/// The trigger state machine. See the [module docs](self).
#[derive(Debug)]
pub struct CheckpointPolicy {
    debounce: Duration,
    max_wait: Duration,
    dirty_limit: u64,
    dirty_bytes: u64,
    /// When the first un-checkpointed write landed.
    first_dirty: Option<Instant>,
    /// The currently-armed deadline, if any.
    deadline: Option<Instant>,
}

impl CheckpointPolicy {
    /// Default debounce window: coalesce a burst of saves (~5ms apart).
    pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(5);
    /// Default cap on how long a checkpoint can be debounced away.
    pub const DEFAULT_MAX_WAIT: Duration = Duration::from_millis(500);
    /// Default dirty-bytes backstop (64 MiB of staged writes).
    pub const DEFAULT_DIRTY_LIMIT: u64 = 64 * 1024 * 1024;

    /// A policy with explicit knobs.
    pub fn new(debounce: Duration, max_wait: Duration, dirty_limit: u64) -> CheckpointPolicy {
        CheckpointPolicy {
            debounce,
            max_wait,
            dirty_limit,
            dirty_bytes: 0,
            first_dirty: None,
            deadline: None,
        }
    }

    /// Feed one event; returns the (possibly unchanged) current decision.
    pub fn note(&mut self, event: TriggerEvent, now: Instant) -> Decision {
        match event {
            TriggerEvent::Wrote(n) => {
                self.dirty_bytes = self.dirty_bytes.saturating_add(n);
                self.first_dirty.get_or_insert(now);
                if self.dirty_bytes >= self.dirty_limit {
                    return Decision::Now;
                }
            }
            TriggerEvent::Fsync { hard: true } => {
                if self.dirty() {
                    return Decision::Now;
                }
            }
            TriggerEvent::Flush | TriggerEvent::Fsync { hard: false } | TriggerEvent::Release => {
                if self.dirty() {
                    // Debounce, clamped so signals can't starve the checkpoint.
                    let cap = self.first_dirty.map_or(now, |t| t + self.max_wait);
                    self.deadline = Some((now + self.debounce).min(cap));
                }
            }
        }
        self.decision(now)
    }

    /// Re-evaluate at `now` (the scheduler's timeout path).
    pub fn poll(&self, now: Instant) -> Decision {
        self.decision(now)
    }

    /// A checkpoint completed: everything staged up to now is durable.
    pub fn checkpointed(&mut self) {
        self.dirty_bytes = 0;
        self.first_dirty = None;
        self.deadline = None;
    }

    fn dirty(&self) -> bool {
        self.first_dirty.is_some()
    }

    fn decision(&self, now: Instant) -> Decision {
        match self.deadline {
            None => Decision::Idle,
            Some(deadline) if now >= deadline => Decision::Now,
            Some(deadline) => Decision::At(deadline),
        }
    }
}

impl Default for CheckpointPolicy {
    fn default() -> CheckpointPolicy {
        CheckpointPolicy::new(
            Self::DEFAULT_DEBOUNCE,
            Self::DEFAULT_MAX_WAIT,
            Self::DEFAULT_DIRTY_LIMIT,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CheckpointPolicy {
        CheckpointPolicy::new(Duration::from_millis(5), Duration::from_millis(50), 100)
    }

    #[test]
    fn signals_with_nothing_dirty_are_ignored() {
        let mut p = policy();
        let t = Instant::now();
        assert_eq!(p.note(TriggerEvent::Flush, t), Decision::Idle);
        assert_eq!(p.note(TriggerEvent::Fsync { hard: false }, t), Decision::Idle);
        assert_eq!(p.note(TriggerEvent::Fsync { hard: true }, t), Decision::Idle);
        assert_eq!(p.note(TriggerEvent::Release, t), Decision::Idle);
    }

    #[test]
    fn flush_after_write_arms_debounce_and_deadline_fires() {
        let mut p = policy();
        let t = Instant::now();
        assert_eq!(p.note(TriggerEvent::Wrote(1), t), Decision::Idle);
        let armed = p.note(TriggerEvent::Flush, t);
        assert_eq!(armed, Decision::At(t + Duration::from_millis(5)));

        // Before the deadline: still waiting. At it: fire.
        assert_eq!(p.poll(t + Duration::from_millis(4)), armed);
        assert_eq!(p.poll(t + Duration::from_millis(5)), Decision::Now);
    }

    #[test]
    fn repeated_flushes_extend_but_max_wait_clamps() {
        let mut p = policy();
        let t = Instant::now();
        let cap = t + Duration::from_millis(50);
        p.note(TriggerEvent::Wrote(1), t);
        // Keep flushing every 4ms — each extends the debounce, but the armed
        // deadline never slips past first_dirty + max_wait, so the flood
        // cannot starve the checkpoint.
        let mut now = t;
        let mut fired = None;
        for _ in 0..20 {
            now += Duration::from_millis(4);
            match p.note(TriggerEvent::Flush, now) {
                Decision::Now => {
                    fired = Some(now);
                    break;
                }
                Decision::At(deadline) => assert!(deadline <= cap),
                Decision::Idle => panic!("armed policy went idle"),
            }
        }
        assert!(fired.expect("cap forces a checkpoint") <= cap + Duration::from_millis(4));
    }

    #[test]
    fn dirty_bytes_backstop_fires_without_any_flush() {
        let mut p = policy();
        let t = Instant::now();
        assert_eq!(p.note(TriggerEvent::Wrote(99), t), Decision::Idle);
        assert_eq!(p.note(TriggerEvent::Wrote(1), t), Decision::Now);
    }

    #[test]
    fn hard_fsync_fires_immediately_when_dirty() {
        let mut p = policy();
        let t = Instant::now();
        p.note(TriggerEvent::Wrote(1), t);
        assert_eq!(p.note(TriggerEvent::Fsync { hard: true }, t), Decision::Now);
    }

    #[test]
    fn checkpointed_resets_everything() {
        let mut p = policy();
        let t = Instant::now();
        p.note(TriggerEvent::Wrote(60), t);
        p.note(TriggerEvent::Flush, t);
        p.checkpointed();
        assert_eq!(p.poll(t + Duration::from_secs(1)), Decision::Idle);
        // Dirty counter restarted: 60 more bytes don't trip the 100 limit.
        assert_eq!(p.note(TriggerEvent::Wrote(60), t), Decision::Idle);
    }
}
