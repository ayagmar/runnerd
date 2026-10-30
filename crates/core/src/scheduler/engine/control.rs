//! Control from the API: the `Control` handle the daemon's API holds (pause flags, which it records in the store so
//! a restart keeps them, shutdown, a channel of `Command`s) and the engine's side of each command: pause and resume,
//! of one repository or all, with or without cancelling their runs, reseed, cache clean, config replacement, the
//! removal of records and the shells opened in them.
//! Commands run between ticks or while a tick waits for GitHub, so they see the slot state a tick does (ADRs 0019,
//! 0026). It must not serve HTTP or write the config file: the daemon does, through `Commit`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{info, warn};

use super::records::RecordSelection;
use super::{Engine, Occupant};
use crate::api;
use crate::cache;
use crate::config::{ByteSize, Config};
use crate::docker::{DockerApi, Tty, TtySize};
use crate::github::GithubApi;
use crate::isolation;
use crate::scheduler::SlotRef;
use crate::sizing::auto;
use crate::store::Store;
use crate::sync::lock;
use crate::time;

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

/// What a new config may do to slots with a runner in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningSlots {
    /// A slot past a lowered `slots`, or of a removed profile, stays until its runner finishes.
    Drain,
    /// The config is refused while a runner is in a slot it does not have: what auto sizing asks for.
    Keep,
}

/// Who writes a new config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    /// The owner, through `PUT /v1/config`: a save that lowers `memory` under a raise of auto sizing's in the last
    /// day is answered with a warning (`auto::undone_raises`).
    Owner,
    /// Auto sizing, whose own lowering is never a warning about its raise.
    AutoSizing,
}

/// `Err` when `new` moves runnerd to another slice than `running` names: the sampler and the running containers stay
/// in the slice runnerd started in, so the slice changes only at a restart.
pub fn same_slice(running: &Config, new: &Config) -> Result<(), ControlError> {
    if new.host.slice == running.host.slice {
        return Ok(());
    }
    Err(ControlError::Invalid(format!(
        "restart runnerd to change the slice: it runs in {}, and host.slice is {} in the new config",
        running.host.slice, new.host.slice
    )))
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
    /// A pause or resume holds in memory, but the job history could not record it: a restart undoes it.
    #[error("{0}")]
    NotRecorded(String),
    /// The engine stopped.
    #[error("runnerd is stopping")]
    Stopped,
}

/// A command the engine carries out between ticks or while a tick waits for GitHub, with where its answer goes.
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
    /// Resume one repository or all.
    Resume {
        /// What to resume.
        target: Target,
        /// The answer.
        reply: oneshot::Sender<Result<(), ControlError>>,
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
        /// The CPUs of the slice it names, in millicores, read when it sets `cpu_overcommit`.
        cpus: Option<u64>,
        /// Writes the file.
        commit: Commit,
        /// Whether it may leave runners in slots it does not have.
        running: RunningSlots,
        /// Who writes it.
        author: Author,
        /// Answered once the engine runs with it: with the commit's warning and what it undoes of auto sizing's
        /// raises, or why it did not commit.
        reply: oneshot::Sender<Result<Vec<String>, ControlError>>,
    },
    /// Remove records of failed jobs.
    RemoveRecords {
        /// Which.
        selection: RecordSelection,
        /// The answer.
        reply: oneshot::Sender<Result<api::RemovedRecords, ControlError>>,
    },
    /// Start a shell in a job's record, if the budget has room for it.
    OpenShell {
        /// The job.
        job: u64,
        /// Its terminal's size.
        size: TtySize,
        /// The shell's terminal.
        reply: oneshot::Sender<Result<Tty, ControlError>>,
    },
    /// Remove a shell's container: its terminal closed.
    CloseShell {
        /// The container's ID.
        container: String,
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
    /// If the record fails, the change holds until a restart and the answer is `NotRecorded`. The status shows it
    /// at the engine's next publish: the API pauses and resumes through `pause` and `resume`, which publish before
    /// they answer.
    pub async fn set_paused(&self, target: &Target, paused: bool) -> Result<(), ControlError> {
        let recorded = {
            let repositories = lock(&self.repositories);
            let names: Vec<&String> = match target {
                Target::Repository(repository) => {
                    let Some(name) = repositories.get(repository) else {
                        return Err(not_configured(repository, repositories.iter()));
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
        if recorded.await {
            return Ok(());
        }
        let name = match target {
            Target::Repository(repository) => repository.as_str(),
            Target::All => "every repository",
        };
        let (done, undone) = if paused {
            ("paused", "resumes it")
        } else {
            ("resumed", "pauses it again")
        };
        let message = format!(
            "{name} is {done} until runnerd restarts, but the job history cannot record it, so a restart {undone}"
        );
        warn!("{message}");
        Err(ControlError::NotRecorded(message))
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

    /// Resumes one repository or all, and publishes the status before it answers, so a client that reads the status
    /// or the events after the answer sees them resumed. Fails as `set_paused` does.
    pub async fn resume(&self, target: Target) -> Result<(), ControlError> {
        self.send(|reply| Command::Resume { target, reply }).await?
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

    /// Removes the selected records of failed jobs and their images.
    pub async fn remove_records(
        &self,
        selection: RecordSelection,
    ) -> Result<api::RemovedRecords, ControlError> {
        self.send(|reply| Command::RemoveRecords { selection, reply })
            .await?
    }

    /// Starts a shell in a job's record and returns its terminal. The caller closes it with `close_shell` once the
    /// terminal ends, from either side.
    pub async fn open_shell(&self, job: u64, size: TtySize) -> Result<Tty, ControlError> {
        self.send(|reply| Command::OpenShell { job, size, reply })
            .await?
    }

    /// Removes a shell's container. Not waited for, so it can be sent while the terminal is dropped.
    pub fn close_shell(&self, container: String) {
        let _ = self.commands.send(Command::CloseShell { container });
    }

    /// Runs `commit` to write a validated config, and if it commits, runs with the config from then on, before the
    /// next tick. If it does not, nothing changes and the answer is `Failed`. Running jobs keep running: under
    /// `RunningSlots::Drain` removed profiles and slots go as their runners finish; under `Keep` a config without a
    /// slot a runner is in is refused (`Conflict`) before `commit` runs. A config with another `host.slice` is refused
    /// (`Invalid`) before `commit` runs: the slice takes effect when runnerd starts. The answer warns of what may not
    /// survive a crash and, for `Author::Owner`, of each profile whose `memory` it lowers under a raise of auto
    /// sizing's in the last day (`auto::undone_raises`), which are logged too.
    pub async fn replace_config(
        &self,
        config: Config,
        memory_max: Option<u64>,
        cpus: Option<u64>,
        commit: Commit,
        running: RunningSlots,
        author: Author,
    ) -> Result<Vec<String>, ControlError> {
        self.send(|reply| Command::ReplaceConfig {
            config: Box::new(config),
            memory_max,
            cpus,
            commit,
            running,
            author,
            reply,
        })
        .await?
    }
}

/// Names the repositories there are, so a mistyped name shows its fix.
fn not_configured<'name>(
    repository: &str,
    configured: impl Iterator<Item = &'name String>,
) -> ControlError {
    let configured: Vec<&str> = configured.map(String::as_str).collect();
    let configured = if configured.is_empty() {
        "none is configured".to_owned()
    } else {
        format!("configured: {}", configured.join(", "))
    };
    ControlError::NotFound(format!(
        "{repository} is not a configured repository; {configured}"
    ))
}

/// `jobs` for people to read: `job 1 on runner-a, runner-b`.
fn list(jobs: &[api::RunningJob]) -> String {
    jobs.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
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
            Command::Resume { target, reply } => {
                let _ = reply.send(self.control.set_paused(&target, false).await);
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
                cpus,
                commit,
                running,
                author,
                reply,
            } => {
                if let Err(refused) = same_slice(&self.config, &config) {
                    let _ = reply.send(Err(refused));
                    return;
                }
                if running == RunningSlots::Keep
                    && let Some(occupied) = self.occupied_past(&config)
                {
                    let _ = reply.send(Err(ControlError::Conflict(occupied)));
                    return;
                }
                let undone = match author {
                    Author::Owner => self.undone_raises(&config).await,
                    Author::AutoSizing => Vec::new(),
                };
                let answer = match commit.run().await {
                    Ok(warning) => {
                        for line in &undone {
                            warn!("config replaced: {line}");
                        }
                        self.reload(*config, memory_max, cpus);
                        Ok(warning.into_iter().chain(undone).collect())
                    }
                    Err(message) => Err(ControlError::Failed(message)),
                };
                let _ = reply.send(answer);
            }
            Command::RemoveRecords { selection, reply } => {
                let _ = reply.send(self.remove_records(&selection).await);
            }
            Command::OpenShell { job, size, reply } => {
                let opened = self.open_shell(job, size).await;
                // A terminal nobody waits for any more is closed at once.
                if let Err(Ok(tty)) = reply.send(opened) {
                    self.close_shell(&tty.container).await;
                }
            }
            Command::CloseShell { container } => self.close_shell(&container).await,
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
                return Err(not_configured(repository, self.repositories.keys()));
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
            // The jobs only: each client says how to force the pause in its own terms.
            return Err(ControlError::Busy {
                message: runs.join("; "),
                jobs,
            });
        }
        // Paused in memory even if not recorded: a forced pause still cancels the runs it was asked to.
        let recorded = self.control.set_paused(target, true).await;
        match target {
            Target::Repository(repository) => info!("{repository}: paused"),
            Target::All => info!("paused every repository"),
        }
        let mut stopped = Vec::new();
        if running == RunningJobs::Keep {
            return recorded.map(|()| api::Paused { stopped });
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
        match recorded {
            Ok(()) => Ok(api::Paused { stopped }),
            Err(ControlError::NotRecorded(message)) if !stopped.is_empty() => Err(
                ControlError::NotRecorded(format!("{message}; stopped {}", list(&stopped))),
            ),
            Err(error) => Err(error),
        }
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
        // A shell mounts the snapshot a reseed replaces, and may stay open for hours.
        if self.shell_profiles().contains(profile) {
            return Err(ControlError::Conflict(format!(
                "a shell is open in a record of {profile}, which mounts its snapshot; reseed it once the shell exits"
            )));
        }
        info!("{profile}: reseeding once its slots are free");
        self.reseed_requested.insert(profile.to_owned());
        Ok(())
    }

    /// Removes a slot's volume or a shared cache's, unless a job or a reseed uses it; the runners that use it and
    /// wait for a job stop first. Snapshots are rebuilt by reseeding instead: slot layers sit on top of them.
    pub(super) async fn clean_cache(&mut self, name: &str) -> Result<(), ControlError> {
        let Some(volume) = cache::volumes(&self.names, &self.config)
            .into_iter()
            .find(|volume| volume.name == name)
        else {
            return Err(ControlError::NotFound(format!(
                "{name} is not a cache volume of this config; `runnerctl caches` lists them"
            )));
        };
        let using: Vec<SlotRef> = match &volume.kind {
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
                    .map(|(at, _)| at)
                    .collect()
            }
            api::CacheKind::Pnpm
            | api::CacheKind::Maven
            | api::CacheKind::Mirror
            | api::CacheKind::Build => self.occupied().map(|(at, _)| at).collect(),
        };
        let shared = matches!(volume.kind, api::CacheKind::Pnpm | api::CacheKind::Maven);
        if shared && !self.records.shells.is_empty() {
            return Err(ControlError::Conflict(format!(
                "a shell open in a record mounts {name}; clean it once the shell exits"
            )));
        }
        // Runners waiting for a job, warm ones too, give way: the next plan starts them again. One GitHub gave a
        // job meanwhile stays, and holds the clean back.
        for slot in &using {
            if self
                .occupant(slot)
                .is_some_and(|occupant| !occupant.busy && occupant.job.is_none())
            {
                self.stop(slot, RunningJobs::Keep).await;
            }
        }
        let users: Vec<api::RunningJob> = using
            .iter()
            .filter_map(|slot| self.occupant(slot))
            .map(Occupant::running_job)
            .collect();
        if !users.is_empty() {
            return Err(ControlError::Busy {
                message: format!(
                    "{name} is in use by {}; clean it once they finish",
                    list(&users)
                ),
                jobs: users,
            });
        }
        if volume.kind == api::CacheKind::Build {
            let Some(cache) = self.build_cache.clone() else {
                return Err(ControlError::Failed(
                    "the build cache did not open when runnerd started; its log says why"
                        .to_owned(),
                ));
            };
            let trimmed = tokio::task::spawn_blocking(move || cache.clear())
                .await
                .map_err(|error| ControlError::Failed(format!("cannot empty {name}: {error}")))?;
            info!(
                "emptied {name}: {} entries, {}",
                trimmed.before.entries,
                ByteSize(trimmed.before.bytes)
            );
            self.trim_due = true;
            return Ok(());
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
        // Its layer no longer holds what a job cached.
        if let api::CacheKind::Slot { profile, slot } = &volume.kind {
            self.ran.remove(&SlotRef {
                profile: profile.clone(),
                index: slot.saturating_sub(1),
            });
        }
        // Measured again once idle.
        self.trim_due = true;
        Ok(())
    }

    /// What `config` undoes of auto sizing's `memory` raises in the last day; none when they cannot be read, which is
    /// logged: the config runs either way.
    async fn undone_raises(&self, config: &Config) -> Vec<String> {
        let now = time::now_ms() / 1000;
        let since = now.saturating_sub(auto::UNDONE_RAISES).saturating_mul(1000);
        match self.store.sizing_changes(since).await {
            Ok(changes) => auto::undone_raises(&self.config, config, &changes, now),
            Err(store_error) => {
                warn!(
                    "cannot read auto sizing's changes to compare a new config with: {store_error}"
                );
                Vec::new()
            }
        }
    }

    /// The first slot with a runner in it that `config` does not have, for people to read.
    fn occupied_past(&self, config: &Config) -> Option<String> {
        self.slots.iter().find_map(|(name, slots)| {
            let kept = config
                .profiles
                .get(name)
                .map_or(0, |profile| profile.slots as usize);
            slots
                .iter()
                .enumerate()
                .skip(kept)
                .find(|(_, slot)| slot.is_some())
                .map(|(index, _)| {
                    format!(
                        "{name}'s slot {} has a runner, and the new config keeps {kept} slots",
                        index + 1
                    )
                })
        })
    }

    /// Runs with a new, validated config. Running jobs keep running: a removed profile, or slots past a lowered
    /// number, stay until their runners finish, and no runner starts in a profile while it has as many as its
    /// slots. Runners started from now on get the new settings.
    /// `cpus` are the slice's, read when the config sets `cpu_overcommit`.
    pub(super) fn reload(&mut self, config: Config, memory_max: Option<u64>, cpus: Option<u64>) {
        let mut old = std::mem::replace(&mut self.config, config);
        if let Some(cache) = &self.build_cache {
            cache.set_max_size(self.config.caches.build.max_size.0);
        }
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
        self.images.stale_due = true;
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
        self.oversized.retain(|name| profiles.contains_key(name));
        self.isolation = isolation::backend(self.config.host.isolation);
        self.memory_max = memory_max;
        self.cpus = cpus;
        self.caches
            .send_replace(cache::volumes(&self.names, &self.config));
        info!("reloaded the config");
    }
}
