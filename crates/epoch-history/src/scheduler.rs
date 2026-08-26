//! [`Scheduler`] — the background thread that turns [`CheckpointPolicy`]
//! decisions into checkpoint runs.
//!
//! The scheduler owns the policy and an injected checkpoint closure (in
//! practice: seal the working tree under its lock, then
//! [`History::commit_root`](crate::History::commit_root) off-lock). Filesystem
//! decorators send it [`TriggerEvent`]s through a cheap clonable
//! [`SchedulerHandle`]; a **hard fsync** uses
//! [`checkpoint_now`](SchedulerHandle::checkpoint_now) for a blocking,
//! synchronous barrier that reports the result to the caller.
//!
//! Background checkpoint failures have no caller to report to; they are logged
//! to stderr and the policy state is retained, so the next signal or deadline
//! retries.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use epoch_timeline::EventId;

use crate::error::HistoryError;
use crate::policy::{CheckpointPolicy, Decision, TriggerEvent};

/// The injected "do one checkpoint" operation.
pub type CheckpointFn = Box<dyn Fn() -> Result<Option<EventId>, HistoryError> + Send>;

/// How long the thread parks when the policy is idle (any event wakes it).
const IDLE_PARK: Duration = Duration::from_secs(3600);

enum Msg {
    Event(TriggerEvent),
    Sync(mpsc::Sender<Result<Option<EventId>, HistoryError>>),
    Shutdown,
}

/// A running checkpoint scheduler; dropping it shuts the thread down cleanly
/// (running one final checkpoint if work was pending).
pub struct Scheduler {
    tx: mpsc::Sender<Msg>,
    join: Option<JoinHandle<()>>,
}

/// Cheap clonable sender side for filesystem decorators.
#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<Msg>,
}

impl Scheduler {
    /// Start the background thread.
    pub fn spawn(policy: CheckpointPolicy, checkpoint: CheckpointFn) -> Scheduler {
        let (tx, rx) = mpsc::channel::<Msg>();
        let join = std::thread::Builder::new()
            .name("epoch-checkpoint".into())
            .spawn(move || run(policy, checkpoint, rx))
            .expect("spawn checkpoint scheduler thread");
        Scheduler {
            tx,
            join: Some(join),
        }
    }

    /// A handle for decorators to signal through.
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            tx: self.tx.clone(),
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl SchedulerHandle {
    /// Fire-and-forget an event (no-op if the scheduler is gone).
    pub fn signal(&self, event: TriggerEvent) {
        let _ = self.tx.send(Msg::Event(event));
    }

    /// Run a checkpoint synchronously (the hard-fsync barrier), returning its
    /// result. Errors with [`HistoryError::Timeline`]-free
    /// [`TimelineError::Storage`]-style wording if the scheduler is gone.
    pub fn checkpoint_now(&self) -> Result<Option<EventId>, HistoryError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Msg::Sync(reply_tx))
            .map_err(|_| scheduler_gone())?;
        reply_rx.recv().map_err(|_| scheduler_gone())?
    }
}

fn scheduler_gone() -> HistoryError {
    HistoryError::Timeline(epoch_timeline::TimelineError::Storage(
        "checkpoint scheduler is not running".to_string(),
    ))
}

fn run(mut policy: CheckpointPolicy, checkpoint: CheckpointFn, rx: mpsc::Receiver<Msg>) {
    let mut pending = false;
    loop {
        let now = Instant::now();
        let decision = policy.poll(now);
        let timeout = match decision {
            Decision::Now => Duration::ZERO,
            Decision::At(deadline) => deadline.saturating_duration_since(now),
            Decision::Idle => IDLE_PARK,
        };

        match rx.recv_timeout(timeout) {
            Ok(Msg::Event(event)) => {
                pending = true;
                if policy.note(event, Instant::now()) == Decision::Now {
                    run_checkpoint(&checkpoint, &mut policy, &mut pending);
                }
            }
            Ok(Msg::Sync(reply)) => {
                let result = checkpoint();
                if result.is_ok() {
                    policy.checkpointed();
                    pending = false;
                }
                let _ = reply.send(result);
            }
            Ok(Msg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {
                if policy.poll(Instant::now()) == Decision::Now {
                    run_checkpoint(&checkpoint, &mut policy, &mut pending);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Best-effort final checkpoint on clean shutdown, so an orderly unmount
    // never loses staged work to the debounce window.
    if pending {
        run_checkpoint(&checkpoint, &mut policy, &mut pending);
    }
}

fn run_checkpoint(checkpoint: &CheckpointFn, policy: &mut CheckpointPolicy, pending: &mut bool) {
    match checkpoint() {
        Ok(_) => {
            policy.checkpointed();
            *pending = false;
        }
        Err(err) => {
            // No caller to report to; keep the policy armed so the next
            // signal or deadline retries.
            eprintln!("[epoch-history] background checkpoint failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn counting(counter: Arc<AtomicUsize>) -> CheckpointFn {
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        })
    }

    fn wait_for(counter: &AtomicUsize, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::SeqCst) < want {
            assert!(Instant::now() < deadline, "timed out waiting for checkpoint");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn debounced_flush_coalesces_then_checkpoints() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::spawn(
            CheckpointPolicy::new(Duration::from_millis(5), Duration::from_millis(100), u64::MAX),
            counting(Arc::clone(&counter)),
        );
        let handle = scheduler.handle();
        // A burst of write+flush pairs inside one debounce window…
        for _ in 0..3 {
            handle.signal(TriggerEvent::Wrote(10));
            handle.signal(TriggerEvent::Flush);
        }
        wait_for(&counter, 1);
        // …coalesces into one checkpoint (allow the window to fully drain).
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dirty_backstop_checkpoints_without_flush() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::spawn(
            CheckpointPolicy::new(Duration::from_secs(10), Duration::from_secs(10), 100),
            counting(Arc::clone(&counter)),
        );
        scheduler.handle().signal(TriggerEvent::Wrote(200));
        wait_for(&counter, 1);
    }

    #[test]
    fn checkpoint_now_is_synchronous() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::spawn(CheckpointPolicy::default(), counting(Arc::clone(&counter)));
        assert_eq!(scheduler.handle().checkpoint_now().unwrap(), None);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shutdown_flushes_pending_work() {
        let counter = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::spawn(
            CheckpointPolicy::new(Duration::from_secs(10), Duration::from_secs(10), u64::MAX),
            counting(Arc::clone(&counter)),
        );
        scheduler.handle().signal(TriggerEvent::Wrote(1));
        // Give the thread a beat to ingest the event before shutdown.
        std::thread::sleep(Duration::from_millis(10));
        drop(scheduler);
        assert_eq!(counter.load(Ordering::SeqCst), 1, "final checkpoint on unmount");
    }
}
