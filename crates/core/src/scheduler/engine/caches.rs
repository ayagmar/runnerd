//! Cache upkeep: measures what slot 1 changed, reseeds idle profiles, stops the mirror, empties the shared caches and the mirror's storage over
//! their limit and trims the build cache to its own when no runner runs, and counts what the mirror served each
//! runner. Measures, reseeds and cache checks run as background tasks the tick collects; the build cache's trim runs before the
//! tick goes on, as no runner may start during it. It must not start or stop runners.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{Engine, Occupant, Shutdown};
use crate::admission::host;
use crate::api;
use crate::buildcache::BuildCache;
use crate::cache::{self, Emptied, Measured, Refused, Trimmed};
use crate::config::ByteSize;
use crate::docker::{DockerApi, is_not_found};
use crate::github::GithubApi;
use crate::time;

/// How long after the shared caches' last check the next stops warm runners to run (ADR 0050). Without warm runners
/// the check runs whenever no runner runs; warm runners can keep one running all day.
const CHECK_EVERY: Duration = Duration::from_secs(3600);

/// How long after a profile's last measure of slot 1 the next may start (ADR 0094). A profile whose slot 1 runs a
/// job a minute would otherwise run a measure container as often, most of them to find what the last one found.
pub(super) const MEASURE_EVERY: Duration = Duration::from_secs(600);

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

    /// Measures slot 1 of the profiles whose slot 1 ran a job, and reseeds the idle profiles due to reseed, once Docker
    /// has their image; with no runner running at all, stops the mirror and, unless shutting down, empties the shared
    /// caches over their limit.
    pub(super) async fn idle_caches(&mut self, mode: Shutdown) {
        if mode == Shutdown::No {
            self.start_measures();
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
                // One that measures slot 1 would read layers the reseed drops: it ends in seconds.
                .filter(|name| !self.measures.contains_key(*name))
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
                // The seed container runs at its profile's cap: a reseed waits for room in the slice and on the
                // host, still due, as a runner does.
                if !self.fits_seed(&name) {
                    continue;
                }
                self.reseed_due.remove(&name);
                // The reseed weighs slot 1's changes itself.
                self.measure_due.remove(&name);
                let requested = self.reseed_requested.remove(&name);
                let oversized = self.oversized.remove(&name);
                self.spawn_reseed(name, requested || oversized);
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
            // A reseed mounts its profile's slot volumes.
            let slots: BTreeMap<String, usize> = self
                .config
                .profiles
                .iter()
                .filter(|(name, _)| !self.reseeds.contains_key(*name))
                .map(|(name, profile)| (name.clone(), profile.slots as usize))
                .collect();
            // Here, not in the background task: runners start only in the tick, so none starts while it removes
            // entries. Commands are answered meanwhile; none starts a runner.
            let build = match self.build_cache.clone() {
                Some(build) => self.serving(trim_build_cache(build)).await,
                None => None,
            };
            self.trim = Some(tokio::spawn(async move {
                let mut trimmed =
                    cache::trim_shared(&*docker, &names, &caches, mirror, &slots).await?;
                if let Some(bytes) = build {
                    trimmed.sizes.insert(names.build_cache(), bytes);
                }
                Ok(trimmed)
            }));
        }
    }

    /// Measures slot 1 of each profile whose slot 1 ran a job since its last measure (`cache::measure`), once
    /// `MEASURE_EVERY` passed since that one, while slot 1 runs no job and Docker has the profile's image. It holds
    /// no slot and stops no runner: its container reads slot 1 and the snapshot read-only. Not for a profile already
    /// due to reseed, or reseeding.
    fn start_measures(&mut self) {
        let now = tokio::time::Instant::now();
        let due: Vec<String> = self
            .measure_due
            .iter()
            .filter(|name| {
                !self.reseed_due.contains(*name)
                    && !self.reseed_requested.contains(*name)
                    && !self.reseeds.contains_key(*name)
                    && !self.measures.contains_key(*name)
                    && self
                        .measured_at
                        .get(*name)
                        .is_none_or(|at| now.duration_since(*at) >= MEASURE_EVERY)
            })
            // Mid-job, it would measure what the job is still changing.
            .filter(|name| {
                self.slots
                    .get(*name)
                    .and_then(|slots| slots.first())
                    .is_some_and(|slot| {
                        slot.as_ref()
                            .is_none_or(|occupant| !occupant.busy && occupant.job.is_none())
                    })
            })
            .filter(|name| {
                self.config
                    .profiles
                    .get(*name)
                    .is_some_and(|profile| !self.awaits_image(&profile.image))
            })
            .cloned()
            .collect();
        for name in due {
            let Some(profile) = self.config.profiles.get(&name) else {
                continue;
            };
            let docker = self.docker.clone();
            let names = self.names.clone();
            let image = profile.image.reference();
            let slice = self.config.host.slice.clone();
            let isolation = self.isolation;
            let profile_name = name.clone();
            let task = tokio::spawn(async move {
                cache::measure(
                    &*docker,
                    &cache::Measure {
                        names: &names,
                        profile: &profile_name,
                        image: &image,
                        slice: &slice,
                        isolation,
                    },
                )
                .await
            });
            self.measure_due.remove(&name);
            self.measured_at.insert(name.clone(), now);
            self.measures.insert(name, task);
        }
    }

    /// Takes in the measures of slot 1 that ended: a profile whose slot 1 changed `reseed_at` or more, whose last
    /// reseed did not finish, or whose measure failed, is due to reseed, which its upkeep then stops its runners for.
    /// One that changed less keeps them: a reseed would change nothing.
    pub(super) async fn collect_measures(&mut self) {
        let finished: Vec<String> = self
            .measures
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(name, _)| name.clone())
            .collect();
        let reseed_at = self.config.caches.reseed_at;
        for name in finished {
            let Some(task) = self.measures.remove(&name) else {
                continue;
            };
            let measured = match task.await {
                Ok(measured) => measured.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            // A reload removed the profile meanwhile.
            if !self.config.profiles.contains_key(&name) {
                continue;
            }
            match measured {
                Ok(Measured::Changed(bytes)) if bytes < reseed_at.0 => {
                    info!(
                        "{name}: slot 1 changed {}, under reseed_at ({reseed_at}): no reseed",
                        ByteSize(bytes)
                    );
                    continue;
                }
                Ok(Measured::Changed(bytes)) => info!(
                    "{name}: slot 1 changed {}, reseed_at ({reseed_at}) or more: reseeding once its runners are idle",
                    ByteSize(bytes)
                ),
                Ok(Measured::Unfinished) => info!(
                    "{name}: its last reseed did not finish: reseeding once its runners are idle"
                ),
                Err(error) => warn!(
                    "{name}: cannot measure what slot 1 changed, so its runners stop for a reseed that does: {error}"
                ),
            }
            self.reseed_due.insert(name);
        }
    }

    /// Whether `profile`'s seed container, at its profile's cap, fits beside what else runs: in the slice's
    /// `MemoryMax` and in what the host leaves the slice (ADR 0055), and not while measured admission holds the
    /// slice at its high-water mark. The profile's own runners, which stop before it starts, and the room its
    /// waiting seed holds count as free. The host's limit is `waiting_seed`'s, without the larger margin of a short
    /// host: the room it holds keeps other jobs waiting for the host and so the host short.
    fn fits_seed(&self, profile: &str) -> bool {
        let memory = self.config.profiles.get(profile).map_or(0, |p| p.memory.0);
        let runners: u64 = self
            .slots
            .get(profile)
            .into_iter()
            .flatten()
            .flatten()
            .map(|occupant| self.takes(occupant))
            .sum();
        let own = runners.saturating_add(self.waiting_seed(profile).unwrap_or(0));
        let in_use = self.memory_in_use().saturating_sub(own);
        self.memory_max
            .is_none_or(|max| max.saturating_sub(in_use) >= memory)
            && self.host_budget().is_none_or(|host| {
                host::left(host::limit(&host.memory, false), in_use, host.stalled)
                    >= i64::try_from(memory).unwrap_or(i64::MAX)
            })
            && !self.admitting(&mut [], None).held
    }

    /// The room a requested reseed holds while it waits for it once its profile drained: its profile's cap, so
    /// later starts leave that room and the seed starts as runners finish, rather than hold the profile's jobs for
    /// as long as other profiles' take what frees. `None` until then, while a shell holds it back, once it runs, and
    /// while the cap would not fit even with no runner running: the room would then hold every start for a seed that
    /// cannot start, which instead waits as a due one does.
    fn waiting_seed(&self, profile: &str) -> Option<u64> {
        let drained = self
            .slots
            .get(profile)
            .is_some_and(|slots| slots.iter().all(Option::is_none));
        let waits = self.reseed_requested.contains(profile)
            && !self.reseeds.contains_key(profile)
            && drained
            && !self.shell_profiles().contains(profile);
        let memory = self.config.profiles.get(profile)?.memory.0;
        // Not `memory_in_use`, which counts this room. The host's limit as the first runner into an empty slice
        // sees it, without the larger margin of a short or stalling host, which passes.
        let others = self.reseed_memory.values().sum::<u64>()
            + self.shell_memory()
            + self.mirror_memory_in_use();
        let fits = self
            .memory_max
            .is_none_or(|max| max.saturating_sub(others) >= memory)
            && self
                .host_memory()
                .is_none_or(|host| host::limit(&host, false).saturating_sub(others) >= memory);
        (waits && fits).then_some(memory)
    }

    /// The memory each seed container holds of the budget, by profile: a running one's cap as it started, and the
    /// room a requested reseed holds while it waits (`waiting_seed`).
    pub(super) fn seed_memory(&self) -> BTreeMap<String, u64> {
        let mut seeds = self.reseed_memory.clone();
        for profile in &self.reseed_requested {
            if let Some(memory) = self.waiting_seed(profile) {
                seeds.entry(profile.clone()).or_insert(memory);
            }
        }
        seeds
    }

    /// Rebuilds a profile's snapshot in the background; its slots start nothing until it is done. A `requested`
    /// reseed, or that of a snapshot over its limit, runs however little slot 1 changed.
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
            Ok(Ok(Trimmed {
                sizes,
                emptied,
                closed,
                refused,
                free,
            })) => {
                self.mark_oversized(&sizes);
                if let Some(free) = free {
                    self.docker_disk.send_replace(Some(cache::docker_disk(
                        &self.names,
                        &self.config,
                        free,
                        &sizes,
                    )));
                }
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
                for Emptied { name, size } in closed {
                    info!("removed {name} ({size}): its profile has fewer slots now");
                }
                for Refused { name, error } in refused {
                    warn!("cannot remove {name}, tried again at the next check: {error}");
                }
            }
            Ok(Err(error)) => warn!("cannot check the shared caches: {error}"),
            Err(error) => warn!("checking the shared caches stopped: {error}"),
        }
    }

    /// Starts measuring what the slots' Docker Engines may keep (`cache::docker_disk`), for the runners that start
    /// before the first check of the caches, which waits for no runner to run: after a restart that adopted some,
    /// that can be long. Docker measures every volume, which takes minutes on a host whose slots hold Docker stores,
    /// so it runs in the background; until it ends, each image keeps its own default.
    pub(super) fn measure_docker_disk(&mut self) {
        let docker = self.docker.clone();
        self.disk = Some(tokio::spawn(async move {
            Ok(Disk {
                free: docker.free_space().await?,
                sizes: docker.volume_sizes().await?,
            })
        }));
    }

    /// Takes in the measure `measure_docker_disk` started, once it ended. A check of the caches since measured
    /// later, so it wins.
    pub(super) async fn collect_disk(&mut self) {
        if !self.disk.as_ref().is_some_and(JoinHandle::is_finished) {
            return;
        }
        let Some(task) = self.disk.take() else {
            return;
        };
        match task.await {
            Ok(Ok(Disk { free, sizes })) => {
                if self.docker_disk.borrow().is_none() {
                    self.docker_disk.send_replace(Some(cache::docker_disk(
                        &self.names,
                        &self.config,
                        free,
                        &sizes,
                    )));
                }
            }
            Ok(Err(error)) => warn!(
                "cannot measure Docker's disk, so runners keep their image's Docker budget: {error}"
            ),
            Err(error) => warn!("measuring Docker's disk stopped: {error}"),
        }
    }

    /// Makes the profiles whose snapshot `sizes` measured over `snapshot_max` due for a reseed, which then starts
    /// the snapshot over (docs/adr/0041): slot 1 may never change enough to reach `reseed_at`. A reseed running now,
    /// or one that finished after the check started, may have replaced the snapshot after it was measured.
    fn mark_oversized(&mut self, sizes: &BTreeMap<String, u64>) {
        let max = self.config.caches.snapshot_max;
        for profile in self.config.profiles.keys() {
            let Some(&size) = sizes.get(&self.names.base(profile)) else {
                continue;
            };
            let reseeded = self
                .reseeded_at
                .get(profile)
                .is_some_and(|&at| at >= self.checked_at);
            if size > max.0 && !self.reseeds.contains_key(profile) && !reseeded {
                info!(
                    "{profile}: the snapshot holds {}, over snapshot_max ({max}): it starts over at the next reseed, \
                     once the profile is idle",
                    ByteSize(size)
                );
                self.reseed_due.insert(profile.clone());
                self.oversized.insert(profile.clone());
            }
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
            self.reseeded_at.insert(name.clone(), SystemTime::now());
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
            // A reseed that made a snapshot dropped every slot's layer, so none holds a job's caches; one that failed
            // may have dropped some.
            if error.is_some() || cache::seeded(&output) {
                self.ran.retain(|slot, _| slot.profile != name);
            }
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
        // A shell holds back the reseed until it closes, so runners keep serving meanwhile. So does a due reseed
        // with no room for its seed even once they stop: it is not asked for, and may wait long.
        let reseed = (self.reseed_requested.contains(profile)
            || self.reseed_due.contains(profile) && self.fits_seed(profile))
            && !self.shell_profiles().contains(profile)
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

/// Trims the build cache to its limit off the engine's loop; returns its bytes after, or `None` if the trim stopped.
async fn trim_build_cache(build: BuildCache) -> Option<u64> {
    match tokio::task::spawn_blocking(move || build.trim()).await {
        Ok(build) => {
            if build.after != build.before {
                info!(
                    "trimmed the build cache from {} to {}, its least recently used entries first",
                    ByteSize(build.before.bytes),
                    ByteSize(build.after.bytes)
                );
            }
            Some(build.after.bytes)
        }
        Err(error) => {
            warn!("trimming the build cache stopped: {error}");
            None
        }
    }
}

/// Docker's free disk and the volumes' sizes, as `measure_docker_disk` read them.
#[derive(Debug)]
pub(super) struct Disk {
    free: u64,
    sizes: BTreeMap<String, u64>,
}
