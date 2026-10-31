//! Auto sizing: under `[sizing] auto`, every few minutes, writes the changes `sizing::auto::changes` picks into the
//! config through the same check and commit as `PUT /v1/config`, keeping the rest of the file as written. Each change
//! is recorded in the history as pending, synced to disk, before the file is written, then as done (announced as
//! `sizing.changed`) or failed. One left pending by a crash or a failed record is settled at the next look, auto
//! sizing on or off: done when the config has its values, else uncertain, never failed. It never writes a config
//! the check refuses, a file that changed on disk since the daemon read it, or one without a slot a runner is in,
//! and writes nothing while auto sizing is off.
use std::collections::BTreeMap;
use std::time::Duration;

use runnerd_core::api::{AutoPlan, SizingChange, SizingHold};
use runnerd_core::config::{Config, resize_profile};
use runnerd_core::scheduler::engine::{Author, RunningSlots, Shutdown};
use runnerd_core::sizing::auto::{Context, changes, next_look};
use runnerd_core::sizing::memory::CHANGES_READ;
use runnerd_core::sizing::{CpusSet, History, Window};
use runnerd_core::store::{AutoSizing, Resolution, StoreError};
use tracing::{info, warn};

use super::control::replace;
use super::history::now;
use super::{AppState, ConfigText};

/// How often auto sizing looks at the advice in the daemon.
pub(crate) const EVERY: Duration = Duration::from_mins(5);

/// Looks now, then every `every` until the daemon is asked to stop. What a look refuses is logged once until it
/// changes.
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
        // A stopping engine schedules nothing the change would size.
        if state.handle.control.mode() != Shutdown::No {
            return;
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
    let path = file.path.display().to_string();
    // Settled whether auto sizing is on or not: a change written before it was turned off still shows, and holds.
    let auto = match state.handle.store.auto_sizing().await {
        Ok(auto) => auto,
        Err(store_error) => {
            return refuse(
                refused,
                &path,
                format!("cannot read the job history: {store_error}"),
            );
        }
    };
    if !resolve(state, &config, &auto).await {
        // Pending changes hold their profiles' intervals until the history says what became of them.
        return refuse(
            refused,
            &path,
            "the job history cannot record what became of a change".to_owned(),
        );
    }
    if !config.sizing.auto {
        return;
    }
    let read = match read(state, &file, &config).await {
        Ok(read) => read,
        Err(message) => return refuse(refused, &path, message),
    };
    refused.remove(&path);
    let context = Context {
        memory_max: read.memory_max,
        cpus: read.cpus,
        window: read.window,
        last: &read.last,
        cpus_set: &read.cpus_set,
        running: &read.running,
        disk: read.disk,
    };
    for change in changes(&config, &read.history, context) {
        if !write(state, &mut file, change, refused).await {
            return;
        }
    }
}

/// What a look sizes from.
struct Read {
    window: Window,
    history: History,
    memory_max: Option<u64>,
    cpus: Option<u64>,
    /// When each profile last changed, read after `history`.
    last: BTreeMap<String, u64>,
    /// Each profile's `cpus` as the daemon ran them, read with `last`.
    cpus_set: Vec<CpusSet>,
    running: BTreeMap<String, u32>,
    /// What the slots' Docker Engines may keep together, from the engine's last measure.
    disk: Option<u64>,
}

/// Checks the file on disk is the one runnerd runs with, and reads the history, the slice's `MemoryMax` and CPUs and
/// what runs; `Err` says why nothing can change now.
async fn read(state: &AppState, file: &ConfigText, config: &Config) -> Result<Read, String> {
    on_disk(file)?;
    let history_error =
        |store_error: StoreError| format!("cannot read the job history: {store_error}");
    let window = Window::new(None, now());
    let mut history = state
        .handle
        .store
        .sizing(window.since)
        .await
        .map_err(history_error)?;
    let auto = last_changes(state).await.map_err(history_error)?;
    recorded(state, &mut history, &auto, window)
        .await
        .map_err(history_error)?;
    let AutoSizing {
        last,
        cpus: cpus_set,
        ..
    } = auto;
    let (read, slice) = (state.memory_max, config.host.slice.clone());
    let memory_max = tokio::task::spawn_blocking(move || read(&slice))
        .await
        .map_err(|join_error| join_error.to_string())?
        .map_err(|io_error| format!("cannot read {}'s MemoryMax: {io_error}", config.host.slice))?
        .map(|max| max.0);
    Ok(Read {
        window,
        history,
        memory_max,
        cpus: slice_cpus(state, config).await,
        last,
        cpus_set,
        running: running(state),
        disk: *state.handle.disk.borrow(),
    })
}

/// When auto sizing last changed each profile, pending changes included, and each profile's `cpus` and `memory` as
/// the daemon ran them. Read after the history it sizes from, never before: a change recorded between the two reads would otherwise
/// be missing from the holds while the runs after it, which the history has, ask for another, and a profile would be
/// changed twice in one interval.
pub(super) async fn last_changes(state: &AppState) -> Result<AutoSizing, StoreError> {
    // The `profile.changed` events a reload or restart published are written in order with every other write; reading
    // after they land keeps a quick edit away and back from looking like no edit at all.
    state.handle.store.flush().await;
    state.handle.store.auto_sizing().await
}

/// Adds to `history` what a revert reads (ADR 0070): each profile's `memory` as the daemon ran it, from `auto`, and
/// auto sizing's changes since `CHANGES_READ` before `window`'s end, read after the history as `auto` is; and each
/// profile's slots as the daemon ran them, which a lowering of slots reads (ADR 0090).
pub(super) async fn recorded(
    state: &AppState,
    history: &mut History,
    auto: &AutoSizing,
    window: Window,
) -> Result<(), StoreError> {
    history.memory_set.clone_from(&auto.memory);
    history.slots_set.clone_from(&auto.slots);
    history.changes = state
        .handle
        .store
        .sizing_changes(window.now.saturating_sub(CHANGES_READ))
        .await?;
    Ok(())
}

/// The slice's CPUs, in millicores; `None` when they cannot be read, which only keeps auto sizing from raising
/// `cpus`.
async fn slice_cpus(state: &AppState, config: &Config) -> Option<u64> {
    let (read, slice) = (state.cpus, config.host.slice.clone());
    tokio::task::spawn_blocking(move || read(&slice))
        .await
        .ok()
        .and_then(Result::ok)
}

/// Whether the file on disk is the one runnerd runs with; `Err` says why a look writes nothing when it is not.
fn on_disk(file: &ConfigText) -> Result<(), String> {
    match std::fs::read_to_string(&file.path) {
        Ok(text) if text == file.text => Ok(()),
        Ok(_) => {
            Err(
                "the file changed on disk since runnerd read it; nothing is changed until runnerd runs with it \
                 (restart it, or make the edit in the dashboard's editor)"
                    .to_owned(),
            )
        }
        Err(io_error) => Err(format!("cannot read the file: {io_error}")),
    }
}

/// How many runners each profile runs now, by name.
fn running(state: &AppState) -> BTreeMap<String, u32> {
    state
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
        .collect()
}

/// What auto sizing's next look would change as the history and config are now, which `cpus` raises it cannot make,
/// and which profiles a recent change holds, for `GET /v1/sizing`, and what blocks every look, by the same check a
/// look makes; `None` while it is off.
/// It settles nothing and writes nothing.
pub(super) async fn plan(
    state: &AppState,
    config: &Config,
    memory_max: Option<u64>,
) -> Result<Option<AutoPlan>, String> {
    if !config.sizing.auto {
        return Ok(None);
    }
    let history_error =
        |store_error: StoreError| format!("cannot read the job history: {store_error}");
    let window = Window::new(None, now());
    let mut history = state
        .handle
        .store
        .sizing(window.since)
        .await
        .map_err(history_error)?;
    let auto = last_changes(state).await.map_err(history_error)?;
    recorded(state, &mut history, &auto, window)
        .await
        .map_err(history_error)?;
    let AutoSizing {
        last,
        cpus: cpus_set,
        ..
    } = auto;
    let running = running(state);
    let blocked = on_disk(&*state.config.lock().await).err();
    let disk = *state.handle.disk.borrow();
    let context = Context {
        memory_max,
        cpus: slice_cpus(state, config).await,
        window,
        last: &last,
        cpus_set: &cpus_set,
        running: &running,
        disk,
    };
    let interval = config.sizing.interval.as_secs();
    let raise_interval = config.sizing.raise_after().as_secs();
    let at = window.now / 1000;
    let mut held: Vec<SizingHold> = last
        .iter()
        .filter(|(profile, _)| config.profiles.contains_key(*profile))
        .map(|(profile, changed)| SizingHold {
            profile: profile.clone(),
            changed: *changed,
            until: changed.saturating_add(raise_interval),
            lower_until: changed.saturating_add(interval),
        })
        .filter(|hold| hold.lower_until > at)
        .collect();
    held.sort_by_key(|hold| hold.until);
    let look = next_look(config, &history, context);
    Ok(Some(AutoPlan {
        every: EVERY.as_secs(),
        interval,
        raise_interval,
        max_step: config.sizing.max_step,
        next: look.changes,
        held,
        blocked,
        refused: look.refused,
        refused_slots: look.refused_slots,
        waited_before_change: look.waited_before_change,
    }))
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
    let written = replace(
        state,
        file,
        text,
        RunningSlots::Keep,
        Author::AutoSizing,
        on_disk,
    )
    .await;
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
/// uncertain otherwise, keeping its interval and what it replaced. Whether every one was recorded.
async fn resolve(state: &AppState, config: &Config, auto: &AutoSizing) -> bool {
    for (id, change) in &auto.pending {
        let written = config.profiles.get(&change.profile).is_some_and(|profile| {
            (profile.memory.0, profile.slots) == (change.to.memory, change.to.slots)
                && change.to.cpus.is_none_or(|cpus| profile.cpus == cpus)
        });
        // Values that differ do not say it was not written: a person may have changed them since.
        let resolution = if written {
            Resolution::Done
        } else {
            Resolution::Uncertain
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
                "uncertain: the config has other values now"
            }
        );
    }
    true
}
