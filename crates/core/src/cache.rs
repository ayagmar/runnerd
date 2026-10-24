//! The warm caches runner containers share and keep between jobs: each profile's read-only snapshot, each slot's
//! volume (its layers' changes and its Docker Engine's data), the shared pnpm store and Maven repository, and the
//! Docker Hub mirror on the runners' network. Owns their names, mounts and lifecycle; it never decides when a
//! runner starts, and it never touches a volume a running container uses.

use std::collections::{BTreeMap, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, Mount, MountType, NetworkingConfig,
};

use crate::api;
use crate::config::{self, ByteSize};
use crate::docker::{DockerApi, Error, RunnerSpec, is_not_found};
use crate::isolation::Isolation;

/// The Docker Hub pull-through cache (`registry` 3.1.2, multi-platform index).
pub const MIRROR_IMAGE: &str =
    "registry:3.1.2@sha256:01cd312e67026f06f5d89c3e133a4a9adc7cb9eeb9b1194e0b2954dfd770efd2";

/// The host name runner containers reach the mirror at (`image/daemon.json`).
const MIRROR_ALIAS: &str = "mirror";

/// The mirror's `oom_score_adj`: the kernel picks it after the runners' processes, which keep the default 0.
const MIRROR_OOM_SCORE_ADJ: i64 = -500;

/// The names of everything runnerd creates in Docker, under one prefix: `runnerd`, or another for tests.
#[derive(Debug, Clone)]
pub struct Names {
    prefix: String,
}

impl Names {
    /// Names under `prefix`.
    pub fn new(prefix: &str) -> Names {
        Names {
            prefix: prefix.to_owned(),
        }
    }

    /// The network runners and the mirror share.
    pub fn network(&self) -> String {
        self.prefix.clone()
    }

    /// The profile's read-only snapshot.
    pub fn base(&self, profile: &str) -> String {
        format!("{}-{profile}-base", self.prefix)
    }

    /// Slot `slot` (1-based) of the profile: its layers' changes and its Docker Engine's data.
    pub fn slot(&self, profile: &str, slot: usize) -> String {
        format!("{}-{profile}-slot-{slot}", self.prefix)
    }

    /// The pnpm store all slots share.
    pub fn pnpm(&self) -> String {
        format!("{}-pnpm", self.prefix)
    }

    /// The Maven repository all slots share.
    pub fn maven(&self) -> String {
        format!("{}-maven", self.prefix)
    }

    /// The container that rebuilds the profile's snapshot.
    pub fn seed(&self, profile: &str) -> String {
        format!("{}-{profile}-seed", self.prefix)
    }

    /// The mirror's container and its storage volume.
    pub fn mirror(&self) -> String {
        format!("{}-mirror", self.prefix)
    }

    /// What `GET /v1/caches` and `runnerctl caches clean` call the shared build cache: a directory of the daemon's,
    /// not a Docker volume.
    pub fn build_cache(&self) -> String {
        format!("{}-build-cache", self.prefix)
    }
}

fn volume(source: String, target: &str, read_only: bool) -> Mount {
    Mount {
        target: Some(target.to_owned()),
        source: Some(source),
        typ: Some(MountType::VOLUME),
        read_only: Some(read_only),
        ..Default::default()
    }
}

/// What a runner container in slot `slot` (1-based) of the profile mounts (paths from `image/Dockerfile`).
pub fn runner_mounts(names: &Names, profile: &str, slot: usize) -> Vec<Mount> {
    vec![
        volume(names.base(profile), "/base", true),
        volume(names.slot(profile, slot), "/state", false),
        volume(names.pnpm(), "/shared/pnpm", false),
        volume(names.maven(), "/shared/maven", false),
    ]
}

/// The mirror container: Docker Hub through a proxy registry, which refuses pushes, so a job can only make it
/// fetch what Docker Hub serves. Content not pulled again within the TTL expires. No published port.
pub fn mirror_body(names: &Names, mirror: &config::Mirror, slice: &str) -> ContainerCreateBody {
    let memory = i64::try_from(mirror.memory.0).unwrap_or(i64::MAX);
    ContainerCreateBody {
        image: Some(MIRROR_IMAGE.to_owned()),
        env: Some(vec![
            "REGISTRY_PROXY_REMOTEURL=https://registry-1.docker.io".to_owned(),
            format!("REGISTRY_PROXY_TTL={}s", mirror.ttl.as_secs()),
            // Expiry deletes content.
            "REGISTRY_STORAGE_DELETE_ENABLED=true".to_owned(),
        ]),
        labels: Some(HashMap::from([(
            "runnerd.mirror".to_owned(),
            String::new(),
        )])),
        host_config: Some(HostConfig {
            cgroup_parent: Some(slice.to_owned()),
            memory: Some(memory),
            memory_swap: Some(memory),
            nano_cpus: Some(i64::from(mirror.cpus) * 1_000_000_000),
            // When the slice runs out of memory, which measured admission allows, the kernel kills a job's process
            // before the mirror every runner pulls through.
            oom_score_adj: Some(MIRROR_OOM_SCORE_ADJ),
            mounts: Some(vec![volume(names.mirror(), "/var/lib/registry", false)]),
            network_mode: Some(names.network()),
            ..Default::default()
        }),
        networking_config: Some(NetworkingConfig {
            endpoints_config: Some(HashMap::from([(
                names.network(),
                EndpointSettings {
                    aliases: Some(vec![MIRROR_ALIAS.to_owned()]),
                    ..Default::default()
                },
            )])),
        }),
        ..Default::default()
    }
}

/// Creates what a profile's runner in `slot` (1-based) mounts and joins, if missing.
pub async fn prepare(
    docker: &impl DockerApi,
    names: &Names,
    profile: &str,
    slot: usize,
) -> Result<(), Error> {
    docker.ensure_network(&names.network()).await?;
    for name in [
        names.base(profile),
        names.slot(profile, slot),
        names.pnpm(),
        names.maven(),
    ] {
        docker.ensure_volume(&name).await?;
    }
    Ok(())
}

/// Starts the mirror unless it runs.
pub async fn mirror_up(
    docker: &impl DockerApi,
    names: &Names,
    mirror: &config::Mirror,
    slice: &str,
) -> Result<(), Error> {
    match docker.inspect(&names.mirror()).await {
        Ok(state) if state.running => return Ok(()),
        // Stopped on its own: started again from scratch, its storage is the volume.
        Ok(_) => docker.remove(&names.mirror()).await?,
        Err(error) if is_not_found(&error) => {}
        Err(error) => return Err(error),
    }
    docker.ensure_network(&names.network()).await?;
    if !docker.image_present(MIRROR_IMAGE).await? {
        docker.pull(MIRROR_IMAGE).await?;
    }
    docker
        .run(&names.mirror(), mirror_body(names, mirror, slice))
        .await?;
    Ok(())
}

/// Stops the mirror; its storage stays.
pub async fn mirror_down(docker: &impl DockerApi, names: &Names) -> Result<(), Error> {
    match docker.remove(&names.mirror()).await {
        Err(error) if is_not_found(&error) => Ok(()),
        other => other,
    }
}

/// Whether the mirror's container is down, so its storage can be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mirror {
    /// It runs, or may: an earlier daemon may have left it up.
    Up,
    /// It is stopped.
    Down,
}

/// What `trim_shared` measured and emptied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trimmed {
    /// Every volume's size in bytes, by name, as measured before emptying any.
    pub sizes: BTreeMap<String, u64>,
    /// The shared caches it emptied.
    pub emptied: Vec<Emptied>,
    /// Docker's free disk in bytes, as measured before emptying any; `None` when it could not be measured.
    pub free: Option<u64>,
}

/// A shared cache `trim_shared` emptied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emptied {
    /// The volume.
    pub name: String,
    /// How large it had grown.
    pub size: ByteSize,
}

/// Empties the shared caches that outgrew their limit, the mirror's storage among them once the mirror is down
/// (`Mirror::Down`); call only while no runner runs.
pub async fn trim_shared(
    docker: &impl DockerApi,
    names: &Names,
    caches: &config::Caches,
    mirror: Mirror,
) -> Result<Trimmed, Error> {
    let sizes = docker.volume_sizes().await?;
    let free = docker.free_space().await.ok();
    let mut emptied = Vec::new();
    let mut limits = vec![
        (names.pnpm(), caches.pnpm.max_size),
        (names.maven(), caches.maven.max_size),
    ];
    if mirror == Mirror::Down {
        limits.push((names.mirror(), caches.mirror.max_size));
    }
    for (name, limit) in limits {
        if let Some(&size) = sizes.get(&name)
            && size > limit.0
        {
            docker.remove_volume(&name).await?;
            emptied.push(Emptied {
                name,
                size: ByteSize(size),
            });
        }
    }
    Ok(Trimmed {
        sizes,
        emptied,
        free,
    })
}

/// What the slots' Docker Engines may keep together, in bytes: a quarter of Docker's disk not otherwise used, which
/// is what is free plus what the slot volumes already hold, measured together (docs/adr/0054).
pub fn docker_disk(
    names: &Names,
    config: &config::Config,
    free: u64,
    sizes: &BTreeMap<String, u64>,
) -> u64 {
    let held: u64 = config
        .profiles
        .iter()
        .flat_map(|(profile, settings)| {
            (1..=settings.slots as usize).map(move |slot| names.slot(profile, slot))
        })
        .filter_map(|name| sizes.get(&name))
        .sum();
    free.saturating_add(held) / DOCKER_DISK_SHARE
}

/// One slot's share of `disk` among `slots`, in whole GiB as `image/layers.sh` takes it: at least 1, at most
/// `DOCKER_SLOT_MAX_GIB`.
pub fn docker_budget_gib(disk: u64, slots: u64) -> u64 {
    ((disk / slots.max(1)) >> 30).clamp(1, DOCKER_SLOT_MAX_GIB)
}

/// The slots' Docker Engines together keep at most one part in this of Docker's disk not otherwise used.
const DOCKER_DISK_SHARE: u64 = 4;

/// The most one slot's Docker Engine keeps, in GiB: the image's own default, for a daemon that sets none.
const DOCKER_SLOT_MAX_GIB: u64 = 20;

/// Every cache volume the config's profiles use, what it holds and its limit, with size 0.
pub fn volumes(names: &Names, config: &config::Config) -> Vec<api::CacheVolume> {
    let volume = |name: String, kind: api::CacheKind, limit: Option<u64>| api::CacheVolume {
        name,
        kind,
        bytes: 0,
        limit,
    };
    let mut volumes = Vec::new();
    for (profile, settings) in &config.profiles {
        volumes.push(volume(
            names.base(profile),
            api::CacheKind::Snapshot {
                profile: profile.clone(),
            },
            Some(config.caches.snapshot_max.0),
        ));
        for slot in 1..=settings.slots as usize {
            volumes.push(volume(
                names.slot(profile, slot),
                api::CacheKind::Slot {
                    profile: profile.clone(),
                    slot,
                },
                None,
            ));
        }
    }
    volumes.push(volume(
        names.pnpm(),
        api::CacheKind::Pnpm,
        Some(config.caches.pnpm.max_size.0),
    ));
    volumes.push(volume(
        names.maven(),
        api::CacheKind::Maven,
        Some(config.caches.maven.max_size.0),
    ));
    if config.caches.mirror.enabled {
        volumes.push(volume(
            names.mirror(),
            api::CacheKind::Mirror,
            Some(config.caches.mirror.max_size.0),
        ));
    }
    if config.caches.build.enabled {
        volumes.push(volume(
            names.build_cache(),
            api::CacheKind::Build,
            Some(config.caches.build.max_size.0),
        ));
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    volumes
}

/// What `reseed` runs on: the profile's image and limits, its slots and the isolation backend.
#[derive(Debug)]
pub struct Reseed<'profile> {
    /// Where the profile's volumes are.
    pub names: &'profile Names,
    /// The profile to reseed.
    pub profile: &'profile str,
    /// The profile's runner image, which holds the seed script.
    pub image: &'profile str,
    /// How many slots the profile has; each slot's layer changes are dropped.
    pub slots: usize,
    /// The systemd slice the seed container runs in.
    pub slice: &'profile str,
    /// The profile's CPUs.
    pub cpus: u32,
    /// The profile's memory.
    pub memory: ByteSize,
    /// Slot 1's layers must have changed at least this much (`caches.reseed_at`).
    pub min_changes: ByteSize,
    /// A snapshot that would outgrow this starts over (`caches.snapshot_max`).
    pub max_size: ByteSize,
    /// How the seed container is isolated, as runners are.
    pub isolation: &'profile dyn Isolation,
}

/// Makes slot 1's layers the profile's new snapshot and drops every slot's own layer changes (`image/seed.sh`),
/// if slot 1 changed at least `min_changes`. A snapshot that would outgrow `max_size` starts over from slot 1's
/// changes alone, or from the image when those outgrow it too. Call only while none of the profile's slots runs. Returns the seed
/// container's output.
pub async fn reseed(docker: &impl DockerApi, reseed: &Reseed<'_>) -> Result<String, Error> {
    let names = reseed.names;
    let mut mounts = vec![volume(names.base(reseed.profile), "/base", false)];
    mounts.extend((1..=reseed.slots).map(|slot| {
        volume(
            names.slot(reseed.profile, slot),
            &format!("/workers/{slot}"),
            false,
        )
    }));
    for mount in &mounts {
        if let Some(source) = &mount.source {
            docker.ensure_volume(source).await?;
        }
    }
    let name = names.seed(reseed.profile);
    let spec = RunnerSpec {
        name: name.clone(),
        image: reseed.image.to_owned(),
        profile: reseed.profile.to_owned(),
        slice: reseed.slice.to_owned(),
        cpus: reseed.cpus,
        memory: reseed.memory,
        env: Vec::new(),
        mounts,
        network: Some("none".to_owned()),
        labels: BTreeMap::from([("runnerd.seed".to_owned(), String::new())]),
        entrypoint: Some(vec![
            "/usr/local/sbin/runnerd-seed".to_owned(),
            reseed.min_changes.0.to_string(),
            reseed.max_size.0.to_string(),
        ]),
        jit_config: None,
    };
    // A seed container left by a daemon that died while seeding.
    match docker.remove(&name).await {
        Err(error) if !is_not_found(&error) => return Err(error),
        _ => {}
    }
    let id = docker.start(&spec, reseed.isolation).await?;
    finish_reseed(docker, &id).await
}

/// Waits for a seed container to exit, removes it, and returns its output. Also takes over a seed container an
/// earlier daemon left running: stopping it between its `rm` and `mv` would lose that snapshot layer.
pub async fn finish_reseed(docker: &impl DockerApi, container: &str) -> Result<String, Error> {
    let result = async {
        let code = docker.wait(container).await?;
        let output = docker.logs(container).await?;
        if code == 0 {
            Ok(output)
        } else {
            Err(Error::DockerContainerWaitError {
                error: format!("seeding exited {code}: {}", output.trim()),
                code,
            })
        }
    }
    .await;
    let removed = docker.remove(container).await;
    let output = result?;
    removed?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::docker_budget_gib;

    #[test]
    fn a_slots_docker_budget_is_its_share_between_1_and_20_gib() {
        assert_eq!(docker_budget_gib(100 << 30, 10), 10);
        assert_eq!(
            docker_budget_gib(1 << 40, 4),
            20,
            "a large disk still stops at 20 GiB"
        );
        assert_eq!(
            docker_budget_gib(1 << 30, 13),
            1,
            "a full disk still keeps 1 GiB"
        );
        assert_eq!(docker_budget_gib(1 << 30, 0), 1);
    }
}
