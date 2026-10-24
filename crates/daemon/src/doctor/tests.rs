//! `doctor`'s checks on made-up hosts.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

use runnerd_core::api::Sizing;
use runnerd_core::config::{ByteSize, Config};
use runnerd_core::sizing;

use super::{
    Check, DaemonState, DockerFacts, Host, ImageFound, SYSBOX_DAEMONS, Slice, Status, SysboxDaemon,
    checks, process_runs,
};
use crate::testdir::Dir;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The image `examples/runnerd.toml`'s profile `stage` pulls.
fn published(stage: &str) -> String {
    config("sysbox").profiles[stage].image.reference()
}

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

/// Each of sysbox's daemons in the state `state` gives it.
fn daemons(state: impl Fn(&str) -> DaemonState) -> [SysboxDaemon; 2] {
    SYSBOX_DAEMONS.map(|name| SysboxDaemon {
        name,
        state: state(name),
    })
}

/// A machine with sysbox installed and `runnerd.slice` at 33G, as the example config names it.
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
            platform: "linux/amd64".to_owned(),
        }),
        images: ["jvm", "node"]
            .map(|stage| {
                let found = ImageFound::Present {
                    version: Some(VERSION.to_owned()),
                };
                (published(stage), Ok(found))
            })
            .into(),
        records: Some(Ok(0)),
        sizing: None,
        os_release: os_release.to_owned(),
        slice: Some(Ok(Slice {
            unit_file: Some("/etc/systemd/system/runnerd.slice".to_owned()),
            memory_max: Some(ByteSize(33 << 30)),
            cpu_quota: "infinity".to_owned(),
        })),
        gh_scopes: Ok(vec![
            "gist".to_owned(),
            "repo".to_owned(),
            "workflow".to_owned(),
        ]),
        sysbox_daemons: daemons(|_| DaemonState::Unit),
        sysbox_unit: true,
        kvm: true,
    }
}

fn without_sysbox(mut host: Host) -> Host {
    if let Ok(docker) = &mut host.docker {
        docker.runtimes.retain(|runtime| runtime != "sysbox-runc");
    }
    host
}

fn failed(checks: &[Check]) -> Vec<&str> {
    checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .map(|check| check.name.as_str())
        .collect()
}

#[test]
fn a_ready_host_passes_every_check() {
    let config = config("sysbox");
    let checks = checks(&ready_host(ARCH), Some(&config));
    assert_eq!(failed(&checks), Vec::<&str>::new());
    let names: Vec<_> = checks.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Docker",
            "cgroups",
            "isolation",
            "sysbox",
            "slice",
            "config",
            "image",
            "image",
            "records",
            "gh",
            "/dev/kvm"
        ]
    );
}

/// Asserts that every line of `check`'s fix after the first, which says what to do, is shell a user can paste:
/// `bash -n` parses it without running it.
fn assert_pasteable(check: &Check) {
    let fix = check.fix.as_deref().unwrap();
    let mut commands = String::new();
    for line in fix.lines().skip(1) {
        commands.push_str(line);
        commands.push('\n');
    }
    let mut bash = Command::new("bash")
        .arg("-n")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    bash.stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let output = bash.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}'s fix is not valid shell: {}\n{commands}",
        check.name,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sysbox(checks: &[Check]) -> &Check {
    checks.iter().find(|check| check.name == "sysbox").unwrap()
}

#[test]
fn missing_sysbox_fails_with_pasteable_commands_for_the_distribution() {
    let config = config("sysbox");
    for (os_release, expected) in [
        (ARCH, "paru -S sysbox-ce-bin"),
        ("ID=manjaro\nID_LIKE=arch\n", "paru -S sysbox-ce-bin"),
        (
            "ID=ubuntu\nID_LIKE=debian\n",
            "sudo apt-get install jq ./sysbox-ce_0.7.1.linux_amd64.deb",
        ),
        (
            "ID='ubuntu'\nID_LIKE='debian'\n",
            "sudo apt-get install jq ./sysbox-ce_0.7.1.linux_amd64.deb",
        ),
        (
            "ID=debian\n",
            "releases/download/v0.7.1/sysbox-ce_0.7.1.linux_amd64.deb",
        ),
        ("ID=fedora\n", "sudo dnf install -y git make jq"),
        (
            "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
            "sudo ./scr/docker-cfg --sysbox-runtime=enable",
        ),
        (
            "ID='almalinux'\nID_LIKE='rhel centos fedora'\n",
            "make sysbox-static",
        ),
        (
            "ID=\"amzn\"\nID_LIKE=\"fedora\"\n",
            "sudo yum install -y git make jq",
        ),
        ("ID=\"rhel\"\nID_LIKE=\"fedora\"\n", "Sysbox-EE only"),
        ("ID=gentoo\n", "docs/distro-compat.md"),
        ("", "docs/distro-compat.md"),
    ] {
        let checks = checks(&without_sysbox(ready_host(os_release)), Some(&config));
        assert_eq!(failed(&checks), ["sysbox"]);
        let sysbox = sysbox(&checks);
        assert!(
            sysbox.fix.as_deref().unwrap().contains(expected),
            "{os_release}: {sysbox}"
        );
        assert_pasteable(sysbox);
    }
}

#[test]
fn red_hat_enterprise_linux_gets_no_sysbox_ce_build() {
    let checks = checks(
        &without_sysbox(ready_host("ID=\"rhel\"\nID_LIKE=\"fedora\"\n")),
        Some(&config("sysbox")),
    );
    let fix = sysbox(&checks).fix.as_deref().unwrap();
    assert!(!fix.contains("make sysbox-static"), "{fix}");
}

#[test]
fn the_sysbox_package_matches_dockers_architecture() {
    let mut host = without_sysbox(ready_host("ID=ubuntu\nID_LIKE=debian\n"));
    if let Ok(docker) = &mut host.docker {
        docker.platform = "linux/arm64".to_owned();
    }
    let checks = checks(&host, Some(&config("sysbox")));
    let sysbox = sysbox(&checks);
    assert!(
        sysbox
            .fix
            .as_deref()
            .unwrap()
            .contains("sysbox-ce_0.7.1.linux_arm64.deb"),
        "{sysbox}"
    );
}

#[test]
fn sysbox_needs_both_of_its_daemons() {
    let config = config("sysbox");
    for stopped in SYSBOX_DAEMONS {
        let mut host = ready_host(ARCH);
        host.sysbox_daemons = daemons(|name| {
            if name == stopped {
                DaemonState::Stopped
            } else {
                DaemonState::Unit
            }
        });
        let checks = checks(&host, Some(&config));
        assert_eq!(failed(&checks), ["sysbox"], "{stopped} stopped");
        let sysbox = sysbox(&checks);
        assert!(sysbox.detail.contains(stopped), "{sysbox}");
        assert!(
            sysbox
                .fix
                .as_deref()
                .unwrap()
                .contains("sudo systemctl enable --now sysbox"),
            "{sysbox}"
        );
        assert_pasteable(sysbox);
    }
}

#[test]
fn a_source_built_sysbox_passes_while_its_daemons_run_without_systemd() {
    let mut host = ready_host("ID=fedora\n");
    host.sysbox_unit = false;
    host.sysbox_daemons = daemons(|_| DaemonState::Process);
    let checks = checks(&host, Some(&config("sysbox")));
    assert_eq!(failed(&checks), Vec::<&str>::new());
    assert_eq!(sysbox(&checks).status, Status::Pass);
}

#[test]
fn a_source_built_sysbox_with_a_stopped_daemon_is_started_from_its_clone() {
    let mut host = ready_host("ID=fedora\n");
    host.sysbox_unit = false;
    host.sysbox_daemons = daemons(|name| {
        if name == "sysbox-fs" {
            DaemonState::Stopped
        } else {
            DaemonState::Process
        }
    });
    let checks = checks(&host, Some(&config("sysbox")));
    assert_eq!(failed(&checks), ["sysbox"]);
    let sysbox = sysbox(&checks);
    let fix = sysbox.fix.as_deref().unwrap();
    assert!(fix.contains("sudo ./scr/sysbox"), "{fix}");
    assert!(!fix.contains("systemctl"), "{fix}");
    assert_pasteable(sysbox);
}

/// A procfs holding `processes`, each a pid and its `stat`, or no `stat` for one that exited while it was read.
fn proc_fixture(processes: &[(&str, Option<&str>)]) -> Dir {
    let dir = Dir::new();
    for (pid, stat) in processes {
        let process = dir.0.join(pid);
        std::fs::create_dir_all(&process).unwrap();
        if let Some(stat) = stat {
            std::fs::write(process.join("stat"), stat).unwrap();
        }
    }
    dir
}

#[test]
fn a_live_sysbox_daemon_counts_as_running_whatever_its_state() {
    for state in ["S", "R", "D", "I", "T"] {
        let stat = format!("4120 (sysbox-mgr) {state} 1 4120 4120 0 -1");
        let proc = proc_fixture(&[
            ("1", Some("1 (systemd) S 0 1 1 0 -1")),
            ("4120", Some(&stat)),
        ]);
        assert!(
            process_runs(&proc.0, "sysbox-mgr"),
            "state {state} is alive"
        );
    }
}

#[test]
fn an_exited_sysbox_daemon_does_not_count_as_running() {
    for state in ["Z", "X", "x"] {
        let stat = format!("4120 (sysbox-mgr) {state} 1 4120 4120 0 -1");
        let proc = proc_fixture(&[("4120", Some(&stat))]);
        assert!(
            !process_runs(&proc.0, "sysbox-mgr"),
            "state {state} is not alive"
        );
    }
}

#[test]
fn a_missing_sysbox_daemon_does_not_count_as_running() {
    let proc = proc_fixture(&[
        ("1", Some("1 (systemd) S 0 1 1 0 -1")),
        ("4121", Some("4121 (sysbox-fs) S 1 4121 4121 0 -1")),
        ("4122", Some("4122 (my sysbox-mgr) S 1 4122 4122 0 -1")),
        ("self", Some("4123 (sysbox-mgr) S 1 4123 4123 0 -1")),
    ]);
    assert!(
        !process_runs(&proc.0, "sysbox-mgr"),
        "no process is sysbox-mgr"
    );
    let empty = Dir::new();
    assert!(
        !process_runs(&empty.0.join("proc"), "sysbox-mgr"),
        "no procfs, no process"
    );
}

#[test]
fn a_process_that_exits_while_doctor_reads_is_skipped() {
    let proc = proc_fixture(&[
        ("4119", None),
        ("4120", Some("4120 (sysbox-mgr) S 1 4120 4120 0 -1")),
    ]);
    assert!(
        process_runs(&proc.0, "sysbox-mgr"),
        "the live one still counts"
    );
    let gone = proc_fixture(&[("4119", None)]);
    assert!(
        !process_runs(&gone.0, "sysbox-mgr"),
        "the exited one does not"
    );
}

#[test]
fn a_process_name_is_read_up_to_the_last_parenthesis() {
    let proc = proc_fixture(&[("4120", Some("4120 (a) b) S 1 4120 4120 0 -1"))]);
    assert!(
        process_runs(&proc.0, "a) b"),
        "the name is everything between the first ( and the last )"
    );
}

#[test]
fn unsafe_dind_needs_no_sysbox_and_is_flagged_whether_sysbox_is_installed_or_not() {
    let config = config("unsafe-dind");
    for host in [ready_host(ARCH), without_sysbox(ready_host(ARCH))] {
        let checks = checks(&host, Some(&config));
        assert_eq!(failed(&checks), Vec::<&str>::new());
        let isolation = checks
            .iter()
            .find(|check| check.name == "isolation")
            .unwrap();
        assert_eq!(isolation.status, Status::Warn);
        assert!(isolation.detail.contains("privileged"), "{isolation}");
    }
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
    assert_pasteable(checks.iter().find(|check| check.name == "Docker").unwrap());
    let skipped: Vec<_> = checks
        .iter()
        .filter(|check| check.status == Status::Skip)
        .map(|check| check.name.as_str())
        .collect();
    assert_eq!(skipped, ["cgroups", "sysbox", "slice", "config", "image"]);
}

fn image_check<'checks>(checks: &'checks [Check], reference: &str) -> &'checks Check {
    checks
        .iter()
        .find(|check| check.name == "image" && check.detail.contains(reference))
        .unwrap()
}

#[test]
fn a_published_image_not_pulled_yet_passes_and_one_the_registry_refuses_fails() {
    let config = config("sysbox");
    let mut host = ready_host(ARCH);
    host.images = BTreeMap::from([
        (
            published("jvm"),
            Ok(ImageFound::Pullable {
                platforms: vec!["linux/amd64".to_owned()],
            }),
        ),
        (
            published("node"),
            Ok(ImageFound::Missing("manifest unknown".to_owned())),
        ),
    ]);
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), ["image"]);
    assert_eq!(image_check(&checks, &published("jvm")).status, Status::Pass);
    let node = image_check(&checks, &published("node"));
    assert!(node.detail.contains("manifest unknown"), "{node}");
    assert!(
        node.fix.as_deref().unwrap().contains("docker pull"),
        "{node}"
    );
}

#[test]
fn a_stage_never_built_fails_with_the_published_image_and_the_build_command() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/runnerd.toml"
    ))
    .unwrap()
    .replace(
        &format!("image = \"{}\"", published("jvm")),
        "image_stage = \"jvm\"",
    );
    let config = Config::parse(&text).unwrap();
    let mut host = ready_host(ARCH);
    host.images.insert(
        "runnerd-runner:jvm".to_owned(),
        Ok(ImageFound::Missing("not built".to_owned())),
    );
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), ["image"]);
    let fix = image_check(&checks, "runnerd-runner:jvm")
        .fix
        .clone()
        .unwrap();
    // This release's tag, which may differ from the example's.
    assert!(
        fix.contains(&format!("ghcr.io/ayagmar/runnerd-runner:jvm-{VERSION}")),
        "{fix}"
    );
    assert!(
        fix.contains("docker build --target jvm -t runnerd-runner:jvm image"),
        "{fix}"
    );
}

#[test]
fn an_image_built_for_another_release_warns() {
    let config = config("sysbox");
    let mut host = ready_host(ARCH);
    host.images.insert(
        published("jvm"),
        Ok(ImageFound::Present {
            version: Some("0.0.1".to_owned()),
        }),
    );
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), Vec::<&str>::new());
    let jvm = image_check(&checks, &published("jvm"));
    assert_eq!(jvm.status, Status::Warn);
    assert!(jvm.detail.contains("0.0.1"), "{jvm}");
}

#[test]
fn an_amd64_only_image_fails_on_an_arm64_docker_with_the_local_build() {
    let config = config("sysbox");
    let mut host = ready_host(ARCH);
    if let Ok(docker) = &mut host.docker {
        docker.platform = "linux/arm64".to_owned();
    }
    let pullable = || {
        Ok(ImageFound::Pullable {
            platforms: vec!["linux/amd64".to_owned()],
        })
    };
    host.images = BTreeMap::from([
        (published("jvm"), pullable()),
        (published("node"), pullable()),
    ]);
    let checks = checks(&host, Some(&config));
    assert_eq!(failed(&checks), ["image", "image"]);
    let jvm = image_check(&checks, &published("jvm"));
    assert!(jvm.detail.contains("linux/arm64"), "{jvm}");
    assert!(
        jvm.fix.as_deref().unwrap().contains("docker build"),
        "{jvm}"
    );
}

#[test]
fn records_near_their_size_limit_warn_with_how_to_free_them() {
    let config = config("sysbox");
    let max_total = config.records.max_total.0;
    let mut host = ready_host(ARCH);
    let records = |host: &Host| {
        checks(host, Some(&config))
            .into_iter()
            .find(|check| check.name == "records")
            .unwrap()
    };
    host.records = Some(Ok(max_total / 2));
    assert_eq!(records(&host).status, Status::Info);
    host.records = Some(Ok(max_total / 10 * 9));
    let warned = records(&host);
    assert_eq!(warned.status, Status::Warn);
    assert!(
        warned.fix.unwrap().contains("runnerctl records rm"),
        "the fix says how to free them"
    );
}

/// `count` jobs of `repository` in `profile`, peaking at `peak` bytes, each queued a minute before it started.
fn ran(repository: &str, profile: &str, count: u64, peak: u64) -> Vec<sizing::Ran> {
    (0..count)
        .map(|i| sizing::Ran {
            repository: repository.to_owned(),
            workflow: Some("CI".to_owned()),
            name: "build".to_owned(),
            profile: profile.to_owned(),
            memory_cap: Some(config("sysbox").profiles[profile].memory.0),
            cpu_cap: None,
            memory_peak: peak,
            cpu_peak: 4000,
            cpu_average: Some(1000),
            queued_at: 1_000_000 + i,
            runner_started_at: 1_000_000 + i + 60_000,
            started_at: Some(1_000_000 + i + 120_000),
            finished_at: Some(1_000_000 + i + 720_000),
            oom_killed: false,
            reserved: None,
            hit_cap: Some(false),
            measure: sizing::Measure::WithoutPageCache,
            succeeded: true,
        })
        .collect()
}

fn sizing_of(jobs: Vec<sizing::Ran>) -> Sizing {
    sizing::advise(
        &config("sysbox"),
        Some(33 << 30),
        &sizing::History {
            jobs,
            cancelled: Vec::new(),
            runners: Vec::new(),
        },
        sizing::Window::new(Some(0), 2_000_000),
    )
}

fn sizing_check(sizing: Sizing) -> Check {
    let mut host = ready_host(ARCH);
    host.sizing = Some(Ok(sizing));
    checks(&host, Some(&config("sysbox")))
        .into_iter()
        .find(|check| check.name == "sizing")
        .unwrap()
}

#[test]
fn jobs_at_their_cap_warn_and_name_the_command_that_raises_it() {
    let mut jobs = ran("halyard/ledger", "jvm", 12, 8900 << 20);
    jobs.extend(ran("halyard/storefront", "node", 12, 2 << 30));
    let check = sizing_check(sizing_of(jobs));
    assert_eq!(check.status, Status::Warn, "{check}");
    assert!(check.detail.contains("jvm: raise memory to"), "{check}");
    assert!(
        check.detail.contains("node: lower memory to 2.5G"),
        "{check}"
    );
    assert_eq!(
        check.fix.as_deref(),
        Some("runnerctl sizing apply jvm\nrunnerctl sizing apply node")
    );
}

#[test]
fn a_history_too_short_to_advise_on_is_only_information() {
    let check = sizing_check(sizing_of(ran("halyard/ledger", "jvm", 3, 8 << 30)));
    assert_eq!(check.status, Status::Info, "{check}");
    assert_eq!(check.fix, None);
}

#[test]
fn under_measured_admission_caps_over_memory_max_pass_without_claiming_to_fit() {
    let mut config = config("sysbox");
    config.host.admission = runnerd_core::config::Admission::Measured;
    config.profiles.get_mut("jvm").unwrap().slots = 3;
    let checks = checks(&ready_host(ARCH), Some(&config));
    let check = checks.iter().find(|check| check.name == "config").unwrap();
    assert_eq!(check.status, Status::Pass);
    assert!(
        !check.detail.contains("fits in") && check.detail.contains("41.5G"),
        "says the caps sum past MemoryMax: {}",
        check.detail
    );
}
