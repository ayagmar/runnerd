//! `/jobs/{id}`: one job's phases, its steps on a shared time axis, its CPU and memory over time against the caps
//! its container ran with (or, for records without them, the profile's limits as configured now), and its log, live
//! while it runs, and the same job's latest runs (wait, run time, outcome and memory peak) beside it. A failed job
//! whose runner was kept as a record gets the command that opens a shell in it.

use std::fmt::Write as _;

use maud::{Markup, html};
use runnerd_core::api::{
    Job, JobPage, JobQuery, JobState, JobSummary, Sample, SampleSeries, Status, Step,
};
use runnerd_core::client::{Client, ClientError};

use super::{Live, Parts, badge, digest, durations, outcome_class, page, profile_class, time_ago};
use crate::chart::{Band, Base, Series, TimeChart, nice_top};
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
    /// The memory and CPUs the job's profile has in the config now, while it is configured: what the charts show for
    /// records without their container's caps, which a reload since may have changed.
    pub(crate) limits: Option<(u64, u32)>,
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
        let job: Job = daemon.get(&format!("/v1/jobs/{id}")).await?;
        let samples = daemon.get(&format!("/v1/jobs/{id}/samples")).await?;
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
            limits: limits(&status, &job),
            now,
            job,
            samples,
            log_from,
            log,
        })
    }

    pub(crate) fn add_sample(&mut self, sample: Sample) {
        self.samples.samples.push(sample);
    }
}

/// The memory and CPUs of the job's profile as configured now.
pub(crate) fn limits(status: &Status, job: &Job) -> Option<(u64, u32)> {
    let runner = job.summary.runner.as_ref()?;
    status
        .profiles
        .iter()
        .find(|profile| profile.name == runner.profile)
        .map(|profile| (profile.memory, profile.cpus))
}

impl JobView {
    /// Whether the job still runs, so its page is live.
    fn live(&self) -> bool {
        !self.job.summary.state.is_final()
    }
}

/// The live regions, by id, in the order the page has them, with their parts.
pub(crate) fn regions(view: &JobView) -> [(&'static str, Markup, Parts); 4] {
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
    ]
}

/// The whole page.
pub(crate) fn render(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let url = format!("/live/jobs/{}", summary.id);
    let [head, timeline, steps, metrics] = regions(view);
    let live = view.live().then(|| Live {
        url: &url,
        at: view.now,
        log: Some(view.log_from + view.log.len() as u64),
        regions: [&head, &timeline, &steps, &metrics]
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
            p.muted.admission { (admission.line()) }
        }
        (runs(view))
        (record(view))
        (log(view))
    };
    page(&title(summary), None, live.as_ref(), &main)
}

/// The page's title: `workflow / job`, as the terminal clients write a job, or the job's name alone; `job <id>`
/// until the daemon knows its name.
fn title(summary: &JobSummary) -> String {
    match &summary.workflow {
        Some(workflow) => format!("{workflow} / {}", summary.name),
        None if summary.name.is_empty() => format!("job {}", summary.id),
        None => summary.name.clone(),
    }
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
            h1 { (summary.name) " " (badge(summary)) }
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
    let timeline = &view.job.summary.timeline;
    let now = view.now;
    let marks = [
        ("Queued", Some(timeline.queued_at)),
        ("Container started", timeline.container_started_at),
        ("Job started", timeline.started_at),
        ("Finished", timeline.finished_at),
        ("Cleaned up", timeline.cleaned_at),
    ];
    let phases: Vec<(&str, &str, u64, Option<u64>)> = [
        (
            "Waiting for a runner",
            "wait",
            Some(timeline.queued_at),
            timeline.container_started_at,
        ),
        (
            "Runner starting",
            "start",
            timeline.container_started_at,
            timeline.started_at,
        ),
        ("Running", "run", timeline.started_at, timeline.finished_at),
        (
            "Cleaning up",
            "clean",
            timeline.finished_at,
            timeline.cleaned_at,
        ),
    ]
    .into_iter()
    .filter_map(|(label, class, from, to)| Some((label, class, from?, to)))
    .collect();
    let live = view.live();
    html! {
        section #timeline .timeline aria-labelledby="timeline-title" {
            h2 #timeline-title .visually-hidden { "Timeline" }
            // Named by the legend, whose figures are parts that change on their own.
            div.phases role="img" aria-labelledby="phase-legend" {
                @for (_, class, from, to) in &phases {
                    // Each phase grows by its seconds; an ongoing one follows the page's clock.
                    @let seconds = to.unwrap_or(now).saturating_sub(*from);
                    @let follows = live && to.is_none();
                    @let share = format!("--share: {seconds}");
                    div class=(format!("phase {class}{}", if to.is_none() { " ongoing" } else { "" }))
                        style=(if follows { parts.clocked(share) } else { share })
                        data-style:--share=[follows.then(|| format!("Math.max(0, $shown.at - {from})"))] {}
                }
            }
            ol #phase-legend .phase-legend {
                @for (label, class, from, to) in &phases {
                    li class=(*class) {
                        span.swatch aria-hidden="true" {}
                        span { (label) }
                        @let id = format!("phase-{class}-time");
                        (parts.html(&id, html! {
                            span #(id) class=(if to.is_none() { "figure ongoing" } else { "figure" }) {
                                (format::duration(to.unwrap_or(now).saturating_sub(*from))) @if to.is_none() { "…" }
                            }
                        }))
                    }
                }
            }
            details.marks {
                summary { "Exact times" }
                dl {
                    @for (label, at) in marks {
                        @if let Some(at) = at { div { dt { (label) } dd { (format::timestamp(at)) } } }
                    }
                }
            }
        }
    }
}

fn step_class(step: &Step) -> &'static str {
    match (step.status.as_str(), step.conclusion.as_deref()) {
        ("in_progress", _) => "running",
        ("completed", Some("success")) => "success",
        ("completed", Some("failure" | "timed_out")) => "failure",
        ("completed", Some("skipped")) => "skipped",
        ("completed", _) => "neutral",
        _ => "pending",
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
                        @let class = step_class(step);
                        @let seconds = step.started_at.map(|from| step.completed_at.unwrap_or(now).saturating_sub(from));
                        @let usage = step.started_at.and_then(|from| usage(&view.samples.samples, from, step.completed_at.unwrap_or(now)));
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
                                    (parts.html(&id, html! { span.visually-hidden #(id) { ", " (usage.describe()) } }))
                                }
                            }
                            @let id = format!("step-{number}-time");
                            (parts.html(&id, html! {
                                span.step-time #(id) {
                                    @match (seconds, class) {
                                        (_, "skipped") => "skipped",
                                        (Some(seconds), "running") => { (format::duration(seconds)) "…" },
                                        (Some(seconds), _) => { (format::duration(seconds)) },
                                        (None, _) => "–",
                                    }
                                }
                            }))
                            span.step-track aria-hidden="true" {
                                @if let (Some(start), Some(span), Some(from)) = (start, span, step.started_at) {
                                    @let to = step.completed_at.unwrap_or(now).max(from);
                                    @let left = from.saturating_sub(start);
                                    @let right = to.saturating_sub(start);
                                    @let follows = live && step.completed_at.is_none();
                                    @let style = format!("--from: {left}; --to: {right}");
                                    span.step-bar style=(if follows { parts.clocked(style) } else { style })
                                        data-style:--to=[follows.then(|| format!("$shown.at - {start}"))] {}
                                    // Shown on the side of the track away from the bar while the row is lit up:
                                    // the charts may be off screen.
                                    @if let Some(usage) = &usage {
                                        @let id = format!("step-{number}-note");
                                        (parts.html(&id, html! {
                                            span #(id) class=(if left + right > span { "step-usage start" } else { "step-usage end" }) { (usage.describe()) }
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

/// CPU and memory over the job's life, against its container's caps or else its profile's limits as configured now,
/// with the totals.
pub(crate) fn metrics(view: &JobView, parts: &Parts) -> Markup {
    let summary = &view.job.summary;
    let metrics = &summary.metrics;
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
    let memory_limit = memory_line.as_ref().map(|(limit, _)| *limit);
    let cpu_limit = cpu_line.as_ref().map(|(limit, _)| *limit);
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
        limit: memory_line,
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
        limit: cpu_line,
        value: |millicores| format!("{} cores", format::cores(millicores)),
        time,
        gap,
    };
    let optional =
        |value: Option<u64>, write: fn(u64) -> String| value.map_or("–".to_owned(), write);
    let bands = step_bands(view);
    html! {
        section #metrics .metrics aria-labelledby="metrics-title" {
            h2 #metrics-title .visually-hidden { "Resources" }
            @if configured_now {
                p.muted { "This record lacks its container's caps: the dashed lines are " (profile) "'s limits in the config now, which may not be the ones the job ran with." }
            }
            @if samples.is_empty() {
                p.muted { "No samples: the runner's container never ran, or its samples are past `history.samples`." }
            } @else {
                div.charts {
                    div.chart-card {
                        h3 { "Memory" (parts.html("metrics-memory-peak", html! { span.reading #metrics-memory-peak { "sampled peak " (format::bytes(memory_peak)) } })) }
                        (memory.render_with(nice_top(memory_peak.max(memory_limit.unwrap_or(0)), Base::Binary), &bands.memory, parts))
                    }
                    div.chart-card {
                        h3 { "CPU" (parts.html("metrics-cpu-peak", html! { span.reading #metrics-cpu-peak { "sampled peak " (format::cores(cpu_peak)) " cores" } })) }
                        (cpu.render_with(nice_top(cpu_peak.max(cpu_limit.unwrap_or(0)), Base::Decimal), &bands.cpu, parts))
                    }
                }
            }
            dl.totals {
                // The job's own peak, which history keeps after its samples are deleted; the charts' peak is of the
                // samples left.
                div { dt { "Memory peak over the job" } dd { (optional(metrics.memory_peak, format::bytes)) } }
                div { dt { "Average memory" } dd { (optional(metrics.memory_average, format::bytes)) } }
                div { dt { "CPU time" } dd { (optional(metrics.cpu_time.map(|ms| ms / 1000), format::duration)) } }
                div { dt { "Average CPU" } dd { (optional(metrics.cpu_average, |m| format!("{} cores", format::cores(m)))) } }
                div { dt { "Received" } dd { (optional(metrics.network_received, format::bytes)) } }
                div { dt { "Sent" } dd { (optional(metrics.network_sent, format::bytes)) } }
                div { dt { "From the mirror" } dd { (format::bytes(metrics.mirror_bytes)) } }
            }
        }
    }
}

/// The same job's latest runs, oldest first: a column each, its wait under its run time on one scale, and its
/// memory peak under it against the largest; this run is outlined. Each column opens its run.
fn runs(view: &JobView) -> Markup {
    let runs = &view.runs;
    let longest = runs
        .iter()
        .map(|run| {
            let times = durations(run, view.now);
            times.waited.unwrap_or(0) + times.ran.unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let peak = runs
        .iter()
        .filter_map(|run| run.metrics.memory_peak)
        .max()
        .unwrap_or(0);
    let finished: Vec<&JobSummary> = runs
        .iter()
        .filter(|run| run.state.is_final() && run.timeline.started_at.is_some())
        .collect();
    let median = |values: Vec<u64>| {
        let mut values = values;
        values.sort_unstable();
        values.get(values.len().saturating_sub(1) / 2).copied()
    };
    let ran = median(
        finished
            .iter()
            .filter_map(|run| durations(run, view.now).ran)
            .collect(),
    );
    let waited = median(
        finished
            .iter()
            .filter_map(|run| durations(run, view.now).waited)
            .collect(),
    );
    let failed = finished
        .iter()
        .filter(|run| outcome_class(run) == "failure")
        .count();
    let ratio = |part: u64, whole: u64| format!("{:.3}", crate::chart::ratio(part, whole));
    html! {
        @if runs.len() > 1 {
            section #runs class=(format!("runs {}", view.profile_class)) aria-labelledby="runs-title" {
                div.section-head {
                    h2 #runs-title { "Latest " (runs.len()) " runs of this job" }
                    a href=(format!("/history?repository={}", view.job.summary.repository)) { "History of " (view.job.summary.repository) }
                }
                dl.headline {
                    div { dt { "Median run" } dd { (ran.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Median wait" } dd { (waited.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Failed" } dd { (failed) " of " (finished.len()) } }
                    div { dt { "Largest memory peak" } dd { (if peak > 0 { format::bytes(peak) } else { "–".to_owned() }) } }
                }
                p.run-scale aria-hidden="true" { "Wait and run, up to " (format::duration(longest)) }
                // On a narrow screen the runs scroll sideways, and start scrolled to the latest.
                div.run-scroll {
                    ol.run-bars style=(format!("--runs: {}", runs.len())) {
                        @for run in runs {
                            @let times = durations(run, view.now);
                            @let waited = times.waited.unwrap_or(0);
                            @let ran = times.ran.unwrap_or(0);
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
        }
    }
}

/// A run's column, in words: `Oct 22 14:02 UTC, success: waited 12s, ran 7m 34s, memory peak 4.1G`.
fn run_label(run: &JobSummary, now: u64) -> String {
    let times = durations(run, now);
    let mut label = format!(
        "{} {} UTC, {}",
        format::short_date(run.timeline.queued_at),
        format::clock(run.timeline.queued_at),
        super::outcome_label(run)
    );
    match (times.waited, times.ran) {
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
        _ => label.push_str(": did not start"),
    }
    if let Some(peak) = run.metrics.memory_peak {
        let _ = write!(label, ", memory peak {}", format::bytes(peak));
    }
    label
}

/// The dashed memory and CPU lines of the job's charts, with their labels.
/// What a job used over a stretch of its run, from the samples in it.
struct Usage {
    memory_peak: u64,
    cpu_average: u64,
}

impl Usage {
    fn describe(&self) -> String {
        format!(
            "peak {}, average {} cores",
            format::bytes(self.memory_peak),
            format::cores(self.cpu_average)
        )
    }
}

/// The samples taken from `from` to `to`; none when the stretch fell between two samples.
fn usage(samples: &[Sample], from: u64, to: u64) -> Option<Usage> {
    let inside: Vec<&Sample> = samples
        .iter()
        .filter(|sample| (from..=to).contains(&sample.at))
        .collect();
    let count = u64::try_from(inside.len())
        .ok()
        .filter(|count| *count > 0)?;
    Some(Usage {
        memory_peak: inside.iter().map(|sample| sample.memory).max()?,
        cpu_average: inside.iter().map(|sample| sample.cpu).sum::<u64>() / count,
    })
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
        let Some(usage) = usage(
            &view.samples.samples,
            from,
            step.completed_at.unwrap_or(view.now),
        ) else {
            continue;
        };
        let to = step.completed_at;
        let shown = format!("$_step == {}", index + 1);
        bands.memory.push(Band {
            from,
            to,
            shown: shown.clone(),
            label: format!("peak {}", format::bytes(usage.memory_peak)),
        });
        bands.cpu.push(Band {
            from,
            to,
            shown,
            label: format!("average {} cores", format::cores(usage.cpu_average)),
        });
    }
    bands
}

struct LimitLines {
    memory: Option<(u64, String)>,
    cpu: Option<(u64, String)>,
    /// Some line is the profile's limit in the config now, the record lacking its container's cap.
    configured_now: bool,
}

/// The caps the job's container ran with, or for a record without them, its profile's limits in the config now.
fn limit_lines(view: &JobView) -> LimitLines {
    let runner = view.job.summary.runner.as_ref();
    let profile = runner.map_or("profile", |runner| runner.profile.as_str());
    let memory_cap = runner.and_then(|runner| runner.memory_cap);
    let cpu_cap = runner.and_then(|runner| runner.cpu_cap);
    let memory = match memory_cap {
        Some(cap) => Some((cap, format!("slot cap {}", format::bytes(cap)))),
        None => view
            .limits
            .map(|(memory, _)| (memory, format!("{profile} now {}", format::bytes(memory)))),
    };
    let cpu = match cpu_cap {
        Some(cap) => Some((cap, format!("{} CPUs", format::cores(cap)))),
        None => view.limits.map(|(_, cpus)| {
            let millicores = u64::from(cpus) * 1000;
            (
                millicores,
                format!("{profile} now {} CPUs", format::cores(millicores)),
            )
        }),
    };
    LimitLines {
        configured_now: (memory_cap.is_none() && memory.is_some())
            || (cpu_cap.is_none() && cpu.is_some()),
        memory,
        cpu,
    }
}

/// What became of the failed job's record: the command that opens a shell in it while it is kept.
fn record(view: &JobView) -> Markup {
    let Some(note) = &view.job.record else {
        return html! {};
    };
    let id = view.job.summary.id;
    let command = format!("runnerctl shell {id}");
    html! {
        section #record .record aria-labelledby="record-title" {
            @if let Some(why) = &note.skipped {
                h2 #record-title { "Record" }
                p.muted { "Not kept: " (why) "." }
            } @else if let Some(at) = note.removed_at {
                h2 #record-title { "Record" }
                p.muted {
                    "Kept, then " (note.removed_by.map_or_else(|| "removed".to_owned(), |by| by.to_string()))
                    " " (time_ago(at, view.now)) "."
                }
            } @else {
                div.section-head {
                    h2 #record-title { "Open a shell" }
                    @if let Some(bytes) = note.bytes { p.muted { "Record kept, " (format::bytes(bytes)) } }
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

/// The log's last lines, numbered; live lines are appended to `#log-lines`.
pub(crate) fn log(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        section #log .log aria-labelledby="log-title" {
            div.section-head {
                h2 #log-title .visually-hidden { "Log" }
                p.muted {
                    a href=(format!("/jobs/{}/log", summary.id)) { "Full log" }
                    " · " (summary.log.lines) " lines, " (format::bytes(summary.log.bytes))
                    @if summary.log.truncated { ", cut at history.log_max" }
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
