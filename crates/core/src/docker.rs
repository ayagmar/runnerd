//! Runner containers through the Docker Engine API.

use std::collections::HashMap;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, SystemInfo};
use bollard::query_parameters::{CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder};

use crate::config::ByteSize;
use crate::isolation::Isolation;

pub use bollard::errors::Error;

/// One runner container: a profile's image stage with the profile's limits.
#[derive(Debug, Clone)]
pub struct RunnerSpec {
    pub name: String,
    pub image: String,
    pub profile: String,
    /// The systemd slice the container is placed in.
    pub slice: String,
    pub cpus: u32,
    pub memory: ByteSize,
    pub env: Vec<String>,
    pub mounts: Vec<Mount>,
    /// Replaces the image's entrypoint; `None` runs the runner.
    pub entrypoint: Option<Vec<String>>,
}

/// What `inspect` reports about a runner container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerState {
    pub running: bool,
    pub exit_code: Option<i64>,
    /// True under `unsafe-dind`: a job in the container can take over the host.
    pub privileged: bool,
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
        ..Default::default()
    };
    isolation.configure(&mut host_config);
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        env: Some(spec.env.clone()),
        entrypoint: spec.entrypoint.clone(),
        labels: Some(HashMap::from([
            ("runnerd.profile".to_owned(), spec.profile.clone()),
            ("runnerd.isolation".to_owned(), isolation.name().to_owned()),
        ])),
        host_config: Some(host_config),
        ..Default::default()
    }
}

pub struct Docker {
    api: bollard::Docker,
}

impl Docker {
    /// Connects to the Docker Engine at `DOCKER_HOST`, or its local socket.
    pub fn connect() -> Result<Docker, Error> {
        Ok(Docker {
            api: bollard::Docker::connect_with_local_defaults()?,
        })
    }

    pub async fn info(&self) -> Result<SystemInfo, Error> {
        self.api.info().await
    }

    /// Creates and starts a runner container; returns its ID.
    pub async fn start(
        &self,
        spec: &RunnerSpec,
        isolation: &dyn Isolation,
    ) -> Result<String, Error> {
        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let id = self
            .api
            .create_container(Some(options), create_body(spec, isolation))
            .await?
            .id;
        if let Err(error) = self.api.start_container(&id, None).await {
            // Best effort: the start error is the one worth reporting.
            let _ = self.remove(&id).await;
            return Err(error);
        }
        Ok(id)
    }

    pub async fn inspect(&self, container: &str) -> Result<RunnerState, Error> {
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

    /// Removes a runner container, stopping it first if it runs.
    pub async fn remove(&self, container: &str) -> Result<(), Error> {
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        self.api.remove_container(container, Some(options)).await
    }
}
