//! Resource-cost capture (Chris's addition to the bench spec): for every
//! workload, wall clock is not enough — capture the workload's user+sys CPU
//! and the epoch daemon's CPU and peak RSS alongside.
//!
//! Two measurement paths, one per target mode:
//!
//! - **In-process mode** (the mount is a thread inside this binary): the
//!   workload side is this process too, so `RUSAGE_SELF` deltas cover both;
//!   we report the delta as "daemon+bench CPU" honestly labelled. Peak RSS is
//!   sampled from `ps` for our own pid.
//! - **Subprocess daemon mode** (`--daemon-pid <pid>` of a mounted
//!   `epoch-fuse`): workload CPU comes from `RUSAGE_CHILDREN` deltas around
//!   each spawned workload, daemon CPU from `ps -o utime=,stime=` deltas, and
//!   peak RSS from a sampler thread polling `ps -o rss=` every ~50 ms.
//!
//! macOS has no `/proc`; `ps` is the dependable unprivileged source. `ps`
//! time fields print `[[dd-]hh:]mm:ss` (optionally with `.cc`); [`parse_ps_time`]
//! handles all shapes and is unit-tested.

use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

/// CPU time (user + sys) of one rusage sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTime {
    pub user: Duration,
    pub sys: Duration,
}

impl CpuTime {
    pub fn total(self) -> Duration {
        self.user + self.sys
    }
}

impl std::ops::Sub for CpuTime {
    type Output = CpuTime;
    fn sub(self, rhs: CpuTime) -> CpuTime {
        CpuTime {
            user: self.user.saturating_sub(rhs.user),
            sys: self.sys.saturating_sub(rhs.sys),
        }
    }
}

fn rusage(who: libc::c_int) -> CpuTime {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(who, &mut ru) != 0 {
            return CpuTime::default();
        }
        CpuTime {
            user: Duration::new(ru.ru_utime.tv_sec as u64, (ru.ru_utime.tv_usec * 1000) as u32),
            sys: Duration::new(ru.ru_stime.tv_sec as u64, (ru.ru_stime.tv_usec * 1000) as u32),
        }
    }
}

/// CPU burned by this process's threads so far (in-process mode: mount
/// threads + measurement harness together).
pub fn self_cpu() -> CpuTime {
    rusage(libc::RUSAGE_SELF)
}

/// CPU burned by reaped children of this process so far (workload commands).
pub fn children_cpu() -> CpuTime {
    rusage(libc::RUSAGE_CHILDREN)
}

/// Parse `ps` time output: `SS`, `SS.cc`, `MM:SS(.cc)`, `HH:MM:SS(.cc)`,
/// `DD-HH:MM:SS(.cc)`. Returns total seconds (fractional).
pub fn parse_ps_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let mut parts: Vec<&str> = rest.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let secs = parts.pop()?.parse::<f64>().ok()?;
    let mins = if parts.is_empty() { 0.0 } else { parts.pop()?.parse::<f64>().ok()? };
    let hours = if parts.is_empty() { 0.0 } else { parts.pop()?.parse::<f64>().ok()? };
    Some(days * 86400.0 + hours * 3600.0 + mins * 60.0 + secs)
}

/// One `ps` observation of a pid: CPU times (seconds) and RSS (bytes).
#[derive(Clone, Copy, Debug)]
pub struct ProcStat {
    pub user_s: f64,
    pub sys_s: f64,
    pub rss_bytes: u64,
}

pub fn proc_stat(pid: u32) -> Option<ProcStat> {
    let out = Command::new("ps")
        .args(["-o", "utime=,stime=,rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?;
    let mut it = line.split_whitespace();
    let user_s = parse_ps_time(it.next()?)?;
    let sys_s = parse_ps_time(it.next()?)?;
    let rss_bytes = it.next()?.parse::<u64>().ok()? * 1024;
    Some(ProcStat { user_s, sys_s, rss_bytes })
}

/// Resource cost attached to every result row.
#[derive(Clone, Debug, Default)]
pub struct Resources {
    /// user+sys CPU of the workload itself (children in subprocess mode; the
    /// whole bench process in in-process mode, labelled as such).
    pub work: Option<CpuTime>,
    /// user+sys CPU of the epoch daemon over the same span.
    pub daemon: Option<CpuTime>,
    /// Peak RSS of the daemon (bytes) sampled over the span.
    pub daemon_peak_rss: Option<u64>,
    /// Daemon RSS at the start of the span — the RSS *delta* is what prices
    /// a workload (absolute peak includes everything before it).
    pub daemon_start_rss: Option<u64>,
    /// Total bytes the workload wrote — for "daemon RSS per GiB written"
    /// derivations (the §2.3 staging-tier hazard).
    pub bytes_written: Option<u64>,
}

impl Resources {
    pub fn work_cpu(&self) -> String {
        self.work
            .map(|c| crate::bench::fmt_dur(c.total()).trim().to_string())
            .unwrap_or_else(|| "—".into())
    }

    pub fn daemon_cpu(&self) -> String {
        self.daemon
            .map(|c| crate::bench::fmt_dur(c.total()).trim().to_string())
            .unwrap_or_else(|| "—".into())
    }

    pub fn peak_rss(&self) -> String {
        self.daemon_peak_rss
            .map(crate::bench::fmt_bytes)
            .unwrap_or_else(|| "—".into())
    }

    /// "x MiB RSS growth per GiB written" when both sides are known — the
    /// §2.3 in-memory-staging hazard, measured instead of asserted.
    pub fn rss_note(&self) -> Option<String> {
        let (peak, written) = (self.daemon_peak_rss?, self.bytes_written?);
        // Per-GiB growth is only meaningful at real volumes; a 256 KiB
        // workload would extrapolate noise.
        if written < (32 << 20) {
            return None;
        }
        let growth = peak.saturating_sub(self.daemon_start_rss.unwrap_or(0));
        let per_gib = growth as f64 / (written as f64 / (1u64 << 30) as f64);
        Some(format!(
            "+{} RSS per GiB written",
            crate::bench::fmt_bytes(per_gib as u64)
        ))
    }
}

/// Samples a pid's RSS on an interval and remembers the peak. Used for the
/// daemon (subprocess mode) and for the bench process itself (in-process).
pub struct RssSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl RssSampler {
    pub fn start(pid: u32, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let (stop2, peak2) = (Arc::clone(&stop), Arc::clone(&peak));
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                if let Some(stat) = proc_stat(pid) {
                    peak2.fetch_max(stat.rss_bytes, Ordering::Relaxed);
                }
                std::thread::sleep(interval);
            }
        });
        Self { stop, peak, handle: Some(handle) }
    }

    pub fn stop(mut self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let peak = self.peak.load(Ordering::Relaxed);
        (peak > 0).then_some(peak)
    }
}

/// Bracket one workload run with CPU + RSS capture.
///
/// - `daemon_pid: Some(pid)` → subprocess mode: `work` = children-rusage
///   delta (valid only when `f` is itself child-process work), `daemon` =
///   ps-sampled pid CPU delta, RSS from the sampler.
/// - `daemon_pid: None` → in-process mode: `work` = `RUSAGE_SELF` delta
///   (mount threads included — labelled in the row note), daemon CPU/RSS
///   left empty except self-RSS reported under `daemon_peak_rss` since the
///   daemon *is* this process.
pub fn capture(daemon_pid: Option<u32>, bytes_written: Option<u64>, f: impl FnOnce()) -> Resources {
    let self_before = self_cpu();
    let children_before = children_cpu();
    let daemon_before = daemon_pid.and_then(proc_stat);
    // In-process mode the daemon *is* this process: start RSS from ps too.
    let self_rss_before = if daemon_pid.is_none() {
        proc_stat(std::process::id()).map(|s| s.rss_bytes)
    } else {
        None
    };
    let sampler = daemon_pid
        .or_else(|| Some(std::process::id()))
        .map(|pid| RssSampler::start(pid, Duration::from_millis(50)));
    f();
    let resources = match daemon_pid {
        Some(pid) => {
            let after = daemon_before.and_then(|_| proc_stat(pid));
            Resources {
                work: Some(children_cpu() - children_before),
                daemon: after.zip(daemon_before).map(|(a, b)| CpuTime {
                    user: Duration::from_secs_f64((a.user_s - b.user_s).max(0.0)),
                    sys: Duration::from_secs_f64((a.sys_s - b.sys_s).max(0.0)),
                }),
                daemon_peak_rss: None,
                daemon_start_rss: daemon_before.map(|b| b.rss_bytes),
                bytes_written,
            }
        }
        None => Resources {
            work: Some(self_cpu() - self_before),
            daemon: None,
            daemon_peak_rss: None,
            daemon_start_rss: self_rss_before,
            bytes_written,
        },
    };
    let peak = sampler.and_then(RssSampler::stop);
    Resources { daemon_peak_rss: peak, ..resources }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_time_shapes() {
        assert_eq!(parse_ps_time("0:01.50"), Some(1.5));
        assert_eq!(parse_ps_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_ps_time("2-03:04:05"), Some(2.0 * 86400.0 + 3.0 * 3600.0 + 4.0 * 60.0 + 5.0));
        assert_eq!(parse_ps_time("42"), Some(42.0));
        assert_eq!(parse_ps_time("  0:03.25 "), Some(3.25));
        assert_eq!(parse_ps_time("garbage"), None);
        assert_eq!(parse_ps_time("1:2:3:4"), None);
    }

    #[test]
    fn rusage_monotonic_nonnegative() {
        let a = self_cpu();
        // Burn a little CPU.
        let mut x = 0u64;
        for i in 0..100_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let b = self_cpu();
        assert!(b.total() >= a.total());
    }

    #[test]
    fn proc_stat_of_self_has_rss() {
        let stat = proc_stat(std::process::id()).expect("ps on own pid");
        assert!(stat.rss_bytes > 0);
    }

    #[test]
    fn sampler_observes_self() {
        let sampler = RssSampler::start(std::process::id(), Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(30));
        let peak = sampler.stop().expect("peak observed");
        assert!(peak > 0);
    }
}
