//! The conversion screen's state machine.
//!
//! Nothing here knows about Bevy: the screen is a function of the state this module computes, so
//! every transition can be tested directly rather than through a running app. The screen's shape is
//! documented in `docs/specs/modding/launcher.md` ("Conversion screen").

use converter::progress::{format_bytes, format_elapsed};
use converter::{CheckMode, CheckReport, PipelineConfig, PipelineFailure, PipelineReport};
use std::path::PathBuf;
use std::time::Duration;

/// What the window is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConversionState {
    /// Nothing is running and nothing has finished: the folders can still be chosen freely.
    #[default]
    Idle,
    /// A conversion is running on its own thread.
    Running,
    /// The user asked the running conversion to stop; it finishes the asset in flight and reports
    /// the staging folder it kept.
    Stopping,
    /// The last run finished. `Start` converts again from scratch.
    Finished(RunReport),
    /// The last run stopped short of publishing: either stopped by the user (`cancelled`) or
    /// failed. `staging` is the folder that was kept, when one was, and is what `Resume` picks up.
    /// A staging folder left beside the output by an earlier session lands here too, with
    /// `cancelled` false: nobody pressed Stop on it in this session.
    Stopped {
        staging: Option<PathBuf>,
        cancelled: bool,
    },
    /// The output folder is being checked against its manifest on its own thread. `previous` is
    /// the state the check was started from, and the state it returns to, whether the check ends or
    /// is stopped: a check only reads, so it changes nothing a Resume or a Delete staging depends on.
    Checking {
        mode: CheckMode,
        previous: Box<ConversionState>,
    },
    /// Delete staging is removing `staging` on its own thread: a full install's staging folder is
    /// tens of gigabytes and takes a while to delete. Nothing may start, resume or check while it
    /// goes. `cancelled` is the stopped run's, kept for the state the delete returns to.
    Deleting { staging: PathBuf, cancelled: bool },
}

/// What a finished run did, in the shape the window shows it. The pipeline's own report is much
/// wider than the window needs and is not `PartialEq`, which the state machine's tests want.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub complete: bool,
    pub converted: u64,
    pub cache_hits: u64,
    pub skipped: u64,
    pub warnings: Vec<String>,
    pub lod_chunks: u64,
    pub lod_warnings: Vec<String>,
    pub artifacts: usize,
    pub elapsed: Duration,
}

impl RunReport {
    pub fn from_pipeline(report: &PipelineReport) -> Self {
        Self {
            complete: report.complete,
            converted: report.converted,
            cache_hits: report.cache_hits,
            skipped: report.skipped,
            warnings: report.warnings.clone(),
            lod_chunks: report.lod_chunks,
            lod_warnings: report.lod_warnings.clone(),
            artifacts: report.artifacts.len(),
            elapsed: Duration::from_millis(report.elapsed_ms.min(u128::from(u64::MAX)) as u64),
        }
    }

    /// The summary line the window shows when the run is over.
    pub fn headline(&self) -> String {
        let elapsed = format_elapsed(self.elapsed.as_secs_f64());
        if self.complete {
            format!(
                "Conversion complete in {elapsed}: converted {}, reused {}, failed {}.",
                self.converted, self.cache_hits, self.skipped
            )
        } else {
            format!(
                "Conversion incomplete after {elapsed}: {} input(s) were skipped; the manifest lists what is missing. Converted {}, reused {}.",
                self.skipped, self.converted, self.cache_hits
            )
        }
    }

    /// What the run published, for the line under the summary.
    pub fn artifacts_line(&self) -> String {
        format!("{} artifact(s) published.", self.artifacts)
    }

    pub fn lod_line(&self) -> String {
        let result = if self.lod_chunks == 0 {
            "No terrain LOD generated".to_owned()
        } else {
            format!("Terrain LOD: {} chunks", self.lod_chunks)
        };
        format!(
            "{result}; {} worldspace warning(s).",
            self.lod_warnings.len()
        )
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.headline(), self.artifacts_line(), self.lod_line()];
        lines.extend(
            self.warnings
                .iter()
                .map(|warning| format!("Warning: {warning}")),
        );
        lines.extend(
            self.lod_warnings
                .iter()
                .map(|warning| format!("LOD: {warning}")),
        );
        lines
    }
}

/// Why a run stopped short of publishing, in the shape the window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureReport {
    /// The error, with its cause chain, as the command line prints it.
    pub message: String,
    /// The staging folder the run kept, when it kept one. `None` means there is nothing to resume
    /// and nothing to delete.
    pub staging: Option<PathBuf>,
    /// True when the user stopped the run rather than it failing.
    pub cancelled: bool,
}

impl FailureReport {
    pub fn from_pipeline(failure: PipelineFailure) -> Self {
        Self {
            message: format!("{:#}", failure.error),
            staging: failure.staging,
            cancelled: failure.cancelled,
        }
    }

    /// A failure the window itself hit, before the pipeline had a chance to start.
    pub fn before_start(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            staging: None,
            cancelled: false,
        }
    }

    /// The line the window shows when a run stops short.
    pub fn headline(&self) -> String {
        if self.cancelled {
            "Stopped: the run was cancelled and is no longer converting.".to_owned()
        } else {
            format!("Conversion failed: {}", self.message)
        }
    }

    /// The line about the staging folder, which is what tells the reader whether Resume will work.
    pub fn staging_line(&self) -> String {
        match &self.staging {
            Some(path) => format!(
                "Staging kept at {}. Resume continues where it stopped; finished files are checked again, not redone. Delete staging starts over.",
                path.display()
            ),
            None => "No staging folder was kept, so the next run starts from scratch.".to_owned(),
        }
    }
}

/// How many of a check's problems the notice pane lists before "and N more".
pub const CHECK_PROBLEM_LINES: usize = 8;

/// What a check of the output folder found, in the shape the window shows it. The converter's own
/// report is not `PartialEq` and can list every artifact of an install, so only the lines the pane
/// shows are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckSummary {
    pub mode: CheckMode,
    pub files_checked: usize,
    pub bytes_checked: u64,
    pub elapsed: Duration,
    /// How many problems the check found.
    pub problem_count: usize,
    /// The first [`CHECK_PROBLEM_LINES`] of them, in the converter's order and wording.
    pub first_problems: Vec<String>,
    /// What to do about them, or `None` when there are none.
    pub advice: Option<String>,
}

impl CheckSummary {
    pub fn from_report(report: &CheckReport) -> Self {
        Self {
            mode: report.mode,
            files_checked: report.files_checked,
            bytes_checked: report.bytes_checked,
            elapsed: report.elapsed,
            problem_count: report.problems.len(),
            first_problems: report
                .problems
                .iter()
                .take(CHECK_PROBLEM_LINES)
                .map(ToString::to_string)
                .collect(),
            advice: report.advice().map(str::to_owned),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.problem_count == 0
    }

    /// The lines the notice pane shows, worded as `converter check` prints them.
    pub fn lines(&self) -> Vec<String> {
        let mode = check_mode_description(self.mode);
        let seconds = self.elapsed.as_secs_f64();
        let bytes = format_bytes(self.bytes_checked);
        if self.is_ok() {
            return vec![format!(
                "All good: {} files, {bytes}, {mode}, {seconds:.1} s",
                self.files_checked
            )];
        }
        let mut lines = vec![format!(
            "{} problem(s) in {} files, {bytes}, {mode}, {seconds:.1} s:",
            self.problem_count, self.files_checked
        )];
        lines.extend(
            self.first_problems
                .iter()
                .map(|problem| format!("  {problem}")),
        );
        if self.problem_count > self.first_problems.len() {
            lines.push(format!(
                "  and {} more",
                self.problem_count - self.first_problems.len()
            ));
        }
        lines.extend(self.advice.clone());
        lines
    }
}

/// What a check mode looks at, as `converter check` names it.
pub fn check_mode_description(mode: CheckMode) -> &'static str {
    match mode {
        CheckMode::Quick => "quick check: existence and size",
        CheckMode::Full => "full check: size and hash",
    }
}

/// The line the window shows when a check could not run at all (no folder, no manifest).
pub fn check_failure_line(message: &str) -> String {
    format!("Check failed: {message}")
}

/// Something the window did: a button press, a dropped folder, or news from the running conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Start converts the chosen folders from scratch.
    Start { data: PathBuf, output: PathBuf },
    /// Stop asks the running conversion to stop; in `Stopping` a second Stop quits the process.
    /// During a check it stops the check.
    Stop,
    /// Resume carries on from the staging folder of the last stopped run, with the same folders
    /// that run used.
    Resume { data: PathBuf, output: PathBuf },
    /// Delete staging removes the folder the last stopped run kept.
    DeleteStaging,
    /// The staging folder is gone.
    StagingDeleted,
    /// The staging folder could not be deleted, for the reason given; it is offered again.
    DeleteFailed(String),
    /// A staging folder an earlier session left beside the output was found, when the launcher
    /// started or the Output folder changed. It is offered for Resume like one kept in this session.
    FoundStaging { staging: PathBuf },
    /// A progress update from the running conversion. The state machine does not read the numbers
    /// (the window's status does); it is here so the "still running" transition is spelled out.
    Progress,
    /// The run finished.
    Finished(RunReport),
    /// The run stopped short.
    Failed(FailureReport),
    /// Check or Full check: read the output folder against its manifest, converting nothing.
    Check { output: PathBuf, mode: CheckMode },
    /// The check finished and found what the summary says.
    CheckFinished(CheckSummary),
    /// The check could not run: the folder or its manifest is missing or unreadable.
    CheckFailed(String),
    /// The check stopped because Stop was pressed; it reported no result.
    CheckCancelled,
}

/// What the runner should do about an input.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Nothing to do: the press was not legal in this state.
    None,
    /// Start a conversion with this configuration on its own thread.
    Begin(PipelineConfig),
    /// Ask the running conversion to stop.
    Cancel,
    /// End the process now, as the command line's second Ctrl+C does.
    Quit,
    /// Remove the staging folder a stopped run kept, on its own thread.
    DeleteStaging { staging: PathBuf },
    /// Check this output folder on its own thread.
    BeginCheck { output: PathBuf, mode: CheckMode },
    /// Ask the running check to stop.
    CancelCheck,
}

/// The one transition table: what `input` does to `state`, and what the runner has to do about it.
///
/// A press that the design's table does not allow for the state is a no-op, so the buttons and the
/// machine can never disagree about what is legal: the buttons read [`controls`], and an input that
/// gets through anyway still changes nothing.
pub fn apply(state: ConversionState, input: Input) -> (ConversionState, Effect) {
    use ConversionState::{Checking, Deleting, Finished, Idle, Running, Stopped, Stopping};
    match input {
        // Start always converts from scratch, whichever state it is pressed in: a run that kept a
        // staging folder is only picked up again by Resume.
        Input::Start { data, output } => match state {
            Idle | Finished(_) | Stopped { .. } => {
                (Running, Effect::Begin(PipelineConfig::new(data, output)))
            }
            Running | Stopping | Checking { .. } | Deleting { .. } => (state, Effect::None),
        },
        Input::Stop => match state {
            Running => (Stopping, Effect::Cancel),
            // The run is already stopping; pressing again gives the process up, as the command
            // line's second Ctrl+C does.
            Stopping => (Stopping, Effect::Quit),
            // A check stays `Checking` until its thread reports that it stopped, so a second check
            // cannot start beside one that is still winding down.
            checking @ Checking { .. } => (checking, Effect::CancelCheck),
            other => (other, Effect::None),
        },
        Input::Resume { data, output } => match state {
            // A resume reuses the folders the stopped run used, and only its staging folder.
            Stopped {
                staging: Some(staging),
                ..
            } => {
                let mut config = PipelineConfig::new(data, output);
                config.resume_staging = Some(staging);
                (Running, Effect::Begin(config))
            }
            other => (other, Effect::None),
        },
        Input::DeleteStaging => match state {
            // The folder is offered again if the delete fails, so the state keeps it until the
            // deleting thread reports.
            Stopped {
                staging: Some(staging),
                cancelled,
            } => (
                Deleting {
                    staging: staging.clone(),
                    cancelled,
                },
                Effect::DeleteStaging { staging },
            ),
            other => (other, Effect::None),
        },
        Input::StagingDeleted => match state {
            Deleting { cancelled, .. } => (
                Stopped {
                    staging: None,
                    cancelled,
                },
                Effect::None,
            ),
            other => (other, Effect::None),
        },
        // What is left of the folder is still there, so it is offered for Resume and Delete staging
        // again; the notice pane says why the delete failed.
        Input::DeleteFailed(_) => match state {
            Deleting { staging, cancelled } => (
                Stopped {
                    staging: Some(staging),
                    cancelled,
                },
                Effect::None,
            ),
            other => (other, Effect::None),
        },
        // A leftover staging folder is offered only where nothing else is going on and no staging
        // folder is already waiting; the one from this session wins over one found on disk.
        Input::FoundStaging { staging } => match state {
            Idle | Finished(_) | Stopped { staging: None, .. } => (
                Stopped {
                    staging: Some(staging),
                    cancelled: false,
                },
                Effect::None,
            ),
            other => (other, Effect::None),
        },
        Input::Progress => match state {
            Running => (Running, Effect::None),
            other => (other, Effect::None),
        },
        // A run can publish just before a Stop reaches it; it finished all the same.
        Input::Finished(report) => match state {
            Running | Stopping => (Finished(report), Effect::None),
            other => (other, Effect::None),
        },
        Input::Failed(failure) => match state {
            // A run that fails while it is stopping still stopped short, so it lands in the same
            // place, with the staging folder it kept and the reason it actually ended.
            Running | Stopping => (
                Stopped {
                    staging: failure.staging,
                    cancelled: failure.cancelled,
                },
                Effect::None,
            ),
            other => (other, Effect::None),
        },
        // A check can start whenever nothing is running. It remembers the state it started from,
        // so a staging folder waiting for Resume is still waiting when the check is over.
        Input::Check { output, mode } => match state {
            Idle | Finished(_) | Stopped { .. } => (
                Checking {
                    mode,
                    previous: Box::new(state),
                },
                Effect::BeginCheck { output, mode },
            ),
            Running | Stopping | Checking { .. } | Deleting { .. } => (state, Effect::None),
        },
        // However the check ends, the window goes back to what it was doing before; the result is
        // shown in the notice pane, not kept in the state.
        Input::CheckFinished(_) | Input::CheckFailed(_) | Input::CheckCancelled => match state {
            Checking { previous, .. } => (*previous, Effect::None),
            other => (other, Effect::None),
        },
    }
}

/// Which controls the window enables, for the state it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls {
    pub start: bool,
    /// The Stop button. It is still enabled while stopping, because a second press quits the
    /// process; the label changes to say so.
    pub stop: bool,
    pub resume: bool,
    pub delete_staging: bool,
    /// Check and Full check: on whenever nothing is running, conversion or check. The panel also
    /// needs a manifest in the Output folder before it draws them as available.
    pub check: bool,
    /// Whether the two path rows accept a new folder: not while a run is going, and not while a
    /// staging folder is waiting to be resumed or deleted, because a resume must reuse the stopped
    /// run's data and output.
    pub paths: bool,
}

/// The enabled state of every control, from the design's table.
pub fn controls(state: &ConversionState) -> Controls {
    match state {
        ConversionState::Idle => Controls {
            start: true,
            stop: false,
            resume: false,
            delete_staging: false,
            check: true,
            paths: true,
        },
        ConversionState::Running => Controls {
            start: false,
            stop: true,
            resume: false,
            delete_staging: false,
            check: false,
            paths: false,
        },
        ConversionState::Stopping => Controls {
            start: false,
            stop: true,
            resume: false,
            delete_staging: false,
            check: false,
            paths: false,
        },
        ConversionState::Finished(_) => Controls {
            start: true,
            stop: false,
            resume: false,
            delete_staging: false,
            check: true,
            paths: true,
        },
        ConversionState::Stopped { staging, .. } => Controls {
            start: true,
            stop: false,
            resume: staging.is_some(),
            delete_staging: staging.is_some(),
            check: true,
            paths: staging.is_none(),
        },
        // Stop stops the check; nothing else may start beside it, and the folders stay as they are
        // until it reports.
        ConversionState::Checking { .. } => Controls {
            start: false,
            stop: true,
            resume: false,
            delete_staging: false,
            check: false,
            paths: false,
        },
        // A delete cannot be stopped part way to any use, and nothing may touch the folder it is
        // removing: every control is off until it reports.
        ConversionState::Deleting { .. } => Controls {
            start: false,
            stop: false,
            resume: false,
            delete_staging: false,
            check: false,
            paths: false,
        },
    }
}

/// What the Start button says in this state: the run is a fresh conversion either way. "Start
/// over" is for when a staging folder is waiting, which Start would leave behind.
pub fn start_label(state: &ConversionState) -> &'static str {
    match state {
        ConversionState::Finished(_) => "Convert again",
        ConversionState::Stopped {
            staging: Some(_), ..
        }
        | ConversionState::Deleting { .. } => "Start over",
        // Start is off while checking; it keeps the label the check returns to.
        ConversionState::Checking { previous, .. } => start_label(previous),
        _ => "Start",
    }
}

/// What the Stop button says: pressing it in `Stopping` quits rather than stops.
pub fn stop_label(state: &ConversionState) -> &'static str {
    match state {
        ConversionState::Stopping => "Quit now",
        _ => "Stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders() -> (PathBuf, PathBuf) {
        (PathBuf::from(r"C:\Skyrim\Data"), PathBuf::from(r"C:\out"))
    }

    fn start() -> Input {
        let (data, output) = folders();
        Input::Start { data, output }
    }

    fn resume() -> Input {
        let (data, output) = folders();
        Input::Resume { data, output }
    }

    fn staging() -> PathBuf {
        PathBuf::from(r"C:\out.staging-20260928")
    }

    fn stopped(staging: Option<PathBuf>, cancelled: bool) -> ConversionState {
        ConversionState::Stopped { staging, cancelled }
    }

    fn report(complete: bool) -> RunReport {
        RunReport {
            complete,
            converted: 10,
            cache_hits: 2,
            skipped: 0,
            warnings: Vec::new(),
            lod_chunks: 0,
            lod_warnings: Vec::new(),
            artifacts: 12,
            elapsed: Duration::from_secs(90),
        }
    }

    /// The state a `Start` lands in is `Running`, and the effect carries the chosen folders.
    #[test]
    fn idle_starts_a_conversion_of_the_chosen_folders() {
        let (data, output) = folders();
        let (state, effect) = apply(ConversionState::Idle, start());
        assert_eq!(state, ConversionState::Running);
        let Effect::Begin(config) = effect else {
            panic!("Start did not begin a conversion: {effect:?}");
        };
        assert_eq!(config.data_dir, data);
        assert_eq!(config.output_dir, output);
        assert_eq!(config.resume_staging, None, "a fresh run resumes nothing");
    }

    #[test]
    fn finished_and_stopped_start_again_from_scratch() {
        for state in [
            ConversionState::Finished(report(true)),
            stopped(Some(staging()), true),
            stopped(None, false),
        ] {
            let (next, effect) = apply(state.clone(), start());
            assert_eq!(next, ConversionState::Running, "{state:?}");
            let Effect::Begin(config) = effect else {
                panic!("{state:?} did not begin a conversion: {effect:?}");
            };
            assert_eq!(
                config.resume_staging, None,
                "{state:?} started over but reused a staging folder"
            );
        }
    }

    #[test]
    fn start_is_a_no_op_while_a_run_is_going() {
        for state in [ConversionState::Running, ConversionState::Stopping] {
            let (next, effect) = apply(state.clone(), start());
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
        }
    }

    #[test]
    fn stop_asks_a_running_conversion_to_stop() {
        let (state, effect) = apply(ConversionState::Running, Input::Stop);
        assert_eq!(state, ConversionState::Stopping);
        assert!(matches!(effect, Effect::Cancel), "{effect:?}");
    }

    #[test]
    fn a_second_stop_quits_the_process() {
        let (state, effect) = apply(ConversionState::Stopping, Input::Stop);
        assert_eq!(state, ConversionState::Stopping);
        assert!(matches!(effect, Effect::Quit), "{effect:?}");
    }

    #[test]
    fn stop_does_nothing_in_the_states_that_cannot_be_stopped() {
        for state in [
            ConversionState::Idle,
            ConversionState::Finished(report(true)),
            stopped(Some(staging()), true),
        ] {
            let (next, effect) = apply(state.clone(), Input::Stop);
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
        }
    }

    #[test]
    fn progress_keeps_a_running_run_running_and_changes_nothing_elsewhere() {
        let (state, effect) = apply(ConversionState::Running, Input::Progress);
        assert_eq!(state, ConversionState::Running);
        assert!(matches!(effect, Effect::None), "{effect:?}");

        let (state, effect) = apply(ConversionState::Idle, Input::Progress);
        assert_eq!(state, ConversionState::Idle);
        assert!(matches!(effect, Effect::None), "{effect:?}");
    }

    #[test]
    fn a_finished_run_shows_its_report() {
        let (state, effect) = apply(ConversionState::Running, Input::Finished(report(true)));
        assert_eq!(state, ConversionState::Finished(report(true)));
        assert!(matches!(effect, Effect::None), "{effect:?}");
        let ConversionState::Finished(report) = state else {
            unreachable!()
        };
        assert!(
            report.headline().starts_with("Conversion complete"),
            "{report:?}"
        );
    }

    /// A run that publishes just before a Stop reaches it has finished, not stopped: the window
    /// must not stay in `Stopping` with Play off.
    #[test]
    fn a_run_that_finishes_while_stopping_ends_the_stop() {
        for complete in [true, false] {
            let (state, effect) =
                apply(ConversionState::Stopping, Input::Finished(report(complete)));
            assert_eq!(state, ConversionState::Finished(report(complete)));
            assert!(matches!(effect, Effect::None), "{effect:?}");
            assert_eq!(
                controls(&state),
                controls(&ConversionState::Finished(report(complete)))
            );
            assert_eq!(
                stop_label(&state),
                "Stop",
                "the Stop button no longer says Quit now"
            );
        }
    }

    /// A failure that arrives while stopping ends the stop too, keeping what the failure says
    /// about the staging folder and why the run ended.
    #[test]
    fn a_run_that_fails_while_stopping_keeps_its_staging_folder_and_reason() {
        let (state, effect) = apply(
            ConversionState::Stopping,
            Input::Failed(FailureReport {
                message: "no space left".into(),
                staging: Some(staging()),
                cancelled: false,
            }),
        );
        assert_eq!(state, stopped(Some(staging()), false));
        assert!(matches!(effect, Effect::None), "{effect:?}");
        let controls = controls(&state);
        assert!(controls.resume && controls.delete_staging && !controls.stop);
    }

    #[test]
    fn a_failure_shows_a_failure_and_offers_to_resume_when_a_staging_folder_was_kept() {
        let (state, effect) = apply(
            ConversionState::Running,
            Input::Failed(FailureReport {
                message: "no space left".into(),
                staging: Some(staging()),
                cancelled: false,
            }),
        );
        assert_eq!(state, stopped(Some(staging()), false));
        assert!(matches!(effect, Effect::None), "{effect:?}");
        let ConversionState::Stopped { cancelled, .. } = state else {
            unreachable!()
        };
        assert!(!cancelled, "a failure is not a stop");
        let controls = controls(&ConversionState::Stopped {
            staging: Some(staging()),
            cancelled,
        });
        assert!(controls.resume && controls.delete_staging);
    }

    #[test]
    fn a_cancelled_run_reports_itself_as_stopped() {
        let (state, _) = apply(
            ConversionState::Stopping,
            Input::Failed(FailureReport {
                message: "conversion cancelled".into(),
                staging: Some(staging()),
                cancelled: true,
            }),
        );
        assert_eq!(state, stopped(Some(staging()), true));
    }

    #[test]
    fn a_run_that_fails_while_stopping_still_lands_in_stopped() {
        let (state, effect) = apply(
            ConversionState::Stopping,
            Input::Failed(FailureReport {
                message: "the disk went away".into(),
                staging: None,
                cancelled: false,
            }),
        );
        assert_eq!(state, stopped(None, false));
        assert!(matches!(effect, Effect::None), "{effect:?}");
    }

    #[test]
    fn a_failure_without_a_staging_folder_offers_nothing_to_resume_or_delete() {
        let (state, _) = apply(
            ConversionState::Running,
            Input::Failed(FailureReport::before_start("no runtime")),
        );
        assert_eq!(state, stopped(None, false));
        let controls = controls(&state);
        assert!(!controls.resume, "nothing to resume");
        assert!(!controls.delete_staging, "nothing to delete");
        assert!(controls.paths, "the folders can be chosen again");
    }

    #[test]
    fn resume_carries_the_staging_folder_and_the_stopped_runs_folders() {
        let (data, output) = folders();
        let (state, effect) = apply(stopped(Some(staging()), true), resume());
        assert_eq!(state, ConversionState::Running);
        let Effect::Begin(config) = effect else {
            panic!("Resume did not begin a conversion: {effect:?}");
        };
        assert_eq!(config.resume_staging, Some(staging()));
        assert_eq!(config.data_dir, data);
        assert_eq!(config.output_dir, output);
    }

    #[test]
    fn resume_without_a_staging_folder_does_nothing() {
        for state in [
            ConversionState::Idle,
            ConversionState::Running,
            ConversionState::Stopping,
            ConversionState::Finished(report(true)),
            stopped(None, false),
        ] {
            let (next, effect) = apply(state.clone(), resume());
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{state:?} gave {effect:?}");
        }
    }

    fn deleting(cancelled: bool) -> ConversionState {
        ConversionState::Deleting {
            staging: staging(),
            cancelled,
        }
    }

    /// Delete staging asks for the delete and waits in `Deleting` for the thread to report; the
    /// folder is only dropped from the state once it is gone.
    #[test]
    fn deleting_the_staging_folder_removes_the_offer_to_resume_and_delete() {
        let (state, effect) = apply(stopped(Some(staging()), true), Input::DeleteStaging);
        assert_eq!(state, deleting(true));
        let Effect::DeleteStaging { staging: deleted } = effect else {
            panic!("Delete staging did not ask for a delete: {effect:?}");
        };
        assert_eq!(deleted, staging());

        let (state, effect) = apply(state, Input::StagingDeleted);
        assert_eq!(state, stopped(None, true), "the reason it stopped is kept");
        assert!(matches!(effect, Effect::None), "{effect:?}");
        let controls = controls(&state);
        assert!(!controls.resume, "the folder is gone");
        assert!(!controls.delete_staging, "there is nothing left to delete");
        assert!(controls.paths, "the folders can be chosen again");
        assert!(controls.start && controls.check);
    }

    /// A delete that fails offers what is left of the folder again, exactly as before the press.
    #[test]
    fn a_failed_delete_offers_the_staging_folder_again() {
        for cancelled in [true, false] {
            let before = stopped(Some(staging()), cancelled);
            let (during, _) = apply(before.clone(), Input::DeleteStaging);
            let (after, effect) = apply(during, Input::DeleteFailed("access is denied".into()));
            assert_eq!(after, before);
            assert!(matches!(effect, Effect::None), "{effect:?}");
            assert_eq!(controls(&after), controls(&before));
        }
    }

    /// Nothing may start, resume, check or delete again while a folder is being deleted, and a
    /// stray message from a run or a check does not end the delete.
    #[test]
    fn every_other_input_is_a_no_op_while_deleting() {
        let state = deleting(true);
        for input in [
            start(),
            resume(),
            Input::Stop,
            Input::DeleteStaging,
            Input::FoundStaging {
                staging: PathBuf::from(r"C:\out.staging-older"),
            },
            Input::Progress,
            Input::Finished(report(true)),
            Input::Failed(FailureReport::before_start("late")),
            check(CheckMode::Quick),
            Input::CheckFinished(summary(0)),
            Input::CheckFailed("late".into()),
            Input::CheckCancelled,
        ] {
            let (next, effect) = apply(state.clone(), input.clone());
            assert_eq!(next, state, "{input:?}");
            assert!(matches!(effect, Effect::None), "{input:?} gave {effect:?}");
        }
    }

    #[test]
    fn a_delete_result_outside_a_delete_changes_nothing() {
        for state in [
            ConversionState::Idle,
            ConversionState::Running,
            ConversionState::Stopping,
            ConversionState::Finished(report(true)),
            stopped(Some(staging()), true),
            stopped(None, false),
            checking(stopped(Some(staging()), true)),
        ] {
            for input in [Input::StagingDeleted, Input::DeleteFailed("late".into())] {
                let (next, effect) = apply(state.clone(), input);
                assert_eq!(next, state);
                assert!(matches!(effect, Effect::None), "{effect:?}");
            }
        }
    }

    #[test]
    fn deleting_a_staging_folder_that_is_not_there_does_nothing() {
        for state in [
            ConversionState::Idle,
            ConversionState::Running,
            ConversionState::Finished(report(true)),
            stopped(None, false),
            deleting(false),
        ] {
            let (next, effect) = apply(state.clone(), Input::DeleteStaging);
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{state:?} gave {effect:?}");
        }
    }

    /// The design's table, read row by row.
    #[test]
    fn the_table_decides_which_controls_are_enabled() {
        assert_eq!(
            controls(&ConversionState::Idle),
            Controls {
                start: true,
                stop: false,
                resume: false,
                delete_staging: false,
                check: true,
                paths: true,
            }
        );
        assert_eq!(
            controls(&ConversionState::Running),
            Controls {
                start: false,
                stop: true,
                resume: false,
                delete_staging: false,
                check: false,
                paths: false,
            }
        );
        assert_eq!(
            controls(&ConversionState::Stopping),
            Controls {
                start: false,
                stop: true,
                resume: false,
                delete_staging: false,
                check: false,
                paths: false,
            }
        );
        assert_eq!(
            controls(&ConversionState::Finished(report(true))),
            Controls {
                start: true,
                stop: false,
                resume: false,
                delete_staging: false,
                check: true,
                paths: true,
            }
        );
        assert_eq!(
            controls(&stopped(Some(staging()), false)),
            Controls {
                start: true,
                stop: false,
                resume: true,
                delete_staging: true,
                check: true,
                paths: false,
            }
        );
        assert_eq!(
            controls(&stopped(None, true)),
            Controls {
                start: true,
                stop: false,
                resume: false,
                delete_staging: false,
                check: true,
                paths: true,
            }
        );
        assert_eq!(
            controls(&deleting(true)),
            Controls {
                start: false,
                stop: false,
                resume: false,
                delete_staging: false,
                check: false,
                paths: false,
            }
        );
    }

    /// Once the staging folder is deleted there is nothing to start over from: Start says Start
    /// again, as it does after a failure that kept no staging folder.
    #[test]
    fn start_says_start_again_once_no_staging_folder_is_waiting() {
        let (during, _) = apply(stopped(Some(staging()), true), Input::DeleteStaging);
        let (after, _) = apply(during, Input::StagingDeleted);
        assert_eq!(start_label(&after), "Start");
        assert_eq!(start_label(&stopped(None, false)), "Start");
        assert_eq!(start_label(&checking(stopped(None, true))), "Start");
    }

    #[test]
    fn the_start_and_stop_buttons_say_what_they_do() {
        assert_eq!(start_label(&ConversionState::Idle), "Start");
        assert_eq!(
            start_label(&ConversionState::Finished(report(true))),
            "Convert again"
        );
        assert_eq!(start_label(&stopped(Some(staging()), true)), "Start over");
        assert_eq!(start_label(&deleting(true)), "Start over");
        assert_eq!(stop_label(&ConversionState::Running), "Stop");
        assert_eq!(stop_label(&ConversionState::Stopping), "Quit now");
    }

    fn check(mode: CheckMode) -> Input {
        Input::Check {
            output: PathBuf::from(r"C:\out"),
            mode,
        }
    }

    fn checking(previous: ConversionState) -> ConversionState {
        ConversionState::Checking {
            mode: CheckMode::Quick,
            previous: Box::new(previous),
        }
    }

    fn summary(problems: usize) -> CheckSummary {
        CheckSummary {
            mode: CheckMode::Quick,
            files_checked: 100,
            bytes_checked: 3_100_000,
            elapsed: Duration::from_millis(200),
            problem_count: problems,
            first_problems: (0..problems.min(CHECK_PROBLEM_LINES))
                .map(|index| format!("missing: meshes/{index:02}.glb"))
                .collect(),
            advice: (problems > 0).then(|| "Run the converter again.".to_owned()),
        }
    }

    /// The states a check may start from: every one in which nothing is running.
    fn checkable_states() -> [ConversionState; 4] {
        [
            ConversionState::Idle,
            ConversionState::Finished(report(true)),
            stopped(Some(staging()), true),
            stopped(None, false),
        ]
    }

    #[test]
    fn a_check_starts_whenever_nothing_is_running_and_remembers_the_state() {
        for mode in [CheckMode::Quick, CheckMode::Full] {
            for state in checkable_states() {
                let (next, effect) = apply(state.clone(), check(mode));
                assert_eq!(
                    next,
                    ConversionState::Checking {
                        mode,
                        previous: Box::new(state.clone()),
                    }
                );
                let Effect::BeginCheck {
                    output,
                    mode: asked,
                } = effect
                else {
                    panic!("{state:?} did not begin a check: {effect:?}");
                };
                assert_eq!(output, PathBuf::from(r"C:\out"));
                assert_eq!(asked, mode);
            }
        }
    }

    #[test]
    fn a_check_is_a_no_op_while_a_run_or_a_check_is_going() {
        for state in [
            ConversionState::Running,
            ConversionState::Stopping,
            checking(ConversionState::Idle),
        ] {
            let (next, effect) = apply(state.clone(), check(CheckMode::Full));
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{state:?} gave {effect:?}");
        }
    }

    #[test]
    fn a_finished_check_returns_to_the_state_it_started_from() {
        for state in checkable_states() {
            for result in [
                Input::CheckFinished(summary(0)),
                Input::CheckFinished(summary(23)),
            ] {
                let (next, effect) = apply(checking(state.clone()), result);
                assert_eq!(next, state);
                assert!(matches!(effect, Effect::None), "{effect:?}");
            }
        }
    }

    #[test]
    fn a_failed_check_returns_to_the_state_it_started_from() {
        for state in checkable_states() {
            let (next, effect) = apply(
                checking(state.clone()),
                Input::CheckFailed("no manifest".into()),
            );
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
        }
    }

    /// Resume and Delete staging are offered after a check exactly when they were before it.
    #[test]
    fn a_check_result_never_offers_resume_or_delete_staging_it_did_not_have() {
        for state in checkable_states() {
            let before = controls(&state);
            let (during, _) = apply(state.clone(), check(CheckMode::Quick));
            let (after, _) = apply(during, Input::CheckFinished(summary(3)));
            assert_eq!(controls(&after), before, "{state:?}");
            let (during, _) = apply(state.clone(), check(CheckMode::Full));
            let (after, _) = apply(during, Input::CheckFailed("unreadable".into()));
            assert_eq!(controls(&after), before, "{state:?}");
        }
    }

    #[test]
    fn a_check_result_outside_a_check_changes_nothing() {
        for state in [
            ConversionState::Idle,
            ConversionState::Running,
            ConversionState::Stopping,
            ConversionState::Finished(report(true)),
            stopped(Some(staging()), true),
        ] {
            for input in [
                Input::CheckFinished(summary(0)),
                Input::CheckFailed("late".into()),
            ] {
                let (next, effect) = apply(state.clone(), input);
                assert_eq!(next, state);
                assert!(matches!(effect, Effect::None), "{effect:?}");
            }
        }
    }

    /// While a check runs, nothing else may start or change, and a stray message from an earlier
    /// run does not end it.
    #[test]
    fn every_other_input_is_a_no_op_while_checking() {
        let state = checking(stopped(Some(staging()), true));
        for input in [
            start(),
            resume(),
            Input::DeleteStaging,
            Input::FoundStaging {
                staging: PathBuf::from(r"C:\out.staging-older"),
            },
            Input::Progress,
            Input::Finished(report(true)),
            Input::Failed(FailureReport::before_start("late")),
            Input::StagingDeleted,
            Input::DeleteFailed("late".into()),
        ] {
            let (next, effect) = apply(state.clone(), input.clone());
            assert_eq!(next, state, "{input:?}");
            assert!(matches!(effect, Effect::None), "{input:?} gave {effect:?}");
        }
    }

    #[test]
    fn only_stop_is_on_while_checking() {
        for state in checkable_states() {
            assert_eq!(
                controls(&checking(state)),
                Controls {
                    start: false,
                    stop: true,
                    resume: false,
                    delete_staging: false,
                    check: false,
                    paths: false,
                }
            );
        }
        // The Start label does not flicker while it is off.
        assert_eq!(
            start_label(&checking(ConversionState::Finished(report(true)))),
            "Convert again"
        );
        assert_eq!(stop_label(&checking(ConversionState::Idle)), "Stop");
    }

    /// Stop during a check asks the check to stop and waits for it to say it has; only then does
    /// the window go back to where it was, with no result.
    #[test]
    fn stop_during_a_check_stops_it_and_returns_to_the_state_it_started_from() {
        for state in checkable_states() {
            let during = checking(state.clone());
            let (next, effect) = apply(during.clone(), Input::Stop);
            assert_eq!(next, during, "the check is still winding down");
            assert!(matches!(effect, Effect::CancelCheck), "{effect:?}");

            // A second press while it winds down asks again and changes nothing else.
            let (next, effect) = apply(next, Input::Stop);
            assert_eq!(next, during);
            assert!(matches!(effect, Effect::CancelCheck), "{effect:?}");

            let (next, effect) = apply(next, Input::CheckCancelled);
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
            assert_eq!(controls(&next), controls(&state));
        }
    }

    #[test]
    fn a_check_cancelled_outside_a_check_changes_nothing() {
        for state in [
            ConversionState::Idle,
            ConversionState::Running,
            stopped(Some(staging()), true),
        ] {
            let (next, effect) = apply(state.clone(), Input::CheckCancelled);
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
        }
    }

    fn found() -> Input {
        Input::FoundStaging { staging: staging() }
    }

    /// A staging folder left by an earlier session, found at start-up or when the Output folder
    /// changes, is offered exactly as one kept by a Stop in this session: Resume and Delete
    /// staging on, the folders fixed.
    #[test]
    fn a_leftover_staging_folder_found_at_start_up_is_offered_for_resume() {
        for state in [
            ConversionState::Idle,
            ConversionState::Finished(report(false)),
            stopped(None, true),
        ] {
            let (next, effect) = apply(state.clone(), found());
            assert_eq!(next, stopped(Some(staging()), false), "{state:?}");
            assert!(matches!(effect, Effect::None), "{effect:?}");
            let controls = controls(&next);
            assert!(controls.resume && controls.delete_staging, "{state:?}");
            assert!(!controls.paths, "a resume must reuse these folders");

            let (running, effect) = apply(next, resume());
            assert_eq!(running, ConversionState::Running);
            let Effect::Begin(config) = effect else {
                panic!("Resume did not begin a conversion: {effect:?}");
            };
            assert_eq!(config.resume_staging, Some(staging()));
        }
    }

    #[test]
    fn a_leftover_staging_folder_changes_nothing_while_something_else_is_going_on() {
        let kept = PathBuf::from(r"C:\out.staging-this-session");
        for state in [
            ConversionState::Running,
            ConversionState::Stopping,
            checking(ConversionState::Idle),
            stopped(Some(kept), true),
        ] {
            let (next, effect) = apply(state.clone(), found());
            assert_eq!(next, state);
            assert!(matches!(effect, Effect::None), "{effect:?}");
        }
    }

    #[test]
    fn a_clean_check_reads_all_good() {
        assert_eq!(
            summary(0).lines(),
            vec!["All good: 100 files, 3.1 MB, quick check: existence and size, 0.2 s"]
        );
    }

    #[test]
    fn a_check_with_problems_lists_the_first_few_and_the_advice() {
        let lines = summary(23).lines();
        assert_eq!(
            lines[0],
            "23 problem(s) in 100 files, 3.1 MB, quick check: existence and size, 0.2 s:"
        );
        assert_eq!(lines[1], "  missing: meshes/00.glb");
        assert_eq!(lines[CHECK_PROBLEM_LINES], "  missing: meshes/07.glb");
        assert_eq!(lines[CHECK_PROBLEM_LINES + 1], "  and 15 more");
        assert_eq!(lines[CHECK_PROBLEM_LINES + 2], "Run the converter again.");
        assert_eq!(lines.len(), CHECK_PROBLEM_LINES + 3);

        // Few enough problems to list them all: no "and N more".
        let lines = summary(2).lines();
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains("more")), "{lines:?}");
    }

    #[test]
    fn a_summary_is_read_back_from_the_converter_report() {
        let report = CheckReport {
            mode: CheckMode::Full,
            files_checked: 40,
            bytes_checked: 1_000,
            elapsed: Duration::from_secs(2),
            problems: (0..12)
                .map(|index| converter::CheckProblem::WrongHash {
                    output: format!("textures/{index:02}.ktx2"),
                })
                .collect(),
        };
        let summary = CheckSummary::from_report(&report);
        assert!(!summary.is_ok());
        assert_eq!(summary.problem_count, 12);
        assert_eq!(summary.first_problems.len(), CHECK_PROBLEM_LINES);
        assert_eq!(summary.first_problems[0], "wrong hash: textures/00.ktx2");
        assert_eq!(summary.advice.as_deref(), report.advice());
        assert!(summary.lines()[0].contains("full check: size and hash"));
        assert_eq!(
            check_failure_line("no manifest"),
            "Check failed: no manifest"
        );
    }

    #[test]
    fn a_report_is_read_back_from_the_pipeline_report() {
        let pipeline = PipelineReport {
            complete: false,
            converted: 242_969,
            cache_hits: 1_204,
            skipped: 3,
            warnings: vec!["a.nif".into()],
            notices: Vec::new(),
            artifacts: vec![PathBuf::from("a.glb"), PathBuf::from("b.ktx2")],
            inputs_by_kind: Default::default(),
            pruned_texture_references: 0,
            lod_chunks: 1693,
            lod_warnings: vec!["Solstheim terrain skipped".into()],
            elapsed_ms: 18_450_000,
            integration: None,
        };
        let report = RunReport::from_pipeline(&pipeline);
        assert_eq!(report.artifacts, 2);
        assert_eq!(report.elapsed, Duration::from_millis(18_450_000));
        assert!(report.headline().contains("Conversion incomplete"));
        assert!(report.artifacts_line().contains('2'));
        assert_eq!(report.lod_chunks, 1693);
        assert_eq!(report.lod_warnings, pipeline.lod_warnings);
        assert_eq!(report.warnings, pipeline.warnings);
        assert_eq!(
            report.lod_line(),
            "Terrain LOD: 1693 chunks; 1 worldspace warning(s)."
        );
        assert!(
            report
                .lines()
                .contains(&"LOD: Solstheim terrain skipped".to_owned())
        );
    }

    #[test]
    fn zero_lod_chunks_are_not_reported_as_lod_coverage() {
        let report = report(true);
        assert_eq!(
            report.lod_line(),
            "No terrain LOD generated; 0 worldspace warning(s)."
        );
    }
}
