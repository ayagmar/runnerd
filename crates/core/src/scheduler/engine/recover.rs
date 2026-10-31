//! Startup recovery: adopts the runner and seed containers a previous daemon left, removes the rest, and reads what
//! the history left open. It changes nothing unless Docker lists both kinds of container and tells the caps of the
//! runners of removed profiles, so it can be tried again; it must not delete GitHub registrations: the first poll
//! does.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::history::Closing;
use super::{Engine, Occupant};
use crate::api;
use crate::cache;
use crate::docker::{self, DockerApi, is_not_found};
use crate::github::GithubApi;
use crate::record;
use crate::scheduler::warm::Attempt;
use crate::scheduler::{Pool, SlotRef};
use crate::time;

/// The limits an adopted container runs with, and when it started.
struct Caps {
    /// Bytes.
    memory: u64,
    /// Millicores.
    cpus: u64,
    /// When Docker started it; `None` when Docker cannot tell.
    started_at: Option<SystemTime>,
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
    /// changed in the config; the profile's configured caps only when Docker cannot tell. Also when it started.
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
                .as_ref()
                .and_then(|state| state.cpus)
                .or_else(|| configured.map(|settings| u64::from(settings.cpus) * 1000))
                .unwrap_or(0),
            started_at: state
                .and_then(|state| state.started_at)
                .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
        }
    }

    /// The caps of the running runner containers of profiles the config no longer has, which only Docker can tell,
    /// by container ID. An error unless Docker answered for each; one that it says nothing of is left out.
    async fn retired_caps(
        &self,
        containers: &[docker::Container],
    ) -> Result<BTreeMap<String, Caps>, docker::Error> {
        let mut caps = BTreeMap::new();
        for container in containers {
            let profile = container.labels.get("runnerd.profile");
            if !container.running
                || container.labels.contains_key(record::SHELL_LABEL)
                || profile.is_none_or(|profile| {
                    profile.is_empty() || self.config.profiles.contains_key(profile)
                })
            {
                continue;
            }
            let state = match self.docker.inspect(&container.id).await {
                Ok(state) => state,
                Err(error) if is_not_found(&error) => continue,
                Err(error) => return Err(error),
            };
            if let (Some(memory), Some(cpus)) = (state.memory, state.cpus) {
                caps.insert(
                    container.id.clone(),
                    Caps {
                        memory,
                        cpus,
                        started_at: state
                            .started_at
                            .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
                    },
                );
            }
        }
        Ok(caps)
    }

    /// Removes the shell containers an earlier run left; each tick tries again those Docker failed to remove.
    async fn remove_shells(&mut self, shells: Vec<docker::Container>) {
        for shell in shells {
            info!("removing {}, a shell an earlier run left", shell.name);
            if let Err(error) = self.docker.remove(&shell.id).await {
                warn!("cannot remove {}: {error}", shell.name);
                self.leftover.insert(shell.id, shell.name);
            }
        }
    }

    /// What a runner container left running by an earlier daemon runs as, from its labels, `caps` and its record.
    async fn adopted(&self, container: &docker::Container, caps: Caps) -> Occupant {
        let label = |name: &str| container.labels.get(name).cloned().unwrap_or_default();
        Occupant {
            repository: label("runnerd.repository"),
            runner_name: container.name.clone(),
            runner_id: label("runnerd.runner-id").parse().unwrap_or(0),
            container: container.id.clone(),
            busy: true,
            // Its uptime goes on from its container's start, not from this restart.
            started_at: caps.started_at.unwrap_or_else(SystemTime::now),
            memory: caps.memory,
            cpus: caps.cpus,
            // What it reserved before the restart, as its record kept it; its cap if the record has none.
            admission: self
                .store
                .runner_admitted(&container.name)
                .await
                .ok()
                .flatten(),
            // Its capture reports it again; until then, or if it cannot, the history knows it.
            job: self.store.runner_job(&container.name).await.ok().flatten(),
            key: None,
            labels: container
                .labels
                .get(super::runners::LABELS)
                .map(|labels| labels.split(',').map(str::to_owned).collect()),
            image: container.labels.get(super::runners::IMAGE).cloned(),
            spec: container.labels.get(super::runners::SPEC).cloned(),
            observed: false,
        }
    }

    /// Makes room for a running runner the config no longer has room for, as a reload leaves it: its slot past the
    /// profile's count, its profile retired with the limits it runs with, its repository draining.
    fn keep_draining(&mut self, profile: &str, index: usize, occupant: &Occupant) {
        let slots = self.slots.entry(profile.to_owned()).or_default();
        if slots.len() <= index {
            slots.resize(index + 1, None);
        }
        if !self.config.profiles.contains_key(profile) {
            self.retired
                .entry(profile.to_owned())
                .or_insert_with(|| crate::config::Profile {
                    image: crate::config::Image::Pulled(occupant.image.clone().unwrap_or_default()),
                    cpus: u32::try_from(occupant.cpus / 1000).unwrap_or(u32::MAX),
                    memory: crate::config::ByteSize(occupant.memory),
                    slots: 0,
                    record_failures: false,
                    min_memory: None,
                    max_memory: None,
                    max_slots: None,
                    priced_as: None,
                });
        }
        if !self.repositories.contains_key(&occupant.repository) {
            self.draining.insert(occupant.repository.clone());
        }
    }

    /// Adopts the runner and seed containers a previous daemon left running, also those the config no longer has
    /// room for, which drain as after a reload, and removes the runners that stopped, or that run under this
    /// machine's names without a slot to adopt them in. Their registrations, and any of this host's runners without a
    /// container, go at the first poll.
    /// Changes nothing unless Docker lists both kinds and tells the caps of the runners of removed profiles, so it can
    /// be tried again.
    pub(super) async fn recover(&mut self) -> Result<(), docker::Error> {
        let seeds = self.docker.list("runnerd.seed").await?;
        let containers = self.docker.list("runnerd.job").await?;
        // Before anything changes: a retired profile's runner counts at zero if its caps are not known.
        let mut retired_caps = self.retired_caps(&containers).await?;
        self.recover_reseeds(seeds).await;
        // A shell's terminal does not outlive the daemon that served it. Shells carry their record's runner labels.
        let (shells, containers): (Vec<_>, Vec<_>) = containers
            .into_iter()
            .partition(|container| container.labels.contains_key(record::SHELL_LABEL));
        self.remove_shells(shells).await;
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
            let retired = retired_caps.remove(&container.id);
            let known_caps = retired.is_some() || self.config.profiles.contains_key(&profile);
            let caps = match retired {
                Some(caps) => caps,
                None => self.running_caps(&container.id, &profile).await,
            };
            let occupant = self.adopted(&container, caps).await;
            // A runner the config no longer has room for drains as it would have without the restart: in a slot
            // past a lowered count, of a removed profile or for a removed repository. One of a removed profile
            // whose caps Docker does not tell is removed, as it would count at zero.
            if ours
                && container.running
                && known_caps
                && !profile.is_empty()
                && !occupant.repository.is_empty()
                && let Some(index) = index
            {
                self.keep_draining(&profile, index, &occupant);
            }
            let known_repository = self.repositories.contains_key(&occupant.repository)
                || self.draining.contains(&occupant.repository);
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
                    // Its socket back in the directory its container mounts, if it does; its job's writes are
                    // decided again.
                    drop(self.attach_build_cache(
                        &occupant.runner_name,
                        &occupant.repository,
                        &profile,
                    ));
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
                        self.leftover.insert(container.id, container.name);
                    }
                }
            }
        }
        if let Some(cache) = &self.build_cache {
            let keep: Vec<&str> = running.iter().map(String::as_str).collect();
            cache.forget_others(&keep);
        }
        self.drop_closed_slots();
        self.recover_history(&running).await;
        self.measure_docker_disk();
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
                    // Its run is as old as the job, for the warm runners its remaining jobs get, not as new as
                    // this start.
                    let runs = job.state == api::JobState::Queued
                        || job
                            .runner
                            .as_ref()
                            .is_some_and(|runner| running.contains(runner));
                    let pool = self
                        .config
                        .pool(&job.repository, &job.labels)
                        .map(Pool::from);
                    if runs && let Some(pool) = pool {
                        self.note_job(
                            &pool,
                            Attempt {
                                run_id: job.run_id,
                                run_attempt: job.run_attempt,
                            },
                            job.id,
                            job.workflow.as_deref(),
                            job.queued_at,
                        );
                    }
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
        // The slots' layers outlive the daemon, and so does what ran last in each.
        match self.store.slot_jobs().await {
            Ok(jobs) => {
                for job in jobs {
                    if let Some(index) = job.slot.checked_sub(1) {
                        self.ran
                            .entry(SlotRef {
                                profile: job.profile,
                                index,
                            })
                            .or_insert(job.key);
                    }
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
