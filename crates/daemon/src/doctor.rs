//! `runnerd doctor`: what the host is missing to run runners, each with the fix.

use std::fmt;
use std::path::Path;
use std::process::Command;

use runnerd_core::config::{self, ByteSize, Config, Isolation};
use runnerd_core::docker::Docker;

/// What doctor reads from the host, gathered before any check so the checks can be tested without one.
pub struct Host {
    pub docker: Result<DockerFacts, String>,
    /// `/etc/os-release`, empty when missing.
    pub os_release: String,
    /// `None` without a config, which names the slice.
    pub slice: Option<Result<Slice, String>>,
    /// The scopes of `gh`'s token for github.com.
    pub gh_scopes: Result<Vec<String>, String>,
    /// Whether each of `SYSBOX_SERVICES` runs: Docker keeps the runtime registered without them.
    pub sysbox_services: [(&'static str, bool); 2],
    pub kvm: bool,
}

/// The services sysbox-runc needs; `sysbox.service` starts both.
pub const SYSBOX_SERVICES: [&str; 2] = ["sysbox-mgr", "sysbox-fs"];

pub struct DockerFacts {
    pub version: String,
    pub cgroup_version: String,
    pub cgroup_driver: String,
    pub runtimes: Vec<String>,
}

pub struct Slice {
    /// The slice's unit file; without one, systemd creates the slice without limits.
    pub unit_file: Option<String>,
    pub memory_max: Option<ByteSize>,
    /// `CPUQuotaPerSecUSec` as `systemctl show` prints it, such as `16s` or `infinity`.
    pub cpu_quota: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Works, but unsafe.
    Warn,
    /// Not checked, because a check it depends on failed or no config was given.
    Skip,
    Info,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// What to run or change to pass, for a failed check.
    pub fix: Option<String>,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = match self.status {
            Status::Pass => "ok",
            Status::Fail => "FAIL",
            Status::Warn => "WARN",
            Status::Skip => "skip",
            Status::Info => "info",
        };
        write!(f, "{status:<5} {}: {}", self.name, self.detail)?;
        if let Some(fix) = &self.fix {
            for (i, line) in fix.lines().enumerate() {
                let label = if i == 0 { "fix:" } else { "" };
                write!(f, "\n      {label:<4} {line}")?;
            }
        }
        Ok(())
    }
}

impl Host {
    pub fn read(config: Option<&Config>) -> Host {
        Host {
            docker: read_docker(),
            os_release: std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
            slice: config.map(|config| read_slice(&config.host.slice)),
            gh_scopes: read_gh_scopes(),
            // One unit per call: `systemctl is-active` with several units succeeds when any one runs.
            sysbox_services: SYSBOX_SERVICES.map(|unit| {
                let active = Command::new("systemctl")
                    .args(["is-active", "--quiet", unit])
                    .status()
                    .is_ok_and(|status| status.success());
                (unit, active)
            }),
            kvm: Path::new("/dev/kvm").exists(),
        }
    }
}

fn read_docker() -> Result<DockerFacts, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let info = runtime
        .block_on(async { Docker::connect()?.info().await })
        .map_err(|error| error.to_string())?;
    let mut runtimes: Vec<String> = info.runtimes.unwrap_or_default().into_keys().collect();
    runtimes.sort();
    Ok(DockerFacts {
        version: info.server_version.unwrap_or_default(),
        cgroup_version: info
            .cgroup_version
            .map(|v| v.to_string())
            .unwrap_or_default(),
        cgroup_driver: info
            .cgroup_driver
            .map(|d| d.to_string())
            .unwrap_or_default(),
        runtimes,
    })
}

fn read_slice(slice: &str) -> Result<Slice, String> {
    let value = |property: &str| {
        let output = Command::new("systemctl")
            .args(["show", slice, &format!("--property={property}"), "--value"])
            .output()
            .map_err(|error| format!("systemctl: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "systemctl show {slice} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let unit_file = value("FragmentPath")?;
    let memory_max = value("MemoryMax")?;
    Ok(Slice {
        unit_file: (!unit_file.is_empty()).then_some(unit_file),
        memory_max: config::parse_memory_max(&memory_max).ok_or_else(|| {
            format!("systemctl show {slice}: unexpected MemoryMax {memory_max:?}")
        })?,
        cpu_quota: value("CPUQuotaPerSecUSec")?,
    })
}

fn read_gh_scopes() -> Result<Vec<String>, String> {
    let output = Command::new("gh")
        .args([
            "auth",
            "status",
            "--hostname",
            "github.com",
            "--json",
            "hosts",
            "--jq",
            r#".hosts["github.com"][] | select(.active) | .scopes"#,
        ])
        .output()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh is not logged in to github.com: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .split(',')
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .collect())
}

/// Every check, in the order a new user fixes them.
pub fn checks(host: &Host, config: Option<&Config>) -> Vec<Check> {
    let mut checks = Vec::new();
    let docker = match &host.docker {
        Ok(docker) => {
            checks.push(pass(
                "Docker",
                format!("reachable, Docker {}", docker.version),
            ));
            Some(docker)
        }
        Err(error) => {
            checks.push(fail(
                "Docker",
                format!("unreachable: {error}"),
                "start Docker and let your user reach its socket:\n\
                 sudo systemctl enable --now docker\n\
                 sudo usermod -aG docker \"$USER\"   (then log in again)\n\
                 runnerd reaches Docker only through a unix:// socket: unset DOCKER_HOST or point it at one",
            ));
            None
        }
    };
    checks.push(match docker {
        None => skip("cgroups", "needs Docker"),
        Some(docker) if docker.cgroup_version == "2" && docker.cgroup_driver == "systemd" => {
            pass("cgroups", "cgroup v2 with the systemd driver".to_owned())
        }
        Some(docker) => fail(
            "cgroups",
            format!(
                "cgroup v{} with the {} driver; runnerd places runners in a systemd slice, which needs \
                 cgroup v2 with the systemd driver",
                docker.cgroup_version, docker.cgroup_driver
            ),
            "boot with cgroup v2 (the default on current distributions), and remove any \
             \"native.cgroupdriver=cgroupfs\" from /etc/docker/daemon.json",
        ),
    });
    let isolation = config.map_or(Isolation::Sysbox, |config| config.host.isolation);
    checks.push(match isolation {
        Isolation::Sysbox => Check {
            name: "isolation",
            status: Status::Info,
            detail: format!(
                "sysbox: runners run unprivileged{}",
                if config.is_none() {
                    " (the default)"
                } else {
                    ""
                }
            ),
            fix: None,
        },
        Isolation::UnsafeDind => Check {
            name: "isolation",
            status: Status::Warn,
            detail: "unsafe-dind: runners run privileged, so a job can take over the host"
                .to_owned(),
            fix: None,
        },
    });
    checks.push(match docker {
        None => skip("sysbox", "needs Docker"),
        Some(_) if isolation == Isolation::UnsafeDind => skip("sysbox", "unsafe-dind does not use it"),
        Some(docker) if !docker.runtimes.iter().any(|r| r == "sysbox-runc") => fail(
            "sysbox",
            "Docker has no sysbox-runc runtime, which isolation = \"sysbox\" needs".to_owned(),
            &sysbox_fix(&host.os_release),
        ),
        Some(_) if host.sysbox_services.iter().any(|(_, active)| !active) => {
            let stopped: Vec<_> = host
                .sysbox_services
                .iter()
                .filter(|(_, active)| !active)
                .map(|(unit, _)| *unit)
                .collect();
            fail(
                "sysbox",
                format!(
                    "Docker has the sysbox-runc runtime, but not running: {}; runner containers fail to start",
                    stopped.join(", ")
                ),
                "sudo systemctl enable --now sysbox",
            )
        }
        Some(_) => pass(
            "sysbox",
            "Docker has the sysbox-runc runtime and sysbox's services run".to_owned(),
        ),
    });
    checks.extend(slice_checks(host, config));
    checks.push(match &host.gh_scopes {
        Ok(scopes) if scopes.iter().any(|s| s == "repo") => pass(
            "gh",
            "logged in to github.com with the repo scope".to_owned(),
        ),
        Ok(scopes) => fail(
            "gh",
            format!(
                "the token has scopes {scopes:?} but not repo, which runner registration needs"
            ),
            "gh auth refresh --hostname github.com --scopes repo",
        ),
        Err(error) => fail(
            "gh",
            error.clone(),
            "install the GitHub CLI (https://cli.github.com), then:\n\
             gh auth login --hostname github.com --scopes repo",
        ),
    });
    checks.push(Check {
        name: "/dev/kvm",
        status: Status::Info,
        detail: if host.kvm {
            "present, for a future Kata Containers backend".to_owned()
        } else {
            "absent; only a future Kata Containers backend needs it".to_owned()
        },
        fix: None,
    });
    checks
}

fn slice_checks(host: &Host, config: Option<&Config>) -> Vec<Check> {
    let (Some(config), Some(slice)) = (config, &host.slice) else {
        return vec![
            skip("slice", "no config given, which names the slice"),
            skip("config", "no config given: runnerd doctor <runnerd.toml>"),
        ];
    };
    let name = &config.host.slice;
    let slice = match slice {
        Ok(slice) => slice,
        Err(error) => {
            return vec![
                fail("slice", error.clone(), "check that systemd runs"),
                skip("config", "needs the slice"),
            ];
        }
    };
    let slice_check = match (&slice.unit_file, slice.memory_max) {
        (Some(unit_file), Some(memory_max)) => pass(
            "slice",
            format!(
                "{name} ({unit_file}): MemoryMax={memory_max}, CPU quota {}",
                slice.cpu_quota
            ),
        ),
        _ => {
            let caps = config.memory_caps().map_or_else(
                |_| "<total of your caps>".to_owned(),
                |caps| caps.to_string(),
            );
            fail(
                "slice",
                format!(
                    "{name} has no unit file with a MemoryMax, so nothing holds runners to the budget"
                ),
                &format!(
                    "create /etc/systemd/system/{name} with\n\
                     [Slice]\n\
                     MemoryMax={caps}\n\
                     then: sudo systemctl daemon-reload"
                ),
            )
        }
    };
    let config_check = match config.validate(slice.memory_max) {
        Ok(warnings) if warnings.is_empty() => pass("config", format!("fits in {name}")),
        Ok(warnings) => Check {
            name: "config",
            status: Status::Pass,
            detail: format!(
                "valid, with warnings: {}",
                warnings
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            fix: None,
        },
        Err(error) => fail(
            "config",
            error.to_string(),
            "change runnerd.toml or the slice's limits",
        ),
    };
    vec![slice_check, config_check]
}

/// Sysbox ships `.deb` packages only; Arch has it in the AUR; elsewhere it is built from source.
fn sysbox_fix(os_release: &str) -> String {
    let family: Vec<&str> = os_release
        .lines()
        .filter_map(|line| {
            line.strip_prefix("ID=")
                .or_else(|| line.strip_prefix("ID_LIKE="))
        })
        .flat_map(|value| value.trim_matches('"').split_whitespace())
        .collect();
    let restart = "sudo systemctl restart docker   (stops running containers)";
    if family.iter().any(|id| matches!(*id, "debian" | "ubuntu")) {
        "install the .deb from https://github.com/nestybox/sysbox/releases; it registers the runtime and \
         restarts Docker, which stops running containers:\n\
         sudo apt-get install ./sysbox-ce_<version>.linux_amd64.deb"
            .to_owned()
    } else if family.contains(&"arch") {
        format!(
            "install the AUR package sysbox-ce-bin, for example: paru -S sysbox-ce-bin\n\
             sudo systemctl enable --now sysbox\n\
             add to /etc/docker/daemon.json (Arch kernels are not on sysbox's allow-list):\n\
             {{\"runtimes\": {{\"sysbox-runc\": {{\"path\": \"/usr/bin/sysbox-runc\", \"runtimeArgs\": [\"--no-kernel-check\"]}}}}}}\n\
             {restart}"
        )
    } else {
        format!(
            "build sysbox from source: \
             https://github.com/nestybox/sysbox/blob/master/docs/developers-guide/build.md\n\
             register sysbox-runc as a runtime in /etc/docker/daemon.json\n\
             {restart}"
        )
    }
}

fn pass(name: &'static str, detail: String) -> Check {
    Check {
        name,
        status: Status::Pass,
        detail,
        fix: None,
    }
}

fn fail(name: &'static str, detail: String, fix: &str) -> Check {
    Check {
        name,
        status: Status::Fail,
        detail,
        fix: Some(fix.to_owned()),
    }
}

fn skip(name: &'static str, detail: &str) -> Check {
    Check {
        name,
        status: Status::Skip,
        detail: detail.to_owned(),
        fix: None,
    }
}

#[cfg(test)]
mod tests;
