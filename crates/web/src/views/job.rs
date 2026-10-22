//! `/jobs/{id}`: one job's phases, its steps on a shared time axis, its CPU and memory over time against the
//! profile's limits as configured now, and its log, live while it runs.

use maud::{Markup, html};
use runnerd_core::api::{Job, Sample, SampleSeries, Status, Step};

use super::{badge, page, waited_and_ran};
use crate::chart::{Series, TimeChart, nice_top, ratio};
use crate::daemon::{Daemon, Error};
use crate::format;

/// Log lines the page shows; the full log is at `/jobs/{id}/log`.
pub(crate) const LOG_TAIL: u64 = 1000;

/// Everything the job page shows.
#[derive(Debug, Clone)]
pub(crate) struct JobView {
    pub(crate) now: u64,
    pub(crate) job: Job,
    pub(crate) samples: SampleSeries,
    /// The memory and CPUs the job's profile has in the config now, while it is configured. The API does not keep
    /// the caps the job's container had, which a reload since may have changed.
    pub(crate) limits: Option<(u64, u32)>,
    /// The number of the first line in `log`.
    pub(crate) log_from: u64,
    pub(crate) log: Vec<String>,
}

impl JobView {
    pub(crate) async fn load(daemon: &Daemon, id: u64, now: u64) -> Result<JobView, Error> {
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
        Ok(JobView {
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

/// The whole page.
pub(crate) fn render(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let live = (!summary.state.is_final()).then(|| {
        format!(
            "/live/jobs/{}?from={}",
            summary.id,
            view.log_from + view.log.len() as u64
        )
    });
    let main = html! {
        (head(view))
        (timeline(view))
        (steps(view))
        (metrics(view))
        (log(view))
    };
    page(&summary.name, None, live.as_deref(), &main)
}

/// The title, outcome and where the job came from.
pub(crate) fn head(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let (waited, ran) = waited_and_ran(summary, view.now);
    html! {
        header #job-head .job-head {
            p.crumbs {
                a href=(format!("/history?repository={}", summary.repository)) { (summary.repository) }
                @if let Some(workflow) = &summary.workflow { " · " (workflow) }
            }
            h1 { (summary.name) " " (badge(summary)) }
            dl.facts {
                @if let Some(branch) = &summary.branch { div { dt { "Branch" } dd { code { (branch) } } } }
                div { dt { "Run" } dd { (summary.run_id) @if summary.run_attempt > 1 { ", attempt " (summary.run_attempt) } } }
                @if let Some(runner) = &summary.runner {
                    div { dt { "Slot" } dd { (runner.profile) " · " (runner.slot) } }
                }
                @if let Some(waited) = waited { div { dt { "Waited" } dd { (format::duration(waited)) } } }
                @if let Some(ran) = ran { div { dt { "Ran" } dd { (format::duration(ran)) } } }
                div { dt { "Queued" } dd { (format::timestamp(summary.timeline.queued_at)) } }
            }
            @if let Some(url) = &summary.url {
                p { a.external href=(url) rel="noreferrer" { "Open on GitHub" } }
            }
        }
    }
}

/// The phases from queued to cleaned up, as one bar.
pub(crate) fn timeline(view: &JobView) -> Markup {
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
    let end = timeline
        .cleaned_at
        .or(timeline.finished_at)
        .unwrap_or(now)
        .max(timeline.queued_at + 1);
    let total = end.saturating_sub(timeline.queued_at).max(1);
    html! {
        section #timeline .timeline aria-labelledby="timeline-title" {
            h2 #timeline-title { "Timeline" }
            div.phases role="img" aria-label=(phases.iter().map(|(label, _, from, to)| format!("{label} {}", format::duration(to.unwrap_or(now).saturating_sub(*from)))).collect::<Vec<_>>().join(", ")) {
                @for (_, class, from, to) in &phases {
                    @let seconds = to.unwrap_or(now).saturating_sub(*from);
                    @let share = ratio(seconds, total);
                    div class=(format!("phase {class}{}", if to.is_none() { " ongoing" } else { "" })) style=(format!("--share: {share:.4}")) {}
                }
            }
            ol.phase-legend {
                @for (label, class, from, to) in &phases {
                    li class=(*class) {
                        span.swatch aria-hidden="true" {}
                        span { (label) }
                        span.figure { (format::duration(to.unwrap_or(now).saturating_sub(*from))) @if to.is_none() { "…" } }
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
pub(crate) fn steps(view: &JobView) -> Markup {
    let job = &view.job;
    let now = view.now;
    let start = job
        .summary
        .timeline
        .started_at
        .or_else(|| job.steps.iter().filter_map(|step| step.started_at).min());
    let end = job.summary.timeline.finished_at.unwrap_or(now);
    html! {
        section #steps .steps aria-labelledby="steps-title" {
            div.section-head {
                h2 #steps-title { "Steps" }
                @if let Some(start) = start { p.muted { (format::duration(end.saturating_sub(start))) " in " (job.steps.len()) " steps" } }
            }
            @if job.steps.is_empty() {
                p.muted { "GitHub has not reported any steps yet." }
            } @else {
                ol.step-list {
                    @for step in &job.steps {
                        @let class = step_class(step);
                        @let seconds = step.started_at.map(|from| step.completed_at.unwrap_or(now).saturating_sub(from));
                        li class=(class) {
                            span.step-name { (step.name) }
                            span.step-time {
                                @match (seconds, class) {
                                    (_, "skipped") => "skipped",
                                    (Some(seconds), "running") => { (format::duration(seconds)) "…" },
                                    (Some(seconds), _) => { (format::duration(seconds)) },
                                    (None, _) => "–",
                                }
                            }
                            span.step-track aria-hidden="true" {
                                @if let (Some(start), Some(from)) = (start, step.started_at) {
                                    @let span = end.saturating_sub(start).max(1);
                                    @let to = step.completed_at.unwrap_or(now).max(from);
                                    @let left = ratio(from.saturating_sub(start), span) * 100.0;
                                    @let width = ratio(to - from, span) * 100.0;
                                    span.step-bar style=(format!("--left: {left:.2}%; --width: {width:.2}%")) {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// CPU and memory over the job's life, against its profile's limits as configured now, with the totals.
pub(crate) fn metrics(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let metrics = &summary.metrics;
    let samples = &view.samples.samples;
    let start = samples
        .first()
        .map(|sample| sample.at)
        .or(summary.timeline.container_started_at)
        .unwrap_or(summary.timeline.queued_at);
    let end = samples
        .last()
        .map_or(view.now, |sample| sample.at)
        .max(start + 1);
    let gap = view.samples.interval.max(1) * 3;
    let memory_peak = samples.iter().map(|s| s.memory).max().unwrap_or(0);
    let cpu_peak = samples.iter().map(|s| s.cpu).max().unwrap_or(0);
    let memory_limit = view.limits.map(|(memory, _)| memory);
    let cpu_limit = view.limits.map(|(_, cpus)| u64::from(cpus) * 1000);
    let profile = summary
        .runner
        .as_ref()
        .map_or("profile", |runner| runner.profile.as_str());
    let time = |at: u64| format!("{} UTC", format::clock(at));
    let memory = TimeChart {
        title: "Memory over the job".to_owned(),
        start,
        end,
        series: vec![Series {
            label: "memory".to_owned(),
            class: "memory",
            points: samples.iter().map(|s| (s.at, s.memory)).collect(),
            area: true,
        }],
        limit: memory_limit.map(|limit| (limit, format!("{profile} now {}", format::bytes(limit)))),
        value: format::bytes,
        time,
        gap,
    };
    let cpu = TimeChart {
        title: "CPU over the job".to_owned(),
        start,
        end,
        series: vec![Series {
            label: "CPU".to_owned(),
            class: "cpu",
            points: samples.iter().map(|s| (s.at, s.cpu)).collect(),
            area: true,
        }],
        limit: cpu_limit.map(|limit| {
            (
                limit,
                format!("{profile} now {} CPUs", format::cores(limit)),
            )
        }),
        value: |millicores| format!("{} cores", format::cores(millicores)),
        time,
        gap,
    };
    let optional =
        |value: Option<u64>, write: fn(u64) -> String| value.map_or("–".to_owned(), write);
    html! {
        section #metrics .metrics aria-labelledby="metrics-title" {
            div.section-head {
                h2 #metrics-title { "Resources" }
                p.muted {
                    "Sampled every " (view.samples.interval) " s; a peak between samples does not show."
                    @if view.limits.is_some() {
                        " The dashed lines are " (profile) "'s limits in the config now, which may not be the ones the job ran with."
                    }
                }
            }
            @if samples.is_empty() {
                p.muted { "No samples: the runner's container never ran, or its samples are past `history.samples`." }
            } @else {
                div.charts {
                    div.chart-card {
                        h3 { "Memory" span.reading { "sampled peak " (format::bytes(memory_peak)) } }
                        (memory.render(nice_top(memory_peak.max(memory_limit.unwrap_or(0)), true)))
                    }
                    div.chart-card {
                        h3 { "CPU" span.reading { "sampled peak " (format::cores(cpu_peak)) " cores" } }
                        (cpu.render(nice_top(cpu_peak.max(cpu_limit.unwrap_or(0)), false)))
                    }
                }
            }
            dl.totals {
                div { dt { "Sampled memory peak" } dd { (optional(metrics.memory_peak, format::bytes)) } }
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

/// The log's last lines, numbered; live lines are appended to `#log-lines`.
pub(crate) fn log(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        section #log .log aria-labelledby="log-title" {
            div.section-head {
                h2 #log-title { "Log" }
                p.muted {
                    (summary.log.lines) " lines, " (format::bytes(summary.log.bytes))
                    @if summary.log.truncated { ", cut at history.log_max" }
                    " · " a href=(format!("/jobs/{}/log", summary.id)) { "Full log" }
                }
            }
            @if view.log_from > 0 {
                p.muted { "Showing the last " (view.log.len()) " lines." }
            }
            @if view.log.is_empty() && summary.log.lines > 0 {
                p.muted { "The log's lines are past " code { "history.logs" } " and were deleted." }
            }
            ol #log-lines .log-lines tabindex="0" aria-label="Log lines" {
                @for (number, line) in (view.log_from + 1..).zip(&view.log) { (log_line(number, line)) }
            }
        }
    }
}

/// Log line `number`, counted from 1, without the terminal colour codes steps print. Each line carries its number,
/// so the stream can drop the oldest from the page without renumbering the rest.
pub(crate) fn log_line(number: u64, text: &str) -> Markup {
    html! { li value=(number) { (strip_escapes(text)) } }
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
