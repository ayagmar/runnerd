//! Publishes `api::Status` after each tick and command, and records the profiles, slots, budget and repositories
//! that changed as events. It reads engine state and must not change it, besides the status channel.

use std::collections::{BTreeMap, BTreeSet};

use super::{Engine, Shutdown, unix_secs};
use crate::api;
use crate::docker::DockerApi;
use crate::github::GithubApi;
use crate::scheduler::{Plan, SlotRef};

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    pub(super) fn publish(&mut self) {
        let mode = self.control.mode();
        // Why jobs wait and which runners are warm, from a plan of the engine as it is now, not as the tick's plan
        // saw it: a start that failed, a runner stopped over budget or a pause since then would make that one wrong.
        // A shutdown starts nothing, so no job says why it waits and no runner is warm.
        let plan = match mode {
            Shutdown::No => self.plan().0,
            Shutdown::Graceful | Shutdown::Force => Plan::default(),
        };
        let profiles = self
            .config
            .profiles
            .iter()
            .chain(&self.retired)
            .map(|(name, profile)| api::ProfileStatus {
                name: name.clone(),
                image: profile.image.reference(),
                cpus: profile.cpus,
                memory: profile.memory.0,
                reseeding: self.is_reseeding(name),
                slots: self
                    .slots
                    .get(name)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    // Slots a reload removed show while their runner runs.
                    .filter(|(index, slot)| {
                        slot.is_some()
                            || self
                                .config
                                .profiles
                                .get(name)
                                .is_some_and(|profile| *index < profile.slots as usize)
                    })
                    .map(|(index, slot)| api::SlotStatus {
                        slot: index + 1,
                        runner: slot.as_ref().map(|occupant| api::RunnerStatus {
                            repository: occupant.repository.clone(),
                            name: occupant.runner_name.clone(),
                            runner_id: occupant.runner_id,
                            container: occupant.container.clone(),
                            busy: occupant.busy || occupant.job.is_some(),
                            started_at: unix_secs(occupant.started_at),
                            reserved: self.takes(occupant),
                            warm: plan.warm.contains(&SlotRef {
                                profile: name.clone(),
                                index,
                            }),
                        }),
                    })
                    .collect(),
            })
            .collect();
        // The queue as clients list it: a job a runner took, which GitHub still lists for a few seconds, is not counted.
        let queued = self.queued(&plan.waiting);
        let repositories = self.repository_statuses(&queued);
        let previous = self.status.borrow().clone();
        let announced = self.announced_profiles.take();
        let status = api::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: mode != Shutdown::No,
            budget: self.budget(),
            profiles,
            repositories,
            queued,
        };
        self.announce_changes(&previous, announced.as_deref(), &status);
        self.status.send_replace(status);
        self.publish_records();
    }

    /// Each configured repository once, with its pools and its jobs in `queued`.
    fn repository_statuses(&self, queued: &[api::QueuedJob]) -> Vec<api::RepositoryStatus> {
        self.config
            .repository_names()
            .into_iter()
            .map(|name| {
                let pools: Vec<api::PoolStatus> = self
                    .config
                    .repositories
                    .iter()
                    .filter(|pool| pool.name == name)
                    .map(|pool| api::PoolStatus {
                        profile: pool.profile.clone(),
                        labels: pool.labels.clone(),
                        queued_jobs: queued
                            .iter()
                            .filter(|job| {
                                job.repository == name
                                    && job.profile.as_ref() == Some(&pool.profile)
                            })
                            .count(),
                    })
                    .collect();
                api::RepositoryStatus {
                    pools,
                    paused: self.control.is_paused(&name),
                    queued_jobs: queued.iter().filter(|job| job.repository == name).count(),
                    name,
                }
            })
            .collect()
    }

    /// The memory caps in use, with the mirror's and each seed container's part.
    fn budget(&self) -> api::Budget {
        api::Budget {
            slice: self.config.host.slice.clone(),
            memory_max: self.memory_max,
            memory_in_use: self.memory_in_use(),
            admission: match self.config.host.admission {
                crate::config::Admission::Caps => api::AdmissionPolicy::Caps,
                crate::config::Admission::Measured => api::AdmissionPolicy::Measured,
            },
            held: self.measured.high_water,
            host: self.host_budget(),
            mirror_memory: self.mirror_memory_in_use(),
            seed_memory: self
                .seed_memory()
                .into_iter()
                .map(|(profile, memory)| api::SeedMemory { profile, memory })
                .collect(),
            shell_memory: self.shell_budget(),
            slice_cpus: self.cpus,
            cpu: self
                .cpus
                .zip(self.cpu_budget())
                .map(|(cpus, budget)| api::CpuBudget {
                    cpus,
                    budget,
                    in_use: self.cpu_in_use(),
                    held: self
                        .repositories
                        .values()
                        .any(|state| !state.cpu_held.is_empty()),
                }),
        }
    }

    /// The queued jobs the repositories' pools can take and none holds, and those the plan says no pool serves,
    /// oldest first, with why each `waiting`; none of a repository whose last listing failed or was skipped.
    fn queued(&self, waiting: &BTreeMap<u64, api::Waiting>) -> Vec<api::QueuedJob> {
        let taken: BTreeSet<u64> = self
            .occupied()
            .filter_map(|(_, occupant)| occupant.job)
            .collect();
        let mut queued: Vec<api::QueuedJob> = self
            .repositories
            .iter()
            .flat_map(|(repository, state)| {
                // Unlisted at the last tick: a failed listing or a shutdown, which the history does not follow.
                let queued = if state.listed { &state.queued[..] } else { &[] };
                queued
                    .iter()
                    .filter(|job| !taken.contains(&job.id))
                    .filter_map(move |job| {
                        let pool = self.config.pool(repository, &job.labels);
                        let unserved =
                            matches!(waiting.get(&job.id), Some(api::Waiting::NoPool { .. }));
                        (pool.is_some() || unserved).then_some((job, pool))
                    })
                    .map(|(job, pool)| api::QueuedJob {
                        id: job.id,
                        repository: repository.clone(),
                        profile: pool.map(|pool| pool.profile.clone()),
                        run_id: job.run_id,
                        run_attempt: job.run_attempt,
                        workflow: job.workflow.clone(),
                        name: job.name.clone(),
                        branch: job.branch.clone(),
                        url: job.url.clone(),
                        queued_at: job.created_at / 1000,
                        waiting: waiting.get(&job.id).cloned(),
                    })
            })
            .collect();
        // IDs grow with time.
        queued.sort_by_key(|job| job.id);
        queued
    }

    /// Announces the profiles, slots, budget and repositories that changed between two statuses. The events carry
    /// state, not transitions: a pause undone before the next publish announces nothing, and a client that applies
    /// the events still ends where the status is. `announced` replaces the previous profiles on the first publish;
    /// their slots are compared with nothing then, so every slot is announced.
    fn announce_changes(
        &self,
        previous: &api::Status,
        announced: Option<&[api::ProfileStatus]>,
        status: &api::Status,
    ) {
        let profiles_before = announced.unwrap_or(&previous.profiles);
        for profile in &status.profiles {
            let outline_unchanged = profiles_before
                .iter()
                .find(|candidate| candidate.name == profile.name)
                .is_some_and(|before| same_outline(before, profile));
            if !outline_unchanged {
                self.store.event(api::Event::ProfileChanged {
                    profile: profile.clone(),
                });
            }
            let before = previous
                .profiles
                .iter()
                .find(|candidate| candidate.name == profile.name);
            for slot in &profile.slots {
                let unchanged = before
                    .and_then(|before| before.slots.iter().find(|old| old.slot == slot.slot))
                    .is_some_and(|old| old == slot);
                if !unchanged {
                    self.store.event(api::Event::SlotChanged {
                        profile: profile.name.clone(),
                        slot: slot.clone(),
                    });
                }
            }
        }
        for before in profiles_before {
            if !status
                .profiles
                .iter()
                .any(|profile| profile.name == before.name)
            {
                self.store.event(api::Event::ProfileRemoved {
                    profile: before.name.clone(),
                });
            }
        }
        if previous.budget != status.budget {
            self.store.event(api::Event::BudgetChanged {
                budget: status.budget.clone(),
            });
        }
        for repository in &status.repositories {
            if !previous.repositories.contains(repository) {
                self.store.event(api::Event::RepositoryChanged {
                    repository: repository.clone(),
                });
            }
        }
        for before in &previous.repositories {
            if !status
                .repositories
                .iter()
                .any(|repository| repository.name == before.name)
            {
                self.store.event(api::Event::RepositoryRemoved {
                    repository: before.name.clone(),
                });
            }
        }
    }
}

/// Whether two statuses of a profile agree on everything but the runners in its slots, which `slot.changed` announces.
fn same_outline(before: &api::ProfileStatus, now: &api::ProfileStatus) -> bool {
    let api::ProfileStatus {
        name,
        image,
        cpus,
        memory,
        reseeding,
        slots,
    } = now;
    *name == before.name
        && *image == before.image
        && *cpus == before.cpus
        && *memory == before.memory
        && *reseeding == before.reseeding
        && slots
            .iter()
            .map(|slot| slot.slot)
            .eq(before.slots.iter().map(|slot| slot.slot))
}
