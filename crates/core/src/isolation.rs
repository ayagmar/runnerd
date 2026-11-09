//! How a runner container is kept from the host while it runs its own Docker Engine. A backend decides only how
//! the container is created; its name, limits, labels and slice are the same for every backend (`docker`). It also
//! words what the host must have for a backend to start one, so a refused start and `runnerd doctor` say it alike; it
//! reads nothing from the host.

use std::fmt;

use bollard::models::HostConfig;

use crate::api::{Check, CheckStatus};
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

/// What Sysbox's runtime says when it cannot mount the running kernel's modules into a runner, up to the release.
const MODULES_STAT_REFUSED: &str = "failed to stat mount source at /lib/modules/";

/// The kernel release whose modules directory Docker's `error` says it could not stat, such as `7.2.11-arch1-1`.
fn missing_modules_release(error: &str) -> Option<&str> {
    let (_, after) = error.split_once(MODULES_STAT_REFUSED)?;
    let release = after.split(['/', ':']).next()?;
    (!release.is_empty()).then_some(release)
}

/// Why `runner` did not start, as the daemon logs it and announces it (`runner.failed`): what Docker said, and when
/// that is the running kernel's modules gone from `/lib/modules`, which a package upgrade does while the old kernel
/// runs, what happened and that the host must reboot.
pub fn start_failure(runner: &str, docker_error: &str) -> String {
    match missing_modules_release(docker_error) {
        Some(release) => format!(
            "cannot start {runner}: the running kernel's modules are gone (/lib/modules/{release} does not exist), \
             most likely removed by a package upgrade; reboot to run the installed kernel. Docker said: {docker_error}"
        ),
        None => format!("cannot start {runner}: {docker_error}"),
    }
}

/// The doctor's check that Sysbox can mount the running kernel's modules, which it does into every runner:
/// `present` says whether `/lib/modules/{release}` exists.
pub fn modules_check(release: &str, present: bool) -> Check {
    if present {
        return Check {
            name: "kernel modules".to_owned(),
            status: CheckStatus::Pass,
            detail: format!("/lib/modules/{release} exists"),
            fix: None,
        };
    }
    Check {
        name: "kernel modules".to_owned(),
        status: CheckStatus::Fail,
        detail: format!(
            "/lib/modules/{release} does not exist, which sysbox mounts into every runner, so no runner starts; a \
             package upgrade removes it while that kernel runs"
        ),
        fix: Some("reboot to run the installed kernel".to_owned()),
    }
}

#[cfg(test)]
mod tests;
