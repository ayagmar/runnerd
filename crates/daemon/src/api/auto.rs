//! Auto sizing: under `[sizing] auto`, every few minutes, writes the changes `sizing::auto::changes` picks into the
//! config through the same check and commit as `PUT /v1/config`, keeping the rest of the file as written, and
//! records each as a `sizing.changed` event, which is also what the interval between changes counts from. It never
//! writes a config the check refuses, and does nothing while auto sizing is off.

use std::collections::BTreeMap;
use std::time::Duration;

use runnerd_core::api::Event;
use runnerd_core::config::{Config, resize_profile};
use runnerd_core::sizing::Window;
use runnerd_core::sizing::auto::{Context, changes};
use tracing::{info, warn};

use super::AppState;
use super::control::replace;
use super::history::now;

/// How often auto sizing looks at the advice in the daemon.
pub(crate) const EVERY: Duration = Duration::from_mins(5);

/// Looks every `every` until the server stops. A refusal is logged once until it changes.
pub(super) async fn run(state: AppState, every: Duration) {
    let mut stopping = state.stopping.clone();
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick is now: wait one period, so a daemon starting does not change its config at once.
    ticks.tick().await;
    let mut refused = BTreeMap::new();
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = stopping.wait_for(|stopping| *stopping) => return,
        }
        resize(&state, &mut refused).await;
    }
}

/// Writes the changes due now, one profile after the other; `refused` holds the last refusal of each profile.
async fn resize(state: &AppState, refused: &mut BTreeMap<String, String>) {
    // Held throughout, so no config replaced meanwhile is overwritten.
    let mut file = state.config.lock().await;
    let Ok(config) = Config::parse(&file.text) else {
        return;
    };
    if !config.sizing.auto {
        return;
    }
    let window = Window::new(None, now());
    let read = async {
        let history = state.handle.store.sizing(window.since).await?;
        let earlier = state.handle.store.sizing_changes(window.since).await?;
        Ok::<_, runnerd_core::store::StoreError>((history, earlier))
    };
    let (history, earlier) = match read.await {
        Ok(read) => read,
        Err(store_error) => {
            warn!("auto sizing cannot read the job history: {store_error}");
            return;
        }
    };
    let (read, slice) = (state.memory_max, config.host.slice.clone());
    let memory_max = match tokio::task::spawn_blocking(move || read(&slice)).await {
        Ok(Ok(memory_max)) => memory_max.map(|max| max.0),
        Ok(Err(io_error)) => {
            warn!(
                "auto sizing cannot read {}'s MemoryMax: {io_error}",
                config.host.slice
            );
            return;
        }
        Err(_) => return,
    };
    let running: BTreeMap<String, u32> = state
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
    let context = Context {
        memory_max,
        window,
        earlier: &earlier,
        running: &running,
    };
    for change in changes(&config, &history, context) {
        let profile = change.profile.clone();
        let written = match resize_profile(&file.text, &profile, change.to) {
            Ok(text) => replace(state, &mut file, text)
                .await
                .map_err(|refusal| refusal.to_string()),
            Err(resize_error) => Err(resize_error.to_string()),
        };
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
                state.handle.store.event(Event::SizingChanged { change });
                // The next look reads it, to wait the interval.
                state.handle.store.flush().await;
            }
            Err(message) => {
                if refused.get(&profile) != Some(&message) {
                    warn!(
                        "{profile}: auto sizing to {} refused: {message}",
                        change.line()
                    );
                    refused.insert(profile, message);
                }
            }
        }
    }
}
