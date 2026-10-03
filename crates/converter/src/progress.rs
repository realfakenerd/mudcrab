use serde::{Deserialize, Serialize};
use std::{fmt::Write as _, path::PathBuf, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStage {
    Discovering,
    Extracting,
    Database,
    Textures,
    Meshes,
    Scripts,
    LodChunks,
    Validating,
    Publishing,
    Complete,
}

/// How a fresh conversion shares its time between stages, in the order the stages run. The shares
/// are the measured costs of a real Skyrim SE conversion (242,969 assets in 5 h 7 m): textures were
/// about 4 h 30 m of it, extraction wrote 24.2 GB and rebuilt the ingestion cache, meshes took
/// about 3 minutes, validation re-read every artifact and hashed 18 GB, and the small stages are
/// what was left of the run, apportioned by the counts it logged.
///
/// The weights only turn a stage's own completion into a whole-run fraction. A reconversion that
/// reuses most outputs moves through the early stages far faster than these shares assume, so the
/// overall percentage runs ahead of the clock; that is right, because the run really is finishing
/// sooner.
const STAGE_WEIGHTS: [(ProgressStage, f32); 9] = [
    (ProgressStage::Discovering, 0.005),
    (ProgressStage::Extracting, 0.04),
    (ProgressStage::Database, 0.025),
    (ProgressStage::Meshes, 0.01),
    (ProgressStage::Textures, 0.85),
    (ProgressStage::Scripts, 0.005),
    // No calibrated LOD share yet; retain the measured weights and stage order.
    (ProgressStage::LodChunks, 0.0),
    (ProgressStage::Validating, 0.04),
    (ProgressStage::Publishing, 0.025),
];

impl ProgressStage {
    /// The share of a fresh conversion this stage takes.
    pub fn weight(self) -> f32 {
        STAGE_WEIGHTS
            .iter()
            .find(|(stage, _)| *stage == self)
            .map_or(0.0, |(_, weight)| *weight)
    }

    /// The share of a fresh conversion that finishes before this stage starts.
    pub fn offset(self) -> f32 {
        if self == ProgressStage::Complete {
            return 1.0;
        }
        STAGE_WEIGHTS
            .iter()
            .take_while(|(stage, _)| *stage != self)
            .map(|(_, weight)| weight)
            .sum()
    }
}

/// The whole-run completion of a stage that is `fraction` done, in `0.0..=1.0`.
pub fn overall_fraction(stage: ProgressStage, fraction: f32) -> f32 {
    (stage.offset() + stage.weight() * fraction.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

/// How one asset's conversion ended, for the events that report that end. Front ends read this
/// rather than the event's message, which is text for people and may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetOutcome {
    /// The asset failed and the run carried on without it; the manifest lists it as a failure.
    Skipped,
    /// The asset failed and the run stops because of it (`--fail-fast`).
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub stage: ProgressStage,
    pub completed: u64,
    pub total: u64,
    pub current_file: Option<PathBuf>,
    pub message: String,
    /// Bytes of input handled and expected, where the stage knows them cheaply: an archive's file
    /// table gives the sizes it will extract, and the conversion batches sum the source files they
    /// are about to read. `bytes_total` counts what is known at the time of the event, which during
    /// extraction means the archives parsed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_completed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    /// The stage's completion when `completed`/`total` understate it: extraction counts archives
    /// but works through them file by file. `None` means the counts are the whole story.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_fraction: Option<f32>,
    /// Whole-run completion in `0.0..=1.0`, from [`STAGE_WEIGHTS`].
    pub overall: f32,
    /// A one-off line for the person watching (a warning about one asset) rather than a progress
    /// update: it is printed on its own line, and a GUI can list it in a log pane.
    #[serde(default)]
    pub notice: bool,
    /// Set on the event that ends a failed asset's conversion; `None` on every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AssetOutcome>,
}

impl ProgressEvent {
    pub fn new(
        stage: ProgressStage,
        completed: u64,
        total: u64,
        current_file: Option<PathBuf>,
        message: &str,
    ) -> Self {
        let mut event = Self {
            stage,
            completed,
            total,
            current_file,
            message: message.to_owned(),
            bytes_completed: None,
            bytes_total: None,
            stage_fraction: None,
            overall: 0.0,
            notice: false,
            outcome: None,
        };
        event.refresh_overall();
        event
    }

    /// A line to print as it stands, rather than a progress update: the run has something to say
    /// about one asset (a dangling reference it pruned) and must not write over the status line.
    pub fn notice(stage: ProgressStage, current_file: Option<PathBuf>, message: &str) -> Self {
        let mut event = Self::new(stage, 0, 0, current_file, message);
        event.notice = true;
        event
    }

    /// Adds the bytes worked through so far and the bytes expected, where both are known.
    pub fn with_bytes(mut self, completed: u64, total: u64) -> Self {
        self.bytes_completed = Some(completed);
        self.bytes_total = Some(total);
        self.refresh_overall();
        self
    }

    /// Marks the event as the end of one asset's conversion, and how it ended.
    pub fn with_outcome(mut self, outcome: AssetOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// Whether this event reports an asset that failed, whether or not the run carried on.
    pub fn is_asset_failure(&self) -> bool {
        matches!(
            self.outcome,
            Some(AssetOutcome::Skipped | AssetOutcome::Failed)
        )
    }

    /// Overrides the stage fraction, for a stage whose units are not its files.
    pub fn with_stage_fraction(mut self, fraction: f32) -> Self {
        self.stage_fraction = Some(fraction.clamp(0.0, 1.0));
        self.refresh_overall();
        self
    }

    /// How many of the stage's items are done, against the total the stage reported.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.completed as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }

    /// How much of the stage's input is done, when the stage reported bytes.
    pub fn bytes_fraction(&self) -> Option<f32> {
        let (completed, total) = (self.bytes_completed?, self.bytes_total?);
        (total > 0).then(|| (completed as f32 / total as f32).clamp(0.0, 1.0))
    }

    /// The stage's completion: an explicit override, else bytes, else items.
    pub fn progress_fraction(&self) -> f32 {
        self.stage_fraction
            .or_else(|| self.bytes_fraction())
            .unwrap_or_else(|| self.fraction())
    }

    /// Whole-run completion, `0.0..=1.0`.
    pub fn overall(&self) -> f32 {
        self.overall
    }

    fn refresh_overall(&mut self) {
        self.overall = overall_fraction(self.stage, self.progress_fraction());
    }
}

/// The line `--verbose` prints for one event: the stage, how far through it the run is, what
/// happened, and the file it happened to.
fn verbose_line(event: &ProgressEvent) -> String {
    let mut line = format!("{:<11}", format!("{:?}", event.stage));
    if event.total > 0 {
        let _ = write!(line, " {}/{}", event.completed, event.total);
    }
    let _ = write!(line, "  {}", event.message);
    if let Some(file) = &event.current_file {
        let _ = write!(line, ": {}", file.display());
    }
    line
}

/// `line` put back on the terminal row at the start of `previous_width`, padded with spaces so
/// nothing of a longer line stays visible to the right of it. The padding replaces the
/// clear-to-end-of-line escape some consoles print literally.
fn redraw_text(line: &str, previous_width: usize) -> String {
    let padding = previous_width.saturating_sub(line.chars().count());
    format!("\r{line}{}", " ".repeat(padding))
}

/// `h:mm:ss.s`, for log lines that are read by eye and compared across runs.
pub fn format_elapsed(seconds: f64) -> String {
    let tenths = (seconds.max(0.0) * 10.0).round() as u64;
    let (hours, rest) = (tenths / 36_000, tenths % 36_000);
    let (minutes, rest) = (rest / 600, rest % 600);
    format!("{hours}:{minutes:02}:{:02}.{}", rest / 10, rest % 10)
}

/// `hh:mm:ss`, for the status line and the time-left estimate, where tenths are noise.
pub fn format_clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    let (hours, rest) = (seconds / 3600, seconds % 3600);
    let (minutes, seconds) = (rest / 60, rest % 60);
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// A byte count with a unit a person reads at a glance, in decimal units (what disk tools show).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A smoothed rate of a run: how much of the whole conversion is done per second, how many items
/// and bytes it is working through, and how long the rest should take.
///
/// The status line and a GUI show the same numbers, so the estimate is one thing here that both
/// read rather than a calculation each of them repeats. The caller owns the clock:
/// [`ProgressEstimate::observe`] is handed the elapsed time of the event it is given, which is what
/// lets the estimate be tested against a fake clock and keeps a run that is not moving from reading
/// as a fast one.
///
/// Samples arrive per event, which for a conversion is thousands of times a second, so a sample
/// closer than 250 ms to the last accepted one is folded into the next instead of reading as an
/// infinite rate. Accepted samples are folded in with a weight that decays over 20 s, so the
/// estimate reacts to a slowdown over tens of seconds rather than in one sample.
#[derive(Debug, Clone, Default)]
pub struct ProgressEstimate {
    /// The stage the samples belong to. A stage change starts the rate again, because one stage's
    /// pace says nothing about the next stage's.
    stage: Option<ProgressStage>,
    last: Option<Sample>,
    fraction_per_second: Option<f64>,
    items_per_second: Option<f64>,
    bytes_per_second: Option<f64>,
    samples: u32,
    /// The highest whole-run fraction seen so far. A stage can finish short of its total (a skipped
    /// archive, a failed asset), so the estimate holds its position rather than run backwards.
    overall: f32,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    elapsed: Duration,
    overall: f32,
    items: u64,
    bytes: u64,
}

impl ProgressEstimate {
    /// The gap below which a sample is folded into the next one.
    const MIN_GAP: Duration = Duration::from_millis(250);
    /// How long a sample stays influential: the weight of a new one decays over this.
    const TIME_CONSTANT: f64 = 20.0;
    /// Samples before the estimate is worth showing; an early conversion's rate is all ramp-up.
    const MIN_SAMPLES: u32 = 3;
    /// The longest guess worth putting on screen; beyond this the run is not being measured yet.
    const MAX_TIME_LEFT: Duration = Duration::from_secs(100 * 3600);

    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one progress event, `elapsed` after the run started, into the estimate.
    ///
    /// A notice is the run talking about one asset rather than moving forward, so it is ignored: it
    /// neither moves the overall fraction nor restarts the rate of the stage it came from.
    pub fn observe(&mut self, event: &ProgressEvent, elapsed: Duration) {
        if event.notice {
            return;
        }
        self.overall = self.overall.max(event.overall.clamp(0.0, 1.0));
        if self.stage != Some(event.stage) {
            self.stage = Some(event.stage);
            self.forget_samples();
        }
        let bytes = event.bytes_completed.unwrap_or(0);
        let Some(last) = self.last else {
            self.last = Some(Sample {
                elapsed,
                overall: self.overall,
                items: event.completed,
                bytes,
            });
            return;
        };
        let seconds = elapsed.saturating_sub(last.elapsed).as_secs_f64();
        if seconds < Self::MIN_GAP.as_secs_f64() {
            return;
        }
        let weight = 1.0 - (-seconds / Self::TIME_CONSTANT).exp();
        let fraction_rate = (self.overall - last.overall).max(0.0) as f64 / seconds;
        let item_rate = event.completed.saturating_sub(last.items) as f64 / seconds;
        let byte_rate = bytes.saturating_sub(last.bytes) as f64 / seconds;
        self.fraction_per_second = Some(smooth(self.fraction_per_second, fraction_rate, weight));
        self.items_per_second = Some(smooth(self.items_per_second, item_rate, weight));
        self.bytes_per_second = Some(smooth(self.bytes_per_second, byte_rate, weight));
        self.samples += 1;
        self.last = Some(Sample {
            elapsed,
            overall: self.overall,
            items: event.completed,
            bytes,
        });
    }

    /// Whole-run completion in `0.0..=1.0`. It never moves backwards, however the stages report
    /// themselves.
    pub fn overall(&self) -> f32 {
        self.overall
    }

    /// Items per second, once the run is moving fast enough for a rate to mean something.
    pub fn items_per_second(&self) -> Option<f64> {
        self.items_per_second.filter(|rate| *rate > 0.5)
    }

    /// Bytes of input per second, once the run is moving fast enough for a rate to mean something.
    pub fn bytes_per_second(&self) -> Option<f64> {
        self.bytes_per_second.filter(|rate| *rate > 1.0)
    }

    /// How much of the run is left, once enough of it has been measured to say: `None` before three
    /// samples have been accepted and five seconds have passed, once the run is complete, and
    /// whenever the run has not moved through the window, where no estimate is better than a guess.
    pub fn time_left(&self, elapsed: Duration) -> Option<Duration> {
        if self.samples < Self::MIN_SAMPLES || elapsed < Duration::from_secs(5) {
            return None;
        }
        if !(0.0..1.0).contains(&self.overall) || self.overall <= 0.0 {
            return None;
        }
        let rate = self.fraction_per_second?;
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        let seconds = (f64::from(1.0 - self.overall) / rate).round();
        (seconds.is_finite() && seconds <= Self::MAX_TIME_LEFT.as_secs_f64())
            .then(|| Duration::from_secs_f64(seconds))
    }

    /// Starts the rate again for a new stage, keeping the overall fraction where it is: the new
    /// stage picks up where the old one left off.
    fn forget_samples(&mut self) {
        self.last = None;
        self.fraction_per_second = None;
        self.items_per_second = None;
        self.bytes_per_second = None;
        self.samples = 0;
    }
}

fn smooth(previous: Option<f64>, sample: f64, weight: f64) -> f64 {
    match previous {
        Some(previous) => previous + weight * (sample - previous),
        None => sample,
    }
}

/// Turns progress events into what the converter should print.
///
/// On a terminal it keeps one line on screen and redraws it at most four times a second, printing
/// a run of finished lines only when the stage changes. Off a terminal (a log file, CI) it prints
/// one plain line every few seconds, and on every stage change, so a log stays readable and still
/// shows where a slow run was. `verbose` prints every event, one line per asset, as the converter
/// did before.
///
/// The renderer only decides *what* to print: the caller owns the clock and the stream. That is
/// what lets the throttling, the rate and the time-left estimate be tested against a fake clock.
pub struct ProgressRenderer {
    terminal: bool,
    verbose: bool,
    stage: Option<ProgressStage>,
    last_emit: Option<Duration>,
    /// The rates and the time-left estimate the line is drawn from, held by the renderer so the
    /// command line shows exactly the numbers a GUI reading its own [`ProgressEstimate`] sees.
    estimate: ProgressEstimate,
    /// Whether a redrawn status line is on screen without a newline after it.
    open_line: bool,
    /// How wide the line on screen is, so the next redraw can cover it. Legacy Windows consoles
    /// print a clear-to-end-of-line escape literally, so nothing here may rely on one.
    line_width: usize,
    /// The event the status line was last drawn from, so a timer can redraw it with fresh elapsed
    /// time and a fresh time-left estimate between events.
    last_event: Option<ProgressEvent>,
}

impl ProgressRenderer {
    /// How often a terminal redraw is due. The caller drives this with its own clock, so the
    /// renderer can be tested without one.
    pub const TERMINAL_REFRESH: Duration = Duration::from_millis(250);
    const LOG_REFRESH: Duration = Duration::from_secs(5);

    pub fn new(terminal: bool, verbose: bool) -> Self {
        Self {
            terminal,
            verbose,
            stage: None,
            last_emit: None,
            estimate: ProgressEstimate::default(),
            open_line: false,
            line_width: 0,
            last_event: None,
        }
    }

    /// What to write for this event, or `None` when it is not time to print. A terminal redraw
    /// starts with a carriage return and is padded to cover the line it replaces.
    pub fn update(&mut self, event: &ProgressEvent, elapsed: Duration) -> Option<String> {
        if event.notice {
            return Some(self.notice(event, elapsed));
        }

        let stage_changed = self.stage != Some(event.stage);
        self.estimate.observe(event, elapsed);
        // Kept even when this event isn't printed, so a timer redraw shows the latest progress.
        self.last_event = Some(event.clone());

        let refresh = if self.terminal {
            Self::TERMINAL_REFRESH
        } else {
            Self::LOG_REFRESH
        };
        let due = self
            .last_emit
            .is_none_or(|last| elapsed.saturating_sub(last) >= refresh);
        if !self.verbose && !stage_changed && !due {
            return None;
        }

        self.stage = Some(event.stage);
        self.last_emit = Some(elapsed);
        if self.verbose {
            self.open_line = false;
            return Some(format!(
                "[{}] {}
",
                format_elapsed(elapsed.as_secs_f64()),
                verbose_line(event)
            ));
        }
        let line = self.line(event, elapsed);
        if !self.terminal {
            self.open_line = false;
            Some(format!(
                "[{}] {line}\n",
                format_elapsed(elapsed.as_secs_f64())
            ))
        } else {
            // A stage change ends the open row first, so the finished stage's last line stays in
            // the scrollback, and the new stage starts an open row of its own.
            let end_previous = if stage_changed && self.open_line {
                "\n"
            } else {
                ""
            };
            if stage_changed {
                self.line_width = 0;
            }
            self.open_line = true;
            Some(format!("{end_previous}{}", self.draw(&line)))
        }
    }

    /// Redraws the status line between events, so the elapsed time and the time-left estimate keep
    /// moving during a long asset. Nothing is printed off a terminal, where a line per few seconds
    /// already carries that.
    pub fn tick(&mut self, elapsed: Duration) -> Option<String> {
        if self.verbose || !self.terminal {
            return None;
        }
        let event = self.last_event.clone()?;
        let due = self
            .last_emit
            .is_none_or(|last| elapsed.saturating_sub(last) >= Self::TERMINAL_REFRESH);
        if !due {
            return None;
        }
        self.last_emit = Some(elapsed);
        let line = self.line(&event, elapsed);
        let text = self.draw(&line);
        // The redraw leaves the row open again, so a notice that follows ends it first.
        self.open_line = true;
        Some(text)
    }

    /// Ends the status line, so the caller can print a summary without printing over it.
    pub fn finish(&mut self) -> Option<String> {
        if self.open_line {
            self.open_line = false;
            Some("\n".to_owned())
        } else {
            None
        }
    }

    /// Puts `line` on the current terminal row, covering the line it replaces.
    fn draw(&mut self, line: &str) -> String {
        let text = redraw_text(line, self.line_width);
        self.line_width = self.line_width.max(line.chars().count());
        text
    }

    /// A line the run wants the person to see as it stands. On a terminal it ends whatever status
    /// line is open first, so a warning never splices into it.
    fn notice(&mut self, event: &ProgressEvent, elapsed: Duration) -> String {
        let line = format!(
            "[{}] {}",
            format_elapsed(elapsed.as_secs_f64()),
            event.message
        );
        if self.verbose || !self.terminal {
            return format!("{line}\n");
        }
        let prefix = if self.open_line { "\n" } else { "" };
        self.open_line = false;
        self.line_width = 0;
        format!("{prefix}{line}\n")
    }

    fn line(&self, event: &ProgressEvent, elapsed: Duration) -> String {
        let mut line = format!(
            "{:<11} {:>3.0}%  [overall {:>3.0}%]",
            format!("{:?}", event.stage),
            event.progress_fraction() * 100.0,
            self.estimate.overall() * 100.0,
        );
        if let Some(rate) = self.estimate.items_per_second() {
            let _ = write!(line, "  {rate:.0} items/s");
        }
        if let Some(rate) = self.estimate.bytes_per_second() {
            let _ = write!(line, "  {:.1} MB/s", rate / 1_000_000.0);
        }
        let _ = write!(line, "  {} elapsed", format_clock(elapsed.as_secs_f64()));
        if let Some(left) = self.estimate.time_left(elapsed) {
            let _ = write!(line, "  ~{} left", format_clock(left.as_secs_f64()));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn event(stage: ProgressStage, completed: u64, total: u64) -> ProgressEvent {
        ProgressEvent::new(stage, completed, total, None, "test")
    }

    #[test]
    fn stage_weights_cover_a_fresh_run_in_run_order() {
        let total: f32 = STAGE_WEIGHTS.iter().map(|(_, weight)| *weight).sum();
        assert!(
            (total - 1.0).abs() < 1e-6,
            "stage weights sum to {total}, not 1"
        );
        assert_eq!(ProgressStage::Complete.weight(), 0.0);
        assert_eq!(ProgressStage::Complete.offset(), 1.0);
        // The table is written in run order: each stage starts where the one before it ended.
        let order = [
            ProgressStage::Discovering,
            ProgressStage::Extracting,
            ProgressStage::Database,
            ProgressStage::Meshes,
            ProgressStage::Textures,
            ProgressStage::Scripts,
            ProgressStage::LodChunks,
            ProgressStage::Validating,
            ProgressStage::Publishing,
        ];
        for pair in order.windows(2) {
            assert!(
                (overall_fraction(pair[0], 1.0) - overall_fraction(pair[1], 0.0)).abs() < 1e-6,
                "{:?} does not follow {:?}",
                pair[1],
                pair[0]
            );
        }
    }

    #[test]
    fn overall_fraction_never_moves_backwards_across_stages() {
        let mut previous = 0.0;
        let order = [
            ProgressStage::Discovering,
            ProgressStage::Extracting,
            ProgressStage::Database,
            ProgressStage::Meshes,
            ProgressStage::Textures,
            ProgressStage::Scripts,
            ProgressStage::LodChunks,
            ProgressStage::Validating,
            ProgressStage::Publishing,
            ProgressStage::Complete,
        ];
        for stage in order {
            for step in 0..=10 {
                let overall = overall_fraction(stage, step as f32 / 10.0);
                assert!(
                    overall >= previous,
                    "{:?} at {step}/10 gave {overall}, below {previous}",
                    stage
                );
                previous = overall;
            }
        }
        assert_eq!(previous, 1.0);
    }

    #[test]
    fn an_event_prefers_bytes_over_items_and_an_override_over_both() {
        let counted = event(ProgressStage::Textures, 25, 100);
        assert_eq!(counted.fraction(), 0.25);
        assert_eq!(counted.progress_fraction(), 0.25);

        let by_bytes = counted.clone().with_bytes(50, 100);
        assert_eq!(by_bytes.fraction(), 0.25, "fraction() stayed count-based");
        assert_eq!(by_bytes.progress_fraction(), 0.5);
        assert!(by_bytes.overall() > counted.overall());

        let overridden = by_bytes.with_stage_fraction(0.75);
        assert_eq!(overridden.progress_fraction(), 0.75);
        assert_eq!(
            overridden.overall(),
            overall_fraction(ProgressStage::Textures, 0.75)
        );
    }

    #[test]
    fn formats_elapsed_and_clock_times() {
        assert_eq!(format_elapsed(0.0), "0:00:00.0");
        assert_eq!(format_elapsed(62.25), "0:01:02.3");
        assert_eq!(
            format_elapsed(5.0 * 3600.0 + 7.0 * 60.0 + 9.94),
            "5:07:09.9"
        );
        assert_eq!(format_clock(0.0), "00:00:00");
        assert_eq!(format_clock(62.4), "00:01:02");
        assert_eq!(format_clock(5.0 * 3600.0 + 7.0 * 60.0 + 9.0), "05:07:09");
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(24_200_000_000), "24.2 GB");
    }

    #[test]
    fn redraws_a_terminal_at_most_four_times_a_second() {
        let mut renderer = ProgressRenderer::new(true, false);
        let first = renderer.update(&event(ProgressStage::Textures, 0, 100), Duration::ZERO);
        let first = first.expect("the first event draws the line");
        assert!(first.starts_with('\r'), "{first:?}");
        // The first stage's line stays open, so its redraws land on the same row.
        assert!(!first.ends_with('\n'), "{first:?}");
        assert!(first.contains("Textures"), "{first:?}");

        assert!(
            renderer
                .update(
                    &event(ProgressStage::Textures, 10, 100),
                    Duration::from_millis(50)
                )
                .is_none(),
            "redrew less than 250ms after the last draw"
        );
        assert!(
            renderer
                .update(
                    &event(ProgressStage::Textures, 20, 100),
                    Duration::from_millis(200)
                )
                .is_none()
        );
        let redrawn = renderer
            .update(
                &event(ProgressStage::Textures, 30, 100),
                Duration::from_millis(300),
            )
            .expect("250ms have passed");
        assert!(!redrawn.ends_with('\n'), "a redraw stays on one line");
        assert!(
            !redrawn.contains('\x1b'),
            "no escape sequences: {redrawn:?}"
        );

        let changed = renderer
            .update(
                &event(ProgressStage::Validating, 0, 10),
                Duration::from_millis(320),
            )
            .expect("a stage change prints immediately");
        // It ends the finished stage's row first, so that line stays in the scrollback, then opens
        // a row for the new stage.
        assert!(changed.starts_with("\n\r"), "{changed:?}");
        assert!(!changed.ends_with('\n'), "{changed:?}");
        assert!(changed.contains("Validating"), "{changed:?}");
    }

    #[test]
    fn a_notice_after_a_timer_redraw_starts_on_a_new_row() {
        let mut renderer = ProgressRenderer::new(true, false);
        renderer
            .update(&event(ProgressStage::Meshes, 1, 10), Duration::ZERO)
            .expect("the first event draws");
        let mut notice = event(ProgressStage::Meshes, 1, 10);
        notice.notice = true;
        renderer
            .update(&notice, Duration::from_millis(10))
            .expect("a notice prints");
        renderer
            .tick(Duration::from_millis(400))
            .expect("the timer redraws the status line");
        let second = renderer
            .update(&notice, Duration::from_millis(410))
            .expect("a notice prints");
        assert!(second.starts_with('\n'), "{second:?}");
    }

    #[test]
    fn a_timer_redraw_shows_the_latest_event_even_if_it_was_throttled() {
        let mut renderer = ProgressRenderer::new(true, false);
        renderer
            .update(&event(ProgressStage::Textures, 0, 100), Duration::ZERO)
            .expect("the first event draws");
        // Inside the refresh interval: not printed, but it is the latest progress.
        assert!(
            renderer
                .update(
                    &event(ProgressStage::Textures, 40, 100),
                    Duration::from_millis(100)
                )
                .is_none()
        );
        let redrawn = renderer
            .tick(Duration::from_millis(400))
            .expect("the timer redraws after 250ms");
        assert!(redrawn.contains(" 40%"), "{redrawn:?}");
    }

    #[test]
    fn redrawing_pads_over_the_line_it_replaces() {
        assert_eq!(redraw_text("short", 10), "\rshort     ");
        assert_eq!(redraw_text("1234567890", 10), "\r1234567890");
        assert_eq!(redraw_text("a longer line", 4), "\ra longer line");
        assert_eq!(redraw_text("", 3), "\r   ");

        // A redraw never leaves text from a longer line visible, and never uses an escape.
        let mut renderer = ProgressRenderer::new(true, false);
        let mut widths = Vec::new();
        for (step, completed) in [(0u64, 0u64), (1, 10), (2, 20), (3, 30)] {
            let event = event(ProgressStage::Textures, completed, 100)
                .with_bytes(completed * 1000, 100_000);
            if let Some(text) = renderer.update(&event, Duration::from_secs(step)) {
                assert!(!text.contains('\x1b'), "{text:?}");
                let visible = text.trim_start_matches('\r').trim_end_matches('\n');
                widths.push(visible.chars().count());
            }
        }
        for pair in widths.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "a redraw is shorter than the line it replaced: {widths:?}"
            );
        }
    }

    #[test]
    fn a_timer_redraws_the_status_line_between_events() {
        let mut renderer = ProgressRenderer::new(true, false);
        renderer.update(&event(ProgressStage::Textures, 0, 100), Duration::ZERO);
        let ticked = renderer
            .tick(Duration::from_secs(1))
            .expect("a second has passed since the last draw");
        assert!(ticked.starts_with('\r'), "{ticked:?}");
        assert!(ticked.contains("00:00:01 elapsed"), "{ticked:?}");
        assert!(
            renderer.tick(Duration::from_millis(1_100)).is_none(),
            "the timer is throttled like an event"
        );
        assert!(
            ProgressRenderer::new(false, false)
                .tick(Duration::from_secs(10))
                .is_none(),
            "a log line every few seconds is enough off a terminal"
        );
    }

    #[test]
    fn a_notice_ends_an_open_status_line_before_it_prints() {
        let mut renderer = ProgressRenderer::new(true, false);
        assert!(
            renderer
                .update(&event(ProgressStage::Textures, 0, 100), Duration::ZERO)
                .expect("draws the line")
                .starts_with('\r')
        );
        let text = renderer
            .update(
                &ProgressEvent::notice(
                    ProgressStage::Textures,
                    Some(PathBuf::from("meshes/rock.glb")),
                    "warning: pruned dangling texture textures/rock.dds",
                ),
                Duration::from_secs(1),
            )
            .expect("a notice always prints");
        assert!(
            text.starts_with('\n'),
            "ends the status line first: {text:?}"
        );
        assert!(text.ends_with('\n'), "{text:?}");
        assert!(text.contains("pruned dangling texture"), "{text:?}");
        // The next redraw starts on a clean row.
        let redrawn = renderer
            .update(
                &event(ProgressStage::Textures, 1, 100),
                Duration::from_secs(1),
            )
            .expect("250ms have passed");
        assert!(redrawn.starts_with('\r'), "{redrawn:?}");
        assert!(!redrawn.contains('\n'), "{redrawn:?}");
    }

    #[test]
    fn prints_one_line_per_stage_change_and_a_few_seconds_of_log_otherwise() {
        let mut renderer = ProgressRenderer::new(false, false);
        let first = renderer
            .update(&event(ProgressStage::Textures, 0, 100), Duration::ZERO)
            .expect("the first line is always printed");
        assert!(first.starts_with("[0:00:00.0] "), "{first:?}");
        assert!(first.ends_with('\n'));
        assert!(
            renderer
                .update(
                    &event(ProgressStage::Textures, 10, 100),
                    Duration::from_secs(1)
                )
                .is_none(),
            "a log does not print every event"
        );
        assert!(
            renderer
                .update(
                    &event(ProgressStage::Textures, 20, 100),
                    Duration::from_secs(4)
                )
                .is_none()
        );
        let later = renderer
            .update(
                &event(ProgressStage::Textures, 30, 100),
                Duration::from_secs(5),
            )
            .expect("five seconds have passed");
        assert!(later.starts_with("[0:00:05.0] "), "{later:?}");
        assert!(
            renderer
                .update(
                    &event(ProgressStage::Meshes, 0, 10),
                    Duration::from_millis(5_100)
                )
                .is_some(),
            "a stage change prints whatever the interval"
        );
    }

    #[test]
    fn prints_every_asset_when_verbose() {
        let mut renderer = ProgressRenderer::new(false, true);
        let mut lines = 0;
        for index in 0..20 {
            let printed = renderer.update(
                &event(ProgressStage::Textures, index, 20),
                Duration::from_millis(index * 10),
            );
            assert!(printed.is_some(), "verbose prints event {index}");
            lines += 1;
        }
        assert_eq!(lines, 20);
    }

    #[test]
    fn verbose_lines_name_the_asset_and_what_happened() {
        let mut renderer = ProgressRenderer::new(true, true);
        let converted = ProgressEvent::new(
            ProgressStage::Textures,
            3,
            20,
            Some(PathBuf::from("textures/rock.dds")),
            "Converted asset",
        );
        let line = renderer
            .update(&converted, Duration::from_millis(1_500))
            .expect("verbose prints every event");
        assert_eq!(
            line,
            format!(
                "[0:00:01.5] Textures    3/20  Converted asset: {}
",
                Path::new("textures/rock.dds").display()
            )
        );

        // An event about the stage rather than one file says what happened and nothing more.
        let started = ProgressEvent::new(ProgressStage::Database, 0, 0, None, "Building world");
        let line = renderer
            .update(&started, Duration::from_secs(2))
            .expect("verbose prints every event");
        assert_eq!(
            line,
            "[0:00:02.0] Database     Building world
"
        );
    }

    #[test]
    fn shows_time_left_once_the_rate_has_settled() {
        let mut renderer = ProgressRenderer::new(true, false);
        let mut printed = String::new();
        // A stage advancing at 2% of the whole run per second.
        for step in 0..=20 {
            let overall = 0.02 * step as f32;
            let event = event(ProgressStage::Textures, step * 5, 100).with_stage_fraction(overall);
            if let Some(line) = renderer.update(&event, Duration::from_secs(step)) {
                if step < 5 {
                    assert!(!line.contains(" left"), "guessed early: {line:?}");
                }
                printed = line;
            }
        }
        assert!(printed.contains(" left"), "{printed:?}");
        assert!(printed.contains("items/s"), "{printed:?}");
    }

    /// An event the estimator can read: its whole-run fraction is `overall` whatever the stage
    /// counts say (that fraction is the estimator's input), with `completed` items and `bytes` of
    /// input worked through.
    fn progress(overall: f32, completed: u64, bytes: u64) -> ProgressEvent {
        let mut event =
            event(ProgressStage::Textures, completed, 1_000).with_bytes(bytes, 1_000_000_000);
        event.overall = overall;
        event
    }

    #[test]
    fn the_estimate_reports_the_rates_of_a_steady_run() {
        let mut estimate = ProgressEstimate::default();
        // Two percent of the run a second, ten items and a megabyte with it.
        for step in 0..=25u64 {
            estimate.observe(
                &progress(step as f32 / 50.0, step * 10, step * 1_000_000),
                Duration::from_secs(step),
            );
        }
        assert_eq!(estimate.items_per_second(), Some(10.0));
        assert_eq!(estimate.bytes_per_second(), Some(1_000_000.0));
        assert_eq!(estimate.overall(), 0.5);
        // Half the run done at two percent a second leaves 25 seconds.
        assert_eq!(
            estimate.time_left(Duration::from_secs(25)),
            Some(Duration::from_secs(25))
        );
    }

    #[test]
    fn the_estimate_waits_for_enough_samples_and_enough_time() {
        // Enough time on the clock, but too few samples to have a rate: one every ten seconds.
        let mut sparse = ProgressEstimate::default();
        for (step, overall) in [(0u64, 0.0f32), (1, 0.05), (2, 0.10), (3, 0.15)] {
            let elapsed = Duration::from_secs(step * 10);
            sparse.observe(&progress(overall, step, 0), elapsed);
            if step < 3 {
                assert_eq!(
                    sparse.time_left(elapsed),
                    None,
                    "guessed a time left from {step} samples"
                );
            }
        }
        assert_eq!(
            sparse.time_left(Duration::from_secs(30)),
            Some(Duration::from_secs(170)),
            "three samples of half a percent a second"
        );

        // Enough samples, but the run is younger than five seconds: still no number.
        let mut young = ProgressEstimate::default();
        for step in 0..=5u64 {
            let elapsed = Duration::from_millis(step * 1_000);
            young.observe(&progress(step as f32 / 50.0, step * 10, 0), elapsed);
            if step < 5 {
                assert_eq!(young.time_left(elapsed), None, "guessed under five seconds");
            }
        }
        assert_eq!(
            young.time_left(Duration::from_secs(5)),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn the_estimate_never_moves_the_overall_fraction_backwards() {
        let mut estimate = ProgressEstimate::default();
        let mut seen = 0.0f32;
        for (step, overall) in [
            (0u64, 0.0f32),
            (1, 0.24),
            (2, 0.48),
            // A stage that finishes short of its total, then the next stage starting over.
            (3, 0.44),
            (4, 0.52),
            (5, 0.52),
        ] {
            estimate.observe(&progress(overall, step * 10, 0), Duration::from_secs(step));
            assert!(
                estimate.overall() >= seen,
                "the estimate moved back from {seen} to {}",
                estimate.overall()
            );
            seen = estimate.overall();
        }
        assert_eq!(seen, 0.52);
    }

    #[test]
    fn the_estimate_eases_into_a_slowdown_rather_than_stepping_to_it() {
        let mut estimate = ProgressEstimate::default();
        // Two percent of the run a second, sampled once a second.
        for step in 0..=25u64 {
            estimate.observe(
                &progress(step as f32 / 50.0, step * 10, step * 1_000_000),
                Duration::from_secs(step),
            );
        }
        let before = estimate
            .time_left(Duration::from_secs(25))
            .expect("25 seconds left at two percent a second");
        // Two seconds in which the run does not move at all: the rate drops towards zero.
        for step in 26..=27u64 {
            estimate.observe(
                &progress(0.5, step * 10, step * 1_000_000),
                Duration::from_secs(step),
            );
        }
        let after = estimate
            .time_left(Duration::from_secs(27))
            .expect("two seconds of stall must not read as an infinite rate");
        assert!(after > before, "a slowdown must raise the time left");
        assert!(
            after < Duration::from_secs(35),
            "one sample must not jump to the stalled rate: {after:?}"
        );
    }

    #[test]
    fn a_notice_is_not_progress_and_leaves_the_estimate_alone() {
        let mut estimate = ProgressEstimate::default();
        for step in 0..=10u64 {
            estimate.observe(
                &progress(step as f32 / 50.0, step * 10, step * 1_000_000),
                Duration::from_secs(step),
            );
        }
        let rate = estimate.items_per_second().expect("the run was moving");
        // A warning about one asset arrives from a stage the run has not reached.
        estimate.observe(
            &ProgressEvent::notice(
                ProgressStage::Validating,
                None,
                "warning: pruned a reference",
            ),
            Duration::from_secs(11),
        );
        assert_eq!(estimate.overall(), 0.2, "a notice moved the bar");
        assert_eq!(
            estimate.items_per_second(),
            Some(rate),
            "a notice restarted the rate"
        );
    }

    #[test]
    fn the_status_line_shows_the_numbers_the_estimate_holds() {
        let mut estimate = ProgressEstimate::default();
        let mut renderer = ProgressRenderer::new(true, false);
        let mut line = String::new();
        for step in 0..=25u64 {
            let elapsed = Duration::from_secs(step);
            let event = progress(step as f32 / 50.0, step * 10, step * 1_000_000);
            estimate.observe(&event, elapsed);
            if let Some(text) = renderer.update(&event, elapsed) {
                line = text;
            }
        }
        let items = estimate.items_per_second().expect("a steady rate");
        let bytes = estimate.bytes_per_second().expect("a steady rate");
        let left = estimate
            .time_left(Duration::from_secs(25))
            .expect("half the run done at two percent a second");
        assert!(line.contains(&format!("{items:.0} items/s")), "{line:?}");
        assert!(
            line.contains(&format!("{:.1} MB/s", bytes / 1_000_000.0)),
            "{line:?}"
        );
        assert!(
            line.contains(&format!("~{} left", format_clock(left.as_secs_f64()))),
            "{line:?}"
        );
        assert!(
            line.contains(&format!("[overall {:>3.0}%]", estimate.overall() * 100.0)),
            "{line:?}"
        );
    }

    #[test]
    fn the_status_line_shows_the_numeric_time_left() {
        let mut renderer = ProgressRenderer::new(true, false);
        let mut line = String::new();
        for step in 0..=30u64 {
            let mut event = event(ProgressStage::Textures, step * 10, 300);
            // Two percent of the run a second, whatever the stage counts do.
            event.overall = 0.02 * step as f32;
            if let Some(text) = renderer.update(&event, Duration::from_secs(step)) {
                line = text;
            }
        }
        assert!(line.contains("~00:00:20 left"), "{line:?}");
    }

    #[test]
    fn holds_the_overall_fraction_when_a_stage_finishes_short() {
        let mut renderer = ProgressRenderer::new(true, false);
        let mut seen = 0.0f32;
        for (stage, completed) in [
            (ProgressStage::Extracting, 23),
            (ProgressStage::Extracting, 24),
            // A skipped archive leaves the stage fraction short of 1.0.
            (ProgressStage::Extracting, 23),
            (ProgressStage::Database, 1),
        ] {
            let line =
                renderer.update(&event(stage, completed, 24), Duration::from_secs(completed));
            if let Some(line) = line {
                let overall: f32 = line
                    .split("[overall ")
                    .nth(1)
                    .and_then(|rest| rest.split('%').next())
                    .expect("the line carries the overall fraction")
                    .trim()
                    .parse()
                    .unwrap();
                assert!(overall >= seen, "{line:?} moved back from {seen}");
                seen = overall;
            }
        }
    }

    #[test]
    fn ends_an_open_status_line_before_the_summary() {
        let mut renderer = ProgressRenderer::new(true, false);
        assert!(renderer.finish().is_none());
        renderer.update(&event(ProgressStage::Textures, 1, 2), Duration::ZERO);
        assert_eq!(renderer.finish().as_deref(), Some("\n"));
        assert!(renderer.finish().is_none(), "the newline is written once");
    }
}
