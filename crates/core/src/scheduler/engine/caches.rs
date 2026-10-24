//! Cache upkeep: reseeds idle profiles, stops the mirror, empties the shared caches and the mirror's storage over
//! their limit and trims the build cache to its own when no runner runs, and counts what the mirror served each
//! runner. Reseeds and cache checks
//! run as background tasks the tick collects; it must not start or stop runners.

use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{Engine, Occupant, Shutdown};
use crate::api;
use crate::cache::{self, Emptied, Trimmed};
use crate::config::ByteSize;
use crate::docker::{DockerApi, is_not_found};
use crate::github::GithubApi;
use crate::time;

/// How long after the shared caches' last check the next stops warm runners to run (ADR 0050). Without warm runners
/// the check runs whenever no runner runs; warm runners can keep one running all day.
const CHECK_EVERY: Duration = Duration::from_secs(3600);

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Counts what the mirror served each runner since the last count.
    pub(super) async fn count_mirror_bytes(&mut self) {
        if !self.config.caches.mirror.enabled || !self.mirror_up {
            return;
        }
        let log = match self
            .docker
            .logs_since(&self.names.mirror(), self.mirror_until.at / 1_000_000_000)
            .await
        {
            Ok(log) => log,
            Err(error) if is_not_found(&error) => return,
            Err(error) => {
                warn!("cannot read the mirror's log: {error}");
                return;
            }
        };
        let (bytes, until) = self.leases.attribute(&log, self.mirror_until);
        // Past these lines only once they are stored: if not, the next count reads them again.
        if until != self.mirror_until && self.store.mirror_bytes(bytes, until).await {
            self.mirror_until = until;
            self.leases.forget_before(until.at / 1_000_000);
        }
    }

    /// Reseeds the idle profiles whose slot 1 ran a job, once Docker has their image; with no runner running at all,
    /// stops the mirror and, unless shutting down, empties the shared caches over their limit.
    pub(super) async fn idle_caches(&mut self, mode: Shutdown) {
        if mode == Shutdown::No {
            let due: Vec<String> = self
                .reseed_due
                .union(&self.reseed_requested)
                .filter(|name| {
                    self.slots
                        .get(*name)
                        .is_some_and(|slots| slots.iter().all(Option::is_none))
                })
                // A shell mounts the snapshot a reseed replaces.
                .filter(|name| !self.shell_profiles().contains(name.as_str()))
                // The seed container runs the profile's image: a reseed waits for its pull, still due.
                .filter(|name| {
                    !self
                        .config
                        .profiles
                        .get(*name)
                        .is_some_and(|profile| self.awaits_image(&profile.image))
                })
                .cloned()
                .collect();
            for name in due {
                self.reseed_due.remove(&name);
                let requested = self.reseed_requested.remove(&name);
                self.spawn_reseed(name, requested);
            }
        }
        // A shell mounts the shared caches.
        if self.occupied().next().is_some() || !self.records.shells.is_empty() {
            return;
        }
        if self.mirror_up {
            match cache::mirror_down(&*self.docker, &self.names).await {
                Ok(()) => self.mirror_up = false,
                Err(error) => warn!("cannot stop the mirror: {error}"),
            }
        }
        // On a shutdown, left to the next start, which checks them first.
        if mode == Shutdown::No && self.trim_due && self.trim.is_none() {
            self.trim_due = false;
            self.checked_at = SystemTime::now();
            let docker = self.docker.clone();
            let names = self.names.clone();
            let caches = self.config.caches.clone();
            let mirror = if self.mirror_up {
                cache::Mirror::Up
            } else {
                cache::Mirror::Down
            };
            let build = self.build_cache.clone();
            self.trim = Some(tokio::spawn(async move {
                let mut trimmed = cache::trim_shared(&*docker, &names, &caches, mirror).await?;
                if let Some(build) = build {
                    match tokio::task::spawn_blocking(move || build.trim()).await {
                        Ok(build) => {
                            if build.after != build.before {
                                info!(
                                    "trimmed the build cache from {} to {}, its least recently used entries first",
                                    ByteSize(build.before.bytes),
                                    ByteSize(build.after.bytes)
                                );
                            }
                            trimmed.sizes.insert(names.build_cache(), build.after.bytes);
                        }
                        Err(error) => warn!("trimming the build cache stopped: {error}"),
                    }
                }
                Ok(trimmed)
            }));
        }
    }

    /// Rebuilds a profile's snapshot in the background; its slots start nothing until it is done. A `requested`
    /// reseed runs however little slot 1 changed.
    fn spawn_reseed(&mut self, name: String, requested: bool) {
        let Some(profile) = self.config.profiles.get(&name) else {
            return;
        };
        let docker = self.docker.clone();
        let names = self.names.clone();
        let image = profile.image.reference();
        let slice = self.config.host.slice.clone();
        let (slots, cpus, memory) = (profile.slots as usize, profile.cpus, profile.memory);
        let min_changes = if requested {
            ByteSize(0)
        } else {
            self.config.caches.reseed_at
        };
        let max_size = self.config.caches.snapshot_max;
        let isolation = self.isolation;
        let profile_name = name.clone();
        let task = tokio::spawn(async move {
            cache::reseed(
                &*docker,
                &cache::Reseed {
                    names: &names,
                    profile: &profile_name,
                    image: &image,
                    slots,
                    slice: &slice,
                    cpus,
                    memory,
                    min_changes,
                    max_size,
                    isolation,
                },
            )
            .await
        });
        self.reseed_memory.insert(name.clone(), memory.0);
        self.reseeds.insert(name, task);
    }

    pub(super) async fn collect_trim(&mut self) {
        if !self.trim.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let Some(task) = self.trim.take() else {
            return;
        };
        match task.await {
            Ok(Ok(Trimmed { sizes, emptied })) => {
                let ours = self.names.network();
                self.store.cache_sizes(
                    time::now_ms(),
                    sizes
                        .into_iter()
                        .filter(|(name, _)| name.starts_with(&format!("{ours}-")))
                        .collect(),
                );
                for Emptied { name, size } in emptied {
                    info!("emptied {name}, which had grown to {size}");
                }
            }
            Ok(Err(error)) => warn!("cannot check the shared caches: {error}"),
            Err(error) => warn!("checking the shared caches stopped: {error}"),
        }
    }

    pub(super) async fn collect_reseeds(&mut self) {
        let finished: Vec<String> = self
            .reseeds
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(name, _)| name.clone())
            .collect();
        for name in finished {
            self.reseed_memory.remove(&name);
            let Some(task) = self.reseeds.remove(&name) else {
                continue;
            };
            let (output, error) = match task.await {
                Ok(Ok(output)) => {
                    info!("{name}: {}", output.trim().replace('\n', "; "));
                    (output, None)
                }
                Ok(Err(error)) => {
                    warn!("{name}: reseeding failed: {error}");
                    (String::new(), Some(error.to_string()))
                }
                Err(error) => {
                    warn!("{name}: reseeding stopped: {error}");
                    (String::new(), Some(error.to_string()))
                }
            };
            self.store.event(api::Event::CacheReseeded {
                profile: name,
                output: output.trim().to_owned(),
                error,
            });
        }
        self.drop_closed_slots();
    }

    /// Whether upkeep of `profile` waits only for its runners, all idle: its reseed, or the shared caches' check
    /// (`CHECK_EVERY` after the last, or running), which waits for every profile's runners. Its warm runners then
    /// stop and none starts.
    pub(super) fn upkeep_waits_for_warm(&self, profile: &str) -> bool {
        let idle = |slots: &Vec<Option<Occupant>>| {
            slots
                .iter()
                .flatten()
                .all(|occupant| !occupant.busy && occupant.job.is_none())
        };
        let reseed = (self.reseed_due.contains(profile) || self.reseed_requested.contains(profile))
            && self.slots.get(profile).is_some_and(idle);
        // Until the check ends: a runner started meanwhile mounts the volumes it may remove.
        let check = (self.trim.is_some()
            || self.trim_due
                && self
                    .checked_at
                    .elapsed()
                    .is_ok_and(|age| age >= CHECK_EVERY))
            && self.records.shells.is_empty()
            && self.slots.values().all(idle);
        reseed || check
    }

    /// Whether a profile's snapshot is being rebuilt, or will be once its slots are free.
    pub(super) fn is_reseeding(&self, profile: &str) -> bool {
        self.reseeds.contains_key(profile) || self.reseed_requested.contains(profile)
    }
}
