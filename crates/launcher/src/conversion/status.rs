//! What the conversion screen shows about a run or a check: the bar, the stage, clock and asset
//! lines, and the notice pane. The numbers come from `converter::progress`, the same estimate and
//! formatters the command line prints its status line with.

use bevy::prelude::*;
use converter::{CheckMode, ProgressEstimate, ProgressEvent, ProgressStage};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How far a check of the output folder has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckProgress {
    pub mode: CheckMode,
    /// Manifest entries looked at so far.
    pub done: usize,
    /// Manifest entries in all; zero until the manifest has been read.
    pub total: usize,
    /// True once the check has reported its result.
    pub finished: bool,
    /// True when it reported a result rather than failing to run.
    pub completed: bool,
    /// True when Stop ended it before it had a result.
    pub stopped: bool,
}

/// What the window shows about a run: the shared estimate, the stage it is in, the asset in flight
/// and what the run has said.
#[derive(Resource, Debug)]
pub struct ConversionStatus {
    /// The same estimate the command line prints its status line from; `overall()` is the bar.
    pub estimate: ProgressEstimate,
    pub stage: Option<ProgressStage>,
    pub stage_fraction: f32,
    pub stage_completed: u64,
    pub stage_total: u64,
    pub run_finished: bool,
    pub current_file: Option<String>,
    pub message: String,
    /// Completed results retain all lines; active and post-run notices are bounded.
    pub notices: VecDeque<String>,
    completed_notice_lines: usize,
    /// Set while the bar and the lines show a check of the output folder rather than a run.
    pub check: Option<CheckProgress>,
    /// The staging folder Delete staging is removing, while it does.
    pub deleting: Option<PathBuf>,
    started: Option<Instant>,
    pub elapsed: Duration,
}

impl Default for ConversionStatus {
    fn default() -> Self {
        Self {
            estimate: ProgressEstimate::new(),
            stage: None,
            stage_fraction: 0.0,
            stage_completed: 0,
            stage_total: 0,
            run_finished: false,
            current_file: None,
            message: String::new(),
            notices: VecDeque::new(),
            completed_notice_lines: 0,
            check: None,
            deleting: None,
            started: None,
            elapsed: Duration::ZERO,
        }
    }
}

impl ConversionStatus {
    /// How many active-run notices the pane keeps.
    pub const NOTICE_LINES: usize = 5;

    /// Clears for a new run: the bar goes back to zero, the clock starts again, and the notices
    /// from the last run go away.
    pub fn begin_run(&mut self) {
        self.estimate = ProgressEstimate::new();
        self.stage = None;
        self.stage_fraction = 0.0;
        self.stage_completed = 0;
        self.stage_total = 0;
        self.run_finished = false;
        self.current_file = None;
        self.message.clear();
        self.notices.clear();
        self.completed_notice_lines = 0;
        self.check = None;
        self.started = Some(Instant::now());
        self.elapsed = Duration::ZERO;
    }

    /// Clears for a check of `output`: the bar and the clock start again and show the check.
    pub fn begin_check(&mut self, mode: CheckMode, output: &Path) {
        self.begin_run();
        self.check = Some(CheckProgress {
            mode,
            done: 0,
            total: 0,
            finished: false,
            completed: false,
            stopped: false,
        });
        self.message = format!("Checking {}", output.display());
    }

    /// Folds a check's progress into the bar. The checking threads report out of order, so the
    /// bar keeps the furthest point seen.
    pub fn observe_check(&mut self, done: usize, total: usize) {
        self.tick();
        if let Some(check) = &mut self.check {
            check.total = total;
            check.done = check.done.max(done).min(total);
        }
    }

    /// Ends a check: the clock stops, and the pane shows the result whole, replacing whatever it
    /// held. A check that `completed` fills the bar; one that could not run leaves it where it was.
    pub fn finish_check(&mut self, lines: &[String], completed: bool) {
        self.stop_clock();
        if let Some(check) = &mut self.check {
            check.finished = true;
            check.completed = completed;
            if completed {
                check.done = check.total;
            }
        }
        self.notices = lines.iter().cloned().collect();
    }

    /// Ends a check that Stop cut short: the clock stops, the bar empties and the pane says so. What
    /// the check had looked at so far is not a result, so none of it is shown.
    pub fn cancel_check(&mut self) {
        self.stop_clock();
        if let Some(check) = &mut self.check {
            check.finished = true;
            check.completed = false;
            check.stopped = true;
            check.done = 0;
        }
        self.push_notice("Check stopped.");
    }

    /// Shows that `staging` is being deleted. The rest of the status (the stopped run's bar and
    /// lines) stays as it was.
    pub fn begin_delete(&mut self, staging: &Path) {
        self.deleting = Some(staging.to_path_buf());
    }

    /// Ends a delete and says in the pane how it went.
    pub fn finish_delete(&mut self, staging: &Path, result: &Result<(), String>) {
        self.deleting = None;
        match result {
            Ok(()) => self.push_notice(&format!(
                "Deleted the staging folder {}.",
                staging.display()
            )),
            Err(error) => self.push_notice(&format!(
                "Could not delete {}: {error} - it is still offered for Resume and Delete staging.",
                staging.display()
            )),
        }
    }

    /// Stops the clock at the moment a run or a check ended, so the elapsed time stays what it was.
    pub fn stop_clock(&mut self) {
        self.tick();
        self.started = None;
    }

    pub fn finish_run(&mut self, lines: &[String]) {
        self.stop_clock();
        self.run_finished = true;
        let progress = std::mem::take(&mut self.notices);
        self.notices = lines.iter().cloned().collect();
        self.notices.extend(
            progress
                .into_iter()
                .filter(|notice| !lines.iter().any(|line| line.contains(notice))),
        );
        self.completed_notice_lines = self.notices.len();
    }

    /// Folds one event from a running conversion into what the window shows. The estimate never
    /// moves the bar backwards, whatever the stages report.
    pub fn observe(&mut self, event: &ProgressEvent) {
        self.tick();
        if event.notice {
            // A notice is about one asset rather than the run moving; the estimate ignores it too.
            self.push_notice(&event.message);
            return;
        }
        self.estimate.observe(event, self.elapsed);
        self.stage = Some(event.stage);
        self.stage_fraction = event.progress_fraction();
        self.stage_completed = event.completed;
        self.stage_total = event.total;
        self.current_file = event
            .current_file
            .as_ref()
            .map(|path| path.display().to_string());
        self.message = event.message.clone();
    }

    /// Keeps the clock moving between events, so an asset that takes a minute does not read as a
    /// stalled run.
    pub fn tick(&mut self) {
        if let Some(started) = self.started {
            self.elapsed = started.elapsed();
        }
    }

    /// Adds a notice without discarding a completed result.
    pub fn push_notice(&mut self, line: &str) {
        let retained = if self.run_finished {
            self.completed_notice_lines
        } else {
            0
        };
        while self.notices.len() >= retained + Self::NOTICE_LINES {
            self.notices.remove(retained);
        }
        self.notices.push_back(line.to_owned());
    }

    /// The bar: whole-run completion in percent, from the shared estimate, or the share of the
    /// manifest a check has looked at.
    pub fn overall_percent(&self) -> f32 {
        match self.check {
            Some(check) if check.total > 0 => 100.0 * check.done as f32 / check.total as f32,
            Some(check) if check.completed => 100.0,
            Some(_) => 0.0,
            None => self.estimate.overall() * 100.0,
        }
    }

    /// The stage line: which stage, how far through it, and how fast it is going; or the staging
    /// folder being deleted.
    pub fn stage_line(&self) -> String {
        if let Some(staging) = &self.deleting {
            return format!("Deleting {}...", staging.display());
        }
        if let Some(check) = self.check {
            let mode = match check.mode {
                CheckMode::Quick => "Quick check",
                CheckMode::Full => "Full check",
            };
            return if check.completed {
                format!("{mode} done   {} files", check.total)
            } else if check.stopped {
                format!("{mode} stopped")
            } else if check.finished {
                format!("{mode} could not run")
            } else if check.total == 0 {
                format!("{mode}   reading the manifest")
            } else {
                format!("{mode}   {}/{} files", check.done, check.total)
            };
        }
        if self.stage == Some(ProgressStage::LodChunks) {
            return if self.stage_total == 0 {
                "Building terrain LOD".to_owned()
            } else {
                format!(
                    "Building terrain LOD   {}/{} worldspaces",
                    self.stage_completed, self.stage_total
                )
            };
        }
        let mut line = match self.stage {
            Some(stage) => format!(
                "{:<11} {:>3.0}%",
                format!("{stage:?}"),
                self.stage_fraction * 100.0
            ),
            None => "waiting for the first asset".to_owned(),
        };
        if let Some(rate) = self.estimate.items_per_second() {
            let _ = write!(line, "   {rate:.0} items/s");
        }
        if let Some(rate) = self.estimate.bytes_per_second() {
            let _ = write!(
                line,
                "   {}/s",
                converter::progress::format_bytes(rate as u64)
            );
        }
        line
    }

    /// The clock line: how long the run has been going, and how much is left of it.
    pub fn clock_line(&self) -> String {
        let mut line = format!(
            "{} elapsed",
            converter::progress::format_clock(self.elapsed.as_secs_f64())
        );
        if let Some(check) = self.check {
            // A check goes through its entries at a steady pace, so the rest takes what the part
            // done took, scaled; the run's estimate knows nothing of it.
            if !check.finished && check.done > 0 && self.elapsed >= Duration::from_secs(1) {
                let left = self.elapsed.as_secs_f64() * (check.total - check.done) as f64
                    / check.done as f64;
                let _ = write!(line, "   ~{} left", converter::progress::format_clock(left));
            }
            return line;
        }
        if self.stage != Some(ProgressStage::LodChunks)
            && let Some(left) = self.estimate.time_left(self.elapsed)
        {
            let _ = write!(
                line,
                "   ~{} left",
                converter::progress::format_clock(left.as_secs_f64())
            );
        }
        line
    }

    /// The line about the asset in flight.
    pub fn file_line(&self) -> String {
        match &self.current_file {
            Some(path) if self.message.is_empty() => path.clone(),
            Some(path) => format!("{} {path}", self.message),
            None => self.message.clone(),
        }
    }

    /// The notice pane: active notices or the full completed result.
    pub fn notice_text(&self) -> String {
        self.notices.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_progress_shows_world_counts_without_rates_or_eta() {
        let mut status = ConversionStatus::default();
        status.begin_run();
        status.observe(&ProgressEvent::new(
            ProgressStage::Textures,
            1,
            10,
            None,
            "textures",
        ));
        let previous = status.overall_percent();
        status.observe(&ProgressEvent::new(
            ProgressStage::LodChunks,
            0,
            50,
            None,
            "LOD",
        ));
        status.elapsed = Duration::from_secs(120);
        assert_eq!(
            status.stage_line(),
            "Building terrain LOD   0/50 worldspaces"
        );
        assert_eq!(status.clock_line(), "00:02:00 elapsed");
        assert!(status.overall_percent() >= previous);
        status.observe(&ProgressEvent::notice(
            ProgressStage::LodChunks,
            None,
            "world skipped",
        ));
        assert_eq!(
            status.stage_line(),
            "Building terrain LOD   0/50 worldspaces"
        );
        status.observe(&ProgressEvent::new(
            ProgressStage::LodChunks,
            3,
            50,
            None,
            "LOD",
        ));
        assert_eq!(
            status.stage_line(),
            "Building terrain LOD   3/50 worldspaces"
        );
    }

    #[test]
    fn finished_run_keeps_all_result_lines_and_resets_for_next_run() {
        let mut status = ConversionStatus::default();
        status.begin_run();
        status.push_notice("old progress");
        let lines: Vec<_> = (0..12).map(|i| format!("result {i}")).collect();
        status.finish_run(&lines);
        assert_eq!(
            status
                .notices
                .iter()
                .take(lines.len())
                .cloned()
                .collect::<Vec<_>>(),
            lines
        );
        assert_eq!(
            status.notices.back().map(String::as_str),
            Some("old progress")
        );
        assert!(status.run_finished);
        assert!(status.started.is_none());
        status.push_notice("Asset readiness checked.");
        assert!(status.run_finished);
        assert_eq!(status.notices.len(), lines.len() + 2);
        assert_eq!(status.notices.front(), lines.first());
        assert_eq!(
            status.notices.back().map(String::as_str),
            Some("Asset readiness checked.")
        );
        for i in 0..100 {
            status.push_notice(&format!("readiness notice {i}"));
        }
        assert_eq!(
            status.notices.len(),
            lines.len() + 1 + ConversionStatus::NOTICE_LINES
        );
        assert_eq!(status.notices[lines.len()], "old progress");
        assert_eq!(status.notices.front(), lines.first());
        status.begin_run();
        assert!(!status.run_finished);
        assert!(status.notices.is_empty());
        assert_eq!(status.stage_total, 0);
    }

    /// The status shows the numbers the estimate holds, through the converter's own formatters.
    #[test]
    fn the_status_lines_read_from_the_estimate() {
        let mut status = ConversionStatus::default();
        status.begin_run();
        assert_eq!(status.notice_text(), "");
        for (step, overall) in [(0u64, 0.1f32), (1, 0.2), (2, 0.3), (3, 0.4)] {
            let mut event = ProgressEvent::new(
                ProgressStage::Textures,
                step * 10,
                100,
                Some(PathBuf::from("textures/clutter/barrel01.dds")),
                "converting",
            )
            .with_bytes(step * 1_000_000, 10_000_000);
            event.overall = overall;
            status.observe(&event);
        }
        assert_eq!(status.overall_percent(), 40.0);
        assert!(
            status.stage_line().starts_with("Textures"),
            "{:?}",
            status.stage_line()
        );
        assert!(
            status.stage_line().contains('%'),
            "{:?}",
            status.stage_line()
        );
        assert!(
            status.file_line().contains("barrel01.dds"),
            "{:?}",
            status.file_line()
        );
        assert!(
            status.clock_line().starts_with("00:00:00 elapsed"),
            "{:?}",
            status.clock_line()
        );
    }

    #[test]
    fn a_delete_shows_in_the_stage_line_and_its_result_in_the_pane() {
        let staging = PathBuf::from("C:/out.staging-1");
        let mut status = ConversionStatus::default();
        status.begin_delete(&staging);
        assert_eq!(status.stage_line(), "Deleting C:/out.staging-1...");

        status.finish_delete(&staging, &Ok(()));
        assert_eq!(status.stage_line(), "waiting for the first asset");
        assert_eq!(
            status.notice_text(),
            "Deleted the staging folder C:/out.staging-1."
        );

        status.begin_delete(&staging);
        status.finish_delete(&staging, &Err("Access is denied. (os error 5)".to_owned()));
        assert!(status.deleting.is_none());
        assert!(
            status
                .notice_text()
                .ends_with("Could not delete C:/out.staging-1: Access is denied. (os error 5) - it is still offered for Resume and Delete staging."),
            "{:?}",
            status.notice_text()
        );
    }
}
