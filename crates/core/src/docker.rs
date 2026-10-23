//! Runner containers through the Docker Engine API: the connection, container, volume and image requests, and the
//! `DockerApi` trait the engine and the caches call. It decides nothing about scheduling, and a JIT config only
//! ever reaches a container as a root-only file, never the create request or a log line.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use bollard::exec::{CreateExecOptions, StartExecResults};
use std::pin::Pin;

use bollard::models::{
    ChangeType, ContainerConfig, ContainerCreateBody, CreateImageInfo, HostConfig, Mount,
    NetworkCreateRequest, SystemInfo, VolumeCreateRequest,
};
use bollard::query_parameters::{
    AttachContainerOptionsBuilder, CommitContainerOptionsBuilder, CreateContainerOptionsBuilder,
    CreateImageOptionsBuilder, DataUsageOptionsBuilder, InspectContainerOptionsBuilder,
    ListContainersOptionsBuilder, ListImagesOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
    ResizeContainerTTYOptionsBuilder, UploadToContainerOptionsBuilder, WaitContainerOptionsBuilder,
};
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};
use tracing::info;

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
    /// Its main process on the host, while it runs.
    pub pid: Option<u32>,
    /// Its address on its first network.
    pub address: Option<String>,
    /// When it started, in milliseconds since the epoch.
    pub started_at: Option<u64>,
    /// Its memory cap in bytes; `None` when it has none.
    pub memory: Option<u64>,
    /// Its CPU limit in millicores; `None` when it has none.
    pub cpus: Option<u64>,
}

/// What a command run in a container prints on stdout, as it prints it.
pub type Output = BoxStream<'static, Result<Vec<u8>, Error>>;

/// An image `images` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its first tag, such as `runnerd-record:42`; its ID when it has none.
    pub reference: String,
    /// Its labels.
    pub labels: HashMap<String, String>,
    /// The disk it takes beyond its parent image: what removing it frees.
    pub bytes: u64,
}

/// A container's terminal, attached once it was created and before it started, so none of its output is missed.
pub struct Tty {
    /// The container's ID.
    pub container: String,
    /// What the terminal shows, as it shows it; it ends when the container's process exits.
    pub output: BoxStream<'static, Result<Vec<u8>, Error>>,
    /// What is typed into it.
    pub input: Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
}

impl fmt::Debug for Tty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tty")
            .field("container", &self.container)
            .finish_non_exhaustive()
    }
}

/// A terminal's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtySize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
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
    /// A container's output since `since` (seconds since the epoch), each line prefixed with Docker's RFC 3339
    /// timestamp and a space.
    fn logs_since(
        &self,
        container: &str,
        since: u64,
    ) -> impl Future<Output = Result<String, Error>> + Send;
    /// Runs a command as root in a running container; its stdout streams until it exits or the container stops.
    /// Its stderr is dropped.
    fn exec(
        &self,
        container: &str,
        command: Vec<String>,
    ) -> impl Future<Output = Result<Output, Error>> + Send;
    /// Whether Docker has an image.
    fn image_present(&self, image: &str) -> impl Future<Output = Result<bool, Error>> + Send;
    /// Pulls an image, logging its progress every few seconds.
    fn pull(&self, image: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Creates a bridge network unless it exists.
    fn ensure_network(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Creates a volume unless it exists.
    fn ensure_volume(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// Removes a volume; a volume that does not exist is not an error.
    fn remove_volume(&self, name: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// The disk size of every volume Docker knows, by name. Slow: Docker measures each volume.
    fn volume_sizes(&self) -> impl Future<Output = Result<BTreeMap<String, u64>, Error>> + Send;
    /// The paths a container's own layer added or changed, not those it deleted.
    fn changes(&self, container: &str) -> impl Future<Output = Result<Vec<String>, Error>> + Send;
    /// The bytes a container's own layer holds.
    fn written(&self, container: &str) -> impl Future<Output = Result<u64, Error>> + Send;
    /// Commits a stopped container's own layer as the image `reference` (`repository:tag`) with `labels`; returns
    /// the disk the image takes beyond its parent.
    fn commit(
        &self,
        container: &str,
        reference: &str,
        labels: BTreeMap<String, String>,
    ) -> impl Future<Output = Result<u64, Error>> + Send;
    /// Every image with the label.
    fn images(&self, label: &str) -> impl Future<Output = Result<Vec<Image>, Error>> + Send;
    /// Removes an image and the layers only it used; one that does not exist is not an error.
    fn remove_image(&self, reference: &str) -> impl Future<Output = Result<(), Error>> + Send;
    /// The free disk where Docker keeps its images and containers.
    fn free_space(&self) -> impl Future<Output = Result<u64, Error>> + Send;
    /// Creates a container with a terminal of `size`, attaches to it and starts it. The container's stdin closes
    /// when `Tty::input` is dropped.
    fn shell(
        &self,
        name: &str,
        body: ContainerCreateBody,
        size: TtySize,
    ) -> impl Future<Output = Result<Tty, Error>> + Send;
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

    /// An image's labels; `None` when Docker does not have it.
    pub async fn image_labels(
        &self,
        image: &str,
    ) -> Result<Option<HashMap<String, String>>, Error> {
        match self.api.inspect_image(image).await {
            Ok(inspect) => Ok(Some(
                inspect
                    .config
                    .and_then(|config| config.labels)
                    .unwrap_or_default(),
            )),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Asks the image's registry for its manifest, without pulling it, and returns the platforms it is built for,
    /// such as `linux/amd64`. An error means `pull` could not get it.
    pub async fn registry_platforms(&self, image: &str) -> Result<Vec<String>, Error> {
        let inspect = self.api.inspect_registry_image(image, None).await?;
        Ok(inspect
            .platforms
            .into_iter()
            .filter_map(|platform| Some(format!("{}/{}", platform.os?, platform.architecture?)))
            // Attestation manifests in an index carry the platform unknown/unknown.
            .filter(|platform| platform != "unknown/unknown")
            .collect())
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
        let address = response
            .network_settings
            .and_then(|settings| settings.networks)
            .and_then(|networks| {
                networks
                    .into_values()
                    .find_map(|network| network.ip_address.filter(|address| !address.is_empty()))
            });
        Ok(RunnerState {
            running: state.running.unwrap_or(false),
            exit_code: state.exit_code,
            privileged: host_config.privileged.unwrap_or(false),
            runtime: host_config.runtime,
            pid: state
                .pid
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0),
            address,
            started_at: state.started_at.as_deref().and_then(crate::time::parse_ms),
            memory: host_config
                .memory
                .and_then(|memory| u64::try_from(memory).ok())
                .filter(|memory| *memory > 0),
            cpus: host_config
                .nano_cpus
                .and_then(|nano| u64::try_from(nano / 1_000_000).ok())
                .filter(|cpus| *cpus > 0),
        })
    }

    async fn remove(&self, container: &str) -> Result<(), Error> {
        // With its anonymous volumes, such as a shell's /state.
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();
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

    async fn logs_since(&self, container: &str, since: u64) -> Result<String, Error> {
        let options = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .since(i32::try_from(since).unwrap_or(i32::MAX))
            .build();
        let chunks: Vec<_> = self
            .api
            .logs(container, Some(options))
            .try_collect()
            .await?;
        Ok(chunks.iter().map(ToString::to_string).collect())
    }

    async fn exec(&self, container: &str, command: Vec<String>) -> Result<Output, Error> {
        let options = CreateExecOptions {
            cmd: Some(command),
            user: Some("root".to_owned()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        let exec = self.api.create_exec(container, options).await?;
        match self.api.start_exec(&exec.id, None).await? {
            StartExecResults::Attached { output, .. } => Ok(output
                .try_filter_map(|output| {
                    std::future::ready(Ok(match output {
                        bollard::container::LogOutput::StdOut { message } => Some(message.to_vec()),
                        _ => None,
                    }))
                })
                .boxed()),
            StartExecResults::Detached => Ok(futures_util::stream::empty().boxed()),
        }
    }

    async fn image_present(&self, image: &str) -> Result<bool, Error> {
        Ok(self.image_labels(image).await?.is_some())
    }

    async fn pull(&self, image: &str) -> Result<(), Error> {
        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let mut stream = self.api.create_image(Some(options), None, None);
        let mut progress = PullProgress::default();
        let mut logged = Instant::now();
        info!("pulling {image}");
        while let Some(update) = stream.try_next().await? {
            progress.update(&update);
            if logged.elapsed() >= PULL_LOG_INTERVAL {
                logged = Instant::now();
                info!("pulling {image}: {progress}");
            }
        }
        info!("pulled {image}");
        Ok(())
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

    async fn changes(&self, container: &str) -> Result<Vec<String>, Error> {
        Ok(self
            .api
            .container_changes(container)
            .await?
            .unwrap_or_default()
            .into_iter()
            .filter(|change| change.kind != ChangeType::_2)
            .map(|change| change.path)
            .collect())
    }

    async fn written(&self, container: &str) -> Result<u64, Error> {
        let options = InspectContainerOptionsBuilder::new().size(true).build();
        let response = self.api.inspect_container(container, Some(options)).await?;
        Ok(response
            .size_rw
            .and_then(|bytes| u64::try_from(bytes).ok())
            .unwrap_or(0))
    }

    async fn commit(
        &self,
        container: &str,
        reference: &str,
        labels: BTreeMap<String, String>,
    ) -> Result<u64, Error> {
        let (repository, tag) = reference.rsplit_once(':').unwrap_or((reference, "latest"));
        let options = CommitContainerOptionsBuilder::new()
            .container(container)
            .repo(repository)
            .tag(tag)
            .pause(false)
            .build();
        let config = ContainerConfig {
            labels: Some(labels.into_iter().collect()),
            ..Default::default()
        };
        // The image the container runs is the record's parent.
        let parent = match self.api.inspect_container(container, None).await?.image {
            Some(parent) => self.api.inspect_image(&parent).await?.size.unwrap_or(0),
            None => 0,
        };
        let id = self.api.commit_container(options, config).await?.id;
        let size = self.api.inspect_image(&id).await?.size.unwrap_or(0);
        Ok(u64::try_from(size - parent).unwrap_or(0))
    }

    async fn images(&self, label: &str) -> Result<Vec<Image>, Error> {
        let options = ListImagesOptionsBuilder::new()
            .filters(&HashMap::from([("label", vec![label])]))
            .build();
        let images = self.api.list_images(Some(options)).await?;
        let mut parents: HashMap<String, i64> = HashMap::new();
        let mut found = Vec::new();
        for image in images {
            let parent = if image.parent_id.is_empty() {
                0
            } else if let Some(size) = parents.get(&image.parent_id) {
                *size
            } else {
                let size = self
                    .api
                    .inspect_image(&image.parent_id)
                    .await
                    .ok()
                    .and_then(|parent| parent.size)
                    .unwrap_or(0);
                parents.insert(image.parent_id.clone(), size);
                size
            };
            found.push(Image {
                reference: image.repo_tags.into_iter().next().unwrap_or(image.id),
                labels: image.labels,
                bytes: u64::try_from(image.size - parent).unwrap_or(0),
            });
        }
        Ok(found)
    }

    async fn remove_image(&self, reference: &str) -> Result<(), Error> {
        let options = RemoveImageOptionsBuilder::new().build();
        match self.api.remove_image(reference, Some(options), None).await {
            Err(error) if is_not_found(&error) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    async fn free_space(&self) -> Result<u64, Error> {
        let root = self.api.info().await?.docker_root_dir.unwrap_or_default();
        // Only the directories above Docker's root need to be searchable: the daemon's user cannot read the root.
        let stat = rustix::fs::statvfs(root.as_str()).map_err(|error| Error::IOError {
            err: std::io::Error::other(format!("cannot measure the disk of {root}: {error}")),
        })?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }

    async fn shell(
        &self,
        name: &str,
        body: ContainerCreateBody,
        size: TtySize,
    ) -> Result<Tty, Error> {
        let options = CreateContainerOptionsBuilder::new().name(name).build();
        let id = self.api.create_container(Some(options), body).await?.id;
        let attached = async {
            let options = AttachContainerOptionsBuilder::new()
                .stream(true)
                .stdin(true)
                .stdout(true)
                .stderr(true)
                .build();
            let attached = self.api.attach_container(&id, Some(options)).await?;
            self.api.start_container(&id, None).await?;
            let resize = ResizeContainerTTYOptionsBuilder::new()
                .w(i32::from(size.cols))
                .h(i32::from(size.rows))
                .build();
            self.api.resize_container_tty(&id, resize).await?;
            Ok::<_, Error>(attached)
        };
        match attached.await {
            Ok(attached) => Ok(Tty {
                container: id,
                output: attached
                    .output
                    .map_ok(|output| output.into_bytes().to_vec())
                    .boxed(),
                input: attached.input,
            }),
            Err(error) => {
                let _ = self.remove(&id).await;
                Err(error)
            }
        }
    }
}

/// How often `pull` logs its progress.
const PULL_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// A pull's layers so far, from the statuses Docker streams per layer.
#[derive(Debug, Default)]
struct PullProgress {
    layers: BTreeMap<String, Layer>,
}

#[derive(Debug, Default)]
struct Layer {
    downloaded: u64,
    size: u64,
    done: bool,
}

impl PullProgress {
    fn update(&mut self, update: &CreateImageInfo) {
        let (Some(id), Some(status)) = (&update.id, &update.status) else {
            return;
        };
        let bytes = |value: Option<i64>| value.and_then(|value| u64::try_from(value).ok());
        let detail = update.progress_detail.as_ref();
        match status.as_str() {
            "Pulling fs layer" | "Waiting" => {
                self.layers.entry(id.clone()).or_default();
            }
            "Downloading" => {
                let layer = self.layers.entry(id.clone()).or_default();
                layer.downloaded =
                    bytes(detail.and_then(|detail| detail.current)).unwrap_or(layer.downloaded);
                layer.size = bytes(detail.and_then(|detail| detail.total)).unwrap_or(layer.size);
            }
            "Download complete" => {
                if let Some(layer) = self.layers.get_mut(id) {
                    layer.downloaded = layer.size;
                }
            }
            "Pull complete" | "Already exists" => {
                let layer = self.layers.entry(id.clone()).or_default();
                layer.downloaded = layer.size;
                layer.done = true;
            }
            _ => {}
        }
    }
}

impl fmt::Display for PullProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let done = self.layers.values().filter(|layer| layer.done).count();
        let downloaded: u64 = self.layers.values().map(|layer| layer.downloaded).sum();
        let size: u64 = self.layers.values().map(|layer| layer.size).sum();
        write!(
            f,
            "{done} of {} layers done, {} of {} downloaded",
            self.layers.len(),
            ByteSize(downloaded),
            ByteSize(size)
        )
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
