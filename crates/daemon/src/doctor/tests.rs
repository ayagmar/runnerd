use runnerd_core::config::{ByteSize, Config};

use super::{Check, DockerFacts, Host, Slice, Status, checks};

const ARCH: &str = "NAME=\"Arch Linux\"\nID=arch\n";

fn config(isolation: &str) -> Config {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/runnerd.toml"
    ))
    .unwrap()
    .replace(
        "isolation = \"sysbox\"",
        &format!("isolation = \"{isolation}\"\nallow_unsafe = true"),
    );
    Config::parse(&text).unwrap()
}

/// The owner's machine with sysbox installed and `ci.slice` at 33G.
fn ready_host(os_release: &str) -> Host {
    Host {
        docker: Ok(DockerFacts {
            version: "29.9.2".to_owned(),
            cgroup_version: "2".to_owned(),
            cgroup_driver: "systemd".to_owned(),
            runtimes: vec![
                "io.containerd.runc.v2".to_owned(),
                "runc".to_owned(),
                "sysbox-runc".to_owned(),
            ],
        }),
        os_release: os_release.to_owned(),
        slice: Some(Ok(Slice {
            unit_file: Some("/etc/systemd/system/ci.slice".to_owned()),
            memory_max: Some(ByteSize(33 << 30)),
            cpu_quota: "infinity".to_owned(),
        })),
        gh_scopes: Ok(vec![
            "gist".to_owned(),
            "repo".to_owned(),
            "workflow".to_owned(),
        ]),
        sysbox_services: true,
        kvm: true,
    }
}

fn without_sysbox(mut host: Host) -> Host {
    if let Ok(docker) = &mut host.docker {
        docker.runtimes.retain(|runtime| runtime != "sysbox-runc");
    }
    host
}

fn failed(checks: &[Check]) -> Vec<&'static str> {
    checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .map(|check| check.name)
        .collect()
}

#[test]
fn a_ready_host_passes_every_check() {
    let config = config("sysbox");
    let checks = checks(&ready_host(ARCH), Some(&config));
    assert_eq!(failed(&checks), Vec::<&str>::new());
    let names: Vec<_> = checks.iter().map(|check| check.name).collect();
    assert_eq!(
        names,
        [
            "Docker", "cgroups", "sysbox", "slice", "config", "gh", "/dev/kvm"
        ]
    );
}

#[test]
fn missing_sysbox_fails_with_the_package_for_the_distribution() {
    let config = config("sysbox");
    for (os_release, package) in [
        (ARCH, "sysbox-ce-bin"),
        ("ID=ubuntu\nID_LIKE=debian\n", ".deb"),
        ("ID=fedora\n", "build sysbox from source"),
    ] {
        let checks = checks(&without_sysbox(ready_host(os_release)), Some(&config));
        assert_eq!(failed(&checks), ["sysbox"]);
        let sysbox = checks.iter().find(|check| check.name == "sysbox").unwrap();
        assert!(
            sysbox.fix.as_deref().unwrap().contains(package),
            "{os_release}: {sysbox}"
        );
    }
}

#[test]
fn sysbox_with_stopped_services_fails_with_the_command_to_start_them() {
    let config = config("sysbox");
    let mut host = ready_host(ARCH);
    host.sysbox_services = false;
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), ["sysbox"]);
    let sysbox = checks.iter().find(|check| check.name == "sysbox").unwrap();
    assert!(
        sysbox
            .fix
            .as_deref()
            .unwrap()
            .contains("systemctl enable --now sysbox"),
        "{sysbox}"
    );
}

#[test]
fn unsafe_dind_does_not_need_sysbox() {
    let config = config("unsafe-dind");
    let checks = checks(&without_sysbox(ready_host(ARCH)), Some(&config));
    assert_eq!(failed(&checks), Vec::<&str>::new());
}

#[test]
fn without_a_config_sysbox_is_still_required() {
    let mut host = without_sysbox(ready_host(ARCH));
    host.slice = None;
    assert_eq!(failed(&checks(&host, None)), ["sysbox"]);
}

#[test]
fn a_slice_without_limits_and_a_token_without_repo_fail() {
    let config = config("sysbox");
    let mut host = ready_host(ARCH);
    host.slice = Some(Ok(Slice {
        unit_file: None,
        memory_max: None,
        cpu_quota: "infinity".to_owned(),
    }));
    host.gh_scopes = Ok(vec!["gist".to_owned()]);
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), ["slice", "gh"]);
    let slice = checks.iter().find(|check| check.name == "slice").unwrap();
    assert!(
        slice.fix.as_deref().unwrap().contains("MemoryMax=32.5G"),
        "{slice}"
    );
}

#[test]
fn unreachable_docker_skips_what_depends_on_it() {
    let mut host = ready_host(ARCH);
    host.docker = Err("connection refused".to_owned());
    let checks = checks(&host, None);
    assert_eq!(failed(&checks), ["Docker"]);
    let skipped: Vec<_> = checks
        .iter()
        .filter(|check| check.status == Status::Skip)
        .map(|check| check.name)
        .collect();
    assert_eq!(skipped, ["cgroups", "sysbox", "slice", "config"]);
}
