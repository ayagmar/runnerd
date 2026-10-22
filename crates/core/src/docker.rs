//! Runner containers through the Docker Engine API.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, NetworkCreateRequest, SystemInfo, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, DataUsageOptionsBuilder,
    ListContainersOptionsBuilder, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveVolumeOptionsBuilder, UploadToContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures_util::{StreamExt, TryStreamExt};

use crate::config::ByteSize;
use crate::isolation::Isolation;

pub use bollard::errors::Error;

/// One runner container: a profile's image stage with the profile's limits.
#[derive(Debug, Clone)]
pub struct RunnerSpec {
    /// The container's name; for a runner, also the runner's.
    pub name: String,
    /// The image, such as `runnerd-runner:base`.
    pub image: String,
    /// The profile, put on the `runnerd.profile` label.
    pub profile: String,
    /// The systemd slice the container is placed in.
    pub slice: String,
    /// CPUs the container may use.
    pub cpus: u32,
    /// Memory the container may use, swap included.
    pub memory: ByteSize,
    /// `NAME=value` environment variables.
    pub env: Vec<String>,
    /// Volumes to mount.
    pub mounts: Vec<Mount>,
    /// The network the container joins; `None` is Docker's default bridge.
    pub network: Option<String>,
    /// Labels on top of `runnerd.profile` and `runnerd.isolation`.
    pub labels: BTreeMap<String, String>,
    /// Replaces the image's entrypoint; `None` runs the runner.
    pub entrypoint: Option<Vec<String>>,
    /// The runner's just-in-time config, which the entrypoint reads from `JIT_CONFIG_PATH`.
    pub jit_config: Option<JitConfig>,
}

/// Where `start` puts the just-in-time config in the container: a root-only file, never the create request,
/// which Docker stores and clients may log.
pub const JIT_CONFIG_PATH: &str = "/run/runnerd/jitconfig";

/// A runner's just-in-time config from GitHub: credentials, never printed.
#[derive(Clone)]
pub struct JitConfig(pub String);

impl fmt::Debug for JitConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JitConfig(<redacted>)")
    }
}

/// What `inspect` reports about a runner container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerState {
    /// Whether the container runs.
    pub running: bool,
    /// Its exit code, once it stopped.
    pub exit_code: Option<i64>,
    /// True under `unsafe-dind`: a job in the container can take over the host.
    pub privileged: bool,
    /// The runtime Docker runs it with, such as `sysbox-runc`.
    pub runtime: Option<String>,
}

/// The create-container request for a runner container under an isolation backend.
pub fn create_body(spec: &RunnerSpec, isolation: &dyn Isolation) -> ContainerCreateBody {
    // Clamped to 8 EiB, the most Docker takes.
    let memory = i64::try_from(spec.memory.0).unwrap_or(i64::MAX);
    let mut host_config = HostConfig {
        cgroup_parent: Some(spec.slice.clone()),
        memory: Some(memory),
        // Swap equal to memory: the container gets no swap on top of its memory.
        memory_swap: Some(memory),
        nano_cpus: Some(i64::from(spec.cpus) * 1_000_000_000),
        init: Some(true),
        mounts: Some(spec.mounts.clone()),
        network_mode: spec.network.clone(),
        ..Default::default()
    };
    isolation.configure(&mut host_config);
    let mut labels: HashMap<String, String> = spec.labels.clone().into_iter().collect();
    labels.insert("runnerd.profile".to_owned(), spec.profile.clone());
    labels.insert("runnerd.isolation".to_owned(), isolation.name().to_owned());
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        env: Some(spec.env.clone()),
        entrypoint: spec.entrypoint.clone(),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// A container `list` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The Docker container ID.
    pub id: String,
    /// Without Docker's leading `/`.
    pub name: String,
    /// Whether it runs.
    pub running: bool,
    /// Its labels.
    pub labels: HashMap<String, String>,
}

/// Whether a Docker error says the container, volume or image does not exist.
pub fn is_not_found(error: &Error) -> bool {
    matches!(
        error,
        Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// The Docker Engine calls the scheduler and the caches make. `Docker` is the real one; the engine's tests use a
/// fake.
pub trait DockerApi: Send + Sync {
    /// Creates and starts a runner container; returns its ID.
    fn start(
        &self,
        spec: &RunnerSpec,
        isolation: &dyn Isolation,
    ) -> impl Future<Output = Result<String, Error>> + Send;
    /// A container's state and isolation.
    fn inspect(&self, container: &str) -> impl Future<Output = Result<RunnerState, Error>> + Send;
    /// Removes a runner container, stopping it first if it runs.
    fn remove(&self, container: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Creates and starts a container that is not a runner, such as the mirror; returns its ID.
    fn run(
        &self,
        name: &str,
        body: ContainerCreateBody,
    ) -> impl Future<Output = Result<String, Error>> + Send;
    /// Every container, running or not, that has the label.
    fn list(&self, label: &str) -> impl Future<Output = Result<Vec<Container>, Error>> + Send;
    /// Waits for a container to exit; returns its exit code.
    fn wait(&self, container: &str) -> impl Future<Output = Result<i64, Error>> + Send;
    /// A container's output so far, stdout and stderr interleaved.
    fn logs(&self, container: &str) -> impl Future<Output = Result<String, Error>> + Send;
    /// Pulls an image unless it is present.
    fn ensure_image(&self, image: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Creates a bridge network unless it exists.
    fn ensure_network(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Creates a volume unless it exists.
    fn ensure_volume(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Removes a volume; a volume that does not exist is not an error.
    fn remove_volume(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// The disk size of every volume Docker knows, by name. Slow: Docker measures each volume.
    fn volume_sizes(&self) -> impl Future<Output = Result<BTreeMap<String, u64>, Error>> + Send;
}

/// The local Docker Engine.
#[derive(Debug)]
pub struct Docker {
    api: bollard::Docker,
}

impl Docker {
    /// Connects to the Docker Engine at `DOCKER_HOST`, or its local socket. Only `unix://` hosts are supported.
    pub fn connect() -> Result<Docker, Error> {
        connect_to(std::env::var("DOCKER_HOST").ok().as_deref())
    }

    /// The Engine's system information.
    pub async fn info(&self) -> Result<SystemInfo, Error> {
        self.api.info().await
    }
}

impl DockerApi for Docker {
    async fn start(&self, spec: &RunnerSpec, isolation: &dyn Isolation) -> Result<String, Error> {
        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let id = self
            .api
            .create_container(Some(options), create_body(spec, isolation))
            .await?
            .id;
        let started = async {
            if let Some(jit_config) = &spec.jit_config {
                let options = UploadToContainerOptionsBuilder::new().path("/").build();
                let tar = jit_config_tar(jit_config)?;
                // As a stream: bollard traces a full body's bytes, but a stream only as "(stream)".
                let tar = futures_util::stream::once(std::future::ready(tar.into()));
                self.api
                    .upload_to_container(&id, Some(options), bollard::body_stream(tar))
                    .await?;
            }
            self.api.start_container(&id, None).await
        };
        if let Err(error) = started.await {
            // Best effort: the start error is the one worth reporting.
            let _ = self.remove(&id).await;
            return Err(error);
        }
        Ok(id)
    }

    async fn inspect(&self, container: &str) -> Result<RunnerState, Error> {
        let response = self.api.inspect_container(container, None).await?;
        let state = response.state.unwrap_or_default();
        let host_config = response.host_config.unwrap_or_default();
        Ok(RunnerState {
            running: state.running.unwrap_or(false),
            exit_code: state.exit_code,
            privileged: host_config.privileged.unwrap_or(false),
            runtime: host_config.runtime,
        })
    }

    async fn remove(&self, container: &str) -> Result<(), Error> {
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        self.api.remove_container(container, Some(options)).await
    }

    async fn run(&self, name: &str, body: ContainerCreateBody) -> Result<String, Error> {
        let options = CreateContainerOptionsBuilder::new().name(name).build();
        let id = self.api.create_container(Some(options), body).await?.id;
        if let Err(error) = self.api.start_container(&id, None).await {
            let _ = self.remove(&id).await;
            return Err(error);
        }
        Ok(id)
    }

    async fn list(&self, label: &str) -> Result<Vec<Container>, Error> {
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&HashMap::from([("label", vec![label])]))
            .build();
        let containers = self.api.list_containers(Some(options)).await?;
        Ok(containers
            .into_iter()
            .map(|container| Container {
                id: container.id.unwrap_or_default(),
                name: container
                    .names
                    .and_then(|names| names.into_iter().next())
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned(),
                running: container.state
                    == Some(bollard::models::ContainerSummaryStateEnum::RUNNING),
                labels: container.labels.unwrap_or_default(),
            })
            .collect())
    }

    async fn wait(&self, container: &str) -> Result<i64, Error> {
        let options = WaitContainerOptionsBuilder::new()
            .condition("not-running")
            .build();
        let mut responses = self.api.wait_container(container, Some(options));
        match responses.next().await {
            Some(Ok(response)) => Ok(response.status_code),
            // Bollard reports a non-zero exit as an error.
            Some(Err(Error::DockerContainerWaitError { code, .. })) => Ok(code),
            Some(Err(error)) => Err(error),
            None => Ok(0),
        }
    }

    async fn logs(&self, container: &str) -> Result<String, Error> {
        let options = LogsOptionsBuilder::new().stdout(true).stderr(true).build();
        let chunks: Vec<_> = self
            .api
            .logs(container, Some(options))
            .try_collect()
            .await?;
        Ok(chunks.iter().map(ToString::to_string).collect())
    }

    async fn ensure_image(&self, image: &str) -> Result<(), Error> {
        match self.api.inspect_image(image).await {
            Err(error) if is_not_found(&error) => {
                let options = CreateImageOptionsBuilder::new().from_image(image).build();
                self.api
                    .create_image(Some(options), None, None)
                    .try_collect::<Vec<_>>()
                    .await?;
                Ok(())
            }
            other => other.map(|_| ()),
        }
    }

    async fn ensure_network(&self, name: &str) -> Result<(), Error> {
        match self.api.inspect_network(name, None).await {
            Err(error) if is_not_found(&error) => {
                let request = NetworkCreateRequest {
                    name: name.to_owned(),
                    driver: Some("bridge".to_owned()),
                    ..Default::default()
                };
                self.api.create_network(request).await.map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    async fn ensure_volume(&self, name: &str) -> Result<(), Error> {
        // Creating a volume that exists is a no-op in Docker.
        let request = VolumeCreateRequest {
            name: Some(name.to_owned()),
            ..Default::default()
        };
        self.api.create_volume(request).await.map(|_| ())
    }

    async fn remove_volume(&self, name: &str) -> Result<(), Error> {
        let options = RemoveVolumeOptionsBuilder::new().build();
        match self.api.remove_volume(name, Some(options)).await {
            Err(error) if is_not_found(&error) => Ok(()),
            other => other,
        }
    }

    async fn volume_sizes(&self) -> Result<BTreeMap<String, u64>, Error> {
        // Without a type filter: bollard cannot encode one in the query.
        let options = DataUsageOptionsBuilder::new().verbose(true).build();
        let usage = self.api.df(Some(options)).await?;
        // API 1.53 leaves the items untyped; they are volumes as `GET /volumes` describes them.
        Ok(usage
            .volume_usage
            .and_then(|volumes| volumes.items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|volume| {
                let name = volume.get("Name")?.as_str()?.to_owned();
                let size = volume.get("UsageData")?.get("Size")?.as_u64()?;
                Some((name, size))
            })
            .collect())
    }
}

fn connect_to(docker_host: Option<&str>) -> Result<Docker, Error> {
    let api = match docker_host.filter(|host| !host.is_empty()) {
        None => bollard::Docker::connect_with_socket_defaults()?,
        Some(host) if host.starts_with("unix://") => {
            bollard::Docker::connect_with_socket(host, 120, bollard::API_DEFAULT_VERSION)?
        }
        // Bollard would quietly fall back to the local socket: a different Engine than the one asked for.
        Some(host) => {
            return Err(Error::UnsupportedURISchemeError {
                uri: host.to_owned(),
            });
        }
    };
    Ok(Docker { api })
}

/// A tar archive, to extract at `/`, holding the config as a root-only file at `JIT_CONFIG_PATH`.
fn jit_config_tar(jit_config: &JitConfig) -> std::io::Result<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    let path = JIT_CONFIG_PATH.trim_start_matches('/');
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o700);
    dir.set_size(0);
    tar.append_data(
        &mut dir,
        path.rsplit_once('/').map_or("", |(dir, _)| dir),
        std::io::empty(),
    )?;
    let mut file = tar::Header::new_gnu();
    file.set_mode(0o400);
    file.set_size(jit_config.0.len() as u64);
    tar.append_data(&mut file, path, jit_config.0.as_bytes())?;
    tar.into_inner()
}

#[cfg(test)]
mod tests;
