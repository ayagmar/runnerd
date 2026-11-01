//! `/jobs/{id}`: one job's phases, its steps on a shared time axis, its CPU and memory over time against what
//! admission reserved and the caps its container ran with (or, for records without them, the profile's limits as
//! configured now), and its log, live while it runs, and the same job's latest runs (wait, run time, outcome and
//! memory peak) beside it. A failed job whose runner was kept as a record gets the command that opens a shell in it.

use std::fmt::Write as _;

use maud::{Markup, html};
use runnerd_core::api::{
    BuildCacheCounts, ChartLine, ChartLineKind, Job, JobPage, JobQuery, JobState, JobSummary,
    OutcomeKind, PhaseKind, ProfileStatus, RecordNote, RecordState, Sample, SampleSeries, Status,
    in_cores,
};
use runnerd_core::client::{Client, ClientError};

use super::{
    Live, Parts, badge, digest, outcome_class, outcome_kind_class, page, profile_class, time_ago,
};
use crate::chart::{Band, Base, Limit, Series, TimeChart, nice_top};
use crate::format;

/// Log lines the page shows; the full log is at `/jobs/{id}/log`.
pub(crate) const LOG_TAIL: u64 = 1000;
/// Runs of the same job the page compares.
const RUNS: u32 = 30;

/// Everything the job page shows.
#[derive(Debug, Clone)]
pub(crate) struct JobView {
    pub(crate) now: u64,
    pub(crate) job: Job,
    pub(crate) samples: SampleSeries,
    /// The job's profile as configured now, while it is: its memory and CPUs are what the charts show for records
    /// without their container's caps, which a reload since may have changed.
    pub(crate) profile: Option<ProfileStatus>,
    /// The number of the first line in `log`.
    pub(crate) log_from: u64,
    pub(crate) log: Vec<String>,
    /// The latest runs of the same job (repository, workflow and name), oldest first: this one among them unless 30
    /// newer ran since.
    pub(crate) runs: Vec<JobSummary>,
    /// The colour class of the job's profile, which its runs' memory peaks are drawn in.
    pub(crate) profile_class: String,
}

impl JobView {
    pub(crate) async fn load(daemon: &Client, id: u64, now: u64) -> Result<JobView, ClientError> {
        let mut job: Job = daemon.get(&format!("/v1/jobs/{id}")).await?;
        let samples: SampleSeries = daemon.get(&format!("/v1/jobs/{id}/samples")).await?;
        // Samples stored since the job was read raise a running job's peaks, as those that arrive later do.
        for sample in &samples.samples {
            job.observe(sample);
        }
        let status: Status = daemon.get("/v1/status").await?;
        let log_from = job.summary.log.lines.saturating_sub(LOG_TAIL);
        let log = daemon
            .text(&format!("/v1/jobs/{id}/logs?from={log_from}"))
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let page: JobPage = daemon
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    repository: Some(job.summary.repository.clone()),
                    workflow: job.summary.workflow.clone(),
                    name: Some(job.summary.name.clone()),
                    limit: Some(RUNS),
                    ..JobQuery::default()
                },
            )
            .await?;
        // Without a workflow the query matches every workflow's job of that name.
        let mut runs: Vec<JobSummary> = page
            .jobs
            .into_iter()
            .filter(|run| run.workflow == job.summary.workflow)
            .collect();
        runs.reverse();
        let profile_class = job.summary.runner.as_ref().map_or_else(
            || "p-none".to_owned(),
            |runner| profile_class(&status, &runner.profile),
        );
        Ok(JobView {
            runs,
            profile_class,
            profile: configured(&status, &job),
            now,
            job,
            samples,
            log_from,
            log,
        })
    }

    /// A sample as it arrives: the charts take it, and the job's peaks rise to it while it runs.
    pub(crate) fn add_sample(&mut self, sample: Sample) {
        self.job.observe(&sample);
        self.samples.samples.push(sample);
    }
}

/// The job's profile as configured now.
fn configured(status: &Status, job: &Job) -> Option<ProfileStatus> {
    let runner = job.summary.runner.as_ref()?;
    status
        .profiles
        .iter()
        .find(|profile| profile.name == runner.profile)
        .cloned()
}

impl JobView {
    /// Whether the job still runs, so its page is live.
    fn live(&self) -> bool {
        !self.job.summary.state.is_final()
    }
}

/// The live regions, by id, in the order the page has them, with their parts.
pub(crate) fn regions(view: &JobView) -> [(&'static str, Markup, Parts); 5] {
    let region = |id, render: &dyn Fn(&Parts) -> Markup| {
        let parts = Parts::default();
        let markup = render(&parts);
        (id, markup, parts)
    };
    [
        region("job-head", &|_| head(view)),
        region("timeline", &|parts| timeline(view, parts)),
        region("steps", &|parts| steps(view, parts)),
        region("metrics", &|parts| metrics(view, parts)),
        region("runs", &|_| runs(view)),
    ]
}

/// The whole page.
pub(crate) fn render(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let url = format!("/live/jobs/{}", summary.id);
    let [head, timeline, steps, metrics, runs] = regions(view);
    let live = view.live().then(|| Live {
        url: &url,
        at: view.now,
        log: Some(view.log_from + view.log.len() as u64),
        regions: [&head, &timeline, &steps, &metrics, &runs]
            .iter()
            .map(|(id, markup, _)| (*id, digest(&markup.0)))
            .collect(),
    });
    let main = html! {
        (head.1)
        (timeline.1)
        (steps.1)
        (metrics.1)
        @if let Some(admission) = &view.job.admission {
            p.admission { (format::sentence(&admission.line())) }
        }
        (runs.1)
        (record(view))
        (log(view))
    };
    page(&summary.title().to_string(), None, live.as_ref(), &main)
}

/// The title, outcome and where the job came from.
pub(crate) fn head(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        header #job-head .job-head {
            p.crumbs {
                a href=(format!("/history?repository={}", summary.repository)) { (summary.repository) }
                @if let Some(workflow) = &summary.workflow { " · " (workflow) }
            }
            h1 { (summary.title().name) " " (badge(summary)) }
            // Waited, ran and the queued time are the timeline's.
            p.job-facts {
                @if let Some(branch) = &summary.branch { code { (branch) } }
                @let run = html! { "run " (summary.run_id) @if summary.run_attempt > 1 { ", attempt " (summary.run_attempt) } };
                @if let Some(url) = &summary.url {
                    a.external href=(url) rel="noreferrer" aria-label=(format!("Run {} on GitHub", summary.run_id)) { (run) }
                } @else {
                    span { (run) }
                }
                @if let Some(runner) = &summary.runner { span { (runner.profile) " " (runner.slot) } }
            }
        }
    }
}

/// The phases from queued to cleaned up, as one bar.
pub(crate) fn timeline(view: &JobView, parts: &Parts) -> Markup {
    let summary = &view.job.summary;
    let now = view.now;
    let phases = summary.phases();
    let live = view.live();
    html! {
        section #timeline .timeline aria-labelledby="timeline-title" {
            h2 #timeline-title .visually-hidden { "Timeline" }
            // Named by the legend, whose figures are parts that change on their own.
            div.phases role="img" aria-labelledby="phase-legend" {
                @for phase in &phases {
                    // Each phase grows by its seconds; an ongoing one follows the page's clock.
                    @let (class, from, to) = (phase_class(phase.kind), phase.from, phase.to);
                    @let seconds = phase.seconds(now);
                    @let follows = live && to.is_none();
                    @let share = format!("--share: {seconds}");
                    div class=(format!("phase {class}{}", if to.is_none() { " ongoing" } else { "" }))
                        style=(if follows { parts.clocked(share) } else { share })
                        data-style:--share=[follows.then(|| format!("Math.max(0, $shown.at - {from})"))] {}
                }
            }
            ol #phase-legend .phase-legend {
                @for phase in &phases {
                    @let class = phase_class(phase.kind);
                    li class=(class) {
                        span.swatch aria-hidden="true" {}
                        span { (phase.kind.label()) }
                        @let id = format!("phase-{class}-time");
                        @let ongoing = phase.to.is_none();
                        (parts.html(&id, html! {
                            span #(id) class=(if ongoing { "figure ongoing" } else { "figure" }) {
                                (format::duration(phase.seconds(now))) @if ongoing { "…" }
                            }
                        }))
                        @if phase.kind == PhaseKind::Waiting {
                            @if let Some(note) = summary.unseen_note(now) { span.phase-note { (note) } }
                        }
                    }
                }
            }
            details.marks {
                summary { "Exact times" }
                dl {
                    @for milestone in summary.timeline.milestones() {
                        div { dt { (milestone.label) } dd { (format::timestamp(milestone.at)) } }
                    }
                }
            }
        }
    }
}

/// The CSS class of a phase of the timeline.
fn phase_class(kind: PhaseKind) -> &'static str {
    match kind {
        PhaseKind::Waiting => "wait",
        PhaseKind::Starting => "start",
        PhaseKind::Running => "run",
        PhaseKind::CleaningUp => "clean",
    }
}

/// The steps on one axis from the job's start to its end, with their durations.
pub(crate) fn steps(view: &JobView, parts: &Parts) -> Markup {
    let job = &view.job;
    let now = view.now;
    let start = job
        .summary
        .timeline
        .started_at
        .or_else(|| job.steps.iter().filter_map(|step| step.started_at).min());
    let end = job.summary.timeline.finished_at.unwrap_or(now);
    let live = view.live();
    // The bars' places are seconds since the start against the track's `--span`, which follows the page's clock
    // while the job runs, as a running step's end does.
    let span = start.map(|start| end.saturating_sub(start).max(1));
    let follow = |start: u64| format!("Math.max(1, $shown.at - {start})");
    html! {
        section #steps .steps aria-labelledby="steps-title" {
            h2 #steps-title .visually-hidden { "Steps" }
            @if job.steps.is_empty() {
                p.muted { "GitHub has not reported any steps yet." }
            } @else {
                // A step that ran lights up its stretch of the memory and CPU charts on hover, focus or tap.
                ol.step-list "data-signals:_step__ifmissing"="0"
                    style=[span.map(|span| format!("--span: {span}")).map(|style| if live { parts.clocked(style) } else { style })]
                    data-style:--span=[start.filter(|_| live).map(follow)] {
                    @for (index, step) in job.steps.iter().enumerate() {
                        @let outcome = step.outcome();
                        @let class = outcome_kind_class(outcome.kind);
                        @let seconds = step.took(now);
                        @let usage = step.started_at.and_then(|from| view.samples.usage_between(from, from + seconds.unwrap_or(0)));
                        @let number = index + 1;
                        @let enter = format!("$_step = {number}");
                        li class=(class) tabindex=[usage.is_some().then_some("0")]
                            data-on:pointerenter=[usage.is_some().then_some(&enter)]
                            data-on:focus=[usage.is_some().then_some(&enter)]
                            data-on:pointerleave=[usage.is_some().then_some("evt.pointerType == 'mouse' && ($_step = 0)")]
                            data-on:blur=[usage.is_some().then_some("$_step = 0")]
                            data-class:linked=[usage.is_some().then(|| format!("$_step == {number}"))] {
                            span.step-name {
                                (step.name)
                                @if let Some(usage) = &usage {
                                    @let id = format!("step-{number}-usage");
                                    (parts.html(&id, html! { span.visually-hidden #(id) { ", " (usage.description()) } }))
                                }
                            }
                            @let id = format!("step-{number}-time");
                            (parts.html(&id, html! {
                                span.step-time #(id) {
                                    @match (seconds, outcome.kind) {
                                        // A step that ended without running says how it ended: `skipped`.
                                        (Some(0) | None, OutcomeKind::Ended) => (outcome.label),
                                        (Some(seconds), OutcomeKind::Running) => { (format::duration(seconds)) "…" },
                                        (Some(seconds), _) => { (format::duration(seconds)) },
                                        (None, _) => "–",
                                    }
                                }
                            }))
                            span.step-track aria-hidden="true" {
                                @if let (Some(start), Some(span), Some(from)) = (start, span, step.started_at) {
                                    @let to = from + seconds.unwrap_or(0);
                                    @let left = from.saturating_sub(start);
                                    @let right = to.saturating_sub(start);
                                    @let follows = live && step.is_running();
                                    @let style = format!("--from: {left}; --to: {right}");
                                    span.step-bar style=(if follows { parts.clocked(style) } else { style })
                                        data-style:--to=[follows.then(|| format!("$shown.at - {start}"))] {}
                                    // Shown on the side of the track away from the bar while the row is lit up:
                                    // the charts may be off screen.
                                    @if let Some(usage) = &usage {
                                        @let id = format!("step-{number}-note");
                                        (parts.html(&id, html! {
                                            span #(id) class=(if left + right > span { "step-usage start" } else { "step-usage end" }) { (usage.description()) }
                                        }))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// CPU and memory over the job's life, against what admission reserved and its container's caps or else its profile's
/// limits as configured now, with the totals.
pub(crate) fn metrics(view: &JobView, parts: &Parts) -> Markup {
    let summary = &view.job.summary;
    let samples = &view.samples.samples;
    let start = samples
        .first()
        .map(|sample| sample.at)
        .or(summary.timeline.container_started_at)
        .unwrap_or(summary.timeline.queued_at);
    // A running job's charts reach the page's clock.
    let live = view.live();
    let last = samples.last().map_or(view.now, |sample| sample.at);
    let end = if live { last.max(view.now) } else { last }.max(start + 1);
    let gap = view.samples.interval.max(1) * 3;
    let memory_peak = samples.iter().map(|s| s.memory).max().unwrap_or(0);
    let cpu_peak = samples.iter().map(|s| s.cpu).max().unwrap_or(0);
    let LimitLines {
        memory: memory_line,
        cpu: cpu_line,
        configured_now,
    } = limit_lines(view);
    let highest = |lines: &[Limit]| lines.iter().map(|line| line.value).max().unwrap_or(0);
    let memory_limit = highest(&memory_line);
    let cpu_limit = highest(&cpu_line);
    let profile = summary
        .runner
        .as_ref()
        .map_or("profile", |runner| runner.profile.as_str());
    let time = |at: u64| format!("{} UTC", format::clock(at));
    let memory = TimeChart {
        title: "Memory over the job".to_owned(),
        start,
        end,
        window: None,
        live: live.then(|| "metrics-memory".to_owned()),
        series: vec![Series {
            label: "memory".to_owned(),
            class: "memory",
            points: samples.iter().map(|s| (s.at, s.memory)).collect(),
            area: true,
        }],
        limits: memory_line,
        value: format::bytes,
        time,
        gap,
    };
    let cpu = TimeChart {
        title: "CPU over the job".to_owned(),
        start,
        end,
        window: None,
        live: live.then(|| "metrics-cpu".to_owned()),
        series: vec![Series {
            label: "CPU".to_owned(),
            class: "cpu",
            points: samples.iter().map(|s| (s.at, s.cpu)).collect(),
            area: true,
        }],
        limits: cpu_line,
        value: in_cores,
        time,
        gap,
    };
    let bands = step_bands(view);
    html! {
        section #metrics .metrics aria-labelledby="metrics-title" {
            h2 #metrics-title .visually-hidden { "Resources" }
            @if configured_now {
                p.muted { "This record lacks its container's caps: the lines marked “" (profile) " now” are its limits in the config now, which may not be the ones the job ran with." }
            }
            @if samples.is_empty() {
                p.muted { "No samples: the runner's container never ran, or its samples are past `history.samples`." }
            } @else {
                div.charts {
                    div.chart-card {
                        h3 { "Memory" (parts.html("metrics-memory-peak", html! { span.reading #metrics-memory-peak { "sampled peak " (format::bytes(memory_peak)) } })) }
                        (memory.render_with(nice_top(memory_peak.max(memory_limit), Base::Binary), &bands.memory, parts))
                    }
                    div.chart-card {
                        h3 { "CPU" (parts.html("metrics-cpu-peak", html! { span.reading #metrics-cpu-peak { "sampled peak " (in_cores(cpu_peak)) } })) }
                        (cpu.render_with(nice_top(cpu_peak.max(cpu_limit), Base::Decimal), &bands.cpu, parts))
                    }
                }
            }
            (totals(view))
        }
    }
}

/// The job's totals over its whole run, which history keeps after its samples are deleted.
fn totals(view: &JobView) -> Markup {
    let metrics = &view.job.summary.metrics;
    html! {
        dl.totals {
            @for total in metrics.totals() {
                div { dt { (total.label) } dd { (total.value) } }
            }
            @if let Some(held) = view.job.summary.cpu_held(view.now) {
                div { dt { "CPU held back" } dd { (held) } }
            }
            (build_cache(&metrics.build_cache))
        }
    }
}

/// What the shared build cache served the job and took from it, for a job that used it at all.
fn build_cache(cache: &BuildCacheCounts) -> Markup {
    html! {
        @if let Some(activity) = cache.activity() {
            div {
                dt { "From the build cache" }
                dd { (activity.lookups_line()) ", " (activity.served_line()) }
            }
            div {
                dt { "To the build cache" }
                dd {
                    (activity.stored_line())
                    @if let Some(refused) = activity.refused_line() { ", " (refused) }
                }
            }
        }
    }
}

/// The same job's latest runs, oldest first: a column each, its wait under its run time on one scale, and its
/// memory peak under it against the largest; this run is outlined, as it is now. Each column opens its run. A live
/// region: as this run ends its column, the medians and the failures follow.
fn runs(view: &JobView) -> Markup {
    let compared = view.job.compare(&view.runs, view.now);
    let runs = &compared.runs;
    let longest = runs
        .iter()
        .map(|run| run.waited(view.now).unwrap_or(0) + run.ran(view.now).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let peak = compared.largest_peak.unwrap_or(0);
    let ratio = |part: u64, whole: u64| format!("{:.3}", crate::chart::ratio(part, whole));
    html! {
        @if runs.len() > 1 {
            section #runs class=(format!("runs {}", view.profile_class)) aria-labelledby="runs-title" {
                div.section-head {
                    h2 #runs-title { "Latest " (runs.len()) " runs of this job" }
                    a href=(format!("/history?repository={}", view.job.summary.repository)) { "History of " (view.job.summary.repository) }
                }
                dl.headline {
                    div { dt { "Median run" } dd { (compared.median_ran.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Median wait" } dd { (compared.median_waited.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Failed" } dd { (compared.failed) " of " (compared.finished) } }
                    div { dt { "Largest memory peak" } dd { (if peak > 0 { format::bytes(peak) } else { "–".to_owned() }) } }
                }
                p.run-scale aria-hidden="true" { "Wait and run, up to " (format::duration(longest)) }
                // On a narrow screen the runs scroll sideways, and start scrolled to the latest.
                div.run-scroll {
                    ol.run-bars style=(format!("--runs: {}", runs.len())) {
                        @for run in runs {
                            @let waited = run.waited(view.now).unwrap_or(0);
                            @let ran = run.ran(view.now).unwrap_or(0);
                            @let this = run.id == view.job.summary.id;
                            @let label = run_label(run, view.now);
                            li class=(if this { "this" } else { "" }) {
                                a href=(format!("/jobs/{}", run.id)) aria-label=(label) title=(label)
                                    aria-current=[this.then_some("page")] {
                                    span.run-time aria-hidden="true" {
                                        span class=(format!("ran {}", outcome_class(run))) style=(format!("--h: {}", ratio(ran, longest))) {}
                                        span.waited style=(format!("--h: {}", ratio(waited, longest))) {}
                                    }
                                    span.run-peak aria-hidden="true" {
                                        span style=(format!("--h: {}", ratio(run.metrics.memory_peak.unwrap_or(0), peak))) {}
                                    }
                                }
                            }
                        }
                    }
                }
                ul.legend {
                    li.success { span.swatch {} "ran, succeeded" }
                    li.failure { span.swatch {} "ran, failed" }
                    li.wait { span.swatch {} "waited for a runner" }
                    li.peak { span.swatch {} "memory peak" }
                }
            }
        } @else {
            // The region's place, for when a second run comes.
            section #runs hidden {}
        }
    }
}

/// A run's column, in words: `Oct 22 14:02 UTC, success: waited 12s, ran 7m 34s, memory peak 4.1G`.
fn run_label(run: &JobSummary, now: u64) -> String {
    let mut label = format!(
        "{} {} UTC, {}",
        format::short_date(run.timeline.queued_at),
        format::clock(run.timeline.queued_at),
        run.outcome().label
    );
    match (run.waited(now), run.ran(now)) {
        (Some(waited), Some(ran)) => {
            let verb = if run.state == JobState::Running {
                "running for"
            } else {
                "ran"
            };
            let _ = write!(
                label,
                ": waited {}, {verb} {}",
                format::duration(waited),
                format::duration(ran)
            );
        }
        (Some(waited), None) if run.timeline.started_at.is_some() => {
            let _ = write!(label, ": waited {}", format::duration(waited));
        }
        (Some(waited), None) => {
            let _ = write!(label, ": waiting for {}", format::duration(waited));
        }
        (None, _) => label.push_str(": did not start"),
    }
    if let Some(peak) = run.metrics.memory_peak {
        let _ = write!(label, ", memory peak {}", format::bytes(peak));
    }
    label
}

/// Each step's stretch over the memory and CPU charts, labelled with what it used.
struct StepBands {
    memory: Vec<Band>,
    cpu: Vec<Band>,
}

fn step_bands(view: &JobView) -> StepBands {
    let mut bands = StepBands {
        memory: Vec::new(),
        cpu: Vec::new(),
    };
    for (index, step) in view.job.steps.iter().enumerate() {
        let Some(from) = step.started_at else {
            continue;
        };
        let end = from + step.took(view.now).unwrap_or(0);
        let Some(usage) = view.samples.usage_between(from, end) else {
            continue;
        };
        let to = step.completed_at;
        let shown = format!("$_step == {}", index + 1);
        bands.memory.push(Band {
            from,
            to,
            shown: shown.clone(),
            label: usage.memory(),
        });
        bands.cpu.push(Band {
            from,
            to,
            shown,
            label: usage.cpu(),
        });
    }
    bands
}

struct LimitLines {
    memory: Vec<Limit>,
    cpu: Vec<Limit>,
    /// Some line is the profile's limit in the config now, the record lacking its container's cap.
    configured_now: bool,
}

/// The job's chart lines as `Job::chart_lines` labels them, a reservation drawn apart from a cap.
fn limit_lines(view: &JobView) -> LimitLines {
    let lines = view.job.chart_lines(view.profile.as_ref());
    let configured_now = lines
        .memory
        .iter()
        .chain(&lines.cpu)
        .any(|line| line.kind == ChartLineKind::ProfileNow);
    let limits = |lines: Vec<ChartLine>| {
        lines
            .into_iter()
            .map(|line| Limit {
                value: line.value,
                class: if line.kind == ChartLineKind::Reserved {
                    "reserved"
                } else {
                    ""
                },
                label: line.label,
            })
            .collect()
    };
    LimitLines {
        memory: limits(lines.memory),
        cpu: limits(lines.cpu),
        configured_now,
    }
}

/// What became of the failed job's record: the command that opens a shell in it while it is kept.
fn record(view: &JobView) -> Markup {
    let Some(note) = &view.job.record else {
        return html! {};
    };
    let id = view.job.summary.id;
    let command = RecordNote::shell_command(id);
    html! {
        section #record .record aria-labelledby="record-title" {
            @match note.state() {
                state @ RecordState::Skipped { .. } => {
                    h2 #record-title { "Record" }
                    p.muted { (format::sentence(&state.line())) "." }
                }
                state @ RecordState::Removed { at, .. } => {
                    h2 #record-title { "Record" }
                    p.muted { "Kept, then " (state.line()) " " (time_ago(at, view.now)) "." }
                }
                state @ RecordState::Kept { .. } => {
                div.section-head {
                    h2 #record-title { "Open a shell" }
                    p.muted { (format::sentence(&state.line())) }
                }
                p {
                    "runnerd kept this job's runner as it failed. In a terminal on the host, this opens a shell in its "
                    "workspace and shows the step that failed:"
                }
                div.copy-command {
                    code { (command) }
                    button.button-quiet type="button"
                        data-on:click=(format!("navigator.clipboard.writeText('{command}')"))
                        aria-label="Copy the command" { "Copy" }
                }
                }
            }
        }
    }
}

/// The log's last lines, numbered; live lines are appended to `#log-lines`.
pub(crate) fn log(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        section #log .log aria-labelledby="log-title" {
            div.section-head {
                h2 #log-title .visually-hidden { "Log" }
                p.muted {
                    a href=(format!("/jobs/{}/log", summary.id)) { "Full log" }
                    " · " (summary.log.description())
                }
            }
            @if view.log_from > 0 {
                p.muted { "Showing the last " (view.log.len()) " lines." }
            }
            @if view.log.is_empty() && summary.log.lines > 0 {
                p.muted { "The log's lines are past " code { "history.logs" } " and were deleted." }
            }
            (log_lines(view))
        }
    }
}

/// The log's last lines, which the live stream replaces whenever it connects and appends to after.
pub(crate) fn log_lines(view: &JobView) -> Markup {
    html! {
        ol #log-lines .log-lines tabindex="0" aria-label="Log lines" {
            @for (number, line) in (view.log_from + 1..).zip(&view.log) { (log_line(number, line)) }
        }
    }
}

/// Log line `number`, counted from 1, without the terminal colour codes steps print. Each line carries its number,
/// so the stream can drop the oldest from the page without renumbering the rest.
pub(crate) fn log_line(number: u64, text: &str) -> Markup {
    html! { li value=(number) { (strip_escapes(text)) } }
}

/// The last of the log lines a stream appends: it also sets `shown.log` to its number, in the same patch as the
/// lines, so what the page says it has stays what it has even if the stream drops before its next signal.
pub(crate) fn last_log_line(number: u64, text: &str) -> Markup {
    html! { li value=(number) "data-signals:shown.log"=(number) { (strip_escapes(text)) } }
}

/// `text` without ANSI escape sequences: `ESC [ … final byte` and lone escapes.
fn strip_escapes(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            // Parameters and intermediates up to the final byte, `@` to `~`.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coloured_log_line_shows_as_its_text() {
        let line = "\u{1b}[36;1mdocker info --format x\u{1b}[0m done";
        assert_eq!(
            log_line(7, line).into_string(),
            r#"<li value="7">docker info --format x done</li>"#
        );
    }
}
