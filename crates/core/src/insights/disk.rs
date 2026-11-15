//! What the jobs of a window and the drive under Docker's root wrote (ADR 0143): the jobs' figures together, the
//! workflows and jobs that wrote most, and the drive's windows the daemon read, set against the jobs that ended in
//! them. Pure.

use super::Run;
use crate::api::{Commits, DiskWrites, DriveShares, DriveWrites, JobWrites, Measure, WriteTop};
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
    })
}

/// The drive's windows that end within `since` to `now` and read it, added up, with whose its bytes were over the
/// windows that also read the runner slice, or over all of them when none did.
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
    let shared = if read_slice.is_empty() {
        &read
    } else {
        &read_slice
    };
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
            slice: sum(&read_slice, |window| window.slice).filter(|_| !read_slice.is_empty()),
            daemon: sum(shared, |window| window.daemon),
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
