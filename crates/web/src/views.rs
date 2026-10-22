//! The dashboard's HTML, rendered on the server with maud from `runnerd_core::api` types. Each page is a module with
//! a struct of what it shows, a `load` that reads it from the daemon, and pure render functions: one per region a
//! live update replaces by its `id`. Views never write to the daemon; `crate::actions` does.

use maud::{DOCTYPE, Markup, PreEscaped, html};
use runnerd_core::api::{JobState, JobSummary, Status};

use crate::assets;
use crate::format;

pub(crate) mod caches;
pub(crate) mod config;
pub(crate) mod history;
pub(crate) mod job;
pub(crate) mod overview;

/// The pages in the navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Overview,
    History,
    Caches,
    Config,
}

const MARK: &str = include_str!("../assets/mark.svg");

/// A full page: the navigation, `main`, and, with `live`, the event stream that keeps its regions current.
pub(crate) fn page(title: &str, active: Option<Page>, live: Option<&str>, main: &Markup) -> Markup {
    let nav = [
        (Page::Overview, "/", "Overview"),
        (Page::History, "/history", "History"),
        (Page::Caches, "/caches", "Caches"),
        (Page::Config, "/config", "Config"),
    ];
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="light dark";
                title { (title) " · runnerd" }
                link rel="icon" href=(assets::path("mark.svg")) type="image/svg+xml";
                link rel="preload" href="/assets/libre-franklin.woff2" as="font" type="font/woff2" crossorigin;
                link rel="stylesheet" href=(assets::path("style.css"));
                script src=(assets::path("theme.js")) {}
                script type="module" src=(assets::path("datastar.js")) {}
            }
            body {
                a.skip href="#main" { "Skip to content" }
                header.top {
                    a.wordmark href="/" aria-label="runnerd overview" {
                        span.mark aria-hidden="true" { (PreEscaped(MARK)) }
                        "runnerd"
                    }
                    nav aria-label="Dashboard" {
                        @for (page, href, label) in nav {
                            @if active == Some(page) {
                                a href=(href) aria-current="page" { (label) }
                            } @else {
                                a href=(href) { (label) }
                            }
                        }
                    }
                    @if let Some(url) = live {
                        // The indicator before the init: it has to listen when the request starts.
                        span.connection data-indicator:_live=""
                            data-init=(format!("@get('{url}', {{retryMaxCount: 1000000, retryMaxWait: 10000}})"))
                            data-class:online="$_live"
                            data-attr:title="$_live ? 'Updates as it happens' : 'Not connected; retrying'" {
                            span.dot aria-hidden="true" {}
                            span data-text="$_live ? 'Live' : 'Offline'" { "Live" }
                        }
                    }
                    button.theme type="button" data-theme-toggle aria-label="Switch between light and dark" {
                        svg viewBox="0 0 20 20" aria-hidden="true" {
                            circle cx="10" cy="10" r="8" fill="none" stroke="currentColor" stroke-width="2" {}
                            path d="M10 2a8 8 0 0 1 0 16z" fill="currentColor" {}
                        }
                    }
                }
                main #main { (main) }
                div #notice role="status" aria-live="polite" {}
            }
        }
    }
}

/// Why a request gets the signed-out page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignedOut {
    /// It came without the session cookie.
    NoSession,
    /// It brought a login code that was used already or never issued.
    CodeUsed,
}

/// The page every request without the session cookie gets.
pub(crate) fn signed_out(reason: SignedOut) -> Markup {
    let main = html! {
        section.signed-out {
            h1 { @if reason == SignedOut::CodeUsed { "That link was used already" } @else { "Open the dashboard from your terminal" } }
            p { "Run " code { "runnerctl web" } " on this machine. It opens a one-time link that signs this browser in." }
            p.muted { "Each link works once. The daemon writes a fresh one to " code { "$XDG_RUNTIME_DIR/runnerd.web" } " after every use." }
        }
    };
    page("Sign in", None, None, &main)
}

/// A page for a daemon error, such as an unknown job.
pub(crate) fn error_page(title: &str, message: &str) -> Markup {
    let main = html! {
        section.signed-out {
            h1 { (title) }
            p { (message) }
            p { a href="/" { "Back to the overview" } }
        }
    };
    page(title, None, None, &main)
}

/// Whether a notice reports something done or something that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Done,
    Failed,
}

/// A notice for `#notice`, such as why a pause failed. It goes away by itself.
pub(crate) fn notice(message: &str, tone: Tone) -> Markup {
    html! {
        div #notice role="status" aria-live="polite" {
            p class=(if tone == Tone::Failed { "toast failed" } else { "toast" }) data-init="setTimeout(() => el.remove(), 8000)" { (message) }
        }
    }
}

/// A notice that offers a next step, such as cancelling the job a pause waits for. It stays until answered.
pub(crate) fn question(message: &str, action: &str, label: &str) -> Markup {
    html! {
        div #notice role="status" aria-live="polite" {
            div.toast.failed.question {
                p { (message) }
                div.toast-actions {
                    button.button-danger type="button" data-indicator:_answering=""
                        data-attr:aria-busy="$_answering && 'true'" data-attr:disabled="$_answering"
                        data-on:click=(action) { (label) }
                    button.button-quiet type="button" data-on:click="el.closest('.toast').remove()" { "Keep it running" }
                }
            }
        }
    }
}

/// The colour class of a profile: its place among the configured profiles.
pub(crate) fn profile_class(status: &Status, profile: &str) -> String {
    let index = status
        .profiles
        .iter()
        .position(|candidate| candidate.name == profile)
        .unwrap_or(usize::MAX);
    if index == usize::MAX {
        "p-none".to_owned()
    } else {
        format!("p{}", index % 6)
    }
}

/// What a job's outcome is called and its CSS class.
pub(crate) fn outcome(job: &JobSummary) -> (&'static str, &'static str) {
    match (job.state, job.conclusion.as_deref()) {
        (JobState::Queued, _) => ("queued", "queued"),
        (JobState::Running, _) => ("running", "running"),
        (JobState::Lost, _) => ("lost", "failure"),
        (JobState::Elsewhere, _) => ("elsewhere", "neutral"),
        (JobState::Completed, Some("success")) => ("success", "success"),
        (JobState::Completed, Some("failure")) => ("failure", "failure"),
        (JobState::Completed, Some("timed_out")) => ("timed out", "failure"),
        (JobState::Completed, Some("cancelled")) => ("cancelled", "neutral"),
        (JobState::Completed, Some("skipped")) => ("skipped", "neutral"),
        (JobState::Completed, _) => ("completed", "neutral"),
    }
}

/// A job's outcome as a badge.
pub(crate) fn badge(job: &JobSummary) -> Markup {
    let (label, class) = outcome(job);
    html! { span class=(format!("badge {class}")) { (label) } }
}

/// A time as text with its exact value in a tooltip.
pub(crate) fn time_ago(at: u64, now: u64) -> Markup {
    html! { time datetime=(format::rfc3339(at)) title=(format::timestamp(at)) { (format::ago(at, now)) } }
}

/// Seconds a job waited for a runner, and ran.
pub(crate) fn waited_and_ran(job: &JobSummary, now: u64) -> (Option<u64>, Option<u64>) {
    let timeline = &job.timeline;
    let waited = timeline
        .started_at
        .map(|started| started.saturating_sub(timeline.queued_at));
    let ran = timeline
        .started_at
        .map(|started| timeline.finished_at.unwrap_or(now).saturating_sub(started));
    (waited, ran)
}

/// The path of a repository's row, for the ids live updates use: `halyard/ledger` → `halyard-ledger`.
pub(crate) fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
