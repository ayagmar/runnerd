//! `/insights`: what the job history says (ADR 0051): jobs that got slower and stayed so, jobs that fail and pass on
//! a re-run, jobs near their memory cap, one card each, warnings first; the last 24 hours in a line; and each
//! repository's waits this week against the week before. Reads only.

use maud::{Markup, html};
use runnerd_core::api::{
    Digest, DiskWrites, HostHazard, Insight, InsightKind, Insights, LabelledFigure,
    RepositoryWaits, RunLink, ShareOwner, Tone, View, WriteTop,
};
use runnerd_core::client::{Client, ClientError};

use super::{Page, heading, page, time_ago};
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
        (heading(View::Insights.title(), Some(Page::Insights), Some(html! {
            p.page-figure {
                strong { (count) @if count == 1 { " insight" } @else { " insights" } }
                span.muted { " " (insights.window_line()) }
            }
        })))
        div.page-head {
            p.muted {
                "Last 24 hours: " (day.summary_line()) "."
            }
            @if let Some(host) = day.host_line() {
                p.muted { (host) }
            }
        }
        @if insights.insights.is_empty() {
            p.empty { (format::sentence(&insights.empty_line())) "." }
        } @else {
            ul.insight-list aria-label=(View::Insights.title()) {
                @for insight in &insights.insights {
                    (card(insight, view.now))
                }
            }
        }
        @if !insights.host.is_empty() {
            section aria-labelledby="host-title" {
                div.section-head {
                    h2 #host-title { (HostHazard::TITLE) }
                    span.muted { "Each UTC day, and how much of it ran on a host credit." }
                }
                ul.host-days {
                    @for host in &insights.host {
                        li { (host.dated_line()) }
                    }
                }
            }
        }
        @if let Some(disk) = &insights.disk {
            (disk_writes(disk))
        }
        @if !insights.waits.is_empty() {
            (waits(&insights.waits))
        }
    };
    page(View::Insights.title(), Some(Page::Insights), None, &main)
}

/// An insight as a row: its kind and the number it is about, then core's sentence and the runs that show it.
fn card(insight: &Insight, now: u64) -> Markup {
    let tone = match insight.tone() {
        Tone::Warn => " warn",
        Tone::Bad => " bad",
        Tone::Good | Tone::Neutral | Tone::Quiet => "",
    };
    html! {
        li class=(format!("insight {}{tone}", insight.kind.word())) {
            div.insight-figure {
                span.insight-kind { span.tone-mark aria-hidden="true" {} (insight.kind.label()) }
                @if let Some(figure) = insight.figure() {
                    strong.insight-value { (figure.value) }
                    span.insight-label { (figure.label) }
                }
            }
            div.insight-body {
                p.sentence { (insight.sentence) }
                @if let InsightKind::Slower { since: Some(since), .. } = &insight.kind {
                    p.insight-run { "Since " (run_link(since, now)) }
                }
                @if let Some(latest) = insight.runs.first() {
                    p.insight-run {
                        @if insight.runs.len() == 1 { "Run " } @else { "Latest of " (insight.runs.len()) " runs " }
                        (run_link(latest, now))
                    }
                }
            }
        }
    }
}

/// The disk writes as figures and ranked bars: what the jobs wrote, what the drive wrote and whose it was, and the
/// workflows and jobs that wrote most; core's sentences, with what the figures leave out, folded under them.
fn disk_writes(disk: &DiskWrites) -> Markup {
    html! {
        section.disk aria-labelledby="disk-title" {
            div.section-head {
                h2 #disk-title { (DiskWrites::TITLE) }
            }
            (figure_list("headline disk-figures", &disk.figures()))
            @if let Some(drive) = disk.drive.as_ref().filter(|drive| !drive.shares.is_empty()) {
                @let parts = drive.shares.parts();
                div.share {
                    h3 { (DiskWrites::SHARES_TITLE) }
                    div.share-bar role="img" aria-label=(drive.shares.line(drive.watched)) {
                        @for part in parts.iter().filter(|part| part.bytes > 0) {
                            span class=(format!("share-part {}", owner_class(part.owner))) style=(format!("flex-grow: {}", part.bytes)) {}
                        }
                    }
                    ul.share-key aria-hidden="true" {
                        @for part in &parts {
                            li class=(owner_class(part.owner)) {
                                span.swatch {}
                                strong { (part.percent) "%" }
                                " " (part.owner.label())
                            }
                        }
                    }
                }
            }
            div.write-tops {
                (ranked(DiskWrites::WORKFLOWS_TITLE, &disk.workflows))
                (ranked(DiskWrites::TOP_JOBS_TITLE, &disk.top_jobs))
            }
            details.in-words {
                summary { "In sentences" }
                @for part in disk.sections() {
                    h3 { (part.title) }
                    ul.host-days {
                        @for line in &part.lines {
                            li { (line) }
                        }
                    }
                }
            }
        }
    }
}

/// Figures core words, each its label over its value and what it is out of, in a list classed `class`.
pub(crate) fn figure_list(class: &str, figures: &[LabelledFigure]) -> Markup {
    html! {
        @if !figures.is_empty() {
            dl class=(class) {
                @for figure in figures {
                    div {
                        dt { (figure.label) }
                        dd {
                            (figure.value)
                            @if let Some(of) = &figure.of { span.muted { " " (of) } }
                        }
                    }
                }
            }
        }
    }
}

/// The class of a share's owner, which colours its part of the bar.
fn owner_class(owner: ShareOwner) -> &'static str {
    match owner {
        ShareOwner::Jobs => "owner-jobs",
        ShareOwner::Slice => "owner-slice",
        ShareOwner::Daemon => "owner-daemon",
        ShareOwner::Unknown => "owner-unknown",
    }
}

/// The workflows or jobs that wrote most, each a bar as long as its share of the first's bytes.
fn ranked(title: &str, tops: &[WriteTop]) -> Markup {
    let most = tops.iter().map(|top| top.total).max().unwrap_or(0);
    html! {
        @if !tops.is_empty() {
            div.ranked {
                h3 { (title) }
                ol {
                    @for top in tops {
                        li style=(format!("--share: {:.4}", crate::chart::ratio(top.total, most))) {
                            span.ranked-name { (top.name()) }
                            span.ranked-value { (format::bytes(top.total)) }
                            span.ranked-bar aria-hidden="true" {}
                            span.ranked-note { (top.note()) }
                        }
                    }
                }
            }
        }
    }
}

/// A run in the history, with its page on GitHub.
fn run_link(run: &RunLink, now: u64) -> Markup {
    html! {
        a.hit href=(format!("/jobs/{}", run.job)) {
            "job " (run.job) @if run.run_attempt > 1 { ", attempt " (run.run_attempt) }
        }
        ", " (time_ago(run.at, now))
        @if let Some(url) = &run.url {
            " · " a.hit href=(url) rel="noopener" target="_blank" { "on GitHub" }
        }
    }
}

fn waits(waits: &[RepositoryWaits]) -> Markup {
    html! {
        section aria-labelledby="waits-title" {
            div.section-head {
                h2 #waits-title { (RepositoryWaits::TITLE) }
                span.muted { "From queued to started, against the 7 days before. Warm: a runner was already up." }
            }
            div.table-wrap {
                table.job-table.waits-table {
                    thead {
                        tr {
                            th scope="col" { "Repository" }
                            @for figure in waits.first().map(RepositoryWaits::figures).unwrap_or_default() {
                                th scope="col" .number { (figure.label) }
                            }
                        }
                    }
                    tbody {
                        @for repository in waits {
                            tr {
                                th scope="row" { (repository.repository) }
                                @for figure in repository.figures() {
                                    td.number data-label=(figure.label) { (figure.value) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
