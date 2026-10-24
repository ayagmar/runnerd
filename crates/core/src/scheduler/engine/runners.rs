//! A slot's runner from start to removal: registers it with a JIT config and starts its container, stops idle or
//! forced ones, frees the slots of ended containers and holds back a repository whose runners end without taking
//! a job. It must not decide which slots start or stop: `tick` does, through `plan`.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::{Engine, Occupant, Purpose, RunningJobs, unix_secs};
use crate::api::{Admission, Event, HoldCause, RunnerFailure};
use crate::cache;
use crate::docker::{DockerApi, RunnerSpec, is_not_found};
use crate::github::{Deleted, GithubApi};
use crate::record;
use crate::scheduler::{Hold, IDLE_TIMEOUT, SlotRef};

/// The runner container's label with the labels its runner registered with, comma-separated as GitHub's runner
/// takes them, so a restart knows them.
pub(super) const LABELS: &str = "runnerd.labels";

/// The runner container's label with the image it runs, as the config named it, so a restart knows it.
pub(super) const IMAGE: &str = "runnerd.image";

/// Why a slot's runner is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Release {
    /// Its container stopped on its own.
    Ended,
    /// It was idle and is no longer needed.
    Idle,
}

/// The labels of a runner container: what a restart adopts it by.
fn container_labels(
    name: &str,
    repository: &str,
    slot: usize,
    runner_id: u64,
    labels: &[String],
    image: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("runnerd.job".to_owned(), name.to_owned()),
        ("runnerd.repository".to_owned(), repository.to_owned()),
        ("runnerd.slot".to_owned(), slot.to_string()),
        ("runnerd.runner-id".to_owned(), runner_id.to_string()),
        (LABELS.to_owned(), labels.join(",")),
        (IMAGE.to_owned(), image.to_owned()),
    ])
}

/// Where the next backoff of a repository whose runners keep failing ends.
pub(super) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(600))
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Frees the slots whose container stopped: the runner took its job and exited, or it never started.
    pub(super) async fn collect_finished(&mut self) {
        let occupied: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in occupied {
            let running = match self.docker.inspect(&occupant.container).await {
                // A kill in the last interval before it stopped, which no sample of its cgroup saw.
                Ok(state) if state.oom_killed => {
                    self.store.runner_oom_killed(&occupant.runner_name);
                    state.running
                }
                Ok(state) => state.running,
                Err(error) if is_not_found(&error) => false,
                Err(error) => {
                    warn!("cannot inspect {}: {error}", occupant.runner_name);
                    continue;
                }
            };
            if !running {
                info!("{}: {} ended", occupant.repository, occupant.runner_name);
                self.release(&slot, &occupant, Release::Ended).await;
            }
        }
    }

    /// Removes a slot's container and its runner, then frees the slot. If the container `Ended` and its runner took
    /// no job, the repository's next runners are held back for a while.
    async fn release(&mut self, slot: &SlotRef, occupant: &Occupant, why: Release) {
        // The runner logs each job it takes and how it ended; its exit code says nothing (run.sh exits 0 when it
        // fails to start).
        let logs = if why == Release::Ended {
            match self.docker.logs(&occupant.container).await {
                Ok(logs) => Some(logs),
                Err(error) => {
                    warn!("cannot read the log of {}: {error}", occupant.runner_name);
                    None
                }
            }
        } else {
            None
        };
        let took_job = logs.as_ref().map(|logs| logs.contains("Running job: "));
        // The capture ends with the container; its last lines must be stored before the job's record closes.
        self.end_capture(&occupant.runner_name).await;
        self.count_mirror_bytes().await;
        // A failed job's container is kept as its record, then removed, in the background.
        let recorded =
            logs.as_deref().is_some_and(record::failed) && self.begin_record(slot, occupant).await;
        if !recorded {
            match self.docker.remove(&occupant.container).await {
                Err(error) if !is_not_found(&error) => {
                    warn!("cannot remove {}: {error}", occupant.runner_name);
                    return;
                }
                _ => {}
            }
        }
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            // Busy: left to the next poll's cleanup of runners without a container.
            Ok(_) => {}
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
        self.forget(occupant);
        let ran = occupant.busy
            || occupant.job.is_some()
            || match why {
                // Unread logs: it may have run one.
                Release::Ended => took_job != Some(false),
                Release::Idle => false,
            };
        if let Some(state) = self.repositories.get_mut(&occupant.repository) {
            match took_job {
                Some(true) => {
                    state.failures = 0;
                    state.idle_exits = 0;
                    state.held = None;
                }
                // One that waited `IDLE_TIMEOUT` had started and connected: a warm runner whose container went
                // once (a restart of Docker, the runner removed on GitHub) says nothing of the next one's start. The
                // second in a row is a fault that would restart one every few minutes.
                Some(false)
                    if occupant
                        .started_at
                        .elapsed()
                        .is_ok_and(|waited| waited >= IDLE_TIMEOUT) =>
                {
                    state.idle_exits += 1;
                    // It started and connected: a failure before it is over, and the next one is a new streak's
                    // first, announced. Not so for the second in a row, a fault of its own.
                    if state.idle_exits == 1 {
                        state.failures = 0;
                    }
                    warn!(
                        "{}: {} ended without a job after waiting for one",
                        occupant.repository, occupant.runner_name
                    );
                    if state.idle_exits >= 2 {
                        self.hold(
                            &occupant.repository,
                            &slot.profile,
                            HoldCause::IdleExits,
                            "its runners end without a job after waiting for one".to_owned(),
                        );
                    }
                }
                Some(false) => {
                    warn!(
                        "{}: {} ended without taking a job",
                        occupant.repository, occupant.runner_name
                    );
                    self.hold(
                        &occupant.repository,
                        &slot.profile,
                        HoldCause::NoJob,
                        format!("{} ended without taking a job", occupant.runner_name),
                    );
                }
                None => {}
            }
        }
        self.vacate(slot, ran);
    }

    /// Frees a slot whose runner is gone. Its runner may have grown the shared caches, and if it `ran` a job in slot
    /// 1, its profile's layers: both are checked once idle. A killed job's partial writes count too: they stay in
    /// slot 1 and would reach the snapshot on any later reseed, like those of a job cancelled or out of memory. A
    /// warm runner that leaves slot 1 without a job changed nothing there.
    fn vacate(&mut self, slot: &SlotRef, ran: bool) {
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            *entry = None;
        }
        if slot.index == 0 && ran {
            self.reseed_due.insert(slot.profile.clone());
        }
        self.trim_due = true;
        self.drop_closed_slots();
    }

    /// A runner container's environment: its slot's share of what the slots' Docker Engines may keep, which its
    /// image's job cleanup holds it to, once the caches' check has measured Docker's disk.
    fn runner_env(&self) -> Vec<String> {
        self.docker_disk
            .map(|disk| {
                let slots = self
                    .config
                    .profiles
                    .values()
                    .map(|p| u64::from(p.slots))
                    .sum();
                vec![format!(
                    "RUNNERD_DOCKER_BUDGET_GB={}",
                    cache::docker_budget_gib(disk, slots)
                )]
            })
            .unwrap_or_default()
    }

    /// Removes an idle runner, or with `RunningJobs::Kill` a busy one and its job.
    pub(super) async fn stop(&mut self, slot: &SlotRef, jobs: RunningJobs) {
        let Some(occupant) = self
            .slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .cloned()
            .flatten()
        else {
            return;
        };
        if jobs == RunningJobs::Kill {
            info!("{}: killing {}", occupant.repository, occupant.runner_name);
            if let Err(error) = self.docker.remove(&occupant.container).await
                && !is_not_found(&error)
            {
                warn!("cannot remove {}: {error}", occupant.runner_name);
                return;
            }
            self.end_capture(&occupant.runner_name).await;
            // Responses the mirror logged since this tick's count: on a shutdown, the mirror and its log go next.
            self.count_mirror_bytes().await;
            self.forget(&occupant);
            match self
                .github
                .delete_runner(&occupant.repository, occupant.runner_id)
                .await
            {
                // GitHub refuses until it notices the job is gone, minutes later, even if the run is cancelled.
                Ok(Deleted::Busy) => warn!(
                    "{}: GitHub keeps {} until it notices its job is gone; the next runnerd start deletes it",
                    occupant.repository, occupant.runner_name
                ),
                Ok(_) => {}
                Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
            }
            self.vacate(slot, occupant.busy || occupant.job.is_some());
            return;
        }
        match self
            .github
            .delete_runner(&occupant.repository, occupant.runner_id)
            .await
        {
            Ok(Deleted::Busy) => {
                if let Some(Some(entry)) = self
                    .slots
                    .get_mut(&slot.profile)
                    .and_then(|slots| slots.get_mut(slot.index))
                {
                    entry.busy = true;
                }
            }
            Ok(Deleted::Deleted | Deleted::NotFound) => {
                info!(
                    "{}: removed idle {}",
                    occupant.repository, occupant.runner_name
                );
                self.release(slot, &occupant, Release::Idle).await;
            }
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
    }

    /// Holds back a repository's next runners after one failed, longer each time in a row, and announces the first
    /// hold of a streak as `runner.failed`: a repository that stays broken says so once, not every 10 minutes, until
    /// a runner takes a job. Runners that fail while it is held started before the hold, so they do not lengthen it.
    fn hold(&mut self, repository: &str, profile: &str, cause: HoldCause, error: String) {
        if let Some(state) = self.repositories.get_mut(repository)
            && state
                .held
                .is_none_or(|held| held.until <= SystemTime::now())
        {
            state.failures += 1;
            let wait = backoff(state.failures);
            state.held = Some(Hold {
                until: SystemTime::now() + wait,
                cause,
            });
            warn!("{repository}: no runner starts for {}s", wait.as_secs());
            if state.failures > 1 {
                return;
            }
            self.store.event(Event::RunnerFailed {
                failure: RunnerFailure {
                    repository: repository.to_owned(),
                    profile: profile.to_owned(),
                    error,
                    held_for: wait.as_secs(),
                },
            });
        }
    }

    /// Starts the registry mirror, when enabled and down, for a runner about to start.
    async fn start_mirror(&mut self) {
        let mirror = &self.config.caches.mirror;
        if mirror.enabled && !self.mirror_up {
            match cache::mirror_up(&*self.docker, &self.names, mirror, &self.config.host.slice)
                .await
            {
                Ok(()) => {
                    self.mirror_up = true;
                    self.mirror_memory = mirror.memory.0;
                }
                // Runners pull from Docker Hub directly when the mirror does not answer.
                Err(error) => warn!("cannot start the mirror: {error}"),
            }
        }
    }

    pub(super) async fn start(
        &mut self,
        slot: &SlotRef,
        repository: &str,
        admission: Admission,
        purpose: Purpose,
    ) {
        let Some(labels) = self
            .config
            .repositories
            .iter()
            .find(|candidate| candidate.name == repository)
            .map(|candidate| candidate.labels.clone())
        else {
            return;
        };
        let number = slot.index + 1;
        let now = SystemTime::now();
        let name = self
            .identity
            .runner_name(&slot.profile, number, unix_secs(now));
        if let Err(error) = cache::prepare(&*self.docker, &self.names, &slot.profile, number).await
        {
            warn!(
                "cannot prepare the caches of {} slot {number}: {error}",
                slot.profile
            );
            return;
        }
        self.start_mirror().await;
        let Some(profile) = self.config.profiles.get(&slot.profile) else {
            return;
        };
        let (runner, jit_config) = match self.github.jit_config(repository, &name, &labels).await {
            Ok(registered) => registered,
            Err(error) => {
                let failure = format!("cannot register a runner: {error}");
                warn!("{repository}: {failure}");
                self.hold(repository, &slot.profile, HoldCause::Register, failure);
                return;
            }
        };
        let image = profile.image.reference();
        let spec = RunnerSpec {
            name: name.clone(),
            image: image.clone(),
            profile: slot.profile.clone(),
            slice: self.config.host.slice.clone(),
            cpus: profile.cpus,
            memory: profile.memory,
            env: self.runner_env(),
            mounts: self.runner_mounts(&slot.profile, number, &name, repository),
            network: Some(self.names.network()),
            labels: container_labels(&name, repository, number, runner.id, &labels, &image),
            entrypoint: None,
            jit_config: Some(jit_config),
        };
        match self.docker.start(&spec, self.isolation).await {
            Ok(container) => {
                let warm = match purpose {
                    Purpose::Job => "",
                    Purpose::Warm => " warm, for jobs not queued yet",
                };
                info!(
                    "{repository}: started {name} in {} slot {number}{warm}",
                    slot.profile
                );
                let occupant = Occupant {
                    repository: repository.to_owned(),
                    runner_name: name,
                    runner_id: runner.id,
                    container,
                    busy: false,
                    started_at: now,
                    memory: profile.memory.0,
                    cpus: u64::from(profile.cpus) * 1000,
                    admission: Some(admission),
                    job: None,
                    key: None,
                    labels: Some(labels),
                    image: Some(image),
                };
                if let Some(entry) = self
                    .slots
                    .get_mut(&slot.profile)
                    .and_then(|slots| slots.get_mut(slot.index))
                {
                    *entry = Some(occupant.clone());
                }
                self.observe(slot, &occupant).await;
            }
            Err(error) => {
                self.close_build_cache(&name);
                warn!("{repository}: cannot start {name}: {error}");
                self.recheck_image(&spec.image);
                let failure = format!("cannot start {name}: {error}");
                if let Err(error) = self.github.delete_runner(repository, runner.id).await {
                    warn!("{repository}: cannot delete {name}: {error}");
                }
                self.hold(repository, &slot.profile, HoldCause::Start, failure);
            }
        }
    }
}
