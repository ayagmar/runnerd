//! Auto sizing: under `[sizing] auto`, every few minutes, writes the changes `sizing::auto::changes` picks into the
//! config through the same check and commit as `PUT /v1/config`, keeping the rest of the file as written. Each change
//! is recorded in the history as pending before the file is written, then as done (announced as `sizing.changed`)
//! or failed; one left pending by a crash is resolved from the config at the next look. It never writes a config the
//! check refuses, a file that changed on disk since the daemon read it, or one without a slot a runner is in, and does
//! nothing while auto sizing is off.

use std::collections::BTreeMap;
use std::time::Duration;

use runnerd_core::api::SizingChange;
use runnerd_core::config::{Config, resize_profile};
use runnerd_core::scheduler::engine::RunningSlots;
use runnerd_core::sizing::auto::{Context, changes};
use runnerd_core::sizing::{History, Window};
use runnerd_core::store::{AutoSizing, Resolution, StoreError};
use tracing::{info, warn};

use super::control::replace;
use super::history::now;
use super::{AppState, ConfigText};

/// How often auto sizing looks at the advice in the daemon.
pub(crate) const EVERY: Duration = Duration::from_mins(5);

/// Looks now, then every `every` until the server stops. What a look refuses is logged once until it changes.
pub(super) async fn run(state: AppState, every: Duration) {
    let mut stopping = state.stopping.clone();
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut refused = BTreeMap::new();
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
        look(&state, &mut refused).await;
    }
}

/// Logs `message` about `subject` unless it is what was logged last.
fn refuse(refused: &mut BTreeMap<String, String>, subject: &str, message: String) {
    if refused.get(subject) != Some(&message) {
        warn!("{subject}: auto sizing: {message}");
        refused.insert(subject.to_owned(), message);
    }
}

/// Resolves what a crash left pending, then writes the changes due now, one profile after the other; `refused`
/// holds the last refusal of each profile and of the file.
async fn look(state: &AppState, refused: &mut BTreeMap<String, String>) {
    // Held throughout, so no config replaced meanwhile is overwritten.
    let mut file = state.config.lock().await;
    let Ok(config) = Config::parse(&file.text) else {
        return;
    };
    if !config.sizing.auto {
        return;
    }
    let path = file.path.display().to_string();
    let read = match read(state, &file, &config).await {
        Ok(read) => read,
        Err(message) => return refuse(refused, &path, message),
    };
    refused.remove(&path);
    let context = Context {
        memory_max: read.memory_max,
        window: read.window,
        last: &read.auto.last,
        running: &read.running,
    };
    for change in changes(&config, &read.history, context) {
        if !write(state, &mut file, change, refused).await {
            return;
        }
    }
}

/// What a look sizes from.
struct Read {
    auto: AutoSizing,
    window: Window,
    history: History,
    memory_max: Option<u64>,
    running: BTreeMap<String, u32>,
}

/// Checks the file on disk is the one runnerd runs with, resolves the pending changes, and reads the history, the
/// slice's `MemoryMax` and what runs; `Err` says why nothing can change now.
async fn read(state: &AppState, file: &ConfigText, config: &Config) -> Result<Read, String> {
    match std::fs::read_to_string(&file.path) {
        Ok(text) if text == file.text => {}
        Ok(_) => {
            return Err(
                "the file changed on disk since runnerd read it; nothing is changed until runnerd runs with it \
                 (restart it, or make the edit in the dashboard's editor)"
                    .to_owned(),
            );
        }
        Err(io_error) => return Err(format!("cannot read it: {io_error}")),
    }
    let history_error =
        |store_error: StoreError| format!("cannot read the job history: {store_error}");
    let auto = state
        .handle
        .store
        .auto_sizing()
        .await
        .map_err(history_error)?;
    if !resolve(state, config, &auto).await {
        // Pending changes hold their profiles' intervals until the history says what became of them.
        return Err("the job history cannot record what became of a change".to_owned());
    }
    let window = Window::new(None, now());
    let history = state
        .handle
        .store
        .sizing(window.since)
        .await
        .map_err(history_error)?;
    let (read, slice) = (state.memory_max, config.host.slice.clone());
    let memory_max = tokio::task::spawn_blocking(move || read(&slice))
        .await
        .map_err(|join_error| join_error.to_string())?
        .map_err(|io_error| format!("cannot read {}'s MemoryMax: {io_error}", config.host.slice))?
        .map(|max| max.0);
    let running = state
        .handle
        .status
        .borrow()
        .profiles
        .iter()
        .map(|profile| {
            let running = profile.slots.iter().filter(|slot| slot.runner.is_some());
            (
                profile.name.clone(),
                u32::try_from(running.count()).unwrap_or(u32::MAX),
            )
        })
        .collect();
    Ok(Read {
        auto,
        window,
        history,
        memory_max,
        running,
    })
}

/// Records `change` as pending, writes it, and records what became of it; whether the history recorded both, so
/// the look goes on.
async fn write(
    state: &AppState,
    file: &mut ConfigText,
    change: SizingChange,
    refused: &mut BTreeMap<String, String>,
) -> bool {
    let profile = change.profile.clone();
    let text = match resize_profile(&file.text, &profile, change.to) {
        Ok(text) => text,
        Err(resize_error) => {
            refuse(refused, &profile, resize_error.to_string());
            return true;
        }
    };
    // Recorded first: a written change the history lacks would have no interval and nothing to undo it with.
    let Some(id) = state.handle.store.sizing_intent(change.clone()).await else {
        refuse(
            refused,
            &profile,
            "the job history cannot record the change, so it is not written".to_owned(),
        );
        return false;
    };
    let on_disk = Some(file.text.clone());
    let written = replace(state, file, text, RunningSlots::Keep, on_disk).await;
    let resolution = match &written {
        Ok(_) => Resolution::Done,
        Err(_) => Resolution::Failed,
    };
    // If this does not commit, the change stays pending and holds the interval; the next look resolves it.
    let recorded = state.handle.store.sizing_resolved(id, resolution).await;
    match written {
        Ok(checked) => {
            refused.remove(&profile);
            info!(
                "{profile}: auto sizing wrote {} into {}: {}",
                change.line(),
                file.path.display(),
                change.reason
            );
            for warning in checked.warnings {
                warn!("{profile}: auto sizing: {warning}");
            }
        }
        Err(refusal) => refuse(
            refused,
            &profile,
            format!("{} refused: {refusal}", change.line()),
        ),
    }
    recorded
}

/// Records what became of the changes left pending: done when `config` has the profile at what it was changed to,
/// failed otherwise. Whether every one was recorded.
async fn resolve(state: &AppState, config: &Config, auto: &AutoSizing) -> bool {
    for (id, change) in &auto.pending {
        let written = config.profiles.get(&change.profile).is_some_and(|profile| {
            (profile.memory.0, profile.slots) == (change.to.memory, change.to.slots)
        });
        let resolution = if written {
            Resolution::Done
        } else {
            Resolution::Failed
        };
        if !state.handle.store.sizing_resolved(*id, resolution).await {
            return false;
        }
        info!(
            "{}: auto sizing's pending change to {} is {}",
            change.profile,
            change.line(),
            if written {
                "in the config"
            } else {
                "not in the config"
            }
        );
    }
    true
}
