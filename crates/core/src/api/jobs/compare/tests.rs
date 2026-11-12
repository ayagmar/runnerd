use super::*;
use crate::api::{JobMetrics, JobRunner, LogSummary, Timeline};

fn summary(id: u64, queued_at: u64) -> JobSummary {
    JobSummary {
        id,
        repository: "halyard/ledger".to_owned(),
        run_id: id,
        run_attempt: 1,
        workflow: Some("CI".to_owned()),
        name: "build".to_owned(),
        branch: Some("main".to_owned()),
        url: None,
        labels: vec![],
        state: JobState::Completed,
        conclusion: Some("success".to_owned()),
        runner: Some(JobRunner {
            name: format!("r{id}"),
            profile: "jvm".to_owned(),
            slot: 1,
            memory_cap: None,
            cpu_cap: None,
        }),
        timeline: Timeline {
            queued_at,
            started_at: Some(queued_at + 10),
            finished_at: Some(queued_at + 100),
            ..Timeline::default()
        },
        metrics: JobMetrics {
            memory_peak: Some(2 << 30),
            ..JobMetrics::default()
        },
        log: LogSummary::default(),
        runner_fault: false,
        record_mark: None,
    }
}

fn step(number: u32, name: &str, from: u64, to: Option<u64>) -> Step {
    Step {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: Some(from),
        completed_at: to,
    }
}

fn job(summary: JobSummary, steps: Vec<Step>) -> Job {
    Job {
        summary,
        steps,
        record: None,
        admission: None,
    }
}

#[test]
fn the_baseline_is_the_latest_earlier_success_of_the_same_job_on_its_branch_run_here() {
    let this = summary(10, 1000);
    let mut failed = summary(9, 900);
    failed.conclusion = Some("failure".to_owned());
    let mut other_branch = summary(8, 800);
    other_branch.branch = Some("dev".to_owned());
    let mut elsewhere = summary(7, 700);
    elsewhere.state = JobState::Elsewhere;
    elsewhere.runner = None;
    let mut renamed = summary(6, 600);
    renamed.name = "build (linux)".to_owned();
    let later = summary(11, 1100);
    let picked = summary(5, 500);
    let older = summary(4, 400);
    let history = [
        failed,
        other_branch,
        elsewhere,
        renamed,
        later,
        picked.clone(),
        older,
    ];
    assert_eq!(JobComparison::baseline_of(&this, &history).unwrap().id, 5);
    let mut no_branch = this.clone();
    no_branch.branch = None;
    assert_eq!(
        JobComparison::baseline_of(&no_branch, &history).unwrap_err(),
        Unavailable::UnknownBranch,
        "an unknown branch matches nothing, not other unknown branches"
    );
    let mut no_workflow = this.clone();
    no_workflow.workflow = None;
    assert_eq!(
        JobComparison::baseline_of(&no_workflow, &history).unwrap_err(),
        Unavailable::UnknownWorkflow
    );
    assert_eq!(
        JobComparison::baseline_of(&this, &[]).unwrap_err(),
        Unavailable::NoEarlierSuccess
    );
}

#[test]
fn steps_pair_by_a_name_each_job_has_once_never_by_position() {
    let this = job(
        summary(10, 1000),
        vec![
            step(1, "Set up job", 1010, Some(1012)),
            step(2, "Run tests", 1012, Some(1080)),
            step(3, "Run lint", 1080, Some(1085)),
            step(4, "Run lint", 1085, Some(1090)),
            step(5, "Upload", 1090, None),
        ],
    );
    let baseline = job(
        summary(5, 500),
        vec![
            step(1, "Set up job", 510, Some(511)),
            step(2, "Run lint", 511, Some(520)),
            step(3, "Run unit tests", 520, Some(560)),
            step(4, "Upload", 560, Some(565)),
        ],
    );
    let compared = JobComparison::of(
        &this,
        &[],
        BaselineRead::Found {
            job: Box::new(baseline),
            usage: Vec::new(),
            asked: false,
        },
    );
    let rows: Vec<String> = compared
        .steps
        .iter()
        .map(|step| {
            let number = |number: Option<u32>| number.map_or("-".to_owned(), |n| n.to_string());
            format!(
                "{} {:?} {} {} {}",
                step.name,
                step.matched,
                number(step.this),
                number(step.baseline),
                step.time.change_words()
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            "Set up job Matched 1 1 +1s (+100%)",
            "Run tests OnlyThis 2 - unknown",
            "Run lint Ambiguous 3 - unknown",
            "Run lint Ambiguous 4 - unknown",
            "Upload Matched 5 4 unknown",
            "Run lint Ambiguous - 2 unknown",
            "Run unit tests OnlyBaseline - 3 unknown",
        ],
        "a renamed step is unpaired, a duplicated name is never paired, an unfinished step's time is unknown"
    );
    assert_eq!(
        compared.steps[0].time.tone(),
        Tone::Neutral,
        "1s is under the floor"
    );
}

#[test]
fn a_steps_resources_are_compared_only_where_both_steps_samples_cover_them() {
    let steps = || {
        vec![
            step(1, "Run tests", 0, Some(60)),
            step(2, "Run lint", 60, Some(64)),
        ]
    };
    let this = job(summary(10, 1000), steps());
    let baseline = job(summary(5, 500), steps());
    let usage = |number, samples| StepUsage {
        number,
        samples,
        memory_peak: 1 << 30,
        cpu_average: 1500,
    };
    let compared = JobComparison::of(
        &this,
        &[usage(1, 30), usage(2, 2)],
        BaselineRead::Found {
            job: Box::new(baseline),
            // 10 samples over 60s is less than half the 30 expected: the baseline's tests step is not covered.
            usage: vec![usage(1, 10), usage(2, 1)],
            asked: true,
        },
    );
    assert_eq!(
        compared.steps[0].memory, None,
        "a sparse side leaves it out"
    );
    let lint = &compared.steps[1];
    assert_eq!(
        lint.memory
            .map(|memory| (memory.sampled, memory.change_words())),
        Some((true, "same".to_owned()))
    );
    assert!(matches!(
        compared.baseline,
        Baseline::Chosen {
            job: 5,
            asked: true,
            ..
        }
    ));
}

#[test]
fn whole_job_figures_are_given_only_for_final_jobs_and_never_zero_for_unknown() {
    let mut running = summary(10, 1000);
    running.state = JobState::Running;
    running.timeline.finished_at = None;
    running.metrics.memory_peak = Some(9 << 30);
    let compared = JobComparison::of(
        &job(running, vec![]),
        &[],
        BaselineRead::Found {
            job: Box::new(job(summary(5, 500), vec![])),
            usage: Vec::new(),
            asked: false,
        },
    );
    let figure = |kind: FigureKind| {
        *compared
            .figures
            .iter()
            .find(|figure| figure.figure == kind)
            .unwrap()
    };
    assert_eq!(
        figure(FigureKind::Wait).change_words(),
        "same",
        "its wait is over"
    );
    let memory = figure(FigureKind::MemoryPeak);
    assert_eq!(
        (
            memory.this,
            memory.this_words(),
            memory.change(),
            memory.tone()
        ),
        (None, "unknown".to_owned(), ChangeKind::Unknown, Tone::Quiet)
    );
    assert_eq!(figure(FigureKind::CpuTime).baseline_words(), "unknown");
}

#[test]
fn a_change_is_worded_and_toned_by_core() {
    let delta = |figure, this, baseline| FigureDelta {
        figure,
        this: Some(this),
        baseline: Some(baseline),
        sampled: false,
    };
    let slower = delta(FigureKind::Run, 130, 100);
    assert_eq!(
        (slower.change_words(), slower.tone()),
        ("+30s (+30%)".to_owned(), Tone::Warn)
    );
    let lighter = delta(FigureKind::MemoryPeak, 1 << 30, 2 << 30);
    assert_eq!(lighter.tone(), Tone::Good);
    assert!(
        lighter.change_words().starts_with('-'),
        "{}",
        lighter.change_words()
    );
    let busier = delta(FigureKind::CpuAverage, 4000, 1000);
    assert_eq!(busier.tone(), Tone::Neutral, "a CPU average is not a cost");
    assert_eq!(delta(FigureKind::Wait, 5, 0).change_words(), "+5s");
}

#[test]
fn without_a_baseline_the_reason_is_the_answer() {
    let this = job(
        summary(10, 1000),
        vec![step(1, "Run tests", 1010, Some(1020))],
    );
    let unavailable = JobComparison::of(
        &this,
        &[],
        BaselineRead::Unavailable(Unavailable::NoEarlierSuccess),
    );
    assert_eq!(
        unavailable.baseline.words().as_deref(),
        Some("no earlier run of this job on its branch succeeded here")
    );
    assert!(unavailable.steps.is_empty(), "no steps to pair");
    assert!(
        unavailable
            .figures
            .iter()
            .all(|figure| figure.baseline.is_none())
    );
    let incompatible = JobComparison::of(&this, &[], BaselineRead::Incompatible(3));
    assert_eq!(
        incompatible.baseline.words().as_deref(),
        Some("job 3 is not a run of this job: another repository, workflow or name")
    );
}

#[test]
fn a_hostile_step_name_reaches_the_comparison_plain() {
    let named = || vec![step(1, "Run \u{1b}]52;c;aGk=\u{7}evil\r", 0, Some(5))];
    let compared = JobComparison::of(
        &job(summary(10, 1000), named()),
        &[],
        BaselineRead::Found {
            job: Box::new(job(summary(5, 500), named())),
            usage: Vec::new(),
            asked: false,
        },
    );
    assert_eq!(compared.steps.len(), 1);
    assert!(
        !compared.steps[0].name.chars().any(char::is_control),
        "{:?}",
        compared.steps[0].name
    );
    assert_eq!(compared.steps[0].matched, StepMatch::Matched);
}

#[test]
fn a_newer_daemons_answers_read_as_other() {
    let baseline: Baseline = serde_json::from_str(r#"{"kind": "estimated"}"#).unwrap();
    assert_eq!(baseline, Baseline::Other);
    let reason: Unavailable = serde_json::from_str("\"pruned\"").unwrap();
    assert_eq!(reason, Unavailable::Other);
    let matched: StepMatch = serde_json::from_str("\"fuzzy\"").unwrap();
    assert_eq!(matched, StepMatch::Other);
    let figure: FigureKind = serde_json::from_str("\"disk\"").unwrap();
    assert_eq!(figure.label(), "Figure");
}
