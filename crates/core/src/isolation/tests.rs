//! What each isolation backend sets on a runner container.

use std::collections::BTreeMap;

use bollard::models::HostConfig;

use super::{backend, modules_check, start_failure};
use crate::api::CheckStatus;
use crate::config::Isolation;
use crate::docker::{RunnerSpec, create_body};

fn host_config(isolation: Isolation) -> HostConfig {
    let spec = RunnerSpec {
        name: "runnerd-jvm-1".to_owned(),
        image: "runnerd-runner:jvm".to_owned(),
        profile: "jvm".to_owned(),
        slice: "ci.slice".to_owned(),
        cpus: 8,
        memory: "9G".parse().unwrap(),
        env: Vec::new(),
        mounts: Vec::new(),
        network: None,
        labels: BTreeMap::new(),
        entrypoint: None,
        jit_config: None,
    };
    create_body(&spec, backend(isolation)).host_config.unwrap()
}

#[test]
fn sysbox_runs_unprivileged_under_sysbox_runc() {
    let host_config = host_config(Isolation::Sysbox);
    assert_eq!(host_config.runtime.as_deref(), Some("sysbox-runc"));
    assert_eq!(host_config.privileged, Some(false));
}

#[test]
fn unsafe_dind_runs_privileged_and_says_so() {
    let host_config = host_config(Isolation::UnsafeDind);
    assert_eq!(host_config.runtime.as_deref(), Some("runc"));
    assert_eq!(host_config.privileged, Some(true));
    assert!(backend(Isolation::UnsafeDind).is_unsafe());
    assert!(!backend(Isolation::Sysbox).is_unsafe());
}

#[test]
fn every_backend_keeps_the_slice_and_the_profile_limits() {
    for isolation in [Isolation::Sysbox, Isolation::UnsafeDind] {
        let host_config = host_config(isolation);
        let memory = 9 << 30;
        assert_eq!(host_config.cgroup_parent.as_deref(), Some("ci.slice"));
        assert_eq!(host_config.memory, Some(memory));
        assert_eq!(
            host_config.memory_swap,
            Some(memory),
            "no swap beyond memory"
        );
        assert_eq!(host_config.nano_cpus, Some(8_000_000_000));
        assert_eq!(
            host_config.cpu_shares,
            Some(8 * 1024),
            "weighted by its cpus"
        );
    }
}

/// What Docker answered on 2026-11-08 at 15:14:44, after pacman removed the running kernel's modules.
const KERNEL_MODULES_GONE: &str = "Docker responded with status code 400: failed to create task for container: failed to create shim task: OCI runtime create failed: failed to create lib container mount: failed to stat mount source at /lib/modules/7.2.11-arch1-1: stat /lib/modules/7.2.11-arch1-1: no such file or directory";

/// The same refusal as the first of those 640 starts worded it, for the `build` link inside the directory.
const KERNEL_BUILD_LINK_GONE: &str = "Docker responded with status code 400: failed to create task for container: failed to create shim task: OCI runtime create failed: failed to create lib container mount: failed to stat mount source at /lib/modules/7.2.11-arch1-1/build: stat /lib/modules/7.2.11-arch1-1/build: no such file or directory";

#[test]
fn a_start_refused_for_missing_kernel_modules_says_so_and_to_reboot() {
    for error in [KERNEL_MODULES_GONE, KERNEL_BUILD_LINK_GONE] {
        let failure = start_failure("runnerd-node-4", error);
        assert!(
            failure.starts_with(
                "cannot start runnerd-node-4: the running kernel's modules are gone \
                 (/lib/modules/7.2.11-arch1-1 does not exist)"
            ),
            "{failure}"
        );
        assert!(failure.contains("reboot"), "{failure}");
        assert!(
            failure.ends_with(error),
            "Docker's own text stays at the end: {failure}"
        );
    }
}

#[test]
fn other_refusals_keep_docker_text_alone() {
    for error in [
        "Docker responded with status code 409: Conflict. The container name is already in use",
        // A different mount source, and a stat of the modules directory that failed for another reason.
        "failed to stat mount source at /var/lib/sysbox: stat /var/lib/sysbox: no such file or directory",
        "failed to stat mount source at /lib/modules/: stat /lib/modules/: permission denied",
    ] {
        assert_eq!(
            start_failure("runnerd-node-4", error),
            format!("cannot start runnerd-node-4: {error}")
        );
    }
}

#[test]
fn the_modules_check_fails_with_a_reboot_only_when_the_running_kernels_are_gone() {
    let gone = modules_check("7.2.11-arch1-1", false);
    assert_eq!(gone.status, CheckStatus::Fail);
    assert!(
        gone.detail.contains("/lib/modules/7.2.11-arch1-1"),
        "{gone:?}"
    );
    assert_eq!(
        gone.fix.as_deref(),
        Some("reboot to run the installed kernel")
    );
    let present = modules_check("7.2.11-arch1-1", true);
    assert_eq!((present.status, present.fix), (CheckStatus::Pass, None));
}
