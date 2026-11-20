//! Why each profile's jobs queued over a window (ADR 0154): their waits split by the cause each plan that held them
//! named, with the two parts no cause covers named too, the wait before runnerd listed a job and the wait no plan held
//! it for. Pure.

use std::collections::BTreeMap;

use super::{Run, SECOND};
use crate::api::{CauseWait, ProfileQueue, WaitCause};
use crate::sizing::Wait;

/// One profile's waits, in ms, as they add up.
#[derive(Default)]
struct Split {
    jobs: u32,
    waited: u64,
    causes: BTreeMap<WaitCause, u64>,
    before_listed: u64,
    uncovered: u64,
    unrecorded: u32,
}

/// The split of each profile's waits over `window`'s jobs that started, the history holding every job since
/// `kept_since` (ms); the profile whose jobs waited longest first.
pub(super) fn split(window: &[&Run], kept_since: u64) -> Vec<ProfileQueue> {
    let mut profiles: BTreeMap<&str, Split> = BTreeMap::new();
    for run in window {
        let (Some(profile), Some(started_at)) = (run.profile.as_deref(), run.started_at) else {
            continue;
        };
        let split = profiles.entry(profile).or_default();
        let Some(causes) = &run.causes else {
            split.unrecorded += 1;
            continue;
        };
        let from = run.queued_at.max(kept_since);
        let waited = Wait {
            queued_at: run.queued_at,
            started_at,
            unseen: run.unseen,
        }
        .counted(kept_since);
        let counted: u64 = causes.values().sum();
        // A wait counted to the plan after its start can pass the start a little: the causes keep their shares.
        let scale = |ms: u64| {
            if counted > waited {
                u64::try_from(u128::from(ms) * u128::from(waited) / u128::from(counted))
                    .unwrap_or(ms)
            } else {
                ms
            }
        };
        let mut covered = 0;
        for (cause, ms) in causes {
            let ms = scale(*ms);
            covered += ms;
            *split.causes.entry(*cause).or_insert(0) += ms;
        }
        let rest = waited.saturating_sub(covered);
        // What passed while no runnerd ran is out of the wait already, and it came before the first listing.
        let before_listed = run.seen_at.map_or(rest, |seen| {
            seen.saturating_sub(from)
                .saturating_sub(run.unseen)
                .min(rest)
        });
        split.jobs += 1;
        split.waited += waited;
        split.before_listed += before_listed;
        split.uncovered += rest - before_listed;
    }
    let seconds = |ms: u64| ms.saturating_add(SECOND / 2) / SECOND;
    let mut queue: Vec<ProfileQueue> = profiles
        .into_iter()
        .map(|(profile, split)| {
            let mut causes: Vec<CauseWait> = split
                .causes
                .into_iter()
                .map(|(cause, ms)| CauseWait {
                    cause,
                    waited: seconds(ms),
                })
                .filter(|cause| cause.waited > 0)
                .collect();
            causes.sort_by_key(|cause| std::cmp::Reverse(cause.waited));
            ProfileQueue {
                profile: profile.to_owned(),
                jobs: split.jobs,
                waited: seconds(split.waited),
                causes,
                before_listed: seconds(split.before_listed),
                uncovered: seconds(split.uncovered),
                unrecorded: split.unrecorded,
            }
        })
        .collect();
    queue.sort_by(|a, b| {
        b.waited
            .cmp(&a.waited)
            .then_with(|| a.profile.cmp(&b.profile))
    });
    queue
}
