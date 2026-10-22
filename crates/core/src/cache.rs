//! The warm caches runner containers share and keep between jobs: each profile's read-only snapshot, each slot's
//! volume (its layers' changes and its Docker Engine's data), the shared pnpm store and Maven repository, and the
//! Docker Hub mirror on the runners' network. Owns their names, mounts and lifecycle; it never decides when a
//! runner starts, and it never touches a volume a running container uses.

use std::collections::{BTreeMap, HashMap};

use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, Mount, MountType, NetworkingConfig,
};

use crate::config::{self, ByteSize};
use crate::docker::{DockerApi, Error, RunnerSpec, is_not_found};
use crate::isolation::Isolation;

/// The Docker Hub pull-through cache (`registry` 3.1.2, multi-platform index).
pub const MIRROR_IMAGE: &str =
    "registry:3.1.2@sha256:01cd312e67026f06f5d89c3e133a4a9adc7cb9eeb9b1194e0b2954dfd770efd2";

/// The host name runner containers reach the mirror at (`image/daemon.json`).
const MIRROR_ALIAS: &str = "mirror";

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
    docker.ensure_image(MIRROR_IMAGE).await?;
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

/// Empties the shared caches that outgrew their limit; call only while no runner runs. Returns what it emptied
/// and how large it was.
pub async fn trim_shared(
    docker: &impl DockerApi,
    names: &Names,
    caches: &config::Caches,
) -> Result<Vec<(String, ByteSize)>, Error> {
    let sizes = docker.volume_sizes().await?;
    let mut emptied = Vec::new();
    for (name, limit) in [
        (names.pnpm(), caches.pnpm.max_size),
        (names.maven(), caches.maven.max_size),
    ] {
        if let Some(&size) = sizes.get(&name)
            && size > limit.0
        {
            docker.remove_volume(&name).await?;
            emptied.push((name, ByteSize(size)));
        }
    }
    Ok(emptied)
}

/// What `reseed` runs on: the profile's image and limits, its slots and the isolation backend.
pub struct Reseed<'a> {
    pub names: &'a Names,
    pub profile: &'a str,
    pub image: &'a str,
    pub slots: usize,
    pub slice: &'a str,
    pub cpus: u32,
    pub memory: ByteSize,
    /// Slot 1's layers must have changed at least this much (`caches.reseed_at`).
    pub min_changes: ByteSize,
    pub isolation: &'a dyn Isolation,
}

/// Makes slot 1's layers the profile's new snapshot and drops every slot's own layer changes (`image/seed.sh`),
/// if slot 1 changed at least `min_changes`. Call only while none of the profile's slots runs. Returns the seed
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
