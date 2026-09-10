//! Benchmark runner core: measurements, status, ceilings, and reporting.
//!
//! Every number in the suite is reported **both** as an absolute duration and
//! as a ×bare-lower ratio, with the bare lower measured on the same machine in
//! the same run (EPOCH-FIXES §2.4). Ceilings are EPOCH-FIXES §2.2 verbatim; a
//! result that cannot run (known pre-existing breakage: rename on macFUSE 5.x,
//! symlink ENOSYS, …) is [`Status::KnownFail`]/[`Status::Skip`], never a crash.

use std::time::{Duration, Instant};

use crate::resource::Resources;

/// A pass/fail ceiling from EPOCH-FIXES §2.2, or an informational measure.
#[derive(Clone, Debug)]
pub enum Ceiling {
    /// Ratio vs the bare lower must not exceed this multiple.
    MaxRatio(f64),
    /// Absolute duration must not exceed this.
    MaxAbs(Duration),
    /// No ceiling — report only (e.g. bulk overlay writes, §2.3).
    None,
}

/// How a measurement ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Ran but violated its ceiling.
    Fail(String),
    /// Cannot pass today for a reason another lane owns (rename wire bug,
    /// symlink ENOSYS, …). Counted separately from real failures.
    KnownFail(String),
    /// Not applicable here (missing tool, wrong platform, in-process mode…).
    Skip(String),
}

/// One named measurement: wall clock plus resource cost (§2.2 ceilings +
/// Chris's resource-cost addition: workload CPU, daemon CPU, daemon peak RSS).
#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub iters: u64,
    pub wall: Option<Duration>,
    pub resources: Resources,
    pub ceiling: Ceiling,
    pub status: Status,
    pub note: String,
}

impl Row {
    pub fn ok(name: impl Into<String>, iters: u64, wall: Duration) -> Self {
        Self {
            name: name.into(),
            iters,
            wall: Some(wall),
            resources: Resources::default(),
            ceiling: Ceiling::None,
            status: Status::Ok,
            note: String::new(),
        }
    }

    pub fn skip(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut row = Self::ok(name, 0, Duration::ZERO);
        row.wall = None;
        row.status = Status::Skip(reason.into());
        row
    }

    pub fn known_fail(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut row = Self::ok(name, 0, Duration::ZERO);
        row.wall = None;
        row.status = Status::KnownFail(reason.into());
        row
    }

    pub fn fail(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut row = Self::ok(name, 0, Duration::ZERO);
        row.wall = None;
        row.status = Status::Fail(reason.into());
        row
    }

    pub fn with_ceiling(mut self, ceiling: Ceiling) -> Self {
        self.ceiling = ceiling;
        self
    }

    pub fn with_resources(mut self, resources: Resources) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    /// Evaluate the ceiling against the measured wall time. Ratios are
    /// evaluated by [`compare`], which has the bare baseline in hand.
    pub fn eval_abs_ceiling(&mut self) {
        if self.status != Status::Ok {
            return;
        }
        if let (Ceiling::MaxAbs(max), Some(wall)) = (&self.ceiling, self.wall)
            && wall > *max
        {
            self.status = Status::Fail(format!("{} > {}", fmt_dur(wall), fmt_dur(*max)));
        }
    }
}

/// A (bare, mount) pair of rows for the same workload: the reporting unit.
pub struct Pair {
    pub bare: Row,
    pub mount: Row,
}

impl Pair {
    pub fn new(bare: Row, mount: Row) -> Self {
        Self { bare, mount }
    }

    /// Ratio of mount wall to bare wall (None when either side didn't run).
    pub fn ratio(&self) -> Option<f64> {
        match (self.bare.wall, self.mount.wall) {
            (Some(b), Some(m)) if b > Duration::ZERO => Some(m.as_secs_f64() / b.as_secs_f64()),
            _ => None,
        }
    }

    /// Apply a ratio ceiling to the mount row (EPOCH-FIXES §2.2).
    pub fn eval_ratio_ceiling(&mut self, max: f64) {
        if self.mount.status != Status::Ok {
            return;
        }
        match self.ratio() {
            Some(r) if r > max => {
                self.mount.status = Status::Fail(format!("{r:.2}× > {max:.2}× ceiling"));
            }
            None => {
                self.mount.status =
                    Status::Skip("no bare baseline to ratio against".into());
            }
            _ => {}
        }
    }
}

/// Wall-clock timer. CPU/RSS resources are captured by [`crate::resource`].
pub struct Timer(Instant);

impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn stop(self) -> Duration {
        self.0.elapsed()
    }
}

/// Run `op` `iters` times inside one timed span; returns total wall.
pub fn time_iters(iters: u64, mut op: impl FnMut(u64)) -> Duration {
    let t = Timer::start();
    for i in 0..iters {
        op(i);
    }
    t.stop()
}

pub fn fmt_dur(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1_000.0 {
        format!("{us:7.1} µs")
    } else if us < 1_000_000.0 {
        format!("{:7.2} ms", us / 1e3)
    } else {
        format!("{:7.2} s ", us / 1e6)
    }
}

pub fn fmt_bytes(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

fn status_tag(s: &Status) -> String {
    match s {
        Status::Ok => " ok ".to_string(),
        Status::Fail(r) => format!("FAIL({r})"),
        Status::KnownFail(r) => format!("KNOWN-FAIL({r})"),
        Status::Skip(r) => format!("SKIP({r})"),
    }
}

/// Print the suite as a table: absolute bare, absolute mount, ×bare, the
/// §2.2 ceiling, the verdict, and the resource columns (Chris's addition).
pub fn report(title: &str, pairs: &[Pair]) -> (u64, u64, u64, u64) {
    println!("\n══ {title} ══");
    println!(
        "{:<34} {:>10} {:>10} {:>7}  {:<18} {:<12} {:<12} {:>10} verdict",
        "workload", "bare", "mount", "×bare", "ceiling", "work CPU u+s", "daemon CPU u+s", "daemon RSS"
    );
    let (mut ok, mut fail, mut known, mut skip) = (0, 0, 0, 0);
    for pair in pairs {
        let row = &pair.mount;
        let bare = pair
            .bare
            .wall
            .map(fmt_dur)
            .unwrap_or_else(|| "     —    ".into());
        let mount = row.wall.map(fmt_dur).unwrap_or_else(|| "     —    ".into());
        let ratio = pair
            .ratio()
            .map(|r| format!("{r:6.2}×"))
            .unwrap_or_else(|| "     —".into());
        let ceiling = match &row.ceiling {
            Ceiling::MaxRatio(r) => format!("≤ {r:.0}× bare"),
            Ceiling::MaxAbs(d) => format!("≤ {}", fmt_dur(*d).trim()),
            Ceiling::None => "report only".to_string(),
        };
        let mut note = row.note.clone();
        if pair.bare.status != Status::Ok {
            note = format!("[bare: {}] {note}", status_tag(&pair.bare.status));
        }
        if let Some(rss_note) = row.resources.rss_note() {
            note = if note.is_empty() {
                rss_note
            } else {
                format!("{note}; {rss_note}")
            };
        }
        match &row.status {
            Status::Ok => ok += 1,
            Status::Fail(_) => fail += 1,
            Status::KnownFail(_) => known += 1,
            Status::Skip(_) => skip += 1,
        }
        println!(
            "{:<34} {:>10} {:>10} {:>7}  {:<18} {:<12} {:<12} {:>10} {}{}",
            row.name,
            bare,
            mount,
            ratio,
            ceiling,
            row.resources.work_cpu(),
            row.resources.daemon_cpu(),
            row.resources.peak_rss(),
            status_tag(&row.status),
            if note.is_empty() {
                note
            } else {
                format!("  · {note}")
            },
        );
    }
    println!("── {ok} ok · {fail} FAIL · {known} known-fail · {skip} skipped");
    (ok, fail, known, skip)
}

/// Format the suite as a GitHub Flavored Markdown table comparing bare disk
/// vs epoch mount, suitable for GitHub Actions step summaries.
pub fn to_markdown(title: &str, pairs: &[Pair]) -> String {
    let mut out = String::new();
    out.push_str(&format!("### {title}\n\n"));
    out.push_str("| Workload | Bare Disk | Epoch Mount | ×Bare | Ceiling | Work CPU | Daemon CPU | Daemon RSS | Verdict | Note |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for pair in pairs {
        let row = &pair.mount;
        let bare = pair
            .bare
            .wall
            .map(fmt_dur)
            .unwrap_or_else(|| "—".into());
        let mount = row.wall.map(fmt_dur).unwrap_or_else(|| "—".into());
        let ratio = pair
            .ratio()
            .map(|r| format!("{r:.2}×"))
            .unwrap_or_else(|| "—".into());
        let ceiling = match &row.ceiling {
            Ceiling::MaxRatio(r) => format!("≤ {r:.0}× bare"),
            Ceiling::MaxAbs(d) => format!("≤ {}", fmt_dur(*d).trim()),
            Ceiling::None => "report only".to_string(),
        };
        let mut note = row.note.clone();
        if pair.bare.status != Status::Ok {
            note = format!("[bare: {}] {note}", status_tag(&pair.bare.status));
        }
        if let Some(rss_note) = row.resources.rss_note() {
            note = if note.is_empty() {
                rss_note
            } else {
                format!("{note}; {rss_note}")
            };
        }
        let status = match &row.status {
            Status::Ok => "✅ ok".to_string(),
            Status::Fail(r) => format!("❌ FAIL({r})"),
            Status::KnownFail(r) => format!("⚠️ KNOWN-FAIL({r})"),
            Status::Skip(r) => format!("⏭️ SKIP({r})"),
        };
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.name,
            bare.trim(),
            mount.trim(),
            ratio,
            ceiling,
            row.resources.work_cpu(),
            row.resources.daemon_cpu(),
            row.resources.peak_rss(),
            status,
            note.replace('|', "\\|"),
        ));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_is_mount_over_bare() {
        let bare = Row::ok("x", 1, Duration::from_millis(10));
        let mount = Row::ok("x", 1, Duration::from_millis(25));
        let pair = Pair::new(bare, mount);
        assert!((pair.ratio().unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn ratio_none_without_bare_baseline() {
        let mut bare = Row::ok("x", 1, Duration::ZERO);
        bare.wall = None;
        let mount = Row::ok("x", 1, Duration::from_millis(25));
        assert!(Pair::new(bare, mount).ratio().is_none());
    }

    #[test]
    fn ratio_ceiling_flags_violation() {
        let bare = Row::ok("x", 1, Duration::from_millis(10));
        let mount = Row::ok("x", 1, Duration::from_millis(35));
        let mut pair = Pair::new(bare, mount);
        pair.eval_ratio_ceiling(3.0);
        assert!(matches!(pair.mount.status, Status::Fail(_)));
        let bare = Row::ok("x", 1, Duration::from_millis(10));
        let mount = Row::ok("x", 1, Duration::from_millis(20));
        let mut pair = Pair::new(bare, mount);
        pair.eval_ratio_ceiling(3.0);
        assert_eq!(pair.mount.status, Status::Ok);
    }

    #[test]
    fn abs_ceiling_flags_violation() {
        let mut row = Row::ok("x", 1, Duration::from_millis(5))
            .with_ceiling(Ceiling::MaxAbs(Duration::from_millis(2)));
        row.eval_abs_ceiling();
        assert!(matches!(row.status, Status::Fail(_)));
    }

    #[test]
    fn ceilings_do_not_touch_known_fail() {
        let mut row = Row::known_fail("x", "rename broken");
        row.eval_abs_ceiling();
        assert!(matches!(row.status, Status::KnownFail(_)));
        let bare = Row::ok("x", 1, Duration::from_millis(10));
        let mut pair = Pair::new(bare, row);
        pair.eval_ratio_ceiling(1.0);
        assert!(matches!(pair.mount.status, Status::KnownFail(_)));
    }

    /// Best-of-`rounds` wall clock for a repeated closure (the research
    /// harness's methodology, research §3: best-of dampens scheduler noise).
    fn best_of(rounds: u32, mut f: impl FnMut() -> Duration) -> Duration {
        (0..rounds).map(|_| f()).min().unwrap_or(Duration::ZERO)
    }

    #[test]
    fn best_of_takes_minimum() {
        let mut durs = vec![
            Duration::from_millis(9),
            Duration::from_millis(3),
            Duration::from_millis(7),
        ]
        .into_iter();
        assert_eq!(best_of(3, || durs.next().unwrap()), Duration::from_millis(3));
    }

    #[test]
    fn dur_formatting_scales() {
        assert!(fmt_dur(Duration::from_micros(12)).contains("µs"));
        assert!(fmt_dur(Duration::from_millis(12)).contains("ms"));
        assert!(fmt_dur(Duration::from_secs(12)).contains('s'));
    }

    #[test]
    fn bytes_formatting_scales() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert!(fmt_bytes(5 * (1 << 20)).contains("MiB"));
        assert!(fmt_bytes(3 * (1 << 30)).contains("GiB"));
    }
}
