//! `/config`: the daemon's `runnerd.toml` in an editor, checked against the slice's `MemoryMax` as it is typed and
//! drawn as the budget it asks for; saving writes it and has the daemon reload it. From the sizing page, the editor
//! starts with a profile's advised memory and slots in the file, or those an auto sizing change replaced,
//! unsaved. Also the doctor's checks, and a QR code that signs another device in.

use std::fmt::Write as _;

use maud::{Markup, html};
use qrcode::QrCode;
use runnerd_core::api::{
    AdmissionPolicy, Check, CheckStatus, ConfigCheck, ConfigFile, Doctor, Resize, Sizing,
};
use runnerd_core::client::{Client, ClientError};
use runnerd_core::config::{ByteSize, Config, resize_profile};
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
    pub(crate) doctor: Doctor,
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
        let file: ConfigFile = daemon.get("/v1/config").await?;
        let (text, resized) = match (query.resize, query.undo) {
            (None, None) => (file.text.clone(), None),
            (_, Some(profile)) => {
                let sizing: Sizing = daemon.get("/v1/sizing").await?;
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
            (Some(profile), None) => {
                let sizing: Sizing = daemon.get("/v1/sizing").await?;
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
        };
        let check = daemon.post_toml("/v1/config/check", text.clone()).await?;
        Ok(ConfigView {
            doctor: daemon.get("/v1/doctor").await?,
            file,
            text,
            resized,
            check,
        })
    }
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
            section.editor aria-labelledby="editor-title" "data-signals:revision"=[view.file.revision.as_deref().and_then(quoted)] {
                h2 #editor-title .visually-hidden { "runnerd.toml" }
                label.visually-hidden for="toml" { "runnerd.toml" }
                textarea #toml name="toml" spellcheck="false" autocomplete="off" rows="28"
                    data-bind:toml=""
                    "data-on:input__debounce.400ms"="@post('/config/check')" { (view.text) }
                div.editor-actions {
                    button.button type="button" data-indicator:_saving=""
                        data-attr:aria-busy="$_saving && 'true'" data-attr:disabled="$_saving"
                        data-on:click="@put('/config')" { "Save and reload" }
                    button.button-quiet type="button" data-on:click="@post('/config/check')" { "Check" }
                    p.muted { "Running jobs keep their slots; slots a change removes go once their jobs finish." }
                    @if view.text.contains(runnerd_core::config::SECRET_SHOWN) {
                        p.muted { "Notification URLs and header values show as " code { "<secret>" } "; saving keeps them." }
                    }
                }
            }
            div.config-side {
                @match &view.resized {
                    Some(Resized::Applied { profile, resize }) => {
                        p.notice-box {
                            "The editor holds the sizing advice for " strong { (profile) } ": "
                            code { "memory = \"" (ByteSize(resize.memory)) "\"" } " and " code { "slots = " (resize.slots) }
                            ". Nothing is written until you save it."
                        }
                    }
                    Some(Resized::Undone { profile, resize }) => {
                        p.notice-box {
                            "The editor holds " strong { (profile) } "'s values from before auto sizing last changed them: "
                            code { "memory = \"" (ByteSize(resize.memory)) "\"" } " and " code { "slots = " (resize.slots) }
                            ". Nothing is written until you save it."
                        }
                    }
                    Some(Resized::Not(why)) => { p.notice-box { (why) } }
                    None => {}
                }
                (result(&view.check, &view.text, &Outcome::Checked))
                (devices(proxies))
                (doctor(&view.doctor))
            }
        }
    };
    page("Config", Some(Page::Config), None, &main)
}

/// A profile's slots or the mirror, as the budget preview draws it.
struct Block {
    label: String,
    bytes: u64,
    class: String,
}

/// What `admission` holds to `MemoryMax`, a block per profile, from the text itself: under caps, every slot of each
/// profile at its cap; under measured, one runner of the largest profile. The mirror with either.
fn blocks(text: &str, admission: AdmissionPolicy) -> Vec<Block> {
    let Ok(config) = Config::parse(text) else {
        return Vec::new();
    };
    let profiles = config
        .profiles
        .iter()
        .enumerate()
        .map(|(index, (name, profile))| (index, name, profile));
    let mut blocks: Vec<Block> = match admission {
        AdmissionPolicy::Caps => profiles
            .map(|(index, name, profile)| Block {
                label: format!("{name} ×{}", profile.slots),
                bytes: profile.memory.0.saturating_mul(u64::from(profile.slots)),
                class: format!("p{}", index % 6),
            })
            .collect(),
        AdmissionPolicy::Measured => profiles
            .max_by_key(|(_, _, profile)| profile.memory.0)
            .map(|(index, name, profile)| Block {
                label: format!("{name} ×1"),
                bytes: profile.memory.0,
                class: format!("p{}", index % 6),
            })
            .into_iter()
            .collect(),
    };
    if config.caches.mirror.enabled {
        blocks.push(Block {
            label: "mirror".to_owned(),
            bytes: config.caches.mirror.memory.0,
            class: "mirror".to_owned(),
        });
    }
    blocks
}

/// The check's verdict, what its admission rule holds to `MemoryMax` and the budget that is, and its warnings.
pub(crate) fn result(check: &ConfigCheck, text: &str, outcome: &Outcome) -> Markup {
    let blocks = blocks(text, check.admission);
    let caps = check.memory_caps.unwrap_or(0);
    let needed = check.memory_needed.or(check.memory_caps).unwrap_or(0);
    let over = check.memory_max.is_some_and(|max| needed > max);
    let whole = check.memory_max.unwrap_or(needed).max(needed);
    let class = if check.valid {
        "check-result valid"
    } else {
        "check-result invalid"
    };
    let needed_name = match check.admission {
        AdmissionPolicy::Caps => "Memory caps",
        AdmissionPolicy::Measured => "Largest runner + mirror",
    };
    html! {
        section #config-result class=(class) aria-labelledby="result-title" aria-live="polite" {
            h2 #result-title {
                @match (outcome, check.valid) {
                    (Outcome::Saved { .. }, _) => "Saved and reloaded",
                    (Outcome::Refused, _) => "Not saved",
                    (Outcome::Checked, true) => "Fits the budget",
                    (Outcome::Checked, false) => "Does not fit",
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
            @if check.memory_caps.is_some() {
                dl.facts {
                    div { dt { (needed_name) } dd { (format::bytes(needed)) } }
                    div { dt { "MemoryMax" } dd { (check.memory_max.map_or("unlimited".to_owned(), format::bytes)) } }
                    @if let Some(max) = check.memory_max {
                        div {
                            dt { @if over { "Over by" } @else { "Left" } }
                            dd { (format::bytes(needed.abs_diff(max))) }
                        }
                    }
                }
                @if check.admission == AdmissionPolicy::Measured {
                    p.muted {
                        "Admission is measured: runners reserve their predicted peaks, and slots only bound how many run at once. Every slot at its cap at once would take "
                        (format::bytes(caps)) "."
                    }
                }
            }
            @if !blocks.is_empty() {
                div.bar.bar-small role="img" aria-label=(format!("{needed_name} {} of {}: {}", format::bytes(needed), check.memory_max.map_or("no limit".to_owned(), format::bytes), blocks.iter().map(|block| format!("{} {}", block.label, format::bytes(block.bytes))).collect::<Vec<_>>().join(", "))) {
                    @for block in &blocks {
                        div class=(format!("block {}", block.class)) style=(format!("--size: {:.4}", ratio(block.bytes, whole))) title=(format!("{}: {}", block.label, format::bytes(block.bytes))) {
                            span.size { (format::bytes(block.bytes)) }
                            span.name { (block.label) }
                        }
                    }
                    @if let Some(max) = check.memory_max.filter(|max| *max > needed) {
                        div.free style=(format!("--size: {:.4}", ratio(max - needed, whole))) {
                            span.size { (format::bytes(max - needed)) }
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
                p { "Scan a QR code with the phone's camera to sign it in. The code works once, for 5 minutes." }
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

/// The doctor's checks for the config the daemon runs with.
fn doctor(doctor: &Doctor) -> Markup {
    let label = |check: &Check| match check.status {
        CheckStatus::Pass => ("ok", "success"),
        CheckStatus::Fail => ("fail", "failure"),
        CheckStatus::Warn => ("warn", "failure"),
        CheckStatus::Skip => ("skip", "neutral"),
        CheckStatus::Info => ("info", "neutral"),
    };
    html! {
        section.doctor aria-labelledby="doctor-title" {
            h2 #doctor-title { "Doctor" }
            ul.check-list {
                @for check in &doctor.checks {
                    @let (text, class) = label(check);
                    li {
                        span class=(format!("badge {class}")) { (text) }
                        div {
                            p { strong { (check.name) } " " (check.detail) }
                            @if let Some(fix) = &check.fix { pre.fix { code { (fix) } } }
                        }
                    }
                }
            }
        }
    }
}
