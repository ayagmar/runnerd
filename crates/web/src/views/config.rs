//! `/config`: the daemon's `runnerd.toml` in an editor, checked against the slice's `MemoryMax` as it is typed and
//! drawn as the budget it asks for; saving writes it and has the daemon reload it. From the sizing page, the editor
//! starts with a profile's advised memory and slots in the file, unsaved. Also the doctor's checks.

use maud::{Markup, html};
use runnerd_core::api::{Check, CheckStatus, ConfigCheck, ConfigFile, Doctor, Resize, Sizing};
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
    /// Not applied, and why.
    Not(String),
}

impl ConfigView {
    pub(crate) async fn load(
        daemon: &Client,
        resize: Option<String>,
    ) -> Result<ConfigView, ClientError> {
        let file: ConfigFile = daemon.get("/v1/config").await?;
        let (text, resized) = match resize {
            None => (file.text.clone(), None),
            Some(profile) => {
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
    /// Written to the file and reloaded.
    Saved(String),
    /// Refused; the file is as it was.
    Refused,
}

/// The whole page.
pub(crate) fn render(view: &ConfigView) -> Markup {
    let main = html! {
        div.section-head.page-head {
            h1.visually-hidden { "Config" }
            p.muted { code { (view.file.path) } }
        }
        div.config-layout {
            section.editor aria-labelledby="editor-title" {
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
                    Some(Resized::Not(why)) => { p.notice-box { (why) } }
                    None => {}
                }
                (result(&view.check, &view.text, &Outcome::Checked))
                (doctor(&view.doctor))
            }
        }
    };
    page("Config", Some(Page::Config), None, &main)
}

/// A slot or the mirror, as the budget preview draws it.
struct Block {
    label: String,
    bytes: u64,
    class: String,
}

/// The memory caps a config asks for, a block per slot, from the text itself.
fn blocks(text: &str) -> Vec<Block> {
    let Ok(config) = Config::parse(text) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for (index, (name, profile)) in config.profiles.iter().enumerate() {
        for slot in 1..=profile.slots {
            blocks.push(Block {
                label: format!("{name} {slot}"),
                bytes: profile.memory.0,
                class: format!("p{}", index % 6),
            });
        }
    }
    if config.caches.mirror.enabled {
        blocks.push(Block {
            label: "mirror".to_owned(),
            bytes: config.caches.mirror.memory.0,
            class: "mirror".to_owned(),
        });
    }
    blocks
}

/// The check's verdict, the budget the config asks for against `MemoryMax`, and its warnings.
pub(crate) fn result(check: &ConfigCheck, text: &str, outcome: &Outcome) -> Markup {
    let blocks = blocks(text);
    let caps = check.memory_caps.unwrap_or(0);
    let over = check.memory_max.is_some_and(|max| caps > max);
    let whole = check.memory_max.unwrap_or(caps).max(caps);
    let class = if check.valid {
        "check-result valid"
    } else {
        "check-result invalid"
    };
    html! {
        section #config-result class=(class) aria-labelledby="result-title" aria-live="polite" {
            h2 #result-title {
                @match (outcome, check.valid) {
                    (Outcome::Saved(_), _) => "Saved and reloaded",
                    (Outcome::Refused, _) => "Not saved",
                    (Outcome::Checked, true) => "Fits the budget",
                    (Outcome::Checked, false) => "Does not fit",
                }
            }
            @if let Outcome::Saved(path) = outcome {
                p { "Written to " code { (path) } ". runnerd runs with it now." }
            }
            @if *outcome == Outcome::Refused {
                p { (path_unchanged()) }
            }
            // A valid config's message repeats the figures below it.
            @if !check.valid { p.check-message { (check.message) } }
            @if check.memory_caps.is_some() {
                dl.facts {
                    div { dt { "Memory caps" } dd { (format::bytes(caps)) } }
                    div { dt { "MemoryMax" } dd { (check.memory_max.map_or("unlimited".to_owned(), format::bytes)) } }
                    @if let Some(max) = check.memory_max {
                        div {
                            dt { @if over { "Over by" } @else { "Left" } }
                            dd { (format::bytes(caps.abs_diff(max))) }
                        }
                    }
                }
            }
            @if !blocks.is_empty() {
                div.bar.bar-small role="img" aria-label=(format!("Memory caps {} of {}: {}", format::bytes(caps), check.memory_max.map_or("no limit".to_owned(), format::bytes), blocks.iter().map(|block| format!("{} {}", block.label, format::bytes(block.bytes))).collect::<Vec<_>>().join(", "))) {
                    @for block in &blocks {
                        div class=(format!("block {}", block.class)) style=(format!("--size: {:.4}", ratio(block.bytes, whole))) title=(format!("{}: {}", block.label, format::bytes(block.bytes))) {
                            span.size { (format::bytes(block.bytes)) }
                            span.name { (block.label) }
                        }
                    }
                    @if let Some(max) = check.memory_max.filter(|max| *max > caps) {
                        div.free style=(format!("--size: {:.4}", ratio(max - caps, whole))) {
                            span.size { (format::bytes(max - caps)) }
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
