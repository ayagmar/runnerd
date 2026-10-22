//! A slot's runner from start to removal: registers it with a JIT config and starts its container, stops idle or
//! forced ones, frees the slots of ended containers and holds back a repository whose runners end without taking
//! a job. It must not decide which slots start or stop: `tick` does, through `plan`.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::{Engine, Occupant, image, unix_secs};
use crate::cache::{self, runner_mounts};
use crate::docker::{DockerApi, RunnerSpec, is_not_found};
use crate::github::{Deleted, GithubApi};
use crate::scheduler::SlotRef;

/// Where the next backoff of a repository whose runners keep failing ends.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(30 << failures.saturating_sub(1).min(5)).min(Duration::from_secs(600))
}

impl<G: GithubApi, D: DockerApi + 'static> Engine<G, D> {
    /// Frees the slots whose container stopped: the runner took its job and exited, or it never started.
    pub(super) async fn collect_finished(&mut self) {
        let occupied: Vec<(SlotRef, Occupant)> = self
            .occupied()
            .map(|(slot, occupant)| (slot, occupant.clone()))
            .collect();
        for (slot, occupant) in occupied {
            let running = match self.docker.inspect(&occupant.container).await {
                Ok(state) => state.running,
                Err(error) if is_not_found(&error) => false,
                Err(error) => {
                    warn!("cannot inspect {}: {error}", occupant.runner_name);
                    continue;
                }
            };
            if !running {
                info!("{}: {} ended", occupant.repository, occupant.runner_name);
                self.release(&slot, &occupant, true).await;
            }
        }
    }

    /// Removes a slot's container and its runner, then frees the slot. With `ended`, the container stopped on its
    /// own, and if its runner took no job, the repository's next runners are held back for a while.
    async fn release(&mut self, slot: &SlotRef, occupant: &Occupant, ended: bool) {
        // The runner logs each job it takes; its exit code says nothing (run.sh exits 0 when it fails to start).
        let took_job = if ended {
            match self.docker.logs(&occupant.container).await {
                Ok(logs) => Some(logs.contains("Running job: ")),
                Err(error) => {
                    warn!("cannot read the log of {}: {error}", occupant.runner_name);
                    None
                }
            }
        } else {
            None
        };
        // The capture ends with the container; its last lines must be stored before the job's record closes.
        self.end_capture(&occupant.runner_name).await;
        self.count_mirror_bytes().await;
        match self.docker.remove(&occupant.container).await {
            Err(error) if !is_not_found(&error) => {
                warn!("cannot remove {}: {error}", occupant.runner_name);
                return;
            }
            _ => {}
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
        if let Some(state) = self.repositories.get_mut(&occupant.repository) {
            match took_job {
                Some(true) => {
                    state.failures = 0;
                    state.held_until = None;
                }
                Some(false) => {
                    warn!(
                        "{}: {} ended without taking a job",
                        occupant.repository, occupant.runner_name
                    );
                    self.hold(&occupant.repository);
                }
                None => {}
            }
        }
        self.vacate(slot);
    }

    /// Frees a slot whose runner is gone. Its runner may have grown the shared caches, and in slot 1 its
    /// profile's layers: both are checked once idle. A killed job's partial writes count too: they stay in slot 1
    /// and would reach the snapshot on any later reseed, like those of a job cancelled or out of memory.
    fn vacate(&mut self, slot: &SlotRef) {
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            *entry = None;
        }
        if slot.index == 0 {
            self.reseed_due.insert(slot.profile.clone());
        }
        self.trim_due = true;
        self.drop_closed_slots();
    }

    /// Removes an idle runner, or with `force` a busy one and its job.
    pub(super) async fn stop(&mut self, slot: &SlotRef, force: bool) {
        let Some(occupant) = self
            .slots
            .get(&slot.profile)
            .and_then(|slots| slots.get(slot.index))
            .cloned()
            .flatten()
        else {
            return;
        };
        if force {
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
            self.vacate(slot);
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
                self.release(slot, &occupant, false).await;
            }
            Err(error) => warn!("cannot delete {}: {error}", occupant.runner_name),
        }
    }

    /// Holds back a repository's next runners after one failed, longer each time in a row.
    fn hold(&mut self, repository: &str) {
        if let Some(state) = self.repositories.get_mut(repository) {
            state.failures += 1;
            let wait = backoff(state.failures);
            state.held_until = Some(SystemTime::now() + wait);
            warn!("{repository}: no runner starts for {}s", wait.as_secs());
        }
    }

    pub(super) async fn start(&mut self, slot: &SlotRef, repository: &str) {
        let Some(profile) = self.config.profiles.get(&slot.profile) else {
            return;
        };
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
        let (runner, jit_config) = match self.github.jit_config(repository, &name, &labels).await {
            Ok(registered) => registered,
            Err(error) => {
                warn!("{repository}: cannot register a runner: {error}");
                self.hold(repository);
                return;
            }
        };
        let spec = RunnerSpec {
            name: name.clone(),
            image: image(&profile.image_stage),
            profile: slot.profile.clone(),
            slice: self.config.host.slice.clone(),
            cpus: profile.cpus,
            memory: profile.memory,
            env: Vec::new(),
            mounts: runner_mounts(&self.names, &slot.profile, number),
            network: Some(self.names.network()),
            labels: BTreeMap::from([
                ("runnerd.job".to_owned(), name.clone()),
                ("runnerd.repository".to_owned(), repository.to_owned()),
                ("runnerd.slot".to_owned(), number.to_string()),
                ("runnerd.runner-id".to_owned(), runner.id.to_string()),
            ]),
            entrypoint: None,
            jit_config: Some(jit_config),
        };
        match self.docker.start(&spec, self.isolation).await {
            Ok(container) => {
                info!(
                    "{repository}: started {name} in {} slot {number}",
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
                    job: None,
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
                warn!("{repository}: cannot start {name}: {error}");
                if let Err(error) = self.github.delete_runner(repository, runner.id).await {
                    warn!("{repository}: cannot delete {name}: {error}");
                }
                self.hold(repository);
            }
        }
    }
}
