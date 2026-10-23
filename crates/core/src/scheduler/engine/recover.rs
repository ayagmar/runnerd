//! Startup recovery: adopts the runner and seed containers a previous daemon left, removes the rest, and reads what
//! the history left open. It changes nothing unless Docker lists both kinds of container, so it can be tried again;
//! it must not delete GitHub registrations: the first poll does.

use std::collections::BTreeSet;
use std::time::SystemTime;

use tracing::{info, warn};

use super::history::Closing;
use super::{Engine, Occupant};
use crate::api;
use crate::cache;
use crate::docker::{self, DockerApi, is_not_found};
use crate::github::GithubApi;
use crate::record;
use crate::scheduler::SlotRef;
use crate::time;

/// The limits an adopted container runs with.
struct Caps {
    /// Bytes.
    memory: u64,
    /// Millicores.
    cpus: u64,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Takes over the seed containers a previous daemon left: a running one counts as its profile's reseed, so no
    /// runner of the profile starts while it rewrites the snapshot and the slots' layers.
    async fn recover_reseeds(&mut self, containers: Vec<docker::Container>) {
        for container in containers {
            let Some(profile) = container.labels.get("runnerd.profile").cloned() else {
                continue;
            };
            // Only this daemon's own names: tests seed under other prefixes on the same Engine.
            if container.name != self.names.seed(&profile) || !self.slots.contains_key(&profile) {
                continue;
            }
            info!("{profile}: waiting for the reseed an earlier run started");
            let docker = self.docker.clone();
            let id = container.id.clone();
            let memory = self.running_caps(&container.id, &profile).await.memory;
            self.reseed_memory.insert(profile.clone(), memory);
            self.reseeds.insert(
                profile,
                tokio::spawn(async move { cache::finish_reseed(&*docker, &id).await }),
            );
        }
    }

    /// The memory and CPU caps a container left by an earlier daemon runs with, which a reload since may have
    /// changed in the config; the profile's configured caps only when Docker cannot tell.
    async fn running_caps(&self, container: &str, profile: &str) -> Caps {
        let state = self.docker.inspect(container).await.ok();
        let configured = self.config.profiles.get(profile);
        Caps {
            memory: state
                .as_ref()
                .and_then(|state| state.memory)
                .or_else(|| configured.map(|settings| settings.memory.0))
                .unwrap_or(0),
            cpus: state
                .and_then(|state| state.cpus)
                .or_else(|| configured.map(|settings| u64::from(settings.cpus) * 1000))
                .unwrap_or(0),
        }
    }

    /// Adopts the runner and seed containers a previous daemon left running, and removes the runners that
    /// stopped, or that run under this machine's names for no free slot. Their registrations, and any of this host's runners without a container, go at the first poll.
    /// Changes nothing unless Docker lists both kinds, so it can be tried again.
    pub(super) async fn recover(&mut self) -> Result<(), docker::Error> {
        let seeds = self.docker.list("runnerd.seed").await?;
        let containers = self.docker.list("runnerd.job").await?;
        self.recover_reseeds(seeds).await;
        // A shell's terminal does not outlive the daemon that served it. Shells carry their record's runner labels.
        let (shells, containers): (Vec<_>, Vec<_>) = containers
            .into_iter()
            .partition(|container| container.labels.contains_key(record::SHELL_LABEL));
        for shell in shells {
            info!("removing {}, a shell an earlier run left", shell.name);
            if let Err(error) = self.docker.remove(&shell.id).await {
                warn!("cannot remove {}: {error}", shell.name);
            }
        }
        self.list_records().await;
        // A reload may have lowered the configured cap since the mirror started.
        match self.docker.inspect(&self.names.mirror()).await {
            Ok(state) if state.running => {
                self.mirror_memory = state.memory.unwrap_or(self.mirror_memory);
            }
            Ok(_) => self.mirror_memory = 0,
            Err(error) if is_not_found(&error) => self.mirror_memory = 0,
            // Unknown: the configured cap.
            Err(_) => {}
        }
        // Runner containers still running once recovery is done.
        let mut running = BTreeSet::new();
        for container in containers {
            // Runner containers on this Engine are this machine's whatever their name, which older builds or
            // another machine ID may have chosen: a running one is adopted when its labels name a free slot.
            let ours = container.name.starts_with(&self.runner_prefix);
            let label = |name: &str| container.labels.get(name).cloned().unwrap_or_default();
            let profile = label("runnerd.profile");
            let index = label("runnerd.slot")
                .parse::<usize>()
                .ok()
                .and_then(|slot| slot.checked_sub(1));
            let caps = self.running_caps(&container.id, &profile).await;
            let occupant = Occupant {
                repository: label("runnerd.repository"),
                runner_name: container.name.clone(),
                runner_id: label("runnerd.runner-id").parse().unwrap_or(0),
                container: container.id.clone(),
                busy: true,
                started_at: SystemTime::now(),
                memory: caps.memory,
                cpus: caps.cpus,
                admission: None,
                // Its capture reports it again; until then, or if it cannot, the history knows it.
                job: self.store.runner_job(&container.name).await.ok().flatten(),
                key: None,
            };
            let known_repository = self.repositories.contains_key(&occupant.repository);
            let free = index.and_then(|index| {
                self.slots
                    .get_mut(&profile)?
                    .get_mut(index)
                    .filter(|slot| slot.is_none())
            });
            match free {
                Some(slot) if container.running && known_repository => {
                    info!(
                        "adopted {} in {profile} slot {}",
                        occupant.runner_name,
                        index.map_or(0, |index| index + 1)
                    );
                    *slot = Some(occupant.clone());
                    running.insert(occupant.runner_name.clone());
                    let slot = SlotRef {
                        profile,
                        index: index.unwrap_or(0),
                    };
                    self.observe(&slot, &occupant).await;
                }
                _ if !ours && container.running => {
                    running.insert(container.name.clone());
                    warn!(
                        "leaving {}: a runner container not named by this machine, for no free slot",
                        container.name
                    );
                }
                _ => {
                    if !container.running {
                        self.note_lost_record(&profile, occupant.job, &container.id)
                            .await;
                    }
                    info!("removing {}, left by an earlier run", container.name);
                    if let Err(error) = self.docker.remove(&container.id).await {
                        warn!("cannot remove {}: {error}", container.name);
                    }
                }
            }
        }
        self.recover_history(&running).await;
        Ok(())
    }

    /// Reads what the history left open: queued jobs to watch, and jobs to close. A job whose runner's container
    /// is still `running` is left to run: `forget` closes it once its container is gone. Any other lost its
    /// container while no daemon watched: its runner is recorded removed now, unless it was before, as is every
    /// other runner the history holds open whose container is not running, with a job or idle.
    async fn recover_history(&mut self, running: &BTreeSet<String>) {
        self.store
            .runners_gone(running.iter().cloned().collect(), time::now_ms());
        match self.store.open_jobs().await {
            Ok(open) => {
                for job in open {
                    if job.state == api::JobState::Queued {
                        self.queued_jobs.insert(job.id, job.repository);
                        continue;
                    }
                    if let Some(runner) = &job.runner {
                        if running.contains(runner) {
                            continue;
                        }
                        self.store.runner_removed(runner, time::now_ms());
                    }
                    self.closing.insert(job.id, Closing::now(&job.repository));
                }
            }
            Err(error) => warn!("cannot read the job history: {error}"),
        }
        match self.store.mirror_log_until().await {
            Ok(until) => self.mirror_until = until,
            Err(error) => warn!("cannot read the job history: {error}"),
        }
    }
}
