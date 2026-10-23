//! Publishes `api::Status` after each tick and command, and records the profiles, slots, budget and repositories
//! that changed as events. It reads engine state and must not change it, besides the status channel.

use super::{Engine, Shutdown, unix_secs};
use crate::api;
use crate::docker::DockerApi;
use crate::github::GithubApi;

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    pub(super) fn publish(&mut self) {
        let mode = self.control.mode();
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
                            busy: occupant.busy,
                            started_at: unix_secs(occupant.started_at),
                        }),
                    })
                    .collect(),
            })
            .collect();
        let repositories = self
            .config
            .repositories
            .iter()
            .map(|repository| api::RepositoryStatus {
                name: repository.name.clone(),
                profile: repository.profile.clone(),
                labels: repository.labels.clone(),
                paused: self.control.is_paused(&repository.name),
                queued_jobs: self.repositories[&repository.name]
                    .queued
                    .iter()
                    .filter(|job| crate::scheduler::matches(job, &repository.labels))
                    .count(),
            })
            .collect();
        let previous = self.status.borrow().clone();
        let announced = self.announced_profiles.take();
        let status = api::Status {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            shutting_down: mode != Shutdown::No,
            budget: self.budget(),
            profiles,
            repositories,
        };
        self.announce_changes(&previous, announced.as_deref(), &status);
        self.status.send_replace(status);
    }

    /// The memory caps in use, with the mirror's and each seed container's part.
    fn budget(&self) -> api::Budget {
        api::Budget {
            slice: self.config.host.slice.clone(),
            memory_max: self.memory_max,
            memory_in_use: self.memory_in_use(),
            mirror_memory: self.mirror_memory_in_use(),
            seed_memory: self
                .reseed_memory
                .iter()
                .map(|(profile, memory)| api::SeedMemory {
                    profile: profile.clone(),
                    memory: *memory,
                })
                .collect(),
        }
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
