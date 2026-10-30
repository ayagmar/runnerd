//! `runnerd doctor`: what the host is missing to run runners, each with the fix. It reads the host and changes
//! nothing; the user runs the fixes. The checks are `api::Check`s, so `GET /v1/doctor` serves the same ones.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use runnerd_core::admission::host;
use runnerd_core::api::{self, Check, CheckStatus as Status, Sizing};
use runnerd_core::config::{self, ByteSize, Config, Image, Isolation, PUBLISHED_RUNNER};
use runnerd_core::docker::{Docker, DockerApi as _};
use runnerd_core::observe::metrics::{self, Paths};
use runnerd_core::record;
use runnerd_core::sizing::{Window, advise};
use runnerd_core::store;

/// What doctor reads from the host, gathered before any check so the checks can be tested without one.
pub(crate) struct Host {
    pub docker: Result<DockerFacts, String>,
    /// What Docker has of each profile's image, by reference; empty without a config or Docker.
    pub images: BTreeMap<String, Result<ImageFound, String>>,
    /// The disk the records of failed jobs take; `None` without a config or Docker.
    pub records: Option<Result<u64, String>>,
    /// Sizing advice from the job history; `None` without a config or a history. `read` leaves it out: the daemon
    /// reads its own store, `runnerd doctor` the database with `read_sizing`.
    pub sizing: Option<Result<Sizing, String>>,
    /// `/etc/os-release`, empty when missing.
    pub os_release: String,
    /// `None` without a config, which names the slice.
    pub slice: Option<Result<Slice, String>>,
    /// The scopes of `gh`'s token for github.com.
    pub gh_scopes: Result<Vec<String>, String>,
    /// Each configured repository's visibility as GitHub names it (`public`, `private`, `internal`), by name; empty
    /// without a config.
    pub visibility: BTreeMap<String, Result<String, String>>,
    /// How each of `SYSBOX_DAEMONS` runs: Docker keeps the runtime registered without them.
    pub sysbox_daemons: [SysboxDaemon; 2],
    /// Whether systemd has `sysbox.service`, which Sysbox's packages install and a source build does not.
    pub sysbox_unit: bool,
    pub kvm: bool,
    /// The host's memory and how long its tasks stalled for it; `None` when `/proc/meminfo` does not say.
    pub memory: Option<api::HostMemory>,
    /// The config's file; `None` without one, or when it cannot be read.
    pub config_file: Option<ConfigFile>,
}

/// The file a config was read from, and who may read it.
pub(crate) struct ConfigFile {
    pub path: PathBuf,
    /// Its permission bits, of the file a symbolic link points to.
    pub mode: u32,
}

/// The daemons sysbox-runc needs; `sysbox.service` starts both, a source build's `scr/sysbox` too.
pub(crate) const SYSBOX_DAEMONS: [&str; 2] = ["sysbox-mgr", "sysbox-fs"];

/// One of `SYSBOX_DAEMONS` and how it runs.
#[derive(Clone, Copy)]
pub(crate) struct SysboxDaemon {
    pub name: &'static str,
    pub state: DaemonState,
}

/// How one of `SYSBOX_DAEMONS` runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonState {
    /// Its systemd unit is active.
    Unit,
    /// A process runs without its unit, as a source build's `scr/sysbox` starts it.
    Process,
    Stopped,
}

pub(crate) struct DockerFacts {
    pub version: String,
    pub cgroup_version: String,
    pub cgroup_driver: String,
    pub runtimes: Vec<String>,
    /// The platform images must be built for, such as `linux/amd64`.
    pub platform: String,
}

/// A profile's image as Docker has it, or where it could come from.
pub(crate) enum ImageFound {
    /// Docker has it; `version` is its `runnerd.version` label, which published images carry.
    Present { version: Option<String> },
    /// Docker lacks it and its registry has it for `platforms`, such as `linux/amd64`; none when the registry does
    /// not say. runnerd pulls it before the first runner if one is Docker's.
    Pullable { platforms: Vec<String> },
    /// Docker lacks it and cannot get it: never built, or the registry refused, with why.
    Missing(String),
}

/// The label of a published image that names the runnerd release it was built for (`.github/workflows/images.yml`).
/// Not `org.opencontainers.image.version`: every image built on the runner base inherits Ubuntu's.
const VERSION_LABEL: &str = "runnerd.version";

pub(crate) struct Slice {
    /// The slice's unit file; without one, systemd creates the slice without limits.
    pub unit_file: Option<String>,
    pub memory_max: Option<ByteSize>,
    /// `CPUQuotaPerSecUSec` as `systemctl show` prints it, such as `16s` or `infinity`.
    pub cpu_quota: String,
}

impl Host {
    /// The host, for `config` read from the file `path`.
    pub(crate) fn read(config: Option<&Config>, path: Option<&Path>) -> Host {
        let docker = read_docker();
        let gh_scopes = read_gh_scopes();
        Host {
            config_file: path.and_then(|path| {
                let metadata = std::fs::metadata(path).ok()?;
                Some(ConfigFile {
                    path: path.to_owned(),
                    mode: metadata.permissions().mode() & 0o7777,
                })
            }),
            images: match (config, &docker) {
                (Some(config), Ok(_)) => read_images(config),
                _ => BTreeMap::new(),
            },
            records: (config.is_some() && docker.is_ok()).then(read_records),
            sizing: None,
            docker,
            os_release: std::fs::read_to_string("/etc/os-release").unwrap_or_default(),
            slice: config.map(|config| read_slice(&config.host.slice)),
            visibility: config.map_or_else(BTreeMap::new, |config| {
                config
                    .repositories
                    .iter()
                    .map(|repository| {
                        let name = repository.name.clone();
                        let visibility = match &gh_scopes {
                            Ok(_) => read_visibility(&name),
                            Err(_) => Err("needs gh logged in to github.com".to_owned()),
                        };
                        (name, visibility)
                    })
                    .collect()
            }),
            gh_scopes,
            sysbox_daemons: SYSBOX_DAEMONS.map(|name| SysboxDaemon {
                name,
                state: read_daemon(name),
            }),
            sysbox_unit: Command::new("systemctl")
                .args(["show", "sysbox.service", "--property=LoadState", "--value"])
                .output()
                .is_ok_and(|output| {
                    output.status.success() && output.stdout.trim_ascii() == b"loaded"
                }),
            kvm: Path::new("/dev/kvm").exists(),
            memory: {
                let paths = Paths::default();
                let slice = config.map(|config| config.host.slice.as_str());
                let unavailable =
                    slice.map_or(0, |slice| metrics::read_unavailable(&paths.slice(slice)));
                metrics::read_host_memory(&paths, unavailable, 0).map(|memory| api::HostMemory {
                    holders: slice
                        .map_or_else(Vec::new, |slice| metrics::read_holders(&paths, slice)),
                    ..memory
                })
            },
        }
    }
}

/// Whether `name` runs as its systemd unit or, as a source build starts it, as a bare process.
fn read_daemon(name: &str) -> DaemonState {
    // One unit per call: `systemctl is-active` with several units succeeds when any one runs.
    let unit = Command::new("systemctl")
        .args(["is-active", "--quiet", name])
        .status()
        .is_ok_and(|status| status.success());
    if unit {
        return DaemonState::Unit;
    }
    if process_runs(Path::new("/proc"), name) {
        DaemonState::Process
    } else {
        DaemonState::Stopped
    }
}

/// Whether a live process named `name` is in `proc`, a procfs such as `/proc`. Any user may read a process's `stat`,
/// so this sees root's daemons too. A name is not proof of what runs: it only tells a source build's daemons from
/// none. Zombies and dead processes do not count, nor do processes that exit while this reads.
fn process_runs(proc: &Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let is_pid = entry
            .file_name()
            .to_str()
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()));
        // `<pid> (<comm>) <state> ...`; the comm may hold spaces and parentheses, the state follows the last `)`.
        is_pid
            && std::fs::read_to_string(entry.path().join("stat")).is_ok_and(|stat| {
                let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
                    return false;
                };
                let state = stat[close + 1..].trim_start().chars().next();
                open < close
                    && &stat[open + 1..close] == name
                    && state.is_some_and(|state| !matches!(state, 'Z' | 'X' | 'x'))
            })
    })
}

/// The advice from the job history at `database`, read-only, while no daemon runs; `None` when it does not exist.
pub(crate) fn read_sizing(
    config: &Config,
    database: &Path,
    memory_max: Option<u64>,
    now: u64,
) -> Option<Result<Sizing, String>> {
    let window = Window::new(None, now);
    match store::sizing_at(database, window.since) {
        Ok(history) => history.map(|history| Ok(advise(config, memory_max, &history, window))),
        Err(error) => Some(Err(error.to_string())),
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
        platform: format!(
            "{}/{}",
            info.os_type.unwrap_or_default(),
            // Docker reports the kernel's name for it; images name it as Go does.
            match info.architecture.unwrap_or_default().as_str() {
                "x86_64" => "amd64".to_owned(),
                "aarch64" => "arm64".to_owned(),
                other => other.to_owned(),
            }
        ),
    })
}

fn read_images(config: &Config) -> BTreeMap<String, Result<ImageFound, String>> {
    let images = images(config);
    let read = async |docker: &Docker, reference: &str, image: &Image| {
        let Some(mut labels) = docker.image_labels(reference).await? else {
            return Ok(match image {
                Image::Stage(_) => ImageFound::Missing("not built".to_owned()),
                Image::Pulled(_) => match docker.registry_platforms(reference).await {
                    Ok(platforms) => ImageFound::Pullable { platforms },
                    Err(error) => ImageFound::Missing(error.to_string()),
                },
            });
        };
        Ok::<_, runnerd_core::docker::Error>(ImageFound::Present {
            version: labels.remove(VERSION_LABEL),
        })
    };
    let read_all = || -> Result<_, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let docker = Docker::connect().map_err(|error| error.to_string())?;
        Ok(runtime.block_on(async {
            let mut found = BTreeMap::new();
            for (reference, image) in &images {
                let result = read(&docker, reference, image).await;
                found.insert(reference.clone(), result.map_err(|error| error.to_string()));
            }
            found
        }))
    };
    read_all().unwrap_or_else(|error| {
        images
            .keys()
            .map(|reference| (reference.clone(), Err(error.clone())))
            .collect()
    })
}

fn read_records() -> Result<u64, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let images = runtime
        .block_on(async { Docker::connect()?.images(record::LABEL).await })
        .map_err(|error| error.to_string())?;
    Ok(images.iter().map(|image| image.bytes).sum())
}

/// The config's distinct images, by reference.
fn images(config: &Config) -> BTreeMap<String, &Image> {
    config
        .profiles
        .values()
        .map(|profile| (profile.image.reference(), &profile.image))
        .collect()
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
            // Prefixed, so an account without scopes prints a line where none logged in prints nothing.
            r#".hosts["github.com"][]? | select(.active) | "active:" + (.scopes // "")"#,
        ])
        .output()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    gh_scopes(&output)
}

/// The scopes in what `read_gh_scopes`'s `gh auth status` printed: `Err` when no account is logged in, which `gh`
/// says on stderr while it succeeds.
fn gh_scopes(output: &std::process::Output) -> Result<Vec<String>, String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let scopes = stdout.trim().strip_prefix("active:");
    let (true, Some(scopes)) = (output.status.success(), scopes) else {
        let why = first_line(&output.stderr);
        return Err(if why.is_empty() {
            "gh is not logged in to github.com".to_owned()
        } else {
            format!("gh is not logged in to github.com: {why}")
        });
    };
    Ok(scopes
        .split(',')
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .collect())
}

/// The first line of a command's error output, where `gh` says what failed; the rest is advice.
fn first_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// What GitHub says of `repository`'s visibility, through `gh`.
fn read_visibility(repository: &str) -> Result<String, String> {
    let output = Command::new("gh")
        .args([
            "api",
            "--hostname",
            "github.com",
            &format!("repos/{repository}"),
            "--jq",
            ".visibility",
        ])
        .output()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh api repos/{repository} failed: {}",
            first_line(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Every check, in the order a new user fixes them.
#[allow(
    clippy::too_many_lines,
    reason = "one flat list of checks reads better than helpers per check"
)]
pub(crate) fn checks(host: &Host, config: Option<&Config>) -> Vec<Check> {
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
                "start Docker and let your user reach its socket; runnerd reaches Docker only through a unix:// \
                 socket, so unset DOCKER_HOST or point it at one:\n\
                 sudo systemctl enable --now docker\n\
                 sudo usermod -aG docker \"$USER\"   # then log in again",
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
            name: "isolation".to_owned(),
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
            name: "isolation".to_owned(),
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
            &sysbox_fix(&host.os_release, &docker.platform),
        ),
        Some(_)
            if host
                .sysbox_daemons
                .iter()
                .any(|daemon| daemon.state == DaemonState::Stopped) =>
        {
            let stopped: Vec<_> = host
                .sysbox_daemons
                .iter()
                .filter(|daemon| daemon.state == DaemonState::Stopped)
                .map(|daemon| daemon.name)
                .collect();
            fail(
                "sysbox",
                format!(
                    "Docker has the sysbox-runc runtime, but not running: {}; runner containers fail to start",
                    stopped.join(", ")
                ),
                if host.sysbox_unit {
                    "start sysbox's service:\nsudo systemctl enable --now sysbox"
                } else {
                    "Sysbox has no systemd unit here, as after a source build; start its daemons from the clone you \
                     built it in:\n\
                     cd sysbox\n\
                     sudo ./scr/sysbox"
                },
            )
        }
        Some(_)
            if host
                .sysbox_daemons
                .iter()
                .any(|daemon| daemon.state == DaemonState::Process) =>
        {
            pass(
            "sysbox",
            "Docker has the sysbox-runc runtime and sysbox's daemons run without systemd, so start them again \
             after a reboot: sudo ./scr/sysbox in the clone you built it in"
                .to_owned(),
            )
        }
        Some(_) => pass(
            "sysbox",
            "Docker has the sysbox-runc runtime and sysbox's services run".to_owned(),
        ),
    });
    checks.extend(slice_checks(host, config));
    checks.extend(host.config_file.as_ref().map(config_file_check));
    checks.push(memory_check(host.memory.as_ref(), config));
    checks.extend(image_checks(host, config, docker));
    if let (Some(config), Some(records)) = (config, &host.records) {
        checks.push(records_check(records, config));
    }
    if let Some(sizing) = &host.sizing {
        checks.push(sizing_check(sizing));
    }
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
    checks.extend(
        host.visibility
            .iter()
            .map(|(name, visibility)| visibility_check(name, visibility)),
    );
    if let Some(config) = config {
        checks.push(config.caches.build.actions_cache_check());
    }
    checks.push(Check {
        name: "/dev/kvm".to_owned(),
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
    let slice_check = if let (Some(unit_file), Some(memory_max)) =
        (&slice.unit_file, slice.memory_max)
    {
        pass(
            "slice",
            format!(
                "{name} ({unit_file}): MemoryMax={memory_max}, CPU quota {}",
                slice.cpu_quota
            ),
        )
    } else {
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
    };
    let config_check = config.check_against(slice.memory_max).doctor_check();
    vec![slice_check, config_check]
}

/// Warns about a public repository: whoever opens a pull request on it may get a job run here, if the repository's
/// settings let fork pull requests run workflows, and that job shares the caches with trusted ones. A warning, not a
/// refusal: GitHub can require approval for such runs.
fn visibility_check(name: &str, visibility: &Result<String, String>) -> Check {
    match visibility.as_deref() {
        Err(error) => skip("repository", &format!("{name}: cannot read its visibility: {error}")),
        Ok("public") => Check {
            name: "repository".to_owned(),
            status: Status::Warn,
            detail: format!(
                "{name} is public: anyone who opens a pull request on it can run code on this host, and their \
                 jobs share runnerd's caches with your trusted jobs"
            ),
            fix: Some(
                "serve private repositories only; for a public one, have GitHub require approval before it runs \
                 workflows from fork pull requests (the repository's Actions settings)"
                    .to_owned(),
            ),
        },
        Ok(visibility) => pass("repository", format!("{name}: {visibility}")),
    }
}

/// Warns when users other than its owner may read or write the config: the dashboard saves notification secrets in
/// it, such as a webhook's URL, and a save leaves it readable by its user only.
fn config_file_check(file: &ConfigFile) -> Check {
    let path = file.path.display();
    let shared = file.mode & 0o077;
    if shared == 0 {
        return pass("config file", format!("{path}: only you can read it"));
    }
    Check {
        name: "config file".to_owned(),
        status: Status::Warn,
        detail: format!(
            "{path} has mode {:o}, so other users can read it, and any notification secret the dashboard saves \
             in it",
            file.mode
        ),
        fix: Some(format!(
            "chmod 600 '{}'",
            path.to_string().replace('\'', "'\\''")
        )),
    }
}

/// One check per distinct image of the config's profiles.
fn image_checks(host: &Host, config: Option<&Config>, docker: Option<&DockerFacts>) -> Vec<Check> {
    let Some(config) = config else {
        return vec![skip("image", "no config given, which names the images")];
    };
    let Some(docker) = docker else {
        return vec![skip("image", "needs Docker")];
    };
    // The release publishes linux/amd64 images only (.github/workflows/images.yml).
    let published = docker.platform == "linux/amd64";
    let version = env!("CARGO_PKG_VERSION");
    images(config)
        .into_iter()
        .map(|(reference, image)| match host.images.get(&reference) {
            None => skip("image", &format!("{reference}: not checked")),
            Some(Err(error)) => fail(
                "image",
                format!("cannot inspect {reference}: {error}"),
                "check that Docker runs",
            ),
            Some(Ok(ImageFound::Present { version: Some(built) })) if built != version => Check {
                name: "image".to_owned(),
                status: Status::Warn,
                detail: format!(
                    "{reference} is built for runnerd {built}, this is runnerd {version}; its scripts may not \
                     match the daemon's"
                ),
                fix: Some(format!(
                    "set image to this release's tag, such as {PUBLISHED_RUNNER}:<stage>-{version}"
                )),
            },
            Some(Ok(ImageFound::Present { .. })) => pass("image", format!("{reference}: present")),
            Some(Ok(ImageFound::Pullable { platforms }))
                if platforms.is_empty() || platforms.contains(&docker.platform) =>
            {
                pass(
                    "image",
                    format!("{reference}: not pulled yet; runnerd pulls it before the first runner"),
                )
            }
            Some(Ok(ImageFound::Pullable { platforms })) => fail(
                "image",
                format!(
                    "{reference} is built for {} only, and Docker here runs {}",
                    platforms.join(", "),
                    docker.platform
                ),
                "build the image on this host from a clone of the repository and use it with image_stage:\n\
                 docker build --target <stage> -t runnerd-runner:<stage> image",
            ),
            Some(Ok(ImageFound::Missing(why))) => match image {
                Image::Stage(stage) => {
                    let build = format!(
                        "build it from a clone of the repository:\n\
                         docker build --target {stage} -t {reference} image"
                    );
                    let fix = if published {
                        format!(
                            "use the published image instead of image_stage:\n\
                             image = \"{PUBLISHED_RUNNER}:{stage}-{version}\"\nor {build}"
                        )
                    } else {
                        build
                    };
                    fail("image", format!("{reference}: {why}"), &fix)
                }
                Image::Pulled(_) => fail(
                    "image",
                    format!("{reference}: not present, and cannot be pulled: {why}"),
                    &format!("check the reference, then try: docker pull {reference}"),
                ),
            },
        })
        .collect()
}

/// The Sysbox release whose packages the fix names; newer ones are at <https://github.com/nestybox/sysbox/releases>.
const SYSBOX_VERSION: &str = "0.7.1";

/// How to install Sysbox on the distribution `os_release` describes, for Docker's `platform`, such as `linux/amd64`.
/// The first line says what to do; every line after it is a shell command, pasteable as printed. Nestybox packages
/// `.deb`s for Ubuntu and Debian, supports Fedora, `CentOS` Stream, Rocky, Alma and Amazon Linux built from source, and
/// Red Hat Enterprise Linux with Sysbox-EE only (docs/distro-compat.md); Arch has the `.deb` repackaged in the AUR.
fn sysbox_fix(os_release: &str, platform: &str) -> String {
    let value = |key: &str| {
        os_release.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?.trim();
            Some(
                ['"', '\'']
                    .iter()
                    .find_map(|quote| value.strip_prefix(*quote)?.strip_suffix(*quote))
                    .unwrap_or(value),
            )
        })
    };
    let id = value("ID").unwrap_or_default();
    let like: Vec<&str> = value("ID_LIKE")
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let is = |ids: &[&str]| ids.contains(&id) || like.iter().any(|like| ids.contains(like));
    let arch = platform.strip_prefix("linux/").unwrap_or(platform);
    let support = "https://github.com/nestybox/sysbox/blob/master/docs/distro-compat.md";
    if is(&["debian", "ubuntu"]) && matches!(arch, "amd64" | "arm64") {
        let deb = format!("sysbox-ce_{SYSBOX_VERSION}.linux_{arch}.deb");
        format!(
            "install Nestybox's package; it registers the runtime with Docker and restarts it, which stops \
             running containers:\n\
             curl -fLO https://github.com/nestybox/sysbox/releases/download/v{SYSBOX_VERSION}/{deb}\n\
             sudo apt-get install jq ./{deb}"
        )
    } else if is(&["arch"]) {
        "install the AUR package sysbox-ce-bin and register its runtime with Docker; Sysbox does not support Arch \
         officially, so the runtime skips its kernel check:\n\
         paru -S sysbox-ce-bin\n\
         sudo systemctl enable --now sysbox\n\
         # add to /etc/docker/daemon.json, merged with what it has:\n\
         #   {\"runtimes\": {\"sysbox-runc\": {\"path\": \"/usr/bin/sysbox-runc\", \"runtimeArgs\": [\"--no-kernel-check\"]}}}\n\
         sudo systemctl restart docker   # stops running containers"
            .to_owned()
    } else if id == "rhel" {
        format!(
            "Nestybox supports Red Hat Enterprise Linux with Sysbox-EE only, not the Sysbox CE that runnerd \
             uses: {support}"
        )
    } else if is(&["fedora", "centos", "rocky", "almalinux", "amzn", "rhel"]) {
        // Amazon Linux 2 has yum only; every other one has dnf, and Amazon Linux 2023 both.
        let install = if id == "amzn" { "yum" } else { "dnf" };
        format!(
            "Nestybox has no package for this distribution; build Sysbox from source \
             (https://github.com/nestybox/sysbox/blob/master/docs/developers-guide/build.md). The build runs in \
             Docker; the scripts need jq:\n\
             sudo {install} install -y git make jq\n\
             git clone --recursive https://github.com/nestybox/sysbox.git && cd sysbox\n\
             make sysbox-static\n\
             sudo make install\n\
             sudo ./scr/sysbox   # starts sysbox-mgr and sysbox-fs; a source build has no systemd unit\n\
             sudo ./scr/docker-cfg --sysbox-runtime=enable   # registers the runtime and restarts Docker"
        )
    } else {
        format!("see which distributions Sysbox supports and how to install it on each: {support}")
    }
}

/// Warns when the records near `[records] max_total` (`api::records_near_full`), past which new ones evict the oldest.
fn records_check(records: &Result<u64, String>, config: &Config) -> Check {
    let max_total = config.records.max_total;
    match records {
        Err(error) => skip("records", &format!("cannot list them: {error}")),
        Ok(bytes) if api::records_near_full(*bytes, max_total.0) => Check {
            name: "records".to_owned(),
            status: Status::Warn,
            detail: format!(
                "the records of failed jobs take {} of [records] max_total = {max_total}; \
                 new ones remove the oldest to fit",
                ByteSize(*bytes)
            ),
            fix: Some(
                "runnerctl records rm --older-than 2d, or raise max_total under [records]"
                    .to_owned(),
            ),
        },
        Ok(bytes) => Check {
            name: "records".to_owned(),
            status: Status::Info,
            detail: format!(
                "the records of failed jobs take {} of [records] max_total = {max_total}",
                ByteSize(*bytes)
            ),
            fix: None,
        },
    }
}

/// Warns when admission would hold runners back for the host's memory, by `host::first_runner` with the plain
/// margin, beside what the slice's runners hold now and the mirror's cap, which the engine reserves before the mirror
/// starts: while the host stalls short of memory, or while it has less to spare than the smallest profile's runner
/// may reserve. A running mirror counts twice, by what it holds. Admission keeps half a margin more after a wait and
/// while a stall lasts, so it can still hold jobs where this passes. With `[host] memory_ceiling` it counts what the
/// host can still grow by, as admission does (ADR 0084), and warns when the ceiling is under `MemTotal`, where it
/// counts for nothing, beside any other warning; a ceiling equal to `MemTotal` is silent.
fn memory_check(memory: Option<&api::HostMemory>, config: Option<&Config>) -> Check {
    let Some(memory) = memory else {
        return skip("host memory", "/proc/meminfo has no MemAvailable");
    };
    let ceiling = config.and_then(|config| config.host.memory_ceiling);
    let memory = &api::HostMemory {
        ceiling: ceiling.map(|ceiling| ceiling.0),
        ..memory.clone()
    };
    let stalled = |pressure: Option<api::Pressure>| {
        pressure.map_or_else(
            || "unknown: the kernel has no pressure stall information".to_owned(),
            |pressure| {
                format!(
                    "{} of the last 10 s, {} of the last minute",
                    api::percent(pressure.avg10),
                    api::percent(pressure.avg60)
                )
            },
        )
    };
    let mirror = config
        .map(|config| &config.caches.mirror)
        .filter(|mirror| mirror.enabled)
        .map_or(0, |mirror| mirror.memory.0);
    let spare = host::first_runner(memory, memory.slice.saturating_add(mirror));
    let stalls = host::stalls(memory, false);
    let detail = format!(
        "{}, {} to spare for runners past a {} margin{}; every task waited for memory {}",
        memory.available_line(),
        ByteSize(spare),
        ByteSize(host::margin(memory.grown(), false)),
        if mirror > 0 {
            format!(" and the mirror's {}", ByteSize(mirror))
        } else {
            String::new()
        },
        stalled(memory.full)
    );
    let smallest = config.and_then(|config| {
        config
            .profiles
            .iter()
            .min_by_key(|(_, profile)| profile.memory)
    });
    // At MemTotal the ceiling says the host is at its most, which is no mistake; only under it is it one.
    let unused = ceiling.filter(|ceiling| ceiling.0 < memory.total).map(|ceiling| {
        format!(
            "[host] memory_ceiling = \"{ceiling}\" is under the host's {}, so admission goes by MemTotal and the \
             ceiling counts for nothing",
            ByteSize(memory.total)
        )
    });
    let why = if stalls {
        Some(
            "the host stalls short of memory, so runnerd starts no runner until the stall passes"
                .to_owned(),
        )
    } else {
        smallest
            .filter(|(_, profile)| spare < profile.memory.0)
            .map(|(name, profile)| {
                format!(
                    "a {name} runner may reserve up to {}, more than the host has to spare, so runners wait for \
                     the host's memory",
                    profile.memory
                )
            })
    };
    let ceiling_fix = "set [host] memory_ceiling to the most memory the host can grow to, or remove it on a host \
                       whose memory is fixed";
    let (detail, fix) = match (why, unused) {
        (None, None) => return pass("host memory", detail),
        (None, Some(unused)) => (format!("{detail}; {unused}"), ceiling_fix.to_owned()),
        (Some(why), unused) => {
            let short = "close what uses the host's memory beside the runners; runnerd waits for it by itself";
            let detail = format!("{detail}; {}: {why}", memory.outside_line());
            match unused {
                Some(unused) => (
                    format!("{detail}; {unused}"),
                    format!("{short}; {ceiling_fix}"),
                ),
                None => (detail, short.to_owned()),
            }
        }
    };
    Check {
        name: "host memory".to_owned(),
        status: Status::Warn,
        detail,
        fix: Some(fix),
    }
}

/// Warns when a profile's jobs need more than its cap or than the slice holds; says which caps could be lowered.
fn sizing_check(sizing: &Result<Sizing, String>) -> Check {
    let sizing = match sizing {
        Ok(sizing) => sizing,
        Err(error) => return skip("sizing", &format!("cannot read the job history: {error}")),
    };
    sizing.doctor_check()
}

fn pass(name: &str, detail: String) -> Check {
    Check {
        name: name.to_owned(),
        status: Status::Pass,
        detail,
        fix: None,
    }
}

fn fail(name: &str, detail: String, fix: &str) -> Check {
    Check {
        name: name.to_owned(),
        status: Status::Fail,
        detail,
        fix: Some(fix.to_owned()),
    }
}

fn skip(name: &str, detail: &str) -> Check {
    Check {
        name: name.to_owned(),
        status: Status::Skip,
        detail: detail.to_owned(),
        fix: None,
    }
}

#[cfg(test)]
mod tests;
