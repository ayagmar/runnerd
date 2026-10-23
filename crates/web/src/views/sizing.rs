//! `/sizing`: per profile, what its jobs use of their memory cap (sampled peaks by percentile, and what the advice
//! sizes for: the largest with the margin), how long they waited and how busy the slots were, the advice, and a row per job. Applying advice
//! opens the config editor with the profile's new `memory` and `slots` in it; nothing is written until it is saved.

use maud::{Markup, html};
use runnerd_core::api::{Advice, JobSizing, ProfileSizing, Sizing};
use runnerd_core::client::{Client, ClientError};

use super::{Page, page};
use crate::chart::ratio;
use crate::format;

/// Everything the sizing page shows.
#[derive(Debug, Clone)]
pub(crate) struct SizingView {
    pub(crate) sizing: Sizing,
}

impl SizingView {
    pub(crate) async fn load(daemon: &Client) -> Result<SizingView, ClientError> {
        Ok(SizingView {
            sizing: daemon.get("/v1/sizing").await?,
        })
    }
}

/// The whole page.
pub(crate) fn render(view: &SizingView) -> Markup {
    let sizing = &view.sizing;
    let jobs: u64 = sizing.profiles.iter().map(|p| p.peaks.jobs).sum();
    let main = html! {
        div.page-head {
            h1.visually-hidden { "Sizing" }
            p.figures {
                span.big { (jobs) " jobs" }
                span.muted { " since " (format::date(sizing.since)) }
            }
            p.muted {
                "Peaks are sampled every " (sizing.sample_interval) " s, so a shorter spike goes unseen:  advice adds "
                (sizing.margin) "% to the largest, and waits for " (sizing.min_jobs) " jobs."
            }
        }
        @for (index, profile) in sizing.profiles.iter().enumerate() {
            (profile_section(sizing, profile, index))
        }
    };
    page("Sizing", Some(Page::Sizing), None, &main)
}

fn advice_class(advice: Advice) -> &'static str {
    match advice {
        Advice::Raise { .. } | Advice::SliceTooSmall { .. } => "advice raise",
        Advice::Lower { .. } => "advice lower",
        Advice::Keep { .. } => "advice keep",
        Advice::TooFew { .. } => "advice too-few",
    }
}

/// What the jobs need, for a table cell.
fn needs(job: &JobSizing) -> Markup {
    match job.advice {
        Advice::TooFew { jobs } => {
            html! { span.muted title=(format!("{jobs} runs; largest peak shown")) { "max " (format::bytes(job.peaks.memory.max)) } }
        }
        Advice::Raise { memory, .. } | Advice::SliceTooSmall { memory } => {
            html! { strong.over { (format::bytes(memory)) ", over the cap" } }
        }
        Advice::Keep { memory } | Advice::Lower { memory, .. } => html! { (format::bytes(memory)) },
    }
}

fn profile_section(sizing: &Sizing, profile: &ProfileSizing, index: usize) -> Markup {
    let peaks = &profile.peaks;
    let id = format!("sizing-{index}");
    html! {
        section class=(format!("sizing-profile p{}", index % 6)) aria-labelledby=(id) {
            div.section-head {
                h2 #(id) { span.swatch aria-hidden="true" {} (profile.name) }
                dl.facts {
                    div { dt { "Memory" } dd { (format::bytes(profile.memory)) " × " (profile.slots) } }
                    div { dt { "Jobs" } dd { (peaks.jobs) } }
                    div { dt { "Slots busy" } dd { (profile.busy) "%" } }
                    div { dt { "Waited p50 / p95" } dd { (format::duration(profile.wait.p50)) " / " (format::duration(profile.wait.p95)) } }
                    div { dt { "CPU p99" } dd { (format::cores(peaks.cpu.p99)) " of " (profile.cpus) } }
                }
            }
            @if peaks.jobs > 0 { (scale(sizing, profile)) }
            div class=(advice_class(profile.advice)) {
                p { (sizing.advice_line(profile)) }
                @if let Some(change) = profile.advice.change() {
                    a.button href=(format!("/config?{}", serde_urlencoded::to_string([("resize", &profile.name)]).unwrap_or_default())) {
                        "Review memory " (format::bytes(change.memory)) ", " (change.slots) " slots in the config"
                    }
                }
            }
            @if !profile.jobs.is_empty() {
                div.table-wrap {
                    table.job-table.sizing-table {
                        thead {
                            tr {
                                th scope="col" { "Job" }
                                th scope="col" .number { "Runs" }
                                th scope="col" .number { "p50" }
                                th scope="col" .number { "p95" }
                                th scope="col" .number { "p99" }
                                th scope="col" .number { "Max" }
                                th scope="col" .number { "CPU p99" }
                                th scope="col" .number { "Wait p95" }
                                th scope="col" .number { "Needs" }
                            }
                        }
                        tbody {
                            @for job in &profile.jobs {
                                tr {
                                    th scope="row" {
                                        span.job-name { (job.name) }
                                        " "
                                        span.repo.muted { (job.repository) @if let Some(workflow) = &job.workflow { " · " (workflow) } }
                                    }
                                    td.number { (job.peaks.jobs) }
                                    td.number { (format::bytes(job.peaks.memory.p50)) }
                                    td.number { (format::bytes(job.peaks.memory.p95)) }
                                    td.number { (format::bytes(job.peaks.memory.p99)) }
                                    td.number { (format::bytes(job.peaks.memory.max)) }
                                    td.number { (format::cores(job.peaks.cpu.p99)) }
                                    td.number { (format::duration(job.wait.p95)) }
                                    td.number { (needs(job)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The cap as a track, the jobs' peaks from p50 to p99 on it, and what the advice sizes for as a mark: the memory
/// it advises, or with too few jobs, the largest peak with the margin.
fn scale(sizing: &Sizing, profile: &ProfileSizing) -> Markup {
    let memory = &profile.peaks.memory;
    let need = match profile.advice {
        Advice::Keep { memory }
        | Advice::Lower { memory, .. }
        | Advice::Raise { memory, .. }
        | Advice::SliceTooSmall { memory } => memory,
        Advice::TooFew { .. } => memory.max + memory.max * u64::from(sizing.margin) / 100,
    };
    let whole = profile.memory.max(need).max(memory.max).max(1);
    let at = |bytes: u64| format!("{:.2}%", ratio(bytes, whole) * 100.0);
    let label = format!(
        "Memory peaks: p50 {}, p95 {}, p99 {}, largest {}; the largest + {}% is {}; the cap is {}",
        format::bytes(memory.p50),
        format::bytes(memory.p95),
        format::bytes(memory.p99),
        format::bytes(memory.max),
        sizing.margin,
        format::bytes(need),
        format::bytes(profile.memory)
    );
    html! {
        div.peak-scale role="img" aria-label=(label) {
            div.cap style=(format!("--to: {}", at(profile.memory))) { span { "cap " (format::bytes(profile.memory)) } }
            div.spread style=(format!("--from: {}; --to: {}", at(memory.p50), at(memory.p99))) {}
            span.mark.p95 style=(format!("--at: {}", at(memory.p95))) title=(format!("p95 {}", format::bytes(memory.p95))) {}
            // Near the end of the track, the label goes to the mark's left.
            span class=(if ratio(need, whole) > 0.7 { "mark need end" } else { "mark need" }) style=(format!("--at: {}", at(need))) {
                span { "max + " (sizing.margin) "% " (format::bytes(need)) }
            }
        }
        p.scale-keys.muted {
            "p50 " (format::bytes(memory.p50)) " · p95 " (format::bytes(memory.p95)) " · p99 " (format::bytes(memory.p99))
            " · max " (format::bytes(memory.max))
            @if profile.peaks.at_cap > 0 { " · " strong.over { (profile.peaks.at_cap) " at the cap" } }
        }
    }
}
