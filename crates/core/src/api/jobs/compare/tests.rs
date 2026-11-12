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

/// `this`, with its steps as a comparison reads them, beside `baseline`.
fn of(this: &Job, usage: &[StepUsage], baseline: BaselineRead) -> JobComparison {
    JobComparison::of(
        &this.summary,
        &ComparedSteps::of(&this.steps),
        usage,
        baseline,
    )
}

/// `job`, found as a baseline.
fn found(job: Job, usage: Vec<StepUsage>, asked: bool) -> BaselineRead {
    BaselineRead::Found {
        steps: ComparedSteps::of(&job.steps),
        job: Box::new(job.summary),
        usage,
        asked,
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
    let compared = of(&this, &[], found(baseline, Vec::new(), false));
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

/// A sample every 2 s after `from` up to `to`, at `memory` bytes.
fn samples(from: u64, to: u64, memory: u64) -> Vec<Sample> {
    (from + 2..=to)
        .step_by(2)
        .map(|at| Sample {
            at,
            cpu: 1500,
            memory,
            network_received: 0,
            network_sent: 0,
        })
        .collect()
}

#[test]
fn a_steps_resources_are_compared_only_where_both_steps_samples_cover_it_from_start_to_end() {
    let steps = |start: u64| {
        vec![
            step(1, "Run tests", start, Some(start + 60)),
            step(2, "Run lint", start + 60, Some(start + 64)),
        ]
    };
    let this = job(summary(10, 1000), steps(1100));
    let this_usage = StepUsage::of(&samples(1100, 1164, 3 << 30), &this.steps);
    let baseline = job(summary(5, 500), steps(600));
    let memory = |baseline_samples: &[Sample]| {
        let compared = of(
            &this,
            &this_usage,
            found(
                baseline.clone(),
                StepUsage::of(baseline_samples, &baseline.steps),
                true,
            ),
        );
        compared
            .steps
            .iter()
            .map(|step| step.memory)
            .collect::<Vec<_>>()
    };
    let whole = memory(&samples(600, 664, 2 << 30));
    assert_eq!(
        whole[0].map(|memory| (memory.sampled, memory.change_words())),
        Some((true, "+1G (+50%)".to_owned()))
    );
    assert!(whole[1].is_some(), "a short step's two samples cover it");

    // Its first half pruned: 15 of the 30 samples, its peak perhaps in the half gone.
    let pruned = memory(&samples(630, 664, 2 << 30));
    assert_eq!(
        pruned[0], None,
        "a step whose samples start late is not compared"
    );
    // Sampling stopped for 20 s in the middle: 20 samples of 30.
    let mut outage = samples(600, 620, 2 << 30);
    outage.extend(samples(640, 664, 2 << 30));
    assert_eq!(
        memory(&outage)[0],
        None,
        "a step with a hole in its samples is not compared"
    );
    assert!(
        memory(&outage)[1].is_some(),
        "the lint step after it is whole"
    );
}

#[test]
fn whole_job_figures_are_given_only_for_final_jobs_and_never_zero_for_unknown() {
    let mut running = summary(10, 1000);
    running.state = JobState::Running;
    running.timeline.finished_at = None;
    running.metrics.memory_peak = Some(9 << 30);
    let compared = of(
        &job(running, vec![]),
        &[],
        found(job(summary(5, 500), vec![]), Vec::new(), false),
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
    let unavailable = of(
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
    let incompatible = of(&this, &[], BaselineRead::Incompatible(3));
    assert_eq!(
        incompatible.baseline.words().as_deref(),
        Some("job 3 is not a run of this job: another repository, workflow or name")
    );
}

#[test]
fn a_hostile_step_name_reaches_the_comparison_plain() {
    let named = || vec![step(1, "Run \u{1b}]52;c;aGk=\u{7}evil\r", 0, Some(5))];
    let compared = of(
        &job(summary(10, 1000), named()),
        &[],
        found(job(summary(5, 500), named()), Vec::new(), false),
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

#[test]
fn a_comparison_reads_a_bounded_count_and_size_of_steps_and_says_it_left_some_out() {
    let many = |count: u32, name: &dyn Fn(u32) -> String| {
        (1..=count)
            .map(|number| step(number, &name(number), 0, Some(1)))
            .collect::<Vec<_>>()
    };
    let compare = |steps: Vec<Step>| {
        of(
            &job(summary(10, 1000), steps.clone()),
            &[],
            found(job(summary(5, 500), steps), Vec::new(), false),
        )
    };
    let counted = compare(many(600, &|number| format!("step {number}")));
    assert_eq!(counted.steps.len(), COMPARE_STEPS_MAX);
    assert_eq!(
        counted.steps_cut_words(),
        Some(
            "some steps are not compared: a run has more steps, or longer step names, than a comparison reads"
        )
    );

    // Names of 2,000 bytes are read from their first 800, and 163 of them fill the bytes a comparison reads.
    let long = compare(many(200, &|number| {
        format!("{number:04}{}", "é".repeat(998))
    }));
    assert_eq!(long.steps.len(), COMPARE_STEP_BYTES / 800);
    assert!(
        long.steps_cut,
        "the steps past the bytes read are said to be left out"
    );
    assert!(
        long.steps
            .iter()
            .all(|step| step.matched == StepMatch::Matched)
    );

    // One name past every bound is still read, cut, and nothing is said left out.
    let huge = compare(many(1, &|_| "x".repeat(2 * COMPARE_STEP_BYTES)));
    assert_eq!(huge.steps.len(), 1);
    assert!(
        huge.steps[0].name.len() <= 200,
        "{}",
        huge.steps[0].name.len()
    );
    assert!(
        !huge.steps_cut,
        "a job's one step is read whole as far as names go"
    );
    assert_eq!(compare(many(3, &|n| n.to_string())).steps_cut_words(), None);
}

#[test]
fn samples_are_read_only_for_a_comparison_with_a_baseline() {
    let history = [
        job(
            summary(10, 1000),
            vec![step(1, "Run tests", 1010, Some(1020))],
        ),
        job(summary(5, 500), vec![step(1, "Run tests", 510, Some(520))]),
    ];
    let mut other = summary(7, 700);
    other.name = "lint".to_owned();
    let history = [history[0].clone(), history[1].clone(), job(other, vec![])];
    let reads = std::cell::Cell::new(0);
    let usage = |_: &Job| {
        reads.set(reads.get() + 1);
        Vec::new()
    };
    let incompatible = JobComparison::in_history(10, Some(7), &history, usage).unwrap();
    assert_eq!(incompatible.baseline, Baseline::Incompatible { job: 7 });
    let first = JobComparison::in_history(5, None, &history, usage).unwrap();
    assert!(
        matches!(first.baseline, Baseline::Unavailable { .. }),
        "{:?}",
        first.baseline
    );
    assert_eq!(reads.get(), 0, "no baseline, no samples read");
    JobComparison::in_history(10, None, &history, usage).unwrap();
    assert_eq!(reads.get(), 2, "both jobs' samples, once each");
}
