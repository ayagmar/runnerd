//! Jobs' logs as the daemon stores them, and the structure core reads in them (ADR 0133), for the log viewer of every
//! client: a failed job whose steps' lines are told by time and title (a custom step name, two steps of one name, a
//! nested group titled like a step, a group without timestamps, two headers in a second two steps share, set-up and
//! post output, colours, a line longer than a styled line shows, a warning and the failure), one cut at `history.log_max` before its failure, one still running with groups open, one whose log
//! expired with its record kept, one kept by a runnerd that kept no outline, and one with no log at all. Every
//! outline, line and word is core's: `LogOutline`, `StyledLine` and `FailureSummary::headline`.

use runnerd_core::api::{
    Job, JobState, LogOutline, OutlineParts, RecordNote, Step, StoredLog, StyledLine,
};

use crate::{LEDGER, NOW, job, job_runner, summary};

/// A failed job of halyard/ledger whose every step's lines core can tell but two headers', each printed in a second
/// two steps share.
pub const LOG_FAILED: u64 = 9401;
/// The same job, its log cut at `history.log_max` before the failed step.
pub const LOG_TRUNCATED: u64 = 9402;
/// A running job with two groups open.
pub const LOG_RUNNING: u64 = 9403;
/// A failed job whose log was deleted after `history.logs`, its record still kept.
pub const LOG_EXPIRED: u64 = 9404;
/// A job whose log a runnerd that kept no outline stored.
pub const LOG_NOT_OUTLINED: u64 = 9405;
/// A failed job with no log, its record kept.
pub const LOG_MISSING: u64 = 9406;

/// When the logs' jobs started: 2026-10-22T13:53:00Z, ten minutes before [`NOW`].
const START: u64 = NOW - 607;

/// A line the runner wrote `at` seconds and `tenth` tenths after [`START`], under a minute.
fn at(at: u64, tenth: u64, text: &str) -> String {
    format!("2026-10-22T13:53:{at:02}.{tenth}Z {text}")
}

/// A step that ran from `from` to `to` seconds after [`START`], or never.
fn step(number: u32, name: &str, times: Option<(u64, u64)>, conclusion: &str) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: times.map(|(from, _)| START + from),
        completed_at: times.map(|(_, to)| START + to),
    }
}

fn steps() -> Vec<Step> {
    vec![
        step(1, "Set up job", Some((0, 2)), "success"),
        step(2, "Run actions/checkout@v4", Some((2, 4)), "success"),
        step(3, "Build", Some((4, 30)), "success"),
        step(4, "Run ./gradlew test", Some((30, 40)), "success"),
        step(5, "Run ./gradlew test", Some((41, 50)), "failure"),
        step(6, "Lint", None, "skipped"),
        step(7, "Post Run actions/checkout@v4", Some((51, 52)), "success"),
        step(8, "Complete job", Some((53, 53)), "success"),
    ]
}

fn failed_log() -> Vec<String> {
    vec![
        at(0, 1, "Current runner version: '2.330.0'"),
        at(0, 2, "##[group]Operating System"),
        at(0, 2, "Ubuntu 24.04"),
        at(0, 3, "##[endgroup]"),
        at(1, 0, "##[group]GITHUB_TOKEN Permissions"),
        at(1, 0, "Contents: read"),
        at(1, 1, "##[endgroup]"),
        at(2, 1, "##[group]Run actions/checkout@v4"),
        at(2, 1, "with:"),
        at(2, 1, "##[endgroup]"),
        at(2, 5, "##[command]/usr/bin/git version"),
        at(3, 9, "Syncing repository: halyard/ledger"),
        at(4, 2, "##[group]Run ./gradlew assemble"),
        at(4, 2, "\u{1b}[36;1m./gradlew assemble\u{1b}[0m"),
        at(4, 2, "##[endgroup]"),
        at(5, 0, "##[group]Compile"),
        at(6, 0, "##[group]Run kapt"),
        at(20, 0, "\u{1b}[32mBUILD SUCCESSFUL\u{1b}[0m in 14s"),
        at(21, 0, "##[endgroup]"),
        at(22, 0, "##[endgroup]"),
        "##[group]Artifacts".to_owned(),
        "ledger.jar".to_owned(),
        "##[endgroup]".to_owned(),
        at(24, 0, "##[group]Run ./gradlew test"),
        at(25, 0, "##[endgroup]"),
        at(29, 0, "##[warning]Gradle 9 removes this API"),
        at(30, 3, "##[group]Run ./gradlew test"),
        at(30, 3, "##[endgroup]"),
        at(31, 0, "##[group]Cache"),
        at(31, 5, "##[endgroup]"),
        at(39, 0, "\u{1b}[1m42 tests\u{1b}[0m completed"),
        at(41, 0, "##[group]Run ./gradlew test"),
        at(41, 0, "##[endgroup]"),
        at(45, 0, "LedgerTest > balances() \u{1b}[31mFAILED\u{1b}[0m"),
        at(
            45,
            1,
            &format!("expected: {{\"balances\":[{}0]}}", "0,".repeat(40_000)),
        ),
        at(49, 9, "##[error]Process completed with exit code 1."),
        at(51, 0, "Post job cleanup."),
        at(51, 5, "##[command]/usr/bin/git version"),
        at(53, 1, "Cleaning up orphan processes"),
    ]
}

/// A record kept of a failed job.
fn kept() -> RecordNote {
    RecordNote {
        created_at: Some(NOW - 540),
        bytes: Some(3 << 30),
        skipped: None,
        removed_at: None,
        removed_by: None,
    }
}

/// A job of halyard/ledger's `CI / test` in `state`, with the steps of [`steps`].
fn logged_job(id: u64, state: JobState, conclusion: Option<&str>, lines: u64) -> Job {
    let mut summary = summary(id, LEDGER, state, conclusion);
    summary.runner = Some(job_runner("runnerd-ab12-jvm-1", "jvm", 1));
    summary.timeline.queued_at = START - 20;
    summary.timeline.started_at = Some(START);
    summary.timeline.finished_at = state.is_final().then_some(START + 54);
    summary.log.lines = lines;
    let mut job = job(summary);
    job.steps = steps();
    job
}

/// A job, its stored lines, and its outline as the daemon answers it.
#[derive(Debug, Clone)]
pub struct LoggedJob {
    /// `GET /v1/jobs/{id}`.
    pub job: Job,
    /// `GET /v1/jobs/{id}/logs`, line by line; empty where none is stored.
    pub lines: Vec<String>,
    /// `GET /v1/jobs/{id}/log/outline`.
    pub outline: LogOutline,
}

/// An outline the daemon assembles from what its store keeps: `parts` beside `lines`, or nothing for a log no
/// outline was kept of.
fn assembled(job: &Job, lines: &[String], stored: StoredLog, parts: OutlineParts) -> LogOutline {
    LogOutline::assemble(job, stored, parts, &mut |read| read.of(lines))
}

/// Every job with a log, in the order of the constants above.
pub fn logged_jobs() -> Vec<LoggedJob> {
    let log = failed_log();
    let failed = logged_job(
        LOG_FAILED,
        JobState::Completed,
        Some("failure"),
        log.len() as u64,
    );

    let cut = log[..20].to_vec();
    let mut truncated = logged_job(LOG_TRUNCATED, JobState::Completed, Some("failure"), 20);
    truncated.summary.log.truncated = true;

    let open = log[..18].to_vec();
    let mut running = logged_job(LOG_RUNNING, JobState::Running, None, 18);
    running.steps.truncate(3);
    if let Some(build) = running.steps.last_mut() {
        "in_progress".clone_into(&mut build.status);
        build.conclusion = None;
        build.completed_at = None;
    }

    let mut expired = logged_job(
        LOG_EXPIRED,
        JobState::Completed,
        Some("failure"),
        log.len() as u64,
    );
    expired.record = Some(kept());
    let gone = StoredLog {
        lines: log.len() as u64,
        truncated: false,
        kept: false,
    };

    let old = logged_job(
        LOG_NOT_OUTLINED,
        JobState::Completed,
        Some("failure"),
        log.len() as u64,
    );
    let unoutlined = StoredLog {
        lines: log.len() as u64,
        truncated: false,
        kept: true,
    };

    let mut missing = logged_job(LOG_MISSING, JobState::Completed, Some("failure"), 0);
    missing.record = Some(kept());

    vec![
        LoggedJob {
            outline: LogOutline::of(&failed, &log),
            job: failed,
            lines: log.clone(),
        },
        LoggedJob {
            outline: LogOutline::of(&truncated, &cut),
            job: truncated,
            lines: cut,
        },
        LoggedJob {
            outline: LogOutline::of(&running, &open),
            job: running,
            lines: open,
        },
        LoggedJob {
            outline: assembled(&expired, &[], gone, OutlineParts::default()),
            job: expired,
            lines: Vec::new(),
        },
        LoggedJob {
            outline: assembled(&old, &log, unoutlined, OutlineParts::default()),
            job: old,
            lines: log,
        },
        LoggedJob {
            outline: LogOutline::of(&missing, &[]),
            job: missing,
            lines: Vec::new(),
        },
    ]
}

/// What every log viewer must say of one of [`logged_jobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFacts {
    /// The job.
    pub job: u64,
    /// What the evidence says, if anything: `cut at history.log_max`.
    pub evidence: Option<&'static str>,
    /// On a failed job, the failure's first line: `Step 5, Run ./gradlew test, failed with exit code 1`.
    pub headline: Option<String>,
    /// Its lines, styled.
    pub lines: Vec<StyledLine>,
}

/// The facts of every one of [`logged_jobs`], in order.
pub fn log_facts() -> Vec<LogFacts> {
    logged_jobs()
        .into_iter()
        .map(|logged| LogFacts {
            job: logged.job.summary.id,
            evidence: logged.outline.evidence.words(),
            headline: logged
                .outline
                .failure
                .as_ref()
                .map(|failure| failure.headline(&logged.job.steps)),
            lines: (0..)
                .zip(&logged.lines)
                .map(|(number, text)| StyledLine::parse(number, text))
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use runnerd_core::api::{LineRange, LogEvidence, StepLog};

    use super::*;

    #[test]
    fn every_case_a_log_viewer_draws_is_here() {
        let jobs = logged_jobs();
        let evidence: Vec<LogEvidence> =
            jobs.iter().map(|logged| logged.outline.evidence).collect();
        assert_eq!(
            evidence,
            [
                LogEvidence::Complete,
                LogEvidence::Truncated,
                LogEvidence::Running,
                LogEvidence::Expired,
                LogEvidence::NotOutlined,
                LogEvidence::Missing,
            ]
        );
        let failed = &jobs[0].outline;
        assert_eq!(
            failed.unattributed,
            [
                LineRange { from: 12, to: 15 },
                LineRange { from: 26, to: 28 }
            ]
        );
        assert_eq!(failed.steps[5].lines, StepLog::Empty);
        let facts = log_facts();
        assert_eq!(
            facts[0].headline.as_deref(),
            Some("Step 5, Run ./gradlew test, failed with exit code 1")
        );
        assert_eq!(facts[1].evidence, Some("cut at history.log_max"));
        assert!(
            facts[0].lines.iter().any(|line| line.omitted > 0),
            "a line longer than a styled line shows says so"
        );
        assert_eq!(
            jobs[3]
                .outline
                .failure
                .as_ref()
                .and_then(|failure| failure.shell.as_deref()),
            Some("runnerctl shell 9404"),
            "an expired log keeps its record's shell"
        );
        let open: Vec<Option<u64>> = jobs[2]
            .outline
            .groups
            .iter()
            .map(|group| group.end)
            .collect();
        assert_eq!(
            open.iter().filter(|end| end.is_none()).count(),
            2,
            "{open:?}"
        );
    }
}
