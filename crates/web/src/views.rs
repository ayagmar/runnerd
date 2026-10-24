//! The dashboard's HTML, rendered on the server with maud from `runnerd_core::api` types. Each page is a module with
//! a struct of what it shows, a `load` that reads it from the daemon, and pure render functions: one per region a
//! live update replaces by its `id`. Views never write to the daemon; `crate::actions` does.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use maud::{DOCTYPE, Markup, PreEscaped, html};
use runnerd_core::api::{JobState, JobSummary, Status};

use crate::assets;
use crate::format;

pub(crate) mod caches;
pub(crate) mod config;
pub(crate) mod history;
pub(crate) mod job;
pub(crate) mod overview;
pub(crate) mod sizing;

/// The pages in the navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Overview,
    History,
    Caches,
    Sizing,
    Config,
}

const MARK: &str = include_str!("../assets/mark.svg");

/// The pieces of a live region that change on their own, such as a reading or the latest stretch of a chart. When
/// the rest of a region is unchanged, its stream sends only the pieces that changed. Rendering records them here; a
/// page ignores them.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    parts: RefCell<Vec<Part>>,
    clocked: RefCell<Vec<String>>,
}

/// One piece of a region: an element with an `id`, as HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) id: String,
    /// For a stretch of a chart, the SVG group it belongs to: stretches come and go as time passes, so the stream
    /// adds a new one to the group and removes one that is no longer drawn.
    pub(crate) group: Option<String>,
    pub(crate) html: String,
}

impl Parts {
    /// Records `markup`, an HTML element with `id`, as a part, and returns it.
    pub(crate) fn html(&self, id: &str, markup: Markup) -> Markup {
        self.push(id, None, markup)
    }

    /// Records `markup`, an SVG element with `id` in the group with id `group`, as a part, and returns it.
    pub(crate) fn stretch(&self, id: &str, group: &str, markup: Markup) -> Markup {
        self.push(id, Some(group), markup)
    }

    /// Records `value`, an attribute's value that follows the page's clock, such as a live chart's transform, and
    /// returns it. A Datastar binding keeps it current on the page, so a change to it alone sends nothing; the value
    /// is the page's until its script runs.
    pub(crate) fn clocked(&self, value: String) -> String {
        self.clocked.borrow_mut().push(value.clone());
        value
    }

    fn push(&self, id: &str, group: Option<&str>, markup: Markup) -> Markup {
        self.parts.borrow_mut().push(Part {
            id: id.to_owned(),
            group: group.map(str::to_owned),
            html: markup.0.clone(),
        });
        markup
    }

    /// The parts, and the values that follow the clock.
    pub(crate) fn into_parts(self) -> (Vec<Part>, Vec<String>) {
        (self.parts.into_inner(), self.clocked.into_inner())
    }
}

/// A short digest of a region's HTML. A page holds one per live region, which its stream reads on connecting to
/// skip the regions the page has already.
pub(crate) fn digest(html: &str) -> String {
    let mut hasher = DefaultHasher::new();
    html.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The event stream of a live page, and what the page shows: the time it was rendered at and a digest of each
/// region, by id. The stream keeps both current in the `shown` signal; live charts follow `$shown.at`.
pub(crate) struct Live<'url> {
    pub(crate) url: &'url str,
    pub(crate) at: u64,
    /// The number of the last log line the page shows, on a job's page.
    pub(crate) log: Option<u64>,
    pub(crate) regions: Vec<(&'static str, String)>,
}

impl Live<'_> {
    /// The `shown` signal as a Datastar expression.
    fn signals(&self) -> String {
        let mut signals = format!("{{shown: {{at: {}", self.at);
        if let Some(log) = self.log {
            let _ = write!(signals, ", log: {log}");
        }
        for (id, digest) in &self.regions {
            let _ = write!(signals, ", {}: {}", js(id), js(digest));
        }
        signals.push_str("}}");
        signals
    }
}

/// A full page: the navigation, `main`, and, with `live`, the event stream that keeps its regions current.
pub(crate) fn page(
    title: &str,
    active: Option<Page>,
    live: Option<&Live<'_>>,
    main: &Markup,
) -> Markup {
    let nav = [
        (Page::Overview, "/", "Overview"),
        (Page::History, "/history", "History"),
        (Page::Caches, "/caches", "Caches"),
        (Page::Sizing, "/sizing", "Sizing"),
        (Page::Config, "/config", "Config"),
    ];
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="color-scheme" content="light dark";
                // The browser's bar and an installed app's status bar take the page's background; theme.js keeps
                // them on the theme chosen with the switch.
                meta name="theme-color" content="#f8f8f8" media="(prefers-color-scheme: light)";
                meta name="theme-color" content="#0d0f0e" media="(prefers-color-scheme: dark)";
                meta name="description" content="runnerd's dashboard: its slots, jobs, caches and config on this machine.";
                title { (title) " · runnerd" }
                link rel="icon" href=(assets::path("mark.svg")) type="image/svg+xml";
                link rel="manifest" href=(assets::path("manifest.webmanifest"));
                link rel="preload" href=(assets::path("libre-franklin.woff2")) as="font" type="font/woff2" crossorigin;
                link rel="stylesheet" href=(assets::path("style.css"));
                script src=(assets::path("theme.js")) {}
                script type="module" src=(assets::path("datastar.js")) {}
            }
            body data-signals=[live.map(Live::signals)] {
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
                    @if let Some(live) = live {
                        // The indicator before the init: it has to listen when the request starts.
                        span.connection data-indicator:_live=""
                            data-init=(format!("@get('{}', {{retryMaxCount: 1000000, retryMaxWait: 10000}})", live.url))
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
            h1 { @if reason == SignedOut::CodeUsed { "That link was used already, or it expired" } @else { "Sign in to runnerd" } }
            p { "On another device, such as a phone: open the dashboard on runnerd's computer, choose " strong { "Add a device" } " on the Config page, and scan the QR code. Or run " code { "runnerctl web --qr" } " on that computer." }
            p { "On runnerd's computer: run " code { "runnerctl web" } ". It opens a link that signs this browser in." }
            p.muted { "Each link works once. A device stays signed in for 30 days after its last visit." }
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

/// A notice that offers a next step, such as cancelling the job a pause waits for, or `dismiss` to drop it. It stays
/// until answered.
pub(crate) fn question(message: &str, action: &str, label: &str, dismiss: &str) -> Markup {
    html! {
        div #notice role="status" aria-live="polite" {
            div.toast.failed.question {
                p { (message) }
                div.toast-actions {
                    button.button-danger type="button" data-indicator:_answering=""
                        data-attr:aria-busy="$_answering && 'true'" data-attr:disabled="$_answering"
                        data-on:click=(action) { (label) }
                    button.button-quiet type="button" data-on:click="el.closest('.toast').remove()" { (dismiss) }
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

/// What a job's outcome is called, and its CSS class.
struct Outcome {
    label: &'static str,
    class: &'static str,
}

fn outcome(job: &JobSummary) -> Outcome {
    let (label, class) = match (job.state, job.conclusion.as_deref()) {
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
    };
    Outcome { label, class }
}

/// A job's outcome in a word, as its badge says it.
pub(crate) fn outcome_label(job: &JobSummary) -> &'static str {
    outcome(job).label
}

/// The CSS class of a job's outcome: `success`, `failure`, `running`, `queued` or `neutral`.
pub(crate) fn outcome_class(job: &JobSummary) -> &'static str {
    outcome(job).class
}

/// A job's outcome as a badge.
pub(crate) fn badge(job: &JobSummary) -> Markup {
    let Outcome { label, class } = outcome(job);
    html! { span class=(format!("badge {class}")) { (label) } }
}

/// A time as text with its exact value in a tooltip.
pub(crate) fn time_ago(at: u64, now: u64) -> Markup {
    html! { time datetime=(format::rfc3339(at)) title=(format::timestamp(at)) { (format::ago(at, now)) } }
}

/// Seconds a job waited for a runner, and ran; none for a job that has not started.
pub(crate) struct Durations {
    pub(crate) waited: Option<u64>,
    pub(crate) ran: Option<u64>,
}

/// How long a job waited and ran.
pub(crate) fn durations(job: &JobSummary, now: u64) -> Durations {
    let timeline = &job.timeline;
    let waited = timeline
        .started_at
        .map(|started| started.saturating_sub(timeline.queued_at));
    let ran = timeline
        .started_at
        .map(|started| timeline.finished_at.unwrap_or(now).saturating_sub(started));
    Durations { waited, ran }
}

/// Text as a JavaScript string literal, for a Datastar expression: `it's` → `'it\'s'`.
pub(crate) fn js(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('\'');
    for c in text.chars() {
        match c {
            '\\' | '\'' => {
                literal.push('\\');
                literal.push(c);
            }
            '\n' => literal.push_str("\\n"),
            _ => literal.push(c),
        }
    }
    literal.push('\'');
    literal
}

/// `text`'s bytes in hex: an id or signal name no two texts share, as `node-js` and `node_js` would as a slug.
fn hex(text: &str) -> String {
    let mut hex = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The id of a slot's card, which its budget block and its job's rows point at: `jvm` slot 1 → `slot-6a766d-1`.
pub(crate) fn slot_id(profile: &str, slot: usize) -> String {
    format!("slot-{}-{slot}", hex(profile))
}

/// The id of a repository's row, which a pause or resume replaces: `a/b` → `repo-612f62`. Also a signal name's
/// suffix, so it has only letters and digits after its dash.
pub(crate) fn repository_id(name: &str) -> String {
    format!("repo-{}", hex(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_whose_slugs_match_get_their_own_ids() {
        assert_ne!(slot_id("node-js", 1), slot_id("node_js", 1));
        assert_ne!(slot_id("a", 11), slot_id("a1", 1));
        assert_ne!(repository_id("halyard/b-c"), repository_id("halyard/b_c"));
    }

    #[test]
    fn a_quote_in_a_job_name_stays_inside_its_expression_string() {
        assert_eq!(js(r"it's a \ test"), r"'it\'s a \\ test'");
    }
}
