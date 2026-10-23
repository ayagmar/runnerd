//! Control from the API: the `Control` handle the daemon's API holds (pause flags, which it records in the store so
//! a restart keeps them, shutdown, a channel of `Command`s) and the engine's side of each command: pause, of one
//! repository or all, with or without cancelling their runs, reseed, cache clean and config replacement. Commands
//! run between ticks, so they see the slot state a tick does (ADR 0019). It must not serve HTTP or write the config
//! file: the daemon does, through `Commit`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{info, warn};

use super::{Engine, Occupant};
use crate::api;
use crate::cache;
use crate::config::Config;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::isolation;
use crate::scheduler::SlotRef;
use crate::store::Store;
use crate::sync::lock;

/// How long the runner of a run a forced pause cancelled gets to report its job cancelled and exit, before it is
/// removed: GitHub completes a cancelled run only once its runner reports it, which took 40s in a test.
pub(super) const CANCEL_GRACE: Duration = Duration::from_secs(90);

/// How the daemon is asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    /// Keep scheduling.
    No,
    /// Start nothing new, let running jobs finish.
    Graceful,
    /// Kill running jobs and delete their runners.
    Force,
}

/// What a pause, a shutdown or a runner's removal does with the jobs runners are running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningJobs {
    /// Leave them running: a pause refuses, a shutdown waits for them, a busy runner stays.
    Keep,
    /// Kill them: a pause cancels their runs, a shutdown and a removal kill their runners.
    Kill,
}

/// The repositories a pause or a resume applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One repository, `owner/name`.
    Repository(String),
    /// Every configured repository.
    All,
}

/// Why the engine refused or failed a command from the API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// No such repository, profile or cache.
    #[error("{0}")]
    NotFound(String),
    /// Runners use what the command would change.
    #[error("{message}")]
    Busy {
        /// Names the runners and their jobs.
        message: String,
        /// The runners in the way.
        jobs: Vec<api::RunningJob>,
    },
    /// The command conflicts with one already in progress.
    #[error("{0}")]
    Conflict(String),
    /// The command cannot apply to this target.
    #[error("{0}")]
    Invalid(String),
    /// Docker or GitHub failed.
    #[error("{0}")]
    Failed(String),
    /// The engine stopped.
    #[error("runnerd is stopping")]
    Stopped,
}

/// A command the engine carries out between ticks, with where its answer goes.
#[derive(Debug)]
pub enum Command {
    /// Pause one repository or all; with `RunningJobs::Kill`, cancel their running jobs.
    Pause {
        /// What to pause.
        target: Target,
        /// Cancel running jobs, or refuse while there are any.
        jobs: RunningJobs,
        /// The answer.
        reply: oneshot::Sender<Result<api::Paused, ControlError>>,
    },
    /// Make slot 1's layers the profile's snapshot once its slots are free.
    Reseed {
        /// The profile.
        profile: String,
        /// The answer.
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    /// Remove a slot's volume or a shared cache's, unless a runner uses it.
    CleanCache {
        /// The volume.
        name: String,
        /// The answer.
        reply: oneshot::Sender<Result<(), ControlError>>,
    },
    /// Write a new, validated config to its file and run with it, with no tick in between.
    ReplaceConfig {
        /// The config.
        config: Box<Config>,
        /// The `MemoryMax` of the slice it names.
        memory_max: Option<u64>,
        /// Writes the file.
        commit: Commit,
        /// Answered once the engine runs with it: with a warning from the commit, or why it did not commit.
        reply: oneshot::Sender<Result<Option<String>, ControlError>>,
    },
}

/// What makes a new config durable, run by the engine before it runs with the config: `Ok` once the config is in
/// place, with a warning if it may not survive a crash; `Err` with nothing changed.
pub struct Commit(Box<dyn FnOnce() -> Result<Option<String>, String> + Send>);

impl Commit {
    /// A commit that runs `write`, off the engine's thread.
    pub fn new(write: impl FnOnce() -> Result<Option<String>, String> + Send + 'static) -> Commit {
        Commit(Box::new(write))
    }

    /// Runs it.
    pub async fn run(self) -> Result<Option<String>, String> {
        tokio::task::spawn_blocking(self.0)
            .await
            .unwrap_or_else(|join_error| Err(join_error.to_string()))
    }
}

impl std::fmt::Debug for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Commit")
    }
}

/// The engine's end of `Control`'s commands.
#[derive(Debug)]
pub struct Commands(mpsc::UnboundedReceiver<Command>);

impl Commands {
    /// The next command; `None` once `Control` is gone.
    pub async fn recv(&mut self) -> Option<Command> {
        self.0.recv().await
    }
}

/// Commands from the API and signals to the loop.
#[derive(Debug)]
pub struct Control {
    // Locked with `sync::lock`: the guarded data stays consistent, as every critical section is a single insert,
    // remove or read.
    repositories: Mutex<BTreeSet<String>>,
    paused: Mutex<BTreeSet<String>>,
    /// Where `paused` is recorded.
    store: Store,
    shutdown: Mutex<Shutdown>,
    pub(super) wake: Notify,
    commands: mpsc::UnboundedSender<Command>,
}

impl Control {
    /// Controls for the configured repositories, of which those in `paused` start paused, and the commands they
    /// send the engine. A paused repository the config no longer has is forgotten, in `store` too: one added back
    /// starts active.
    pub fn new(
        repositories: impl IntoIterator<Item = String>,
        paused: impl IntoIterator<Item = String>,
        store: Store,
    ) -> (Control, Commands) {
        let repositories: BTreeSet<String> = repositories.into_iter().collect();
        let paused: BTreeSet<String> = paused
            .into_iter()
            .filter(|repository| repositories.contains(repository))
            .collect();
        // Sent at once; a write that fails leaves rows the next start filters out the same way.
        drop(store.paused(paused.iter().cloned().collect()));
        let (commands, receiver) = mpsc::unbounded_channel();
        let control = Control {
            repositories: Mutex::new(repositories),
            paused: Mutex::new(paused),
            store,
            shutdown: Mutex::new(Shutdown::No),
            wake: Notify::new(),
            commands,
        };
        (control, Commands(receiver))
    }

    /// Whether a shutdown was requested, and how.
    pub fn mode(&self) -> Shutdown {
        *lock(&self.shutdown)
    }

    /// Pauses or resumes one repository or all, whatever they run, and records it in the store before it answers,
    /// so a restarted daemon starts with the same ones paused. `NotFound` for a repository the config does not have.
    /// A failed record is logged: the change holds until a restart.
    pub async fn set_paused(&self, target: &Target, paused: bool) -> Result<(), ControlError> {
        let recorded = {
            let repositories = lock(&self.repositories);
            let names: Vec<&String> = match target {
                Target::Repository(repository) => {
                    let Some(name) = repositories.get(repository) else {
                        return Err(not_configured(repository));
                    };
                    vec![name]
                }
                Target::All => repositories.iter().collect(),
            };
            let mut set = lock(&self.paused);
            for name in names {
                if paused {
                    set.insert(name.clone());
                } else {
                    set.remove(name);
                }
            }
            // Sent under the lock, so the records change in the order the set does.
            self.store.paused(set.iter().cloned().collect())
        };
        self.wake.notify_one();
        if !recorded.await {
            warn!(
                "cannot record the paused repositories: a restart starts with the ones recorded before"
            );
        }
        Ok(())
    }

    /// Replaces the configured repositories; the removed ones are no longer paused, in the store too.
    fn set_repositories(&self, repositories: BTreeSet<String>) {
        let mut configured = lock(&self.repositories);
        let mut paused = lock(&self.paused);
        let before = paused.len();
        paused.retain(|repository| repositories.contains(repository));
        if paused.len() != before {
            // Not waited for: a restart forgets a repository the config does not have anyway.
            drop(self.store.paused(paused.iter().cloned().collect()));
        }
        *configured = repositories;
    }

    /// Asks the loop to stop; a forced request overrides a graceful one, never the other way.
    pub fn shutdown(&self, jobs: RunningJobs) {
        let mode = match jobs {
            RunningJobs::Keep => Shutdown::Graceful,
            RunningJobs::Kill => Shutdown::Force,
        };
        let mut shutdown = lock(&self.shutdown);
        *shutdown = (*shutdown).max(mode);
        drop(shutdown);
        self.wake.notify_one();
    }

    /// Whether the repository is paused.
    pub fn is_paused(&self, repository: &str) -> bool {
        lock(&self.paused).contains(repository)
    }

    async fn send<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, ControlError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| ControlError::Stopped)?;
        answer.await.map_err(|_| ControlError::Stopped)
    }

    /// Pauses one repository or all. With `RunningJobs::Keep`, refuses while any of them runs a job, pausing none;
    /// with `Kill`, cancels the runs of their running jobs on GitHub, whose runners are removed once they report it
    /// or `CANCEL_GRACE` later, and removes their other runners at once.
    pub async fn pause(
        &self,
        target: Target,
        jobs: RunningJobs,
    ) -> Result<api::Paused, ControlError> {
        self.send(|reply| Command::Pause {
            target,
            jobs,
            reply,
        })
        .await?
    }

    /// Reseeds a profile as soon as its slots are free; none of them starts a runner until then.
    pub async fn reseed(&self, profile: &str) -> Result<(), ControlError> {
        self.send(|reply| Command::Reseed {
            profile: profile.to_owned(),
            reply,
        })
        .await?
    }

    /// Removes a slot's volume or a shared cache's, unless a runner uses it.
    pub async fn clean_cache(&self, name: &str) -> Result<(), ControlError> {
        self.send(|reply| Command::CleanCache {
            name: name.to_owned(),
            reply,
        })
        .await?
    }

    /// Runs `commit` to write a validated config, and if it commits, runs with the config from then on, before the
    /// next tick. If it does not, nothing changes and the answer is `Failed`. Running jobs keep running: removed
    /// profiles and slots go as their runners finish.
    pub async fn replace_config(
        &self,
        config: Config,
        memory_max: Option<u64>,
        commit: Commit,
    ) -> Result<Option<String>, ControlError> {
        self.send(|reply| Command::ReplaceConfig {
            config: Box::new(config),
            memory_max,
            commit,
            reply,
        })
        .await?
    }
}

fn not_configured(repository: &str) -> ControlError {
    ControlError::NotFound(format!("{repository} is not a configured repository"))
}

/// `jobs` for people to read: `job 1 on runner-a, runner-b`.
fn list(jobs: &[api::RunningJob]) -> String {
    jobs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl<G: GithubApi, D: DockerApi + 'static> Engine<G, D> {
    /// Carries out a command from the API.
    pub(super) async fn command(&mut self, command: Command) {
        match command {
            Command::Pause {
                target,
                jobs,
                reply,
            } => {
                let _ = reply.send(self.pause(&target, jobs).await);
            }
            Command::Reseed { profile, reply } => {
                let _ = reply.send(self.request_reseed(&profile));
            }
            Command::CleanCache { name, reply } => {
                let _ = reply.send(self.clean_cache(&name).await);
            }
            Command::ReplaceConfig {
                config,
                memory_max,
                commit,
                reply,
            } => {
                let answer = match commit.run().await {
                    Ok(warning) => {
                        self.reload(*config, memory_max);
                        Ok(warning)
                    }
                    Err(message) => Err(ControlError::Failed(message)),
                };
                let _ = reply.send(answer);
            }
        }
    }

    /// Pauses one repository or all. A runner of theirs that holds a job, or that GitHub reports busy, refuses it
    /// with `RunningJobs::Keep`; with `Kill`, the runs of their jobs are cancelled, and their runners removed once
    /// they exit or `CANCEL_GRACE` later; a runner whose run cannot be cancelled is removed at once.
    pub(super) async fn pause(
        &mut self,
        target: &Target,
        running: RunningJobs,
    ) -> Result<api::Paused, ControlError> {
        let repositories: BTreeSet<String> = match target {
            Target::Repository(repository) if !self.repositories.contains_key(repository) => {
                return Err(not_configured(repository));
            }
            Target::Repository(repository) => BTreeSet::from([repository.clone()]),
            Target::All => self.repositories.keys().cloned().collect(),
        };
        self.take_found();
        let runners: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .filter(|(_, occupant)| repositories.contains(&occupant.repository))
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        let jobs: Vec<api::RunningJob> = runners
            .iter()
            .filter(|(_, occupant)| occupant.busy || occupant.job.is_some())
            .map(|(_, occupant)| occupant.running_job())
            .collect();
        if running == RunningJobs::Keep && !jobs.is_empty() {
            let mut by_repository: BTreeMap<&str, Vec<api::RunningJob>> = BTreeMap::new();
            for job in &jobs {
                by_repository
                    .entry(&job.repository)
                    .or_default()
                    .push(job.clone());
            }
            let runs: Vec<String> = by_repository
                .iter()
                .map(|(repository, jobs)| format!("{repository} runs {}", list(jobs)))
                .collect();
            return Err(ControlError::Busy {
                message: format!(
                    "{}; pause with force to cancel {}",
                    runs.join("; "),
                    if jobs.len() == 1 { "it" } else { "them" }
                ),
                jobs,
            });
        }
        self.control.set_paused(target, true).await?;
        match target {
            Target::Repository(repository) => info!("{repository}: paused"),
            Target::All => info!("paused every repository"),
        }
        let mut stopped = Vec::new();
        if running == RunningJobs::Keep {
            return Ok(api::Paused { stopped });
        }
        for (slot, occupant) in runners {
            let repository = &occupant.repository;
            let cancelled = if let Some(job) = occupant.job {
                self.cancel(repository, job).await
            } else {
                if occupant.busy {
                    warn!(
                        "{repository}: {} runs a job its log has not named yet; GitHub fails it once the runner is gone",
                        occupant.runner_name
                    );
                }
                false
            };
            if cancelled {
                // `collect_finished` removes it once it exits, `kill_cancelled` if it does not.
                self.cancelling.insert(
                    occupant.runner_name.clone(),
                    tokio::time::Instant::now() + CANCEL_GRACE,
                );
            } else {
                self.stop(&slot, RunningJobs::Kill).await;
                if self
                    .occupied()
                    .any(|(_, candidate)| candidate.runner_name == occupant.runner_name)
                {
                    continue;
                }
            }
            stopped.push(occupant.running_job());
        }
        Ok(api::Paused { stopped })
    }

    /// Removes the runners of cancelled runs that did not exit within `CANCEL_GRACE`, every tick until they are gone.
    pub(super) async fn kill_cancelled(&mut self) {
        let now = tokio::time::Instant::now();
        let running: BTreeSet<String> = self
            .occupied()
            .map(|(_, occupant)| occupant.runner_name.clone())
            .collect();
        self.cancelling.retain(|name, _| running.contains(name));
        let overdue: Vec<(SlotRef, String)> = self
            .occupied()
            .filter(|(_, occupant)| {
                self.cancelling
                    .get(&occupant.runner_name)
                    .is_some_and(|deadline| *deadline <= now)
            })
            .map(|(slot, occupant)| (slot, occupant.runner_name.clone()))
            .collect();
        for (slot, name) in overdue {
            warn!(
                "{name} did not exit {}s after its run was cancelled",
                CANCEL_GRACE.as_secs()
            );
            // The deadline stays until the runner is gone, so a failed removal is tried again next tick.
            self.stop(&slot, RunningJobs::Kill).await;
        }
    }

    /// Cancels the run of a job; whether GitHub accepted it.
    async fn cancel(&self, repository: &str, job: u64) -> bool {
        let run = match self.github.job(repository, job).await {
            Ok(details) => details.run_id,
            Err(error) => {
                warn!("{repository}: cannot read job {job} to cancel its run: {error}");
                return false;
            }
        };
        match self.github.cancel_run(repository, run).await {
            Ok(()) => {
                info!("{repository}: cancelled run {run} of job {job}");
                true
            }
            Err(error) => {
                warn!("{repository}: cannot cancel run {run}: {error}");
                false
            }
        }
    }

    /// Asks for a profile's reseed: none of its slots starts a runner until they are free and it ran.
    pub(super) fn request_reseed(&mut self, profile: &str) -> Result<(), ControlError> {
        if !self.config.profiles.contains_key(profile) {
            return Err(ControlError::NotFound(format!(
                "{profile} is not a configured profile"
            )));
        }
        if self.is_reseeding(profile) {
            return Err(ControlError::Conflict(format!(
                "{profile} is reseeding already"
            )));
        }
        info!("{profile}: reseeding once its slots are free");
        self.reseed_requested.insert(profile.to_owned());
        Ok(())
    }

    /// Removes a slot's volume or a shared cache's, unless a runner or a reseed uses it. Snapshots are rebuilt by
    /// reseeding instead: slot layers sit on top of them.
    pub(super) async fn clean_cache(&mut self, name: &str) -> Result<(), ControlError> {
        let Some(volume) = cache::volumes(&self.names, &self.config)
            .into_iter()
            .find(|volume| volume.name == name)
        else {
            return Err(ControlError::NotFound(format!(
                "{name} is not a cache volume of this config"
            )));
        };
        let users: Vec<api::RunningJob> = match &volume.kind {
            api::CacheKind::Snapshot { profile } => {
                return Err(ControlError::Invalid(format!(
                    "{name} is {profile}'s snapshot, which every slot's layers sit on; reseed {profile} to rebuild it"
                )));
            }
            api::CacheKind::Slot { profile, slot } => {
                if self.reseeds.contains_key(profile) {
                    return Err(ControlError::Conflict(format!(
                        "{profile} is reseeding, which writes {name}"
                    )));
                }
                self.occupied()
                    .filter(|(at, _)| at.profile == *profile && at.index + 1 == *slot)
                    .map(|(_, occupant)| occupant.running_job())
                    .collect()
            }
            api::CacheKind::Pnpm | api::CacheKind::Maven | api::CacheKind::Mirror => self
                .occupied()
                .map(|(_, occupant)| occupant.running_job())
                .collect(),
        };
        if !users.is_empty() {
            return Err(ControlError::Busy {
                message: format!("{name} is in use by {}", list(&users)),
                jobs: users,
            });
        }
        if volume.kind == api::CacheKind::Mirror && self.mirror_up {
            cache::mirror_down(&*self.docker, &self.names)
                .await
                .map_err(|error| {
                    ControlError::Failed(format!("cannot stop the mirror: {error}"))
                })?;
            self.mirror_up = false;
        }
        self.docker
            .remove_volume(name)
            .await
            .map_err(|error| ControlError::Failed(format!("cannot remove {name}: {error}")))?;
        info!("removed {name}");
        // Measured again once idle.
        self.trim_due = true;
        Ok(())
    }

    /// Runs with a new, validated config. Running jobs keep running: a removed profile, or slots past a lowered
    /// number, stay until their runners finish, and no runner starts in a profile while it has as many as its
    /// slots. Runners started from now on get the new settings.
    pub(super) fn reload(&mut self, config: Config, memory_max: Option<u64>) {
        let mut old = std::mem::replace(&mut self.config, config);
        for (name, profile) in &self.config.profiles {
            let slots = self.slots.entry(name.clone()).or_default();
            if slots.len() < profile.slots as usize {
                slots.resize(profile.slots as usize, None);
            }
            self.retired.remove(name);
        }
        for (name, profile) in std::mem::take(&mut old.profiles) {
            if !self.config.profiles.contains_key(&name) {
                self.retired.insert(name, profile);
            }
        }
        self.drop_closed_slots();
        let names: BTreeSet<String> = self
            .config
            .repositories
            .iter()
            .map(|repository| repository.name.clone())
            .collect();
        let removed: Vec<String> = self
            .repositories
            .keys()
            .filter(|name| !names.contains(*name))
            .cloned()
            .collect();
        self.draining.extend(removed);
        self.draining.retain(|name| !names.contains(name));
        self.repositories.retain(|name, _| names.contains(name));
        for name in &names {
            self.repositories.entry(name.clone()).or_default();
        }
        self.control.set_repositories(names);
        let profiles = &self.config.profiles;
        self.reseed_requested
            .retain(|name| profiles.contains_key(name));
        self.isolation = isolation::backend(self.config.host.isolation);
        self.memory_max = memory_max;
        self.caches
            .send_replace(cache::volumes(&self.names, &self.config));
        info!("reloaded the config");
    }
}
