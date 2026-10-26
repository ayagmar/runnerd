//! What a profile's runs say about its `memory` (ADR 0070): each job's sizing window (`admission::recent`), what a
//! raise needs (`raise_need`: only runs that met the current cap since the profile took it), what a lowering must
//! stay above (`lower_floor`: every windowed run's need), and a lowering auto sizing made that went too far, which is
//! reverted. Pure; `advise` and auto sizing read it, never a client.

use std::collections::BTreeSet;

use crate::admission::Key;
use crate::admission::recent::{self, Counted, Ending, JobRuns, RECENT_RUNS, Recent, SIZING_TIMES};
use crate::api::{self, Peaks, Resize, RevertCause, SizingRevert, job_title};
use crate::config::{Admission, Config};

use super::{Capped, History, MemorySet, Ran, spread};

/// How far back the windows read, in ms: 30 days, whatever `since` a client asked for.
pub const HISTORY: u64 = 30 * 24 * 3600 * 1000;

/// How far back auto sizing's recorded changes are read for a revert, in ms: 60 days, so a lowering up to 30 days
/// before the run that met its cap is found while that run is in the history.
pub const CHANGES_READ: u64 = 2 * HISTORY;

/// A lowering is reverted after this many failures in a row at it, of a job that succeeded at the cap it came from.
pub const FAILED_RUNS: usize = 3;

/// Every job's sizing window from `history`'s last 30 days to `now` (ms).
pub(super) fn windows(config: &Config, history: &History, now: u64) -> Recent {
    let size = SIZING_TIMES.saturating_mul(RECENT_RUNS.max(u64::from(config.sizing.min_jobs)));
    let mut recent = Recent::new(size);
    let start = now.saturating_sub(HISTORY);
    for (run, ending) in recent::rows(history) {
        if run.queued_at < start {
            continue;
        }
        recent.read(
            run,
            &run.profile,
            ending,
            cap_of(config, run),
            recent::finished(run),
        );
    }
    recent
}

/// The cap a run's container had, else its profile's now.
fn cap_of(config: &Config, run: &Ran) -> u64 {
    run.memory_cap
        .or_else(|| config.profiles.get(&run.profile).map(|p| p.memory.0))
        .unwrap_or(u64::MAX)
}

/// The peaks of the runs in some windows; `on_cap` how many of their jobs are on a hold at the cap.
pub(super) fn peaks<'run>(runs: impl Iterator<Item = &'run Counted>, on_cap: u64) -> Peaks {
    let runs: Vec<&Counted> = runs.collect();
    Peaks {
        jobs: runs.len() as u64,
        at_cap: runs.iter().filter(|run| run.capped.met()).count() as u64,
        on_cap,
        memory: spread(runs.iter().map(|run| run.peak).collect()),
        cpu: spread(runs.iter().map(|run| run.cpu_peak).collect()),
    }
}

/// What a windowed run says its job needs at least: above its cap when it met it, else its peak's need, at most
/// its cap, since it ran clear of it.
fn run_need(run: &Counted, margin: u32) -> u64 {
    if run.capped.met() {
        api::need(run.peak.max(run.cap), margin)
    } else {
        api::need(run.peak, margin).min(run.cap)
    }
}

/// The largest need of a job's windowed runs.
pub(super) fn floor(runs: &JobRuns, margin: u32) -> u64 {
    runs.window()
        .map(|run| run_need(run, margin))
        .max()
        .unwrap_or(0)
}

/// What a job's windowed runs at caps above `above` say it needs, each at most its own cap: a raise goes back at
/// most to a cap the job ran at, and a need above that comes from a run that meets it. A run recorded without the
/// local counter (`Capped::Assumed`) says nothing here: it never raises a cap.
fn larger_caps(runs: &JobRuns, margin: u32, above: u64) -> u64 {
    runs.window()
        .filter(|run| run.cap > above && run.capped != Capped::Assumed)
        .map(|run| run_need(run, margin).min(run.cap))
        .max()
        .unwrap_or(0)
}

/// Whether `held` is on its cap for sizing: a hold `recent::CLEAN_RUNS` clear runs have not ended, however old.
pub(super) fn on_cap(runs: &JobRuns, now: u64) -> bool {
    runs.held(now, None).is_some()
}

/// When the daemon started running `profile` with `memory`, in ms, by `set`: its first event since the last one
/// with other `memory`. `now` when its latest has other `memory`; 0 with no events.
pub(super) fn took(set: &[MemorySet], profile: &str, memory: u64, now: u64) -> u64 {
    let mut since = None;
    let mut seen = false;
    for event in set.iter().filter(|event| event.profile == profile) {
        seen = true;
        if event.memory != memory {
            since = None;
        } else if since.is_none() {
            since = Some(event.at);
        }
    }
    since.unwrap_or(if seen { now } else { 0 })
}

/// What a raise of `profile`, whose `memory` is `cap` since `took` (ms), needs from the job `key`, `None` for every
/// job: the largest of `need(max(peak, cap))` over its runs that met `cap` since then, and of that job's need at
/// larger caps (`larger_caps`), since its last sample before a kill rarely shows what it needed. 0 without such a
/// run.
pub(super) fn raise_need(
    history: &History,
    recent: &Recent,
    profile: &str,
    cap: u64,
    took: u64,
    margin: u32,
    key: Option<&Key>,
) -> u64 {
    history
        .jobs
        .iter()
        .chain(&history.cancelled)
        .filter(|run| {
            run.profile == profile
                && run.memory_cap == Some(cap)
                && recent::finished(run) >= took
                && run.capped(cap) == Capped::Met
                && key.is_none_or(|key| Key::of(run) == *key)
        })
        .map(|run| {
            let larger = recent
                .job(&Key::of(run), profile)
                .map_or(0, |runs| larger_caps(runs, margin, cap));
            api::need(run.memory_peak.max(cap), margin).max(larger)
        })
        .max()
        .unwrap_or(0)
}

/// The lowerings auto sizing made of `profile` that went too far, oldest first: since each, a run at the cap it
/// lowered to met it, or a job that succeeded at the cap it came from failed `FAILED_RUNS` times in a row at it.
fn too_far<'change>(
    history: &'change History,
    profile: &str,
) -> impl Iterator<Item = (&'change api::SizingChange, SizingRevert)> {
    history
        .changes
        .iter()
        .filter(move |change| change.profile == profile && change.to.memory < change.from.memory)
        .filter_map(move |change| {
            cause(history, profile, change).map(|(job, cause)| {
                (
                    change,
                    SizingRevert {
                        from: change.to.memory,
                        to: change.from.memory,
                        job,
                        cause,
                    },
                )
            })
        })
}

/// What shows that `lowering` of `profile` went too far, if anything: the job and how.
fn cause(
    history: &History,
    profile: &str,
    lowering: &api::SizingChange,
) -> Option<(String, RevertCause)> {
    let (lowered, from) = (lowering.to.memory, lowering.from.memory);
    let since = lowering.at.saturating_mul(1000);
    let after: Vec<(&Ran, Ending)> = recent::rows(history)
        .into_iter()
        .filter(|(run, _)| {
            run.profile == profile
                && run.memory_cap == Some(lowered)
                && recent::finished(run) >= since
        })
        .collect();
    let named = |run: &Ran| {
        format!(
            "{} {}",
            run.repository,
            job_title(run.workflow.as_deref(), &run.name, None)
        )
    };
    if let Some((run, _)) = after
        .iter()
        .find(|(run, _)| run.capped(lowered) == Capped::Met)
    {
        return Some((named(run), RevertCause::Met));
    }
    let jobs: BTreeSet<Key> = after.iter().map(|(run, _)| Key::of(run)).collect();
    for key in jobs {
        // Before the revert only runs from before the lowering ran at `from`; after it, its runs at `from` since keep
        // the floor while the failures are in the history.
        let succeeded_at_from = history.jobs.iter().any(|run| {
            run.profile == profile
                && Key::of(run) == key
                && run.succeeded
                && run.memory_cap == Some(from)
        });
        if !succeeded_at_from {
            continue;
        }
        let mut row: Vec<(u64, Option<&str>)> = Vec::new();
        for (run, ending) in after.iter().filter(|(run, _)| Key::of(run) == key) {
            if *ending == Ending::Cancelled {
                continue;
            }
            if run.succeeded {
                row.clear();
            } else if run.failed && !row.iter().any(|(id, _)| *id == run.run_id) {
                row.push((run.run_id, run.branch.as_deref()));
            }
            let branches: BTreeSet<Option<&str>> = row.iter().map(|(_, branch)| *branch).collect();
            if row.len() >= FAILED_RUNS && branches.len() >= 2 {
                return Some((
                    named(run),
                    RevertCause::Failed {
                        runs: u32::try_from(row.len()).unwrap_or(u32::MAX),
                    },
                ));
            }
        }
    }
    None
}

/// The revert auto sizing makes now of `profile`, whose `memory` and `slots` are `current`, if any: its latest
/// recorded change of `memory` lowered it to what it has, the daemon has run that since (`set`), and that went too
/// far. Changes of only `slots` or `cpus` since do not hide it. Under caps the slots go back to what they were
/// before the lowering, so the memory fits the slice.
pub(super) fn revert(
    config: &Config,
    history: &History,
    profile: &str,
    current: Resize,
) -> Option<(SizingRevert, Resize)> {
    let last = history
        .changes
        .iter()
        .rfind(|change| change.profile == profile && change.to.memory != change.from.memory)?;
    if last.to.memory != current.memory || last.to.memory >= last.from.memory {
        return None;
    }
    let since = last.at.saturating_mul(1000);
    let mut events = history
        .memory_set
        .iter()
        .filter(|event| event.profile == profile && event.at >= since)
        .peekable();
    events.peek()?;
    if events.any(|event| event.memory != current.memory) {
        return None;
    }
    let (_, revert) = too_far(history, profile).find(|(change, _)| *change == last)?;
    let slots = match config.host.admission {
        Admission::Caps => last.from.slots,
        Admission::Measured => current.slots,
    };
    Some((
        revert,
        Resize {
            memory: last.from.memory,
            slots,
            cpus: None,
        },
    ))
}

/// The floor reverts set on `profile`'s lowerings: the largest cap a lowering that went too far came from, while
/// the run that showed it is in the history.
pub(super) fn revert_floor(history: &History, profile: &str) -> u64 {
    too_far(history, profile)
        .map(|(_, revert)| revert.to)
        .max()
        .unwrap_or(0)
}
