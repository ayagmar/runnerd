//! Records of failed jobs: which runner containers may be kept, the files a record must never hold, which records go
//! to keep within `[records]`, and what a shell in a record shows first. Pure functions: the engine
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
    /// From `##[error]Process completed with exit code N.`.
    exit_code: Option<i64>,
}

/// The `##[group]Run …` sections of a log, in order.
fn sections(log: &[String]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut in_group = false;
    for line in log {
        let text = plain(line);
        if let Some(title) = text.strip_prefix("##[group]Run ") {
            sections.push(Section {
                title: title.to_owned(),
                ..Section::default()
            });
            in_group = true;
        } else if text.starts_with("##[endgroup]") {
            in_group = false;
        } else if let Some(section) = sections.last_mut() {
            if in_group {
                section.lines.push(text);
            } else if let Some(code) = text
                .strip_prefix("##[error]Process completed with exit code ")
                .and_then(|code| code.strip_suffix('.'))
                .and_then(|code| code.parse().ok())
            {
                section.exit_code.get_or_insert(code);
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
