//! How a runner container is kept from the host while it runs its own Docker Engine. A backend decides only how
//! the container is created; its name, limits, labels and slice are the same for every backend (`docker`).

use std::fmt;

use bollard::models::HostConfig;

use crate::config;

/// An isolation backend: how runner containers are created so their Docker Engine cannot reach the host.
pub trait Isolation: Send + Sync + fmt::Debug {
    /// The name `runnerd.toml` uses, also put on the container's `runnerd.isolation` label.
    fn name(&self) -> &'static str;
    /// Whether a job in the container can take over the host.
    fn is_unsafe(&self) -> bool;
    /// Sets the container's runtime and privileges.
    fn configure(&self, host_config: &mut HostConfig);
}

/// Sysbox runs Docker inside an unprivileged container, in its own user namespace.
#[derive(Debug)]
pub struct Sysbox;

impl Isolation for Sysbox {
    fn name(&self) -> &'static str {
        "sysbox"
    }

    fn is_unsafe(&self) -> bool {
        false
    }

    fn configure(&self, host_config: &mut HostConfig) {
        host_config.runtime = Some("sysbox-runc".to_owned());
        host_config.privileged = Some(false);
    }
}

/// Privileged Docker-in-Docker: root in the container is root on the host.
#[derive(Debug)]
pub struct UnsafeDind;

impl Isolation for UnsafeDind {
    fn name(&self) -> &'static str {
        "unsafe-dind"
    }

    fn is_unsafe(&self) -> bool {
        true
    }

    fn configure(&self, host_config: &mut HostConfig) {
        // Named, not left to the Engine's default runtime, which may be sysbox-runc.
        host_config.runtime = Some("runc".to_owned());
        host_config.privileged = Some(true);
    }
}

/// The backend `runnerd.toml` names.
pub fn backend(isolation: config::Isolation) -> &'static dyn Isolation {
    match isolation {
        config::Isolation::Sysbox => &Sysbox,
        config::Isolation::UnsafeDind => &UnsafeDind,
    }
}

#[cfg(test)]
mod tests;
