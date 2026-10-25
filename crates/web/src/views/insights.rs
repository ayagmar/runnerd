//! `/insights`: what the job history says (ADR 0051): jobs that got slower and stayed so, jobs that fail and pass on
//! a re-run, jobs near their memory cap, one card each, warnings first; the last 24 hours in a line; and each
//! repository's waits this week against the week before. Reads only.

use maud::{Markup, html};
use runnerd_core::api::{
    Digest, Insight, InsightKind, Insights, RepositoryWaits, RunLink, Tone, WaitSpread,
};
use runnerd_core::client::{Client, ClientError};

use super::{Page, page, time_ago};
use crate::format;

/// Everything the insights page shows.
#[derive(Debug, Clone)]
pub(crate) struct InsightsView {
    pub(crate) now: u64,
    pub(crate) insights: Insights,
    pub(crate) digest: Digest,
}

impl InsightsView {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<InsightsView, ClientError> {
        Ok(InsightsView {
            now,
            insights: daemon.get("/v1/insights").await?,
            digest: daemon.get("/v1/digest").await?,
        })
    }
}

/// The whole page.
pub(crate) fn render(view: &InsightsView) -> Markup {
    let insights = &view.insights;
    let count = insights.insights.len();
    let day = &view.digest;
    let main = html! {
        div.page-head {
            h1.visually-hidden { "Insights" }
            p.figures {
                span.big { (count) @if count == 1 { " insight" } @else { " insights" } }
                span.muted { " " (insights.window_line()) }
            }
            p.muted {
                "Last 24 hours: " (day.summary_line()) "."
            }
        }
        @if insights.insights.is_empty() {
            p.empty { (sentence(&insights.empty_line())) "." }
        } @else {
            ul.insight-list aria-label="Insights" {
                @for insight in &insights.insights {
                    (card(insight, view.now))
                }
            }
        }
        @if !insights.waits.is_empty() {
            (waits(&insights.waits))
        }
    };
    page("Insights", Some(Page::Insights), None, &main)
}

fn card(insight: &Insight, now: u64) -> Markup {
    let tone = match insight.tone() {
        Tone::Warn => " warn",
        Tone::Bad => " bad",
        Tone::Good | Tone::Neutral | Tone::Quiet => "",
    };
    html! {
        li class=(format!("insight {} {}{tone}", insight.severity.label(), insight.kind.word())) {
            span.badge { (insight.kind.label()) }
            p.sentence { (insight.sentence) }
            @if let InsightKind::Slower { since: Some(since), .. } = &insight.kind {
                p.muted { "Since " (run_link(since, now)) }
            }
            @if let Some(latest) = insight.runs.first() {
                p.muted {
                    @if insight.runs.len() == 1 { "Run: " } @else { "Latest of " (insight.runs.len()) " runs: " }
                    (run_link(latest, now))
                }
            }
        }
    }
}

/// A run in the history, with its page on GitHub.
fn run_link(run: &RunLink, now: u64) -> Markup {
    html! {
        a href=(format!("/jobs/{}", run.job)) {
            "job " (run.job) @if run.run_attempt > 1 { ", attempt " (run.run_attempt) }
        }
        ", " (time_ago(run.at, now))
        @if let Some(url) = &run.url {
            " · " a href=(url) rel="noopener" target="_blank" { "on GitHub" }
        }
    }
}

fn wait(spread: &WaitSpread, value: Option<u64>) -> String {
    spread
        .reading(value)
        .map_or_else(|| "–".to_owned(), format::duration)
}

/// `line` as a sentence, its first letter a capital.
fn sentence(line: &str) -> String {
    let mut chars = line.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn waits(waits: &[RepositoryWaits]) -> Markup {
    html! {
        section aria-labelledby="waits-title" {
            div.section-head {
                h2 #waits-title { "Waits this week" }
                span.muted { "From queued to started, against the 7 days before. Warm: a runner was already up." }
            }
            div.table-wrap {
                table.job-table.waits-table {
                    thead {
                        tr {
                            th scope="col" { "Repository" }
                            th scope="col" .number { "Jobs" }
                            th scope="col" .number { "Median" }
                            th scope="col" .number { "p95" }
                            th scope="col" .number { "Warm median" }
                            th scope="col" .number { "Cold median" }
                            th scope="col" .number { "p95 the week before" }
                        }
                    }
                    tbody {
                        @for repository in waits {
                            @let now = &repository.now;
                            tr {
                                th scope="row" { (repository.repository) }
                                td.number data-label="jobs" { (now.all.jobs) }
                                td.number data-label="median" { (wait(&now.all, now.all.p50)) }
                                td.number data-label="p95" { (wait(&now.all, now.all.p95)) }
                                td.number data-label="warm" { (wait(&now.warm, now.warm.p50)) }
                                td.number data-label="cold" { (wait(&now.cold, now.cold.p50)) }
                                td.number data-label="week before" { (wait(&repository.before.all, repository.before.all.p95)) }
                            }
                        }
                    }
                }
            }
        }
    }
}
