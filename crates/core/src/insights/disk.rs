//! What the jobs of a window and the drive under Docker's root wrote (ADR 0143): the jobs' figures together, the
//! workflows and jobs that wrote most, and the drive's windows the daemon read, set against the jobs that ended in
//! them; and how long `[host.drive]`'s rating lasts at the rates those windows read (ADR 0157). Pure.

use super::{DAY, DriveRead, Run};
use crate::api::{
    Commits, DiskWrites, DriveOwners, DriveShares, DriveWrites, Endurance, Insight, InsightKind,
    JobWrites, Measure, RateSpan, Severity, WriteRate, WriteTop, WrittenSince,
};
use crate::config::Drive;
use crate::observe::disk::Window;

/// The workflows, and the jobs, the section names at most.
const TOP: usize = 5;

/// What `window`'s jobs and the drive's `windows` within `since` to `now` (ms) wrote; `None` when no job was measured
/// and the daemon read no drive.
pub(super) fn writes(
    window: &[&Run],
    windows: &[Window],
    since: u64,
    now: u64,
) -> Option<DiskWrites> {
    let measured: Vec<(&Run, u64)> = window
        .iter()
        .filter_map(|run| Some((*run, run.disk?.bytes)))
        .collect();
    let drive = drive(&measured, windows, since, now);
    if measured.is_empty() && drive.is_none() {
        return None;
    }
    let mut bytes: Vec<u64> = measured.iter().map(|(_, bytes)| *bytes).collect();
    bytes.sort_unstable();
    let rank = |percent: usize| {
        let index = (bytes.len() * percent).div_ceil(100).saturating_sub(1);
        bytes.get(index).copied().unwrap_or(0)
    };
    let jobs = JobWrites {
        measured: u32::try_from(measured.len()).unwrap_or(u32::MAX),
        partial: u32::try_from(
            measured
                .iter()
                .filter(|(run, _)| {
                    run.disk
                        .is_some_and(|disk| disk.measure != Measure::Complete)
                })
                .count(),
        )
        .unwrap_or(u32::MAX),
        total: bytes.iter().sum(),
        median: rank(50),
        p90: rank(90),
        dirty: measured.iter().filter_map(|(run, _)| run.disk?.dirty).sum(),
    };
    let top = |job: fn(&Run) -> Option<String>| {
        let mut top: Vec<WriteTop> = Vec::new();
        for (run, bytes) in &measured {
            let job = job(run);
            let at = top.iter().position(|top| {
                top.repository == run.repository && top.workflow == run.workflow && top.job == job
            });
            let at = at.unwrap_or_else(|| {
                top.push(WriteTop {
                    repository: run.repository.clone(),
                    workflow: run.workflow.clone(),
                    job,
                    jobs: 0,
                    total: 0,
                });
                top.len() - 1
            });
            top[at].jobs += 1;
            top[at].total = top[at].total.saturating_add(*bytes);
        }
        top.sort_by_key(|top| std::cmp::Reverse(top.total));
        top.truncate(TOP);
        top
    };
    Some(DiskWrites {
        jobs,
        workflows: top(|_| None),
        top_jobs: top(|run| Some(run.name.clone())),
        drive,
        endurance: None,
    })
}

/// The drive's windows that end within `since` to `now` and read it, added up, with whose its bytes were over the
/// windows that also read the runner slice and split the rest by cgroup (ADR 0157), or, when none did, over those that
/// read the slice, or over all of them when none did that either.
fn drive(
    measured: &[(&Run, u64)],
    windows: &[Window],
    since: u64,
    now: u64,
) -> Option<DriveWrites> {
    let read: Vec<&Window> = windows
        .iter()
        .filter(|window| (since..=now).contains(&window.end) && window.drive.is_some())
        .collect();
    let last = read.last()?;
    let read_slice: Vec<&Window> = read
        .iter()
        .copied()
        .filter(|window| window.slice.is_some())
        .collect();
    let split: Vec<&Window> = read_slice
        .iter()
        .copied()
        .filter(|window| window.owners.is_some())
        .collect();
    let shared = [&split, &read_slice, &read]
        .into_iter()
        .find(|windows| !windows.is_empty())
        .unwrap_or(&read);
    let jobs = measured
        .iter()
        .filter(|(run, _)| {
            run.finished_at.is_some_and(|at| {
                shared
                    .iter()
                    .any(|window| (window.start..=window.end).contains(&at))
            })
        })
        .map(|(_, bytes)| bytes)
        .sum();
    let read_commits: Vec<&Window> = read
        .iter()
        .copied()
        .filter(|window| window.commits.is_some())
        .collect();
    Some(DriveWrites {
        device: last.device.clone(),
        store: last.store.clone(),
        watched: seconds(&read),
        written: sum(&read, |window| window.drive)?,
        shares: DriveShares {
            watched: seconds(shared),
            written: sum(shared, |window| window.drive)?,
            jobs,
            // Over the same windows as the others: the slice's of a longer stretch would fill the whole.
            slice: sum(shared, |window| window.slice),
            daemon: sum(shared, |window| window.daemon),
            owners: (!split.is_empty())
                .then(|| {
                    split
                        .iter()
                        .filter_map(|window| window.owners)
                        .reduce(DriveOwners::plus)
                })
                .flatten(),
        },
        commits: sum(&read_commits, |window| window.commits)
            .filter(|_| !read_commits.is_empty())
            .map(|count| Commits {
                count,
                watched: seconds(&read_commits),
            }),
    })
}

/// The seconds `windows` span.
fn seconds(windows: &[&Window]) -> u64 {
    windows
        .iter()
        .map(|window| window.end.saturating_sub(window.start))
        .sum::<u64>()
        / 1000
}

/// One figure of `windows` added up; `None` when one of them lacks it.
fn sum(windows: &[&Window], figure: fn(&Window) -> Option<u64>) -> Option<u64> {
    windows.iter().map(|window| figure(window)).sum()
}

/// How long `drive`'s rating lasts at `now` (ms): its rates over the last 7 days and 24 hours from its `windows`, and
/// what it has written, its SMART count and what runnerd read it write since (`read`, of the windows after the
/// count). Only the windows of its device count: Docker's root may have moved to another.
pub(super) fn endurance(
    drive: &Drive,
    windows: &[Window],
    read: Option<DriveRead>,
    now: u64,
) -> Endurance {
    let rate = |span: u64| {
        let since = now.saturating_sub(span);
        let read: Vec<&Window> = windows
            .iter()
            .filter(|window| {
                window.device == drive.device
                    && window.end > since
                    && window.end <= now
                    && window.drive.is_some()
            })
            .collect();
        (!read.is_empty()).then(|| WriteRate {
            bytes: read.iter().filter_map(|window| window.drive).sum(),
            watched: seconds(&read),
        })
    };
    Endurance {
        device: drive.device.clone(),
        rated: drive.endurance.0,
        // A count taken after `now`, as the digest's look 24 hours back may find, did not exist then.
        written: drive
            .written
            .filter(|written| written.at <= now / 1000)
            .map(|written| {
                // `read` runs to when it was read; at an earlier `now` the windows that end after it are not counted yet.
                let mut read = read.unwrap_or_default();
                for window in windows.iter().filter(|window| {
                    window.device == drive.device
                        && window.start >= written.at.saturating_mul(1000)
                        && window.end > now
                }) {
                    read.bytes = read.bytes.saturating_sub(window.drive.unwrap_or(0));
                    read.watched = read
                        .watched
                        .saturating_sub(window.end.saturating_sub(window.start));
                }
                WrittenSince {
                    counted: written.bytes.0,
                    at: written.at,
                    read: read.bytes,
                    watched: read.watched / 1000,
                    since: (now / 1000).saturating_sub(written.at),
                }
            }),
        week: rate(7 * DAY),
        day: rate(DAY),
        warn_days: drive.warn_days,
    }
}

/// The warning insight of an `endurance` that warns: past its rating, or reaching it within its `warn_days` at the
/// last 7 days' rate.
pub(super) fn endurance_insight(endurance: &Endurance) -> Option<Insight> {
    endurance.warns().then(|| Insight {
        key: format!("drive-wear:{}", endurance.device),
        severity: Severity::Warning,
        repository: String::new(),
        workflow: None,
        job: None,
        sentence: endurance.sentence(),
        kind: InsightKind::DriveWear {
            device: endurance.device.clone(),
            rated: endurance.rated,
            left: endurance.left(),
            days: endurance.days_left(RateSpan::Week),
        },
        runs: Vec::new(),
    })
}
