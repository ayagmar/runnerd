//! `runnerd init`: writes a starter `runnerd.toml` sized for this host and a systemd user unit that serves it, then
//! prints the doctor's checks and the steps left as commands. It overwrites neither file without `--force` and runs
//! nothing as root: the steps that need root are printed for the user to run.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use runnerd_core::api::{Check, CheckStatus};
use runnerd_core::config::{ByteSize, Config, PUBLISHED_RUNNER, slice_memory_max};

use crate::{doctor, output};

const GIB: u64 = 1 << 30;
/// The Docker Hub mirror's memory, as in `examples/runnerd.toml`.
const MIRROR_MEMORY: ByteSize = ByteSize(512 << 20);
/// A runner's memory when the budget has room for it; a busy JVM or Node runner peaked under 7G.
const SLOT_MEMORY: u64 = 8 * GIB;
/// Below this a runner cannot build much, so init refuses to write a config.
const MIN_SLOT_MEMORY: u64 = 2 * GIB;
/// Slot memory is a multiple of this.
const MEMORY_STEP: u64 = 512 << 20;
/// No more slots than one per this many CPUs.
const CPUS_PER_SLOT: u32 = 4;
/// The image stage the starter profile runs; every other stage builds on it.
const IMAGE_STAGE: &str = "base";

/// The published image of `IMAGE_STAGE` for this release, which runnerd pulls before the first runner.
fn starter_image() -> String {
    format!(
        "{PUBLISHED_RUNNER}:{IMAGE_STAGE}-{}",
        env!("CARGO_PKG_VERSION")
    )
}

/// What the starter profile is sized from.
pub(crate) struct Machine {
    /// The host's memory.
    pub memory: ByteSize,
    /// The CPUs this process may use.
    pub cpus: u32,
    /// The slice's `MemoryMax`; `None` when it has none yet.
    pub slice_memory_max: Option<ByteSize>,
}

impl Machine {
    pub(crate) fn read(slice: &str) -> Result<Machine, String> {
        let meminfo = std::fs::read_to_string("/proc/meminfo")
            .map_err(|error| format!("cannot read /proc/meminfo: {error}"))?;
        let memory = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|value| value.trim().strip_suffix("kB"))
            .and_then(|kib| kib.trim().parse::<u64>().ok())
            .ok_or("/proc/meminfo has no MemTotal")?;
        let cpus = std::thread::available_parallelism()
            .map_err(|error| format!("cannot count CPUs: {error}"))?;
        Ok(Machine {
            memory: ByteSize(memory << 10),
            cpus: u32::try_from(cpus.get()).unwrap_or(u32::MAX),
            slice_memory_max: slice_memory_max(slice).map_err(|error| error.to_string())?,
        })
    }
}

/// The starter profile and the budget it fits in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sizing {
    /// What the profile and the mirror fit in: three quarters of the host's memory, or the slice's `MemoryMax` when
    /// that is smaller. A slice without one should get this as its `MemoryMax`.
    pub budget: ByteSize,
    pub slots: u32,
    /// CPUs per slot.
    pub cpus: u32,
    /// Memory per slot.
    pub memory: ByteSize,
}

/// Sizes one profile to fill the budget: slots of up to 8G, at most one per 4 CPUs, sharing the CPUs.
pub(crate) fn size(machine: &Machine) -> Result<Sizing, String> {
    // A slice's MemoryMax is a limit, not memory the host has: never size past what the host can spare.
    let host = machine.memory.0 / 4 * 3 / MEMORY_STEP * MEMORY_STEP;
    let budget = ByteSize(
        machine
            .slice_memory_max
            .map_or(host, |memory_max| memory_max.0.min(host)),
    );
    let room = budget.0.saturating_sub(MIRROR_MEMORY.0);
    let memory = SLOT_MEMORY.min(room / MEMORY_STEP * MEMORY_STEP);
    if memory < MIN_SLOT_MEMORY {
        return Err(format!(
            "a budget of {budget} leaves {} for runners after the mirror's {MIRROR_MEMORY}; a runner needs at \
             least {}",
            ByteSize(room),
            ByteSize(MIN_SLOT_MEMORY)
        ));
    }
    let by_memory = u32::try_from(room / memory).unwrap_or(u32::MAX);
    let slots = by_memory.min((machine.cpus / CPUS_PER_SLOT).max(1));
    Ok(Sizing {
        budget,
        slots,
        cpus: (machine.cpus / slots).max(1),
        memory: ByteSize(memory),
    })
}

/// Whether `init` may replace files that exist.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overwrite {
    Refuse,
    Replace,
}

/// What `init` writes and where.
pub(crate) struct Request {
    /// `owner/name` of each repository to serve.
    pub repositories: Vec<String>,
    pub slice: String,
    /// The `runnerd` binary the unit runs.
    pub runnerd: PathBuf,
    pub config: PathBuf,
    pub unit: PathBuf,
    pub overwrite: Overwrite,
}

/// Writes the config and the unit for `request`, sized for `machine`. With `Overwrite::Refuse`, writes neither when
/// either exists.
pub(crate) fn init(request: &Request, machine: &Machine) -> Result<Sizing, String> {
    for repository in &request.repositories {
        let valid = repository.split_once('/').is_some_and(|(owner, name)| {
            [owner, name].iter().all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            })
        });
        if !valid {
            return Err(format!(
                "{repository:?} is not a GitHub repository; name each as owner/name"
            ));
        }
        if request
            .repositories
            .iter()
            .filter(|r| *r == repository)
            .count()
            > 1
        {
            return Err(format!("{repository} is named twice"));
        }
    }
    let slice_name = request.slice.strip_suffix(".slice").unwrap_or_default();
    if slice_name.is_empty()
        || !slice_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
    {
        return Err(format!(
            "--slice {:?} is not a systemd slice name, such as runnerd.slice",
            request.slice
        ));
    }
    let sizing = size(machine)?;
    let config = config_text(request, machine, &sizing);
    let unit = unit_text(&request.runnerd, &request.config)?;
    let mut existing = Vec::new();
    for path in [&request.config, &request.unit] {
        match path.symlink_metadata() {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "{} exists and is not a regular file; move it away first",
                    path.display()
                ));
            }
            Ok(_) => existing.push(path.display().to_string()),
            Err(_) => {}
        }
    }
    if request.overwrite == Overwrite::Refuse && !existing.is_empty() {
        return Err(format!(
            "{} already {}; pass --force to replace {}",
            existing.join(" and "),
            if existing.len() == 1 {
                "exists"
            } else {
                "exist"
            },
            if existing.len() == 1 { "it" } else { "them" }
        ));
    }
    let mut config = Staged::write(&request.config, &config)?;
    let mut unit = Staged::write(&request.unit, &unit)?;
    config.publish(request.overwrite)?;
    if let Err(error) = unit.publish(request.overwrite) {
        return Err(match config.undo() {
            Ok(()) => error,
            Err(undo) => format!("{error}; {undo}"),
        });
    }
    Ok(sizing)
}

/// Runs `create` on a fresh path in `dir` named after `name`, trying another name while one exists.
fn unique<T>(
    dir: &Path,
    name: &str,
    mut create: impl FnMut(&Path) -> std::io::Result<T>,
) -> std::io::Result<(PathBuf, T)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos());
    for attempt in 0..16 {
        let path = dir.join(format!(
            ".{name}.runnerd-init-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        match create(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            result => return result.map(|value| (path, value)),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// A file written in full next to its destination, so a failure before `publish` leaves the destination as it was.
/// Removes its temp file, and the backup of what it replaced, on drop.
struct Staged<'path> {
    path: &'path Path,
    temp: PathBuf,
    /// A link to the file `publish` replaced, kept until `undo` or drop.
    backup: Option<PathBuf>,
}

impl Staged<'_> {
    fn write<'path>(path: &'path Path, text: &str) -> Result<Staged<'path>, String> {
        let fail = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(fail(std::io::ErrorKind::InvalidInput.into()));
        };
        std::fs::create_dir_all(dir).map_err(fail)?;
        let (temp, mut file) = unique(dir, &name.to_string_lossy(), |temp| {
            OpenOptions::new().write(true).create_new(true).open(temp)
        })
        .map_err(fail)?;
        // Only now is the temp file ours to remove.
        let staged = Staged {
            path,
            temp,
            backup: None,
        };
        file.write_all(text.as_bytes()).map_err(fail)?;
        Ok(staged)
    }

    /// Moves the file into place: with `Overwrite::Refuse` only where nothing is, with `Replace` in one rename.
    fn publish(&mut self, overwrite: Overwrite) -> Result<(), String> {
        let fail = |error: std::io::Error| format!("cannot write {}: {error}", self.path.display());
        match overwrite {
            Overwrite::Refuse => std::fs::hard_link(&self.temp, self.path).map_err(fail),
            Overwrite::Replace => {
                // `init` refused anything but a regular file here.
                if self.path.symlink_metadata().is_ok() {
                    let dir = self.temp.parent().unwrap_or(Path::new("."));
                    let name = self.path.file_name().unwrap_or_default().to_string_lossy();
                    let (backup, ()) = unique(dir, &format!("{name}.old"), |backup| {
                        std::fs::hard_link(self.path, backup)
                    })
                    .map_err(fail)?;
                    self.backup = Some(backup);
                }
                std::fs::rename(&self.temp, self.path).map_err(fail)
            }
        }
    }

    /// Puts back what `publish` replaced, or removes what it created. When that fails, keeps the backup and names it.
    fn undo(&mut self) -> Result<(), String> {
        match self.backup.take() {
            Some(backup) => std::fs::rename(&backup, self.path).map_err(|error| {
                format!(
                    "restoring {} failed: {error}; the previous file is at {}",
                    self.path.display(),
                    backup.display()
                )
            }),
            None => std::fs::remove_file(self.path).map_err(|error| {
                format!("removing the new {} failed: {error}", self.path.display())
            }),
        }
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
        if let Some(backup) = &self.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
}

/// The labels each repository's runners register with: the ones GitHub gives its own self-hosted runners on this
/// host, so a workflow written for those runs here unchanged.
fn labels() -> Vec<&'static str> {
    let mut labels = vec!["self-hosted", "linux"];
    labels.extend(match std::env::consts::ARCH {
        "x86_64" => Some("x64"),
        "aarch64" => Some("arm64"),
        _ => None,
    });
    labels
}

/// The `runs-on` line that sends a workflow job to the runners of `labels`, and what it does.
fn runs_on(labels: &[&str]) -> String {
    format!(
        "\nSend a workflow job here with:\n\
         \x20 runs-on: [{}]\n\
         A job runs here only when every label in its runs-on is in its repository's labels in runnerd.toml; \
         other jobs, such as runs-on: ubuntu-latest, stay on GitHub's runners.",
        labels.join(", ")
    )
}

fn config_text(request: &Request, machine: &Machine, sizing: &Sizing) -> String {
    let Sizing {
        budget,
        slots,
        cpus,
        memory,
    } = sizing;
    let labels = labels()
        .iter()
        .map(|label| format!("{label:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut repositories = String::new();
    for name in &request.repositories {
        let _ = write!(
            repositories,
            "\n[[repositories]]\nname = \"{name}\"\nprofile = \"default\"\nlabels = [{labels}]\n"
        );
    }
    format!(
        "# Written by `runnerd init` for a host with {host_memory} and {host_cpus} CPUs. Check changes with\n\
         # `runnerd check-config`; every setting is in\n\
         # https://github.com/ayagmar/runnerd/blob/main/examples/runnerd.toml\n\
         \n\
         [host]\n\
         slice = \"{slice}\"\n\
         isolation = \"sysbox\"\n\
         \n\
         [caches]\n\
         mirror = {{ enabled = true, memory = \"{MIRROR_MEMORY}\", cpus = 1, ttl = \"168h\" }}\n\
         pnpm = {{ max_size = \"5G\" }}\n\
         maven = {{ max_size = \"5G\" }}\n\
         \n\
         # runnerd changes each profile's memory, slots and cpus from what its jobs use and how long they wait,\n\
         # a step at a time: https://ayagmar.github.io/runnerd/admission/#auto-sizing\n\
         [sizing]\n\
         auto = true\n\
         \n\
         # {slots} × {memory} + {MIRROR_MEMORY} for the mirror fits in {budget}: three quarters of the host's\n\
         # memory, or the slice's MemoryMax if that is smaller. runnerd pulls the image before the first runner.\n\
         [profiles.default]\n\
         image = \"{image}\"\n\
         cpus = {cpus}\n\
         memory = \"{memory}\"\n\
         slots = {slots}\n\
         {repositories}",
        image = starter_image(),
        host_memory = machine.memory,
        host_cpus = machine.cpus,
        slice = request.slice,
    )
}

/// A user unit running `runnerd serve` with `config`.
fn unit_text(runnerd: &Path, config: &Path) -> Result<String, String> {
    Ok(format!(
        "[Unit]\n\
         Description=runnerd: ephemeral GitHub Actions runners\n\
         Documentation=https://ayagmar.github.io/runnerd/\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={} serve --config {}\n\
         # Runner containers live in Docker, not in this unit: after a stop or crash the next start adopts them.\n\
         Restart=on-failure\n\
         RestartSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec_arg(runnerd)?,
        exec_arg(config)?
    ))
}

/// `path` as one quoted argument of `ExecStart=`, with specifiers and variables escaped.
fn exec_arg(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))?;
    if !path.is_absolute() || text.contains(['\n', '"', '\\']) {
        return Err(format!(
            "{text} cannot go in a systemd unit: it needs an absolute path without quotes, backslashes or newlines"
        ));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('%', "%%").replace('$', "$$")
    ))
}

/// Runs `runnerd init`: writes the files, then prints the doctor's checks and the steps left.
pub(crate) fn run(request: &Request) -> ExitCode {
    let machine = match Machine::read(&request.slice) {
        Ok(machine) => machine,
        Err(error) => return output::fail(&error),
    };
    let sizing = match init(request, &machine) {
        Ok(sizing) => sizing,
        Err(error) => return output::fail(&error),
    };
    output::line(format_args!(
        "wrote {}: {} slots of {} and {} CPUs in {} ({})",
        request.config.display(),
        sizing.slots,
        sizing.memory,
        sizing.cpus,
        request.slice,
        sizing.budget
    ));
    output::line(format_args!("wrote {}", request.unit.display()));
    if std::env::consts::ARCH != "x86_64" {
        output::warning(format_args!(
            "the published runner image is built for linux/amd64 only today, so runners do not start on this {} host",
            std::env::consts::ARCH
        ));
    }
    let config = match Config::load(&request.config) {
        Ok(config) => config,
        Err(error) => return output::fail(&error),
    };
    let host = doctor::Host::read(Some(&config));
    let mut checks = doctor::checks(&host, Some(&config));
    if let Some(Ok(slice)) = &host.slice {
        for check in &mut checks {
            if check.name == "slice" && check.status == CheckStatus::Fail {
                check.fix = Some(slice_fix(
                    &request.slice,
                    sizing.budget,
                    slice.unit_file.as_deref(),
                ));
            }
        }
    }
    output::line("\nrunnerd doctor:");
    for check in &checks {
        output::line(check);
    }
    output::line(next_steps(&checks));
    output::line(runs_on(&labels()));
    ExitCode::SUCCESS
}

/// The commands that hold runners to `budget`: a unit file for the slice, or a drop-in when it has one.
fn slice_fix(slice: &str, budget: ByteSize, unit_file: Option<&str>) -> String {
    let file = match unit_file {
        None => format!("/etc/systemd/system/{slice}"),
        Some(_) => format!("/etc/systemd/system/{slice}.d/runnerd.conf"),
    };
    let mkdir = match unit_file {
        None => String::new(),
        Some(_) => format!("sudo mkdir -p /etc/systemd/system/{slice}.d\n"),
    };
    format!(
        "{mkdir}sudo tee {file} >/dev/null <<'EOF'\n\
         [Slice]\n\
         MemoryMax={budget}\n\
         EOF\n\
         sudo systemctl daemon-reload"
    )
}

fn next_steps(checks: &[Check]) -> String {
    let failed: Vec<&str> = checks
        .iter()
        .filter(|check| check.status == CheckStatus::Fail)
        .map(|check| check.name.as_str())
        .collect();
    let fix_first = if failed.is_empty() {
        String::new()
    } else {
        format!(
            "\nFirst run the fixes above for {}; runnerd init runs nothing as root.",
            failed.join(", ")
        )
    };
    format!(
        "{fix_first}\n\
         Start runnerd and have it start at login; it pulls {image} before the first runner:\n\
         \x20 systemctl --user daemon-reload\n\
         \x20 systemctl --user enable --now runnerd\n\
         \x20 runnerctl status\n\
         To keep it running while you are logged out: loginctl enable-linger \"$USER\"",
        image = starter_image(),
    )
}

#[cfg(test)]
mod tests;
