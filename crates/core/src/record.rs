//! Records of failed jobs: which runner containers may be kept, the files a record must never hold, which records go
//! to keep within `[records]`, what a shell in a record shows first, and whether a failed job's log says its runner
//! failed it (`runner_fault`). Pure functions: the engine
//! (`scheduler::engine`) commits, lists and removes the images, and the daemon serves the shell.
//!
//! A record is the runner container's own layer, committed as the image `runnerd-record:<job>` once the runner
//! exited: the workspace and everything else the job wrote outside the slot's volumes. When the runner exits, the
//! runner image's entrypoint deletes what the job added or changed among the runner's dot-files and in the temporary
//! directories, and the step scripts (`image/scrub.sh`); before a commit, `secrets_left` checks the container's
//! changed paths and the engine keeps no record that still holds one.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::api;
use crate::config;

/// The repository of every record image; the tag is the job's ID.
pub const IMAGE: &str = "runnerd-record";

/// The label every record image carries, with the job's ID.
pub const LABEL: &str = "runnerd.record";

/// The label every shell container carries, with the job's ID.
pub const SHELL_LABEL: &str = "runnerd.shell";

/// The image of a job's record.
pub fn image(job: u64) -> String {
    format!("{IMAGE}:{job}")
}

/// What the runner prints when its job failed. Its exit code says nothing about the job.
const FAILED: &str = "completed with result: Failed";

/// Whether a runner's output says its job failed.
pub fn failed(runner_log: &str) -> bool {
    runner_log.lines().any(|line| line.ends_with(FAILED))
}

/// The runner's home, whose dot-files (`.runner`, `.credentials`, `.gitconfig`, `.aws`, `.gnupg`, …) a job's changes
/// must not leave in a record: the runner keeps its registration and credentials there, and tools their logins and
/// keys.
const HOME: &str = "/home/runner/";

/// Directories nothing a job changed in may be kept: the JIT config's, the step scripts' (`GITHUB_ENV` and the
/// credentials `actions/checkout` and `setup-node` leave), and the temporary directories, where a job may put a key
/// or a kubeconfig.
const SECRET_DIRS: &[&str] = &[
    "/run/runnerd",
    "/home/runner/_work/_temp",
    "/tmp",
    "/var/tmp",
];

/// Directories among the runner's dot-files a record keeps, as the runner image's scrub does: the Rust toolchains
/// rustup and cargo install (`dtolnay/rust-toolchain`), which a shell needs to re-run a step, and cargo's downloaded
/// crates and git checkouts, which hold nothing a workspace's `Cargo.lock` does not name. Not `~/.cargo`'s
/// `credentials.toml` or `config.toml`, which may hold a registry's token.
const HARMLESS: &[&str] = &[
    "/home/runner/.rustup",
    "/home/runner/.cargo/bin",
    "/home/runner/.cargo/registry",
    "/home/runner/.cargo/git",
];

/// Whether `path` is in a `HARMLESS` directory, or is one of them or a parent of one that `changed` shows a path in:
/// a directory's own entry holds nothing.
fn harmless(path: &str, changed: &[&str]) -> bool {
    let below = |dir: &str, path: &str| {
        path.strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    let on_the_way = HARMLESS.iter().any(|dir| *dir == path || below(path, dir));
    HARMLESS.iter().any(|dir| below(dir, path))
        || on_the_way && changed.iter().any(|other| below(path, other))
}

/// The changed paths of a runner container that a record must not hold: anything added or modified among the
/// runner's dot-files but the `HARMLESS` toolchains, in the temporary directories, or where the runner keeps the
/// JIT config and step scripts. A record of it must not be made. `changed` are the paths Docker reports added or
/// modified.
pub fn secrets_left<'path>(changed: impl IntoIterator<Item = &'path str>) -> Vec<String> {
    let changed: Vec<&str> = changed.into_iter().collect();
    changed
        .iter()
        .copied()
        .filter(|path| !harmless(path, &changed))
        .filter(|path| {
            path.strip_prefix(HOME)
                .is_some_and(|rest| rest.starts_with('.'))
                || SECRET_DIRS.iter().any(|dir| {
                    path.strip_prefix(dir)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
        })
        .map(str::to_owned)
        .collect()
}

/// The first few of `paths`, and how many more there are: a container without the scrub may have changed thousands.
pub fn listed(paths: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = paths
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > SHOWN {
        write!(text, " and {} more", paths.len() - SHOWN).ok();
    }
    text
}

/// Which records go, and why, before a new record of `incoming` bytes is made; or why it is not made. Records in
/// `open` have a shell open and stay. `free` is the disk Docker has left, when known. Expired records go too.
pub fn make_room(
    records: &[api::Record],
    open: &BTreeSet<u64>,
    limits: &config::Records,
    incoming: u64,
    free: Option<u64>,
    now: u64,
) -> Result<Vec<(u64, api::RecordRemoval)>, String> {
    if incoming > limits.max_total.0 {
        return Err(format!(
            "it would take {}, more than records.max_total ({})",
            config::ByteSize(incoming),
            limits.max_total
        ));
    }
    let mut removed = expired(records, open, limits, now);
    let mut kept: Vec<&api::Record> = records
        .iter()
        .filter(|record| !removed.iter().any(|(job, _)| *job == record.job))
        .collect();
    kept.sort_by_key(|record| (record.created_at, record.job));
    let mut removable = kept
        .clone()
        .into_iter()
        .filter(|record| !open.contains(&record.job));
    let max_count = usize::try_from(limits.max_count).unwrap_or(usize::MAX);
    let mut count = kept.len();
    let mut total: u64 = kept.iter().map(|record| record.bytes).sum();
    while count >= max_count || total + incoming > limits.max_total.0 {
        let why = if count >= max_count {
            api::RecordRemoval::Count
        } else {
            api::RecordRemoval::Size
        };
        let Some(oldest) = removable.next() else {
            return Err("the records kept within the limits have shells open".to_owned());
        };
        removed.push((oldest.job, why));
        count -= 1;
        total -= oldest.bytes;
    }
    if let Some(free) = free {
        let freed: u64 = records
            .iter()
            .filter(|record| removed.iter().any(|(job, _)| *job == record.job))
            .map(|record| record.bytes)
            .sum();
        if (free + freed).saturating_sub(incoming) < limits.min_free.0 {
            return Err(format!(
                "Docker has {} free; a record of {} would leave less than records.min_free ({})",
                config::ByteSize(free),
                config::ByteSize(incoming),
                limits.min_free
            ));
        }
    }
    Ok(removed)
}

/// The records older than `max_age` at `now`, but those with a shell open.
pub fn expired(
    records: &[api::Record],
    open: &BTreeSet<u64>,
    limits: &config::Records,
    now: u64,
) -> Vec<(u64, api::RecordRemoval)> {
    records
        .iter()
        .filter(|record| {
            !open.contains(&record.job)
                && now.saturating_sub(record.created_at) > limits.max_age.as_secs()
        })
        .map(|record| (record.job, api::RecordRemoval::Age))
        .collect()
}

/// The directory a job's steps run in by default: `GITHUB_WORKSPACE`.
pub fn workspace(repository: &str) -> String {
    let name = repository.rsplit('/').next().unwrap_or(repository);
    format!("/home/runner/_work/{name}/{name}")
}

/// What a shell in a job's record prints before its prompt: the job, the step (`step`, or the one that failed),
/// what the step ran as its log shows it, and what the record lacks. `log` is the job's stored log. Lines end in
/// `\r\n`, for a terminal in raw mode.
pub fn banner(job: &api::Job, log: &[String], step: Option<u32>) -> String {
    let summary = &job.summary;
    let mut text = format!(
        "runnerd: the record of job {} of {}",
        summary.id, summary.repository
    );
    match &summary.workflow {
        Some(workflow) => write!(text, ", {workflow} / {}", summary.name),
        None => write!(text, ", {}", summary.name),
    }
    .ok();
    text.push('\n');
    let failed = job
        .steps
        .iter()
        .find(|candidate| candidate.conclusion.as_deref() == Some("failure"));
    let shown = match step {
        Some(number) => job
            .steps
            .iter()
            .find(|candidate| candidate.number == number),
        None => failed,
    };
    let sections = sections(log);
    match shown {
        Some(shown) => {
            let verb = if failed.is_some_and(|failed| failed.number == shown.number) {
                "failed at"
            } else {
                "shows"
            };
            write!(text, "It {verb} step {}: {}", shown.number, shown.name).ok();
            let section = section_of(job, &sections, shown);
            if let Some(code) = section.and_then(|section| section.exit_code) {
                write!(text, " (exit code {code})").ok();
            }
            text.push('\n');
            match section {
                Some(section) => {
                    text.push_str("Its log shows it ran:\n");
                    for line in &section.lines {
                        writeln!(text, "    {line}").ok();
                    }
                }
                None => text.push_str("Its log does not show what it ran.\n"),
            }
        }
        None if step.is_some() => text.push_str("The job has no such step.\n"),
        None => text.push_str("The job's steps do not say which one failed.\n"),
    }
    writeln!(
        text,
        "You are in {}, as the job left it. Not kept: the job's Docker containers and images; what it changed \
         in ~/.cache, the tool cache and the Android SDK, which show the profile's snapshot; and what it added or \
         changed among your home's dot-files and in /tmp (logins, and tools in ~/.local or ~/.nvm), except \
         ~/.rustup and ~/.cargo's bin, registry and git. Exit to remove this container.",
        workspace(&summary.repository)
    )
    .ok();
    text.replace('\n', "\r\n")
}

/// A top-level `##[group]Run …` section of a job's log: one step that ran.
#[derive(Debug, Default)]
struct Section {
    /// Its title without `Run `.
    title: String,
    /// The group's lines: what the step ran, its shell and its environment.
    lines: Vec<String>,
    /// What the step printed after its group, up to `Post job cleanup.`.
    output: Vec<String>,
    /// From `##[error]Process completed with exit code N.`.
    exit_code: Option<i64>,
}

/// The `##[group]Run …` sections of a log, in order.
fn sections(log: &[String]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let (mut in_group, mut cleaning_up) = (false, false);
    for line in log {
        let text = plain(line);
        if let Some(title) = text.strip_prefix("##[group]Run ") {
            sections.push(Section {
                title: title.to_owned(),
                ..Section::default()
            });
            (in_group, cleaning_up) = (true, false);
        } else if text.starts_with("##[endgroup]") {
            in_group = false;
        } else if text == "Post job cleanup." {
            // The post steps print after the last step, and are none of its.
            cleaning_up = true;
        } else if let Some(section) = sections.last_mut().filter(|_| !cleaning_up) {
            if in_group {
                section.lines.push(text);
            } else {
                if let Some(code) = text
                    .strip_prefix("##[error]Process completed with exit code ")
                    .and_then(|code| code.strip_suffix('.'))
                    .and_then(|code| code.parse().ok())
                {
                    section.exit_code.get_or_insert(code);
                }
                section.output.push(text);
            }
        }
    }
    sections
}

/// The section of a step: the one titled as the step is named (`Run cargo test`), else for a failed step the one
/// that ended with an exit code, else the one at the step's place among the steps that ran after `Set up job`.
fn section_of<'log>(
    job: &api::Job,
    sections: &'log [Section],
    step: &api::Step,
) -> Option<&'log Section> {
    if let Some(title) = step.name.strip_prefix("Run ")
        && let Some(section) = sections.iter().find(|section| section.title == title)
    {
        return Some(section);
    }
    if step.conclusion.as_deref() == Some("failure")
        && let Some(section) = sections.iter().rfind(|section| section.exit_code.is_some())
    {
        return Some(section);
    }
    let place = job
        .steps
        .iter()
        .filter(|candidate| {
            candidate.number > 1
                && !matches!(candidate.conclusion.as_deref(), Some("skipped") | None)
        })
        .position(|candidate| candidate.number == step.number)?;
    sections.get(place)
}

/// Whether a failed job's log says its runner failed it, not its code: the output of the step GitHub reports as
/// `failure` has a line with `function not implemented` on a mount, umount or openat2, or Sysbox's `unsafe procfs
/// detected … invalid argument`. Sysbox answers a trapped call with `ENOSYS` once it lost the runner's seccomp
/// listener, and with `EINVAL` when a descriptor it injected was interrupted; no retry in the same runner helps, and
/// the job's code is not at fault. The step is found as `banner` finds the failed step's section, and only what it
/// printed counts: not the script and environment the runner echoes before it, not the output of a step that
/// passed or was allowed to fail, nor what a post step printed. A step that prints its own `::group::Run …` line cuts
/// its output short there. `log` is the stored lines.
pub fn runner_fault(job: &api::Job, log: &[String]) -> bool {
    let Some(failed) = job
        .steps
        .iter()
        .find(|step| step.conclusion.as_deref() == Some("failure"))
    else {
        return false;
    };
    section_of(job, &sections(log), failed)
        .is_some_and(|section| section.output.iter().any(|line| sysbox_fault(line)))
}

/// What failed a failed job apart from its code, as its log shows it, read only where the runner reports it
/// (actions/runner 2.338's `ActionManager` and `JobExtension`):
///
/// - [`api::FailureCause::ActionArchive`]: `Set up job` failed, and the last error it printed, the one the runner ends
///   it with, says an action could not be downloaded or unpacked (`Failed to download archive '…' after 3 attempts.`,
///   `Can't use 'tar -xzf'`, `Can't un-zip archive file`, a download that timed out).
/// - [`api::FailureCause::ActionResolve`]: `Set up job` failed, and that last error says an action could not be
///   resolved (`Failed to resolve action download info.`, `Unable to resolve action`).
/// - [`api::FailureCause::Checkout`]: the step that failed is `actions/checkout`, by its own section of the log: the
///   section at its place among the job's steps, when the log has one section a step, that runs the action, not a
///   script, and that printed the checkout's `Syncing repository:` and an error.
///
/// A warning a retry printed is no cause: a retry that succeeded prints the same, and what fails after it is not
/// this. `None` for anything else: a job that did not fail, a post step, or a log that does not say. `log` is the
/// stored lines.
pub fn failure_cause(job: &api::Job, log: &[String]) -> Option<api::FailureCause> {
    let failed = job
        .steps
        .iter()
        .find(|step| step.conclusion.as_deref() == Some("failure"))?;
    if failed.name == SET_UP {
        return setup_failure(&setup_lines(log));
    }
    let section = own_section(job, log, failed)?;
    let runs_checkout = section
        .title
        .to_ascii_lowercase()
        .strip_prefix("actions/checkout@")
        .is_some_and(|reference| !reference.is_empty() && !reference.contains(' '));
    let script = section
        .lines
        .iter()
        .any(|line| line.trim_start().starts_with("shell: "));
    let fetched = section
        .output
        .iter()
        .any(|line| line.starts_with("Syncing repository: "));
    let errored = section
        .output
        .iter()
        .any(|line| line.starts_with("##[error]"));
    (runs_checkout && !script && fetched && errored).then_some(api::FailureCause::Checkout)
}

/// The name GitHub gives the step a job sets up in: the runner prepares its actions there.
const SET_UP: &str = "Set up job";

/// The plain lines a log has before its first step's group: what `Set up job` printed.
fn setup_lines(log: &[String]) -> Vec<String> {
    log.iter()
        .map(|line| plain(line))
        .take_while(|text| !text.starts_with("##[group]Run ") && text != "Post job cleanup.")
        .collect()
}

/// The cause of a failed `Set up job`, from the last error it printed (`failure_cause`).
fn setup_failure(lines: &[String]) -> Option<api::FailureCause> {
    let error = lines
        .iter()
        .rev()
        .find(|text| text.starts_with("##[error]"))?;
    let archive = [
        "Failed to download archive '",
        "Failed to download action '",
        "Can't use 'tar -xzf'",
        "Can't un-zip archive file",
        "download has timed out",
    ];
    let resolve = [
        "Failed to resolve action download info",
        "Unable to resolve action",
    ];
    if archive.iter().any(|sign| error.contains(sign)) {
        Some(api::FailureCause::ActionArchive)
    } else if resolve.iter().any(|sign| error.contains(sign)) {
        Some(api::FailureCause::ActionResolve)
    } else {
        None
    }
}

/// The section of the log a failed main step printed, by its place: only when the log has exactly one `##[group]Run`
/// section for each step that ran between `Set up job` and the post steps, as a composite action's inner steps or a
/// step's own groups would shift the places. `None` for a post step, or when the places do not line up.
fn own_section(job: &api::Job, log: &[String], failed: &api::Step) -> Option<Section> {
    if failed.name.starts_with("Post ") {
        return None;
    }
    let main: Vec<&api::Step> = job
        .steps
        .iter()
        .filter(|step| {
            step.name != SET_UP
                && !step.name.starts_with("Post ")
                && !step.name.starts_with("Complete job")
                && !matches!(step.conclusion.as_deref(), Some("skipped") | None)
        })
        .collect();
    let place = main.iter().position(|step| step.number == failed.number)?;
    let mut sections = sections(&main_lines(log));
    if sections.len() != main.len() {
        return None;
    }
    Some(sections.swap_remove(place))
}

/// The lines of a log before its post steps: everything up to its first `Post job cleanup.`.
fn main_lines(log: &[String]) -> Vec<String> {
    log.iter()
        .take_while(|line| plain(line) != "Post job cleanup.")
        .cloned()
        .collect()
}

/// What a call's name is in a line of Sysbox's trouble: the words of `mount`'s family and `openat2`.
const FAULT_CALLS: [&str; 6] = [
    "mount", "umount", "umount2", "unmount", "mounting", "openat2",
];

/// Whether a log line is what a runner whose Sysbox lost its seccomp listener, or failed an injection, prints. The
/// call is a word of its own, between spaces, colons, brackets, quotes and commas: a path or a name that holds
/// `mount`, as `amount-api`, `mounted`, `mount-api/a.txt`, `mount.c` or `test_mount`, is not one.
fn sysbox_fault(text: &str) -> bool {
    let text = text.to_lowercase();
    (text.contains("function not implemented")
        && text
            .split(|c: char| c.is_whitespace() || ":()\"',".contains(c))
            .any(|word| FAULT_CALLS.contains(&word)))
        || (text.contains("unsafe procfs detected") && text.contains("invalid argument"))
}

/// The repositories `lines`, stored lines of a job's log, show the runner downloading as an immutable action package
/// (actions/runner 2.338's `##[group]Download immutable action package 'owner/repo@ref'`), each once, as written.
/// The runner prints it before it looks in an action archive cache, so it shows a package resolution whether or not
/// it then read an archive in the package's place.
pub fn immutable_packages(lines: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in lines {
        let text = plain(line);
        let Some(rest) = text.strip_prefix("##[group]Download immutable action package '") else {
            continue;
        };
        let Some((name, _)) = rest.split_once('@') else {
            continue;
        };
        let mut parts = name.split('/');
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            continue;
        };
        let repository = format!("{owner}/{repo}");
        if !found.contains(&repository) {
            found.push(repository);
        }
    }
    found
}

/// A stored log line without its timestamp and colour codes.
fn plain(line: &str) -> String {
    let text = match line.split_once(' ') {
        Some((stamp, rest)) if stamp.ends_with('Z') && stamp.contains('T') => rest,
        _ => line,
    };
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // An SGR sequence: ESC [ … m.
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

#[cfg(test)]
mod tests;
