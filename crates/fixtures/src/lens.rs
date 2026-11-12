//! The job history's lenses as every client draws them (ADR 0135): a search's marks, a window's timeline of slots
//! and a job beside its baseline. The awkward cases are here: a record kept, removed and not made, and one kept whose
//! job the history no longer has; a job named with `%` and `_`, and one waiting with no wait yet; a lane of a profile
//! the config no longer has, a job whose end is unknown, jobs no runner here took, and an hour runnerd stopped
//! watching in; a comparison with a renamed step, two steps of one name and sampled memory, one with no earlier
//! success, and one asked against another job. Every word is core's.

use runnerd_core::api::{
    Job, JobComparison, JobState, JobSummary, Record, RecordMark, RecordNote, RecordRemoval,
    Records, Sample, SlotTimeline, Span, Step, StepUsage, Tone,
};

use crate::{LEDGER, NOW, STOREFRONT, job, job_runner, summary};

/// A failed job of halyard/ledger whose record is kept.
pub const LENS_KEPT: u64 = 9501;
/// A failed job whose record was removed at `max_age`.
pub const LENS_REMOVED: u64 = 9502;
/// A failed job no record was made of, for too little free disk.
pub const LENS_SKIPPED: u64 = 9503;
/// A queued job named `100%_coverage`, which a search for `%` or `_` finds as written; no runner has it yet.
pub const LENS_WILDCARD: u64 = 9504;
/// A job its runner was lost with in slot 3 of `legacy`, a profile the config no longer has; its end is unknown.
pub const LENS_UNKNOWN_END: u64 = 9505;
/// A failed job whose record is kept and whose history was pruned: only `GET /v1/records` has it.
pub const LENS_ORPHAN: u64 = 9506;
/// A run of `CI / integration` on `main`, the baseline of [`LENS_COMPARED`].
pub const LENS_BASELINE: u64 = 9510;
/// The later run of `CI / integration` on `main`, slower, with a step renamed and two steps of one name.
pub const LENS_COMPARED: u64 = 9511;
/// The first run of `CI / deploy`: no earlier success to compare with.
pub const LENS_FIRST_RUN: u64 = 9512;

/// When the lens's timeline starts: an hour before [`NOW`].
pub const LENS_SINCE: u64 = NOW - 3600;
/// Runnerd stopped watching from here to [`LENS_WATCHED_AGAIN`].
pub const LENS_UNWATCHED: u64 = NOW - 2000;
/// When runnerd watched again.
pub const LENS_WATCHED_AGAIN: u64 = NOW - 1500;

/// A job of halyard/ledger's `CI / name` that ran in a slot, `started` and `finished` seconds before `now`.
struct Run<'name> {
    id: u64,
    name: &'name str,
    state: JobState,
    conclusion: Option<&'name str>,
    profile: &'name str,
    slot: usize,
    started: u64,
    finished: Option<u64>,
}

impl Run<'_> {
    /// Its summary, queued 10 s before it started.
    fn at(&self, now: u64) -> JobSummary {
        let mut job = summary(self.id, LEDGER, self.state, self.conclusion);
        self.name.clone_into(&mut job.name);
        job.runner = Some(job_runner(
            &format!("runnerd-ab12-{}-{}-{}", self.profile, self.slot, self.id),
            self.profile,
            self.slot,
        ));
        job.timeline.queued_at = now - self.started - 10;
        job.timeline.container_started_at = Some(now - self.started - 5);
        job.timeline.started_at = Some(now - self.started);
        job.timeline.finished_at = self.finished.map(|finished| now - finished);
        job.timeline.cleaned_at = self.finished.map(|finished| now - finished + 2);
        job
    }
}

/// A success of `CI / name` in jvm `slot`.
fn succeeded(id: u64, name: &str, slot: usize, started: u64, finished: u64) -> Run<'_> {
    Run {
        id,
        name,
        state: JobState::Completed,
        conclusion: Some("success"),
        profile: "jvm",
        slot,
        started,
        finished: Some(finished),
    }
}

/// A note of a record made `ago` seconds before `now`.
fn made(now: u64, ago: u64) -> RecordNote {
    RecordNote {
        created_at: Some(now - ago),
        bytes: Some(2 << 30),
        skipped: None,
        removed_at: None,
        removed_by: None,
        gone_at: None,
    }
}

/// `summary` with its record noted as `note`, as the store marks it.
fn noted(mut summary: JobSummary, note: RecordNote) -> Job {
    summary.record_mark = Some(RecordMark::of(&note));
    Job {
        record: Some(note),
        ..job(summary)
    }
}

/// A step of `number` and `name` from `from` to `to` seconds after `start`.
fn step(number: u32, name: &str, start: u64, from: u64, to: u64) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: Some(start + from),
        completed_at: Some(start + to),
    }
}

/// `CI / integration`'s steps from `start`: its tests `tests` seconds long, its lint step named `lint`, and two
/// steps named `Run e2e`.
fn integration_steps(start: u64, tests: u64, lint: &str) -> Vec<Step> {
    vec![
        step(1, "Set up job", start, 0, 2),
        step(2, "Run tests", start, 2, 2 + tests),
        step(3, lint, start, 2 + tests, 12 + tests),
        step(4, "Run e2e", start, 12 + tests, 20 + tests),
        step(5, "Run e2e", start, 20 + tests, 30 + tests),
    ]
}

/// The lens's jobs, newest first, and their runners' samples.
#[derive(Debug, Clone)]
pub struct LensHistory {
    /// The jobs, newest first.
    pub jobs: Vec<Job>,
    /// Each sampled job's samples, oldest first.
    pub samples: Vec<LensSamples>,
}

/// One job's samples.
#[derive(Debug, Clone)]
pub struct LensSamples {
    /// The job.
    pub job: u64,
    /// Oldest first.
    pub samples: Vec<Sample>,
}

/// Samples every 2 s of `step`, at `memory` bytes and one CPU.
fn sampled(step: &Step, memory: u64) -> Vec<Sample> {
    let (Some(from), Some(to)) = (step.started_at, step.completed_at) else {
        return Vec::new();
    };
    (from + 2..=to)
        .step_by(2)
        .map(|at| Sample {
            at,
            cpu: 1000,
            memory,
            network_received: 0,
            network_sent: 0,
        })
        .collect()
}

/// The lens's history, at [`NOW`].
pub fn lens_history() -> LensHistory {
    lens_history_at(NOW)
}

/// [`LENS_COMPARED`] and [`LENS_BASELINE`], each with its tests step sampled.
fn integration(now: u64) -> [(Job, LensSamples); 2] {
    let run = |id: u64, started: u64, finished: u64, tests: u64, lint: &str, memory: u64| {
        let mut summary = succeeded(id, "integration", 2, started, finished).at(now);
        summary.metrics.memory_peak = Some(memory);
        let steps = integration_steps(now - started, tests, lint);
        let samples = LensSamples {
            job: id,
            samples: sampled(&steps[1], memory),
        };
        (
            Job {
                steps,
                ..job(summary)
            },
            samples,
        )
    };
    [
        run(LENS_COMPARED, 1400, 1170, 150, "Lint (strict)", 3 << 30),
        run(LENS_BASELINE, 3000, 2800, 120, "Lint", 2 << 30),
    ]
}

/// [`LENS_KEPT`], [`LENS_SKIPPED`] and [`LENS_REMOVED`], newest first.
fn failures(now: u64) -> [Job; 3] {
    let failed = |id: u64, started: u64| {
        Run {
            id,
            name: "test",
            state: JobState::Completed,
            conclusion: Some("failure"),
            profile: "jvm",
            slot: 1,
            started,
            finished: Some(started - 60),
        }
        .at(now)
    };
    [
        noted(failed(LENS_KEPT, 400), made(now, 330)),
        noted(
            failed(LENS_SKIPPED, 1000),
            RecordNote {
                created_at: None,
                bytes: None,
                skipped: Some("too little free disk".to_owned()),
                ..made(now, 0)
            },
        ),
        noted(
            failed(LENS_REMOVED, 3500),
            RecordNote {
                removed_at: Some(now - 1000),
                removed_by: Some(RecordRemoval::Age),
                ..made(now, 3430)
            },
        ),
    ]
}

/// The lens's history with `now` for [`NOW`], as the demo draws it on the clock.
pub fn lens_history_at(now: u64) -> LensHistory {
    let [(compared, compared_samples), (baseline, baseline_samples)] = integration(now);
    let [kept, skipped, removed] = failures(now);
    let mut wildcard = summary(LENS_WILDCARD, STOREFRONT, JobState::Queued, None);
    "100%_coverage".clone_into(&mut wildcard.name);
    wildcard.timeline.queued_at = now - 90;
    let mut unknown_end = Run {
        id: LENS_UNKNOWN_END,
        name: "test",
        state: JobState::Lost,
        conclusion: None,
        profile: "legacy",
        slot: 3,
        started: 3300,
        finished: None,
    }
    .at(now);
    unknown_end.timeline.cleaned_at = Some(now - 2500);
    let first_run = succeeded(LENS_FIRST_RUN, "deploy", 1, 700, 600).at(now);
    LensHistory {
        jobs: vec![
            job(wildcard),
            kept,
            job(first_run),
            skipped,
            compared,
            baseline,
            job(unknown_end),
            removed,
        ],
        samples: vec![compared_samples, baseline_samples],
    }
}

/// The records `GET /v1/records` answers beside [`lens_history`]: [`LENS_KEPT`]'s, and [`LENS_ORPHAN`]'s, whose
/// job the history no longer has.
pub fn lens_records() -> Records {
    lens_records_at(NOW)
}

/// [`lens_records`] with `now` for [`NOW`].
pub fn lens_records_at(now: u64) -> Records {
    let record = |job: u64, ago: u64| Record {
        job,
        repository: LEDGER.to_owned(),
        profile: "jvm".to_owned(),
        workflow: Some("CI".to_owned()),
        name: Some("test".to_owned()),
        created_at: now - ago,
        bytes: 2 << 30,
        image: format!("runnerd-record:{job}"),
    };
    Records {
        records: vec![record(LENS_ORPHAN, 6 * 86_400), record(LENS_KEPT, 330)],
        ..Records::default()
    }
}

/// When runnerd watched from `since` to `now`: all of it but [`LENS_UNWATCHED`] to [`LENS_WATCHED_AGAIN`], each
/// taken with `now` for [`NOW`].
pub fn lens_observed(since: u64, now: u64) -> Vec<Span> {
    vec![
        Span {
            from: since,
            to: now - (NOW - LENS_UNWATCHED),
        },
        Span {
            from: now - (NOW - LENS_WATCHED_AGAIN),
            to: now,
        },
    ]
}

/// The lens's timeline: the hour before [`NOW`], runnerd not watching from [`LENS_UNWATCHED`] to
/// [`LENS_WATCHED_AGAIN`].
pub fn lens_timeline() -> SlotTimeline {
    let jobs: Vec<JobSummary> = lens_history()
        .jobs
        .into_iter()
        .map(|job| job.summary)
        .collect();
    SlotTimeline::of(
        &jobs,
        Span {
            from: LENS_SINCE,
            to: NOW,
        },
        lens_observed(LENS_SINCE, NOW),
        NOW - 30 * 86_400,
    )
}

/// `id` beside the job `with` names, or its baseline, as the daemon answers from [`lens_history`]; `None` without
/// either job.
pub fn lens_comparison(id: u64, with: Option<u64>) -> Option<JobComparison> {
    let history = lens_history();
    JobComparison::in_history(id, with, &history.jobs, |job| {
        history
            .samples
            .iter()
            .find(|samples| samples.job == job.summary.id)
            .map(|samples| StepUsage::of(&samples.samples, &job.steps))
            .unwrap_or_default()
    })
}

/// A job's record mark as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkFact {
    /// The job.
    pub job: u64,
    /// `record kept`, `record removed` or `no record`.
    pub words: &'static str,
    /// Its tone.
    pub tone: Tone,
}

/// A stretch of a lane without a job, as the timeline shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapFact {
    /// The lane's profile.
    pub profile: String,
    /// The lane's slot.
    pub slot: usize,
    /// The stretch.
    pub span: Span,
    /// `no job` or `unknown`.
    pub words: &'static str,
}

/// One of a comparison's rows: a figure or a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaFact {
    /// The figure's label or the step's name.
    pub label: String,
    /// How the step was paired, if it is a step and was not simply matched: `only this run`, `ambiguous`.
    pub matched: Option<&'static str>,
    /// This run's value: `2m 32s`, or `unknown`.
    pub this: String,
    /// The baseline's.
    pub baseline: String,
    /// The change: `+30s (+25%)`, `same` or `unknown`.
    pub change: String,
    /// Its tone.
    pub tone: Tone,
}

/// What every client must say of the lenses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LensFacts {
    /// Each marked job's mark, newest first.
    pub marks: Vec<MarkFact>,
    /// Each lane's gaps, by profile, then slot.
    pub gaps: Vec<GapFact>,
    /// The jobs whose bar has an end core words, and the words: `end unknown`, `waiting`.
    pub ends: Vec<EndFact>,
    /// [`LENS_COMPARED`] beside its baseline: the figures, then the steps' times.
    pub compared: Vec<DeltaFact>,
    /// What [`LENS_FIRST_RUN`]'s comparison says of its baseline.
    pub first_run: String,
    /// What [`LENS_COMPARED`] asked against [`LENS_KEPT`] says of it.
    pub incompatible: String,
}

/// A bar's end in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndFact {
    /// The job.
    pub job: u64,
    /// The words.
    pub words: &'static str,
}

/// The lenses' facts, as core words them.
pub fn lens_facts() -> LensFacts {
    let history = lens_history();
    let timeline = lens_timeline();
    let marks = history
        .jobs
        .iter()
        .filter_map(|job| {
            let mark = job.summary.record_mark?;
            Some(MarkFact {
                job: job.summary.id,
                words: mark.words()?,
                tone: mark.tone(),
            })
        })
        .collect();
    let gaps = timeline
        .lanes
        .iter()
        .flat_map(|lane| {
            lane.gaps(&timeline).into_iter().map(|gap| GapFact {
                profile: lane.profile.clone(),
                slot: lane.slot,
                span: gap.span,
                words: gap.kind.words(),
            })
        })
        .collect();
    let ends = timeline
        .lanes
        .iter()
        .flat_map(|lane| &lane.bars)
        .chain(&timeline.unassigned)
        .filter_map(|bar| {
            Some(EndFact {
                job: bar.job,
                words: bar.end_words()?,
            })
        })
        .collect();
    let compared = lens_comparison(LENS_COMPARED, None)
        .map(|compared| delta_facts(&compared))
        .unwrap_or_default();
    let said = |id: u64, with: Option<u64>| {
        lens_comparison(id, with)
            .and_then(|comparison| comparison.baseline.words())
            .unwrap_or_default()
    };
    LensFacts {
        marks,
        gaps,
        ends,
        compared,
        first_run: said(LENS_FIRST_RUN, None),
        incompatible: said(LENS_COMPARED, Some(LENS_KEPT)),
    }
}

/// A comparison's rows: its figures, then its steps' times.
fn delta_facts(compared: &JobComparison) -> Vec<DeltaFact> {
    compared
        .figures
        .iter()
        .map(|figure| DeltaFact {
            label: figure.figure.label().to_owned(),
            matched: None,
            this: figure.this_words(),
            baseline: figure.baseline_words(),
            change: figure.change_words(),
            tone: figure.tone(),
        })
        .chain(compared.steps.iter().map(|step| DeltaFact {
            label: step.name.clone(),
            matched: step.matched.words(),
            this: step.time.this_words(),
            baseline: step.time.baseline_words(),
            change: step.time.change_words(),
            tone: step.time.tone(),
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use runnerd_core::api::{Baseline, GapKind, JobQuery, RecordFilter, StepMatch, Unavailable};

    use super::*;

    // The lens fixtures are as awkward as their docs say; a change that makes them easy fails here, not silently in
    // every client's test.

    #[test]
    fn the_lens_jobs_hold_a_literal_search_every_record_mark_and_a_record_without_history() {
        let history = lens_history();
        let summaries: Vec<JobSummary> =
            history.jobs.iter().map(|job| job.summary.clone()).collect();
        let found = |query: JobQuery| -> Vec<u64> {
            query
                .page(&summaries)
                .unwrap()
                .page
                .jobs
                .iter()
                .map(|job| job.id)
                .collect()
        };
        assert_eq!(
            found(JobQuery {
                q: Some("0%_".to_owned()),
                ..JobQuery::default()
            }),
            [LENS_WILDCARD],
            "a search for `%` and `_` finds them as written"
        );
        assert_eq!(
            found(JobQuery {
                record: Some(RecordFilter::Kept),
                ..JobQuery::default()
            }),
            [LENS_KEPT]
        );
        let facts = lens_facts();
        let marks: Vec<&str> = facts.marks.iter().map(|mark| mark.words).collect();
        assert_eq!(marks, ["record kept", "no record", "record removed"]);
        assert!(
            !summaries.iter().any(|job| job.id == LENS_ORPHAN)
                && lens_records()
                    .records
                    .iter()
                    .any(|record| record.job == LENS_ORPHAN),
            "a record kept whose history was pruned"
        );
    }

    #[test]
    fn the_lens_timeline_holds_an_old_lane_an_unknown_end_unassigned_jobs_and_unwatched_time() {
        let facts = lens_facts();
        let timeline = lens_timeline();
        let lanes: Vec<(&str, usize)> = timeline
            .lanes
            .iter()
            .map(|lane| (lane.profile.as_str(), lane.slot))
            .collect();
        assert_eq!(lanes, [("jvm", 1), ("jvm", 2), ("legacy", 3)]);
        assert_eq!(
            timeline
                .unassigned
                .iter()
                .map(|bar| bar.job)
                .collect::<Vec<_>>(),
            [LENS_WILDCARD]
        );
        let lost = &timeline.lanes[2].bars[0];
        assert_eq!(lost.end.at(), None, "the lost job's end is unknown");
        assert!(
            facts.ends.contains(&EndFact {
                job: LENS_UNKNOWN_END,
                words: "end unknown"
            }),
            "{:?}",
            facts.ends
        );
        assert!(
            facts
                .gaps
                .iter()
                .any(|gap| gap.words == GapKind::Unknown.words()
                    && gap.span
                        == Span {
                            from: LENS_UNWATCHED,
                            to: LENS_WATCHED_AGAIN
                        }),
            "the hour runnerd was not watching is unknown in every lane: {:?}",
            facts.gaps
        );
        assert!(
            facts
                .gaps
                .iter()
                .any(|gap| gap.words == GapKind::NoJob.words()),
            "and watched time without a job is no job"
        );
    }

    #[test]
    fn the_lens_comparisons_hold_renamed_and_repeated_steps_sampled_memory_and_no_baseline() {
        let facts = lens_facts();
        let compared = lens_comparison(LENS_COMPARED, None).unwrap();
        let matched: Vec<StepMatch> = compared.steps.iter().map(|step| step.matched).collect();
        assert_eq!(
            matched,
            [
                StepMatch::Matched,
                StepMatch::Matched,
                StepMatch::OnlyThis,
                StepMatch::Ambiguous,
                StepMatch::Ambiguous,
                StepMatch::OnlyBaseline,
                StepMatch::Ambiguous,
                StepMatch::Ambiguous
            ],
            "two steps of one name on each side pair with none, and the baseline's are listed too"
        );
        assert!(
            compared.steps[1]
                .memory
                .is_some_and(|memory| memory.sampled),
            "the tests step's memory is sampled on both sides"
        );
        assert_eq!(
            lens_comparison(LENS_FIRST_RUN, None).unwrap().baseline,
            Baseline::Unavailable {
                reason: Unavailable::NoEarlierSuccess
            }
        );
        assert_eq!(
            lens_comparison(LENS_COMPARED, Some(LENS_KEPT))
                .unwrap()
                .baseline,
            Baseline::Incompatible { job: LENS_KEPT }
        );
        assert!(!facts.first_run.is_empty() && !facts.incompatible.is_empty());
        assert!(
            facts
                .compared
                .iter()
                .any(|row| row.label == "Run tests" && row.change == "+30s (+25%)"),
            "{:?}",
            facts.compared
        );
    }
}
