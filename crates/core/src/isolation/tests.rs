use bollard::models::HostConfig;

use super::backend;
use crate::config::Isolation;
use crate::docker::{RunnerSpec, create_body};

fn host_config(isolation: Isolation) -> HostConfig {
    let spec = RunnerSpec {
        name: "runnerd-trellis-1".to_owned(),
        image: "runnerd-runner:trellis".to_owned(),
        profile: "trellis".to_owned(),
        slice: "ci.slice".to_owned(),
        cpus: 8,
        memory: "9G".parse().unwrap(),
        env: Vec::new(),
        mounts: Vec::new(),
        entrypoint: None,
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
    }
}
