//! Why each profile's jobs queued over a window (ADR 0154): their waits split by the cause each plan that held them
//! named, with the two parts no cause covers named too, the wait before runnerd listed a job and the wait no plan held
//! it for. Pure.

use std::collections::BTreeMap;

use super::Run;
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
    estimated: u32,
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
        // Known on its own, so kept whole: up to the first listing, less the downtime its listing found, which is out
        // of the wait already. A job no listing showed queued was not listed before it started: what no cause covers.
        let before_listed = run.seen_at.map_or(waited.saturating_sub(counted), |seen| {
            seen.min(started_at)
                .saturating_sub(from)
                .saturating_sub(run.unseen_unlisted.min(run.unseen))
                .min(waited)
        });
        let listed = waited - before_listed;
        // A cause counted past the start, as when a runner took the job before runnerd read it, is scaled into what
        // the listing leaves, the causes keeping their shares.
        let scaled = counted > listed;
        let mut covered = 0;
        for (cause, ms) in causes {
            let ms = if scaled {
                u64::try_from(u128::from(*ms) * u128::from(listed) / u128::from(counted))
                    .unwrap_or(*ms)
            } else {
                *ms
            };
            covered += ms;
            *split.causes.entry(*cause).or_insert(0) += ms;
        }
        split.jobs += 1;
        split.estimated += u32::from(scaled);
        split.waited += waited;
        split.before_listed += before_listed;
        split.uncovered += listed.saturating_sub(covered);
    }
    let mut queue: Vec<ProfileQueue> = profiles
        .into_iter()
        .map(|(profile, split)| {
            let mut causes: Vec<CauseWait> = split
                .causes
                .into_iter()
                .filter(|(_, ms)| *ms > 0)
                .map(|(cause, waited_ms)| CauseWait { cause, waited_ms })
                .collect();
            causes.sort_by_key(|cause| std::cmp::Reverse(cause.waited_ms));
            ProfileQueue {
                profile: profile.to_owned(),
                jobs: split.jobs,
                waited_ms: split.waited,
                causes,
                before_listed_ms: split.before_listed,
                uncovered_ms: split.uncovered,
                unrecorded: split.unrecorded,
                estimated: split.estimated,
            }
        })
        .collect();
    queue.sort_by(|a, b| {
        b.waited_ms
            .cmp(&a.waited_ms)
            .then_with(|| a.profile.cmp(&b.profile))
    });
    queue
}
