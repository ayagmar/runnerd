//! `/sizing`: per profile, what its jobs use of their memory cap (sampled peaks by percentile, and what the advice
//! sizes for: the largest with the margin), how long they waited and how busy the slots were, the advice, and a row per job. Applying advice
//! opens the config editor with the profile's new `memory` and `slots` in it; nothing is written until it is saved.
//! What auto sizing changed is listed under each profile, with a link that opens the last change undone the same way;
//! above them, whether it is on and, per profile, what its next look changes or until when a recent change holds it.

use maud::{Markup, html};
use runnerd_core::api::{
    Advice, AutoPlan, JobSizing, ProfilePlan, ProfileSizing, Sizing, Tone, job_title,
};
use runnerd_core::client::{Client, ClientError};

use super::{Page, page};
use crate::chart::ratio;
use crate::format;

/// Everything the sizing page shows.
#[derive(Debug, Clone)]
pub(crate) struct SizingView {
    pub(crate) now: u64,
    pub(crate) sizing: Sizing,
}

impl SizingView {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<SizingView, ClientError> {
        Ok(SizingView {
            now,
            sizing: daemon.get("/v1/sizing").await?,
        })
    }
}

/// The whole page.
pub(crate) fn render(view: &SizingView) -> Markup {
    let sizing = &view.sizing;
    let jobs: u64 = sizing
        .profiles
        .iter()
        .map(|p| u64::from(p.starts.total()))
        .sum();
    let main = html! {
        div.page-head {
            h1.visually-hidden { "Sizing" }
            p.figures {
                span.big { (jobs) " jobs" }
                span.muted { " since " (format::date(sizing.since)) }
            }
            p.muted { (format::sentence(&sizing.method_line())) "." }
        }
        (auto_plan(view))
        @for (index, profile) in sizing.profiles.iter().enumerate() {
            (profile_section(sizing, profile, index))
        }
    };
    page("Sizing", Some(Page::Sizing), None, &main)
}

/// Whether auto sizing is on, its rules in words, and per profile what its next look changes, or until when a recent
/// change holds it, or that the advice asks for nothing.
fn auto_plan(view: &SizingView) -> Markup {
    let sizing = &view.sizing;
    let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) else {
        if sizing.auto {
            // A daemon that does not say what it does next.
            return html! {
                section.auto-plan aria-labelledby="auto-title" {
                    h2 #auto-title { "Auto sizing is on" }
                    p { "runnerd writes the advice into the config itself, a step at a time." }
                }
            };
        }
        return html! {
            section.auto-plan.off aria-labelledby="auto-title" {
                h2 #auto-title { "Auto sizing is off" }
                p {
                    "The advice below changes nothing until you review it in the config. With "
                    code { "auto = true" } " under " code { "[sizing]" }
                    ", runnerd applies it itself, a step at a time."
                }
            }
        };
    };
    html! {
        section.auto-plan aria-labelledby="auto-title" {
            h2 #auto-title { "Auto sizing is on" }
            p { "runnerd " (plan.rules()) "." }
            @if let Some(blocked) = plan.blocked_line() {
                p.warning { (blocked) "." }
            }
            ul.plan-list {
                @for (index, profile) in sizing.profiles.iter().enumerate() {
                    li class=(format!("p{}", index % 6)) {
                        span.plan-name { span.swatch aria-hidden="true" {} (profile.name) }
                        (plan_line(view, plan, profile))
                    }
                }
            }
        }
    }
}

/// What auto sizing does next with one profile.
fn plan_line(view: &SizingView, plan: &AutoPlan, profile: &ProfileSizing) -> Markup {
    let next = plan.profile_plan(&profile.name, view.now);
    let class = match next {
        ProfilePlan::Next { .. } => "plan-what changes",
        ProfilePlan::Held { .. } | ProfilePlan::LowerHeld { .. } => "plan-what held",
        ProfilePlan::Nothing => "plan-what",
    };
    let reason = next.reason(&view.sizing, profile);
    html! {
        span class=(class) {
            strong {
                (next.headline(plan))
                @if let Some(until) = next.until() {
                    " before " (format::short_date(until)) " " (format::clock(until)) " UTC"
                    span.muted { ", in " (format::duration(until.saturating_sub(view.now))) }
                }
            }
            @if let Some(changed) = next.changed() {
                span.muted { "Changed " (format::ago(changed, view.now)) "; " (reason) "." }
            } @else {
                span.muted { (reason) }
            }
            // Beside the change, which can still be to its memory or slots.
            @if let Some(refusal) = plan.refusal(&profile.name) {
                span.plan-refused { (refusal) }
            }
        }
    }
}

fn advice_class(advice: Advice) -> &'static str {
    match advice.tone() {
        Tone::Bad => "advice raise",
        Tone::Warn => "advice lower",
        Tone::Good => "advice keep",
        Tone::Neutral | Tone::Quiet => "advice too-few",
    }
}

/// What the jobs need, for a table cell.
fn needs(job: &JobSizing) -> Markup {
    let needs = job.needs();
    html! {
        @match (job.advice, needs.tone) {
            (Advice::TooFew { jobs }, _) => {
                span.muted title=(format!("{jobs} runs, too few to advise on; the largest peak")) { (needs.text) }
            }
            (_, Tone::Bad) => { strong.over { (needs.text) } }
            _ => { (needs.text) }
        }
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
                    div { dt { "Jobs" } dd { (profile.starts.total()) } }
                    div { dt { "Slots busy" } dd { (profile.busy) "%" } }
                    div { dt { "Waited p50 / p95" } dd { (format::duration(profile.wait.p50)) " / " (format::duration(profile.wait.p95)) } }
                    @if profile.starts.warm > 0 {
                        div {
                            dt { "Warm starts" }
                            dd {
                                (profile.starts.warm) " of " (profile.starts.total())
                                ", p50 " (format::duration(profile.starts.warm_p50))
                                " (others " (format::duration(profile.starts.cold_p50)) ")"
                            }
                        }
                    }
                    div {
                        dt { "CPU p99" }
                        dd {
                            @match peaks.cpu_p99() {
                                Some(p99) => { (format::cores_used(p99)) " of " (profile.cpus) }
                                None => { (peaks.cpu_runs_line(profile.cpus)) }
                            }
                        }
                    }
                }
            }
            @if peaks.jobs > 0 { (scale(sizing, profile)) }
            div class=(advice_class(profile.advice)) {
                p { (sizing.advice_line(profile)) }
                @if let Some(change) = profile.advice.change().filter(|_| !sizing.auto) {
                    a.button href=(format!("/config?{}", serde_urlencoded::to_string([("resize", &profile.name)]).unwrap_or_default())) {
                        "Review memory " (format::bytes(change.memory)) ", " (change.slots) " slots in the config"
                    }
                }
            }
            (changes(sizing, profile))
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
                                        span.job-name { (job_title(None, &job.name, None).name) }
                                        " "
                                        span.repo.muted { (job.repository) @if let Some(workflow) = &job.workflow { " · " (workflow) } }
                                    }
                                    td.number { (job.peaks.jobs) }
                                    td.number { (format::bytes(job.peaks.memory.p50)) }
                                    td.number { (format::bytes(job.peaks.memory.p95)) }
                                    td.number { (format::bytes(job.peaks.memory.p99)) }
                                    td.number { (format::bytes(job.peaks.memory.max)) }
                                    td.number { (job.peaks.cpu_p99().map_or_else(|| "–".to_owned(), format::cores_used)) }
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

/// What auto sizing changed in the profile, newest first, with a link that undoes the last change in the editor.
fn changes(sizing: &Sizing, profile: &ProfileSizing) -> Markup {
    let changes: Vec<_> = sizing
        .changes
        .iter()
        .rev()
        .filter(|change| change.profile == profile.name)
        .collect();
    html! {
        @if !changes.is_empty() {
            div.sizing-changes {
                h3 { "Changed by auto sizing" }
                ol {
                    @for change in &changes {
                        li {
                            time datetime=(format::rfc3339(change.at)) { (format::timestamp(change.at)) }
                            " " strong { (change.line()) }
                            " " span.muted { (change.reason) }
                        }
                    }
                }
                a.button-quiet href=(format!("/config?{}", serde_urlencoded::to_string([("undo", &profile.name)]).unwrap_or_default())) {
                    "Review undoing the last change in the config"
                }
            }
        }
    }
}

/// The cap as a track, the jobs' peaks from p50 to p99 on it, and what the advice sizes for as a mark: the memory
/// it advises, or with too few jobs, what it would size for (`Sizing::need`).
fn scale(sizing: &Sizing, profile: &ProfileSizing) -> Markup {
    let memory = &profile.peaks.memory;
    let need = sizing.need(&profile.peaks, profile.advice);
    let reason = need.reason;
    let need = need.memory;
    let whole = profile.memory.max(need).max(memory.max).max(1);
    let at = |bytes: u64| format!("{:.2}%", ratio(bytes, whole) * 100.0);
    let label = format!(
        "Memory peaks: p50 {}, p95 {}, p99 {}, largest {}; {reason}, rounded up, is {}; the cap is {}",
        format::bytes(memory.p50),
        format::bytes(memory.p95),
        format::bytes(memory.p99),
        format::bytes(memory.max),
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
                span { "needs " (format::bytes(need)) }
            }
        }
        p.scale-keys.muted {
            "p50 " (format::bytes(memory.p50)) " · p95 " (format::bytes(memory.p95)) " · p99 " (format::bytes(memory.p99))
            " · max " (format::bytes(memory.max))
            @if profile.peaks.at_cap > 0 { " · " strong.over { (profile.peaks.at_cap) " at the cap" } }
        }
    }
}
