//! Download progress in the style of `pacman -Syu`: a line per item in
//! progress (a file, or a map file being imaged), a total line below them,
//! and finished items printed above.
//!
//! ```text
//!  248 Ascalon City (r12345)   models 34/120     1.2 MiB  540.2 KiB/s 00:03 [#####---------------]  28%
//!  Total (12/300)                               45.3 MiB    2.1 MiB/s 12:34 [#-------------------]   4%
//! ```
//!
//! The lines are only drawn when stderr is a terminal; [`Board::println`]
//! and [`Board::eprintln`] print either way.

use std::collections::HashMap;
use std::fmt::{self, Display};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gw_nav::pathing::Progress;
use indicatif::style::ProgressTracker;
use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressState, ProgressStyle};

/// The name, then the step, bytes received, download rate, time and gauge.
const TEMPLATE: &str = " {wide_msg} {prefix:16!} {size:>10} {rate:>12} {time:>5} {gauge}";
/// How often the lines redraw while nothing changes (for the times and
/// rates).
const TICK: Duration = Duration::from_millis(250);
/// The gauge's bar width.
const GAUGE: usize = 20;

/// The progress lines of a command, with a total line if it counts items.
pub struct Board {
    multi: MultiProgress,
    total: Option<(ProgressBar, u64)>,
    /// Bytes received over all lines.
    received: Arc<AtomicU64>,
}

impl Board {
    /// A board without a total line.
    pub fn new() -> Self {
        Self { multi: MultiProgress::new(), total: None, received: Arc::default() }
    }

    /// A board counting `count` items on its total line.
    pub fn with_total(count: usize) -> Self {
        let mut board = Self::new();
        let count = count as u64;
        let bar = board.multi.add(ProgressBar::new(count));
        bar.set_style(style(&board.received, Time::Eta));
        bar.set_message(format!("Total (0/{count})"));
        bar.enable_steady_tick(TICK);
        board.total = Some((bar, count));
        board
    }

    /// A new line, above the total line. It is removed when dropped.
    pub fn line(&self) -> Line {
        let bar = ProgressBar::new(0);
        let bar = match &self.total {
            Some((total, _)) => self.multi.insert_before(total, bar),
            None => self.multi.add(bar),
        };
        let bytes = Arc::default();
        bar.set_style(style(&bytes, Time::Elapsed));
        bar.unset_length();
        bar.enable_steady_tick(TICK);
        Line {
            bar,
            bytes,
            received: self.received.clone(),
            downloads: HashMap::new(),
            step: String::new(),
            follow: true,
        }
    }

    /// Count `n` more items done on the total line.
    pub fn inc(&self, n: u64) {
        if let Some((bar, count)) = &self.total {
            bar.inc(n);
            bar.set_message(format!("Total ({}/{count})", bar.position()));
        }
    }

    /// Print to stdout, above the lines.
    pub fn println(&self, line: impl Display) {
        self.multi.suspend(|| println!("{line}"));
    }

    /// Print to stderr, above the lines.
    pub fn eprintln(&self, line: impl Display) {
        self.multi.suspend(|| eprintln!("{line}"));
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        if let Some((bar, _)) = &self.total {
            bar.finish_and_clear();
        }
    }
}

/// One line of a [`Board`]: an item's name, the step it is at, the bytes
/// downloaded for it and their rate, the time spent on it, and a gauge.
pub struct Line {
    bar: ProgressBar,
    /// Bytes received for the current item.
    bytes: Arc<AtomicU64>,
    /// The board's count.
    received: Arc<AtomicU64>,
    /// Bytes received so far of the downloads in progress, by file id.
    downloads: HashMap<u32, u32>,
    step: String,
    /// The gauge shows the bytes of the download in progress.
    follow: bool,
}

impl Line {
    /// Show item `name`, from the start. Until a step says otherwise, the
    /// gauge follows its downloads.
    pub fn start(&mut self, name: impl Into<String>) {
        self.bytes.store(0, Ordering::Relaxed);
        self.downloads.clear();
        self.set_step("", true);
        self.bar.unset_length();
        self.bar.set_message(name.into());
        self.bar.reset();
    }

    /// Show a [`gw_nav::PathingStore`] progress report, or the bytes of a
    /// download as [`Progress::Bytes`]. Missing files and failed renders
    /// are left to the caller.
    pub fn progress(&mut self, p: Progress) {
        match p {
            Progress::Connecting => self.no_gauge("connecting"),
            Progress::Manifest => self.set_step("asset manifest", true),
            Progress::Bytes { file_id, done, total } => {
                self.count_bytes(file_id, done, total);
                if self.follow {
                    self.gauge(done as u64, total as u64);
                }
            }
            Progress::Map(done, total) => {
                self.set_step("map file", true);
                self.gauge(done as u64, total as u64);
            }
            Progress::Models(done, total) => {
                self.set_step(&format!("models {done}/{total}"), false);
                self.gauge(done as u64, total as u64);
            }
            Progress::RenderFiles(done, total) => {
                self.set_step(&format!("textures {done}/{total}"), false);
                self.gauge(done as u64, total as u64);
            }
            Progress::Generating => self.no_gauge("generating"),
            Progress::Rendering => self.no_gauge("rendering"),
            Progress::MissingFile(_) | Progress::RenderFailed => {}
        }
    }

    fn set_step(&mut self, step: &str, follow: bool) {
        self.follow = follow;
        if self.step != step {
            step.clone_into(&mut self.step);
            self.bar.set_prefix(step.to_owned());
        }
    }

    fn no_gauge(&mut self, step: &str) {
        self.set_step(step, false);
        self.bar.unset_length();
    }

    fn gauge(&self, done: u64, total: u64) {
        self.bar.set_length(total);
        self.bar.set_position(done);
    }

    fn count_bytes(&mut self, file_id: u32, done: u32, total: u32) {
        let before = self.downloads.get(&file_id).copied().unwrap_or(0);
        // A retried download starts again from 0.
        let new = if done >= before { done - before } else { done };
        self.bytes.fetch_add(new as u64, Ordering::Relaxed);
        self.received.fetch_add(new as u64, Ordering::Relaxed);
        if done >= total {
            self.downloads.remove(&file_id);
        } else {
            self.downloads.insert(file_id, done);
        }
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

/// What a line's time column shows.
#[derive(Clone, Copy)]
enum Time {
    /// The time spent on the item.
    Elapsed,
    /// The time left, from the rate items finish at.
    Eta,
}

/// The [`TEMPLATE`] style, with the bytes received counted in `bytes`.
fn style(bytes: &Arc<AtomicU64>, time: Time) -> ProgressStyle {
    let size = {
        let bytes = bytes.clone();
        move |_: &ProgressState, w: &mut dyn fmt::Write| {
            let n = bytes.load(Ordering::Relaxed);
            if n > 0 {
                let _ = write!(w, "{}", HumanBytes(n));
            }
        }
    };
    let time = move |state: &ProgressState, w: &mut dyn fmt::Write| {
        let t = match time {
            Time::Elapsed => state.elapsed(),
            Time::Eta if state.pos() == 0 => return,
            Time::Eta => state.eta(),
        };
        let secs = t.as_secs();
        let _ = write!(w, "{:02}:{:02}", secs / 60, secs % 60);
    };
    ProgressStyle::with_template(TEMPLATE)
        .expect("valid template")
        .with_key("size", size)
        .with_key("rate", Rate { bytes: bytes.clone(), sample: None, rate: None })
        .with_key("time", time)
        .with_key("gauge", gauge)
}

/// `[####----]  50%` while the length is known, else as many spaces (to
/// keep the columns aligned).
fn gauge(state: &ProgressState, w: &mut dyn fmt::Write) {
    let Some(len) = state.len() else {
        let _ = write!(w, "{:1$}", "", GAUGE + 7);
        return;
    };
    let fraction = if len == 0 { 1.0 } else { (state.pos() as f64 / len as f64).min(1.0) };
    let filled = (fraction * GAUGE as f64) as usize;
    let _ = write!(w, "[{}{}] {:>3}%", "#".repeat(filled), "-".repeat(GAUGE - filled), (fraction * 100.0) as u32);
}

/// The current download rate: bytes received per second, sampled every
/// half second and smoothed. Blank until the first sample, and while no
/// bytes arrive.
#[derive(Clone)]
struct Rate {
    bytes: Arc<AtomicU64>,
    /// When the last sample was taken, and the count then.
    sample: Option<(Instant, u64)>,
    /// Bytes per second.
    rate: Option<f64>,
}

impl ProgressTracker for Rate {
    fn clone_box(&self) -> Box<dyn ProgressTracker> {
        Box::new(self.clone())
    }

    fn tick(&mut self, _: &ProgressState, now: Instant) {
        let bytes = self.bytes.load(Ordering::Relaxed);
        let Some((then, before)) = self.sample else {
            self.sample = Some((now, bytes));
            return;
        };
        let secs = now.duration_since(then).as_secs_f64();
        if secs >= 0.5 {
            // Blank while nothing is downloading (e.g. while rendering).
            let current = bytes.saturating_sub(before) as f64 / secs;
            self.rate = (current > 0.0).then(|| self.rate.map_or(current, |rate| rate + 0.5 * (current - rate)));
            self.sample = Some((now, bytes));
        }
    }

    fn reset(&mut self, _: &ProgressState, _: Instant) {
        (self.sample, self.rate) = (None, None);
    }

    fn write(&self, _: &ProgressState, w: &mut dyn fmt::Write) {
        if let Some(rate) = self.rate {
            let _ = write!(w, "{}/s", HumanBytes(rate as u64));
        }
    }
}
