//! `/config`: the daemon's `runnerd.toml` in an editor, checked against the slice's `MemoryMax` as it is typed and
//! drawn as the budget it asks for; saving writes it and has the daemon reload it. From the sizing page, the editor
//! starts with a profile's advised memory and slots in the file, or those an auto sizing change replaced,
//! unsaved. Also the doctor's checks, which the page asks for once it shows, since the doctor takes a while; and a
//! QR code that signs another device in.

use std::fmt::Write as _;

use maud::{Markup, html};
use qrcode::QrCode;
use runnerd_core::api::{Check, ConfigCheck, ConfigFile, Doctor, Resize, Sizing, Tone};
use runnerd_core::client::{Client, ClientError};
use runnerd_core::config::{Config, profile_sizes, resize_profile, resized_at};
use serde::Deserialize;

use super::{Page, page};
use crate::chart::ratio;
use crate::format;

/// `/config` query parameters.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ConfigQuery {
    /// A profile whose sizing advice the editor starts with, from the sizing page.
    pub(crate) resize: Option<String>,
    /// A profile whose memory and slots from before auto sizing last changed them the editor starts with.
    pub(crate) undo: Option<String>,
}

/// Everything the config page shows.
#[derive(Debug, Clone)]
pub(crate) struct ConfigView {
    pub(crate) file: ConfigFile,
    /// What the editor holds: the file, or the file with a profile's sizing advice in it.
    pub(crate) text: String,
    /// The advice the editor holds, or why it holds none.
    pub(crate) resized: Option<Resized>,
    pub(crate) check: ConfigCheck,
}

/// A profile's advised `memory` and `slots`, written into the editor's text and not saved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resized {
    /// In the editor.
    Applied { profile: String, resize: Resize },
    /// What auto sizing last replaced, in the editor.
    Undone { profile: String, resize: Resize },
    /// Not applied, and why.
    Not(String),
}

impl ConfigView {
    pub(crate) async fn load(
        daemon: &Client,
        query: ConfigQuery,
    ) -> Result<ConfigView, ClientError> {
        let sizing = async {
            match (&query.resize, &query.undo) {
                (None, None) => Ok(None),
                _ => daemon.get::<Sizing>("/v1/sizing").await.map(Some),
            }
        };
        let (file, sizing) = tokio::join!(daemon.get::<ConfigFile>("/v1/config"), sizing);
        let (file, sizing) = (file?, sizing?);
        let (text, resized) = match (query.resize, query.undo, sizing) {
            (_, Some(profile), Some(sizing)) => {
                let last = sizing
                    .changes
                    .iter()
                    .rfind(|change| change.profile == profile);
                match last {
                    None => (
                        file.text.clone(),
                        Some(Resized::Not(format!(
                            "Auto sizing changed {profile} in none of the history read."
                        ))),
                    ),
                    Some(change) => match resize_profile(&file.text, &profile, change.from) {
                        Ok(text) => (
                            text,
                            Some(Resized::Undone {
                                profile,
                                resize: change.from,
                            }),
                        ),
                        Err(error) => (file.text.clone(), Some(Resized::Not(error.to_string()))),
                    },
                }
            }
            (Some(profile), None, Some(sizing)) => {
                let change = sizing
                    .profiles
                    .iter()
                    .find(|advised| advised.name == profile)
                    .map(|advised| advised.advice.change());
                match change {
                    None => (
                        file.text.clone(),
                        Some(Resized::Not(format!("There is no profile {profile}."))),
                    ),
                    Some(None) => (
                        file.text.clone(),
                        Some(Resized::Not(format!(
                            "{profile} has no sizing advice to apply."
                        ))),
                    ),
                    Some(Some(resize)) => match resize_profile(&file.text, &profile, resize) {
                        Ok(text) => (text, Some(Resized::Applied { profile, resize })),
                        Err(error) => (file.text.clone(), Some(Resized::Not(error.to_string()))),
                    },
                }
            }
            _ => (file.text.clone(), None),
        };
        let check = daemon.post_toml("/v1/config/check", text.clone()).await?;
        Ok(ConfigView {
            file,
            text,
            resized,
            check,
        })
    }
}

/// What the editor was opened with from the sizing page: the change in `profile`'s sizes from `file` to `text`, the
/// editor's, as core words it, and a button that selects them in the editor; or why it holds no change.
fn notice(resized: &Resized, file: &str, text: &str) -> Markup {
    let (profile, resize) = match resized {
        Resized::Applied { profile, resize } | Resized::Undone { profile, resize } => {
            (profile, resize)
        }
        Resized::Not(why) => return html! { div.notice-box { p { (why) } } },
    };
    let line = resize.line_from(profile_sizes(file, profile).as_ref());
    let select = resized_at(text, profile, *resize).and_then(|at| {
        // A textarea counts UTF-16 units, and reads each line break as one `\n`.
        let offset = |byte: usize| {
            Some(
                text.get(..byte)?
                    .replace("\r\n", "\n")
                    .encode_utf16()
                    .count(),
            )
        };
        Some(select_in_editor(offset(at.start)?, offset(at.end)?))
    });
    html! {
        div.notice-box {
            p {
                @if let Resized::Applied { .. } = resized {
                    "Sizing advice for " strong { (profile) } ": " (line) ". Not saved yet."
                } @else {
                    "Auto sizing's last change to " strong { (profile) } ", undone: " (line) ". Not saved yet."
                }
            }
            // Its offsets are of the text the page opened with: an edit moves what they select.
            @if let Some(select) = select {
                button.button-quiet type="button" data-show="!$_edited" data-on:click=(select) { "Show in the editor" }
            }
        }
    }
}

/// Selects `start..end`, in UTF-16 units, in the editor and scrolls it there, which setting a selection does not. A
/// hidden textarea as wide as the editor, holding the text before the selection, measures how far down it starts,
/// wrapped lines included.
fn select_in_editor(start: usize, end: usize) -> String {
    format!(
        "const t = document.getElementById('toml'); const m = document.createElement('textarea'); \
         m.style.cssText = 'position: absolute; visibility: hidden; height: 0; min-height: 0; width: ' + t.offsetWidth + 'px'; \
         t.after(m); m.value = t.value.slice(0, {start}); const top = m.scrollHeight; m.remove(); \
         t.focus(); t.setSelectionRange({start}, {end}); t.scrollTop = top - t.clientHeight / 3"
    )
}

/// What a save did, for the result region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Only checked.
    Checked,
    /// Written to the file and reloaded: its path, and the revision the editor saves against next.
    Saved {
        path: String,
        revision: Option<String>,
    },
    /// Refused; the file is as it was.
    Refused,
}

/// A revision as a JavaScript string for a signal; `None` for one that is not hex, which the daemon never sends.
fn quoted(revision: &str) -> Option<String> {
    revision
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        .then(|| format!("'{revision}'"))
}

/// The whole page; `proxies` are `[web] origins`, where another device can be signed in.
pub(crate) fn render(view: &ConfigView, proxies: &[String]) -> Markup {
    let main = html! {
        div.section-head.page-head {
            h1.visually-hidden { "Config" }
            p.muted { code { (view.file.path) } }
        }
        div.config-layout {
            section.editor aria-labelledby="editor-title" "data-signals:revision"=[quoted(&view.file.revision)]
                "data-signals:_edited"="false" {
                h2 #editor-title .visually-hidden { "runnerd.toml" }
                // Before the editor, so it is read and reached before the text it describes.
                @if let Some(resized) = &view.resized { (notice(resized, &view.file.text, &view.text)) }
                label.visually-hidden for="toml" { "runnerd.toml" }
                // The parser drops a line break right after the tag: this one, so the text's own first line stays.
                textarea #toml name="toml" spellcheck="false" autocomplete="off" rows="28"
                    data-bind:toml="" data-indicator:_checking="" data-on:input="$_edited = true"
                    "data-on:input__debounce.400ms"="@post('/config/check')" { "\n" (view.text) }
                div.editor-actions {
                    button.button type="button" data-indicator:_saving=""
                        data-attr:aria-busy="$_saving && 'true'" data-attr:disabled="$_saving"
                        data-on:click="@put('/config')" { "Save and reload" }
                    button.button-quiet type="button" data-indicator:_checking=""
                        data-on:click="@post('/config/check')" { "Check" }
                    p.muted { "Running jobs keep their slots; slots a change removes go once their jobs finish." }
                    @if view.text.contains(runnerd_core::config::SECRET_SHOWN) {
                        p.muted { "Notification URLs and header values show as " code { "<secret>" } "; saving keeps them." }
                    }
                }
            }
            div.config-side {
                (result(&view.check, &view.text, &Outcome::Checked))
                (devices(proxies))
                (doctor_pending(None))
            }
        }
    };
    page("Config", Some(Page::Config), None, &main)
}

/// The check's verdict, what its admission rule holds to `MemoryMax` and the budget that is, and its warnings.
pub(crate) fn result(check: &ConfigCheck, text: &str, outcome: &Outcome) -> Markup {
    let parsed = Config::parse(text).ok();
    let budget = check.budget();
    // Without a total to hold to MemoryMax, as when the caps overflow, there is no budget to show.
    let blocks = parsed
        .as_ref()
        .zip(budget)
        .and_then(|(config, budget)| config.budget_blocks(budget.admission))
        .unwrap_or_default();
    let needed = budget.map_or(0, |budget| budget.needed);
    let over = budget.is_some_and(|budget| budget.over());
    let whole = check.memory_max.unwrap_or(needed).max(needed);
    let verdict = check.verdict(parsed.as_ref());
    let class = match verdict.tone() {
        Tone::Good => "check-result valid",
        Tone::Warn => "check-result valid unbounded",
        Tone::Bad | Tone::Neutral | Tone::Quiet => "check-result invalid",
    };
    let needed_name = budget.map_or("", |budget| budget.needed_label());
    html! {
        // Dimmed while the daemon checks or saves newer text: what it shows is of the text before.
        section #config-result class=(class) aria-labelledby="result-title" aria-live="polite"
            data-attr:aria-busy="($_checking || $_saving) && 'true'" {
            h2 #result-title {
                @match (outcome, check.valid) {
                    (Outcome::Saved { .. }, _) => "Saved and reloaded",
                    (Outcome::Refused, _) => "Not saved",
                    (Outcome::Checked, _) => (verdict.label()),
                }
            }
            @if let Outcome::Saved { path, revision } = outcome {
                p "data-signals:revision"=[revision.as_deref().and_then(quoted)] { "Written to " code { (path) } ". runnerd runs with it now." }
            }
            @if *outcome == Outcome::Refused {
                p { (path_unchanged()) }
            }
            // A valid config's message repeats the figures below it.
            @if !check.valid { p.check-message { (check.message) } }
            @if let Some(budget) = budget {
                dl.facts {
                    div { dt { (needed_name) } dd { (format::bytes(needed)) } }
                    div { dt { "MemoryMax" } dd { (check.memory_max.map_or("unlimited".to_owned(), format::bytes)) } }
                    @if let Some(remainder) = budget.remainder() {
                        div {
                            dt { (remainder.label) }
                            dd { (format::bytes(remainder.memory)) }
                        }
                    }
                }
                @if let Some(note) = budget.admission_note() {
                    p.muted { (note) }
                }
            }
            @if !blocks.is_empty() {
                div.bar.bar-small role="img" aria-label=(format!("{needed_name} {} of {}: {}", format::bytes(needed), check.memory_max.map_or("no limit".to_owned(), format::bytes), blocks.iter().map(|block| format!("{} {}", block.label, format::bytes(block.memory))).collect::<Vec<_>>().join(", "))) {
                    @for block in &blocks {
                        div class=(format!("block {}", block.profile.map_or("mirror".to_owned(), |index| format!("p{}", index % 6)))) style=(format!("--size: {:.4}", ratio(block.memory, whole))) title=(format!("{}: {}", block.label, format::bytes(block.memory))) {
                            (super::block_size(block.memory))
                            span.name { (block.label) }
                        }
                    }
                    @if let Some(max) = check.memory_max.filter(|max| *max > needed) {
                        div.free style=(format!("--size: {:.4}", ratio(max - needed, whole))) {
                            (super::block_size(max - needed))
                            span.name { "free" }
                        }
                    }
                    @if over {
                        @if let Some(max) = check.memory_max {
                            span.max-mark style=(format!("--at: {:.2}%", ratio(max, whole) * 100.0)) { span.max-label { "MemoryMax" } }
                        }
                    }
                }
            }
            @if !check.warnings.is_empty() {
                ul.warnings {
                    @for warning in &check.warnings { li { (warning) } }
                }
            }
        }
    }
}

fn path_unchanged() -> &'static str {
    "The file is as it was, and runnerd runs with the config it had."
}

/// Signing a phone or another computer in: a QR code of a one-time login link on one of `proxies`.
fn devices(proxies: &[String]) -> Markup {
    html! {
        section.devices aria-labelledby="devices-title" {
            h2 #devices-title { "Add a device" }
            @if proxies.is_empty() {
                p.muted {
                    "To sign a phone in, list the address it reaches this dashboard on, such as your "
                    code { "tailscale serve" } " URL, in " code { "[web] origins" } " and restart runnerd."
                }
            } @else {
                @if let [_, _, ..] = proxies {
                    label for="pairing" { "Sign in on" }
                    select #pairing data-bind:pairing="" {
                        @for proxy in proxies { option value=(proxy) { (proxy) } }
                    }
                }
                div #pairing-code {
                    button.button type="button" data-on:click="@post('/pair')" { "Show a QR code" }
                }
            }
        }
    }
}

/// The QR code of `url`, a one-time login link on `origin`, in place of the button that asked for it.
pub(crate) fn pairing_code(url: &str, origin: &str) -> Markup {
    html! {
        div #pairing-code {
            figure.qr {
                (qr_svg(url))
                figcaption { "Signs one device in on " code { (origin) } " within 5 minutes." }
            }
            details.pairing-link {
                summary { "Show the link" }
                code { (url) }
            }
            button.button-quiet type="button" data-on:click="@post('/pair')" { "Show a new code" }
        }
    }
}

/// `text` as a QR code in SVG: dark modules on white with the four-module quiet zone, whatever the theme, since
/// cameras read dark on light best.
fn qr_svg(text: &str) -> Markup {
    let Ok(code) = QrCode::new(text.as_bytes()) else {
        return html! { p { "This link is too long for a QR code." } };
    };
    let width = code.width();
    let path = code
        .to_colors()
        .iter()
        .enumerate()
        .filter(|(_, color)| **color == qrcode::Color::Dark)
        .fold(String::new(), |mut path, (index, _)| {
            let _ = write!(path, "M{},{}h1v1h-1z", index % width + 4, index / width + 4);
            path
        });
    let size = width + 8;
    html! {
        svg.qr-code viewBox=(format!("0 0 {size} {size}")) role="img" aria-label="QR code of a one-time sign-in link" {
            rect width=(size) height=(size) fill="#fff" {}
            path d=(path) fill="#000" shape-rendering="crispEdges" {}
        }
    }
}

/// The doctor's region before its checks arrive: it asks for them as soon as it shows. After a save, `saved` is the
/// revision saved and goes in a header: the attribute differs from the page's, so Datastar asks again, and the address
/// stays the same, since Datastar cancels a request still out only for a new one with the same method and address.
pub(crate) fn doctor_pending(saved: Option<&str>) -> Markup {
    let after = saved
        .filter(|revision| quoted(revision).is_some())
        .map(|revision| format!("headers: {{'Runnerd-After': '{revision}'}}, "));
    html! {
        section #doctor .doctor aria-labelledby="doctor-title" aria-busy="true"
            data-init=(doctor_get(after.as_deref().unwrap_or_default())) {
            h2 #doctor-title { "Doctor" }
            ul.check-list.pending aria-hidden="true" {
                @for _ in 0..4 { li {} }
            }
        }
    }
}

/// A Datastar request for the doctor's region, with `options` before the filter and no signals: a GET sends them in
/// its address, and the editor's text, which may hold a secret just typed, has no place in a proxy's access log.
fn doctor_get(options: &str) -> String {
    format!("@get('/config/doctor', {{{options}filterSignals: {{include: /^$/}}}})")
}

/// The doctor's checks for the config the daemon runs with, or why there are none; in place of `doctor_pending`.
pub(crate) fn doctor(doctor: &Result<Doctor, ClientError>) -> Markup {
    let label = |check: &Check| {
        let class = match check.status.tone() {
            Tone::Good => "success",
            Tone::Bad => "failure",
            Tone::Warn => "warn",
            Tone::Neutral | Tone::Quiet => "neutral",
        };
        (check.status.label(), class)
    };
    html! {
        section #doctor .doctor aria-labelledby="doctor-title" {
            h2 #doctor-title { "Doctor" }
            @match doctor {
                Ok(doctor) => {
                    ul.check-list {
                        @for check in &doctor.checks {
                            @let (text, class) = label(check);
                            // A check that needs nothing from the reader steps back; one that does keeps its fix.
                            li class=[matches!(check.status.tone(), Tone::Bad | Tone::Warn).then_some("needs")] {
                                span class=(format!("badge {class}")) { (text) }
                                div {
                                    p { strong { (check.name) } " " (check.detail) }
                                    @if let Some(fix) = &check.fix { pre.fix { code { (fix) } } }
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    p.check-message { (error) }
                    button.button-quiet type="button" data-indicator:_doctoring=""
                        data-attr:aria-busy="$_doctoring && 'true'"
                        data-on:click=(doctor_get("")) { "Run the doctor again" }
                }
            }
        }
    }
}
