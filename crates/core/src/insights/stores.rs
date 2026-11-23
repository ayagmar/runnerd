//! Profiles whose slots' Docker stores keep outgrowing their share (ADR 0156): after half or more of the profile's
//! last jobs whose image reported, the store held more than the share its cleanup held it to, so the next job pulled
//! and built again what went. Pure, as the rest of insights.

use std::collections::BTreeMap;

use super::Run;
use crate::api::{Insight, InsightKind, Severity};
use crate::cache::shares::StoreReport;

/// The last reporting jobs of a profile read: a share follows them within the hour, so older ones say little.
const STORE_RUNS: usize = 20;

/// The fewest reports that make an insight.
const STORE_MIN_RUNS: usize = 10;

/// The runs linked, newest first.
const LINKED: usize = 5;

/// One insight per profile whose store held more than its share after half or more of its last `STORE_RUNS` jobs
/// in `window` whose image reported, at least `STORE_MIN_RUNS` of them. Its repository is the newest overflowing
/// job's.
pub(super) fn overflowing(window: &[&Run]) -> Vec<Insight> {
    let mut by_profile: BTreeMap<&str, Vec<(&Run, StoreReport)>> = BTreeMap::new();
    for run in window {
        if let (Some(profile), Some(report)) = (run.profile.as_deref(), run.docker_store) {
            by_profile.entry(profile).or_default().push((run, report));
        }
    }
    let mut found = Vec::new();
    for (profile, mut reports) in by_profile {
        reports.sort_by_key(|(run, _)| std::cmp::Reverse((run.finished(), run.id)));
        reports.truncate(STORE_RUNS);
        let over: Vec<&(&Run, StoreReport)> = reports
            .iter()
            .filter(|(_, report)| report.overflowed())
            .collect();
        if reports.len() < STORE_MIN_RUNS || over.len() * 2 < reports.len() {
            continue;
        }
        let Some(((newest, _), (_, latest))) = over.first().zip(reports.first()) else {
            continue;
        };
        let mut held: Vec<u64> = reports.iter().map(|(_, report)| report.held).collect();
        held.sort_unstable();
        let p90 = held[(held.len() * 9).div_ceil(10).max(1) - 1];
        let overflowed = u32::try_from(over.len()).unwrap_or(u32::MAX);
        let total = u32::try_from(reports.len()).unwrap_or(u32::MAX);
        let share = latest.share_gib << 30;
        found.push(Insight {
            key: format!("store-overflow:{profile}"),
            severity: Severity::Notice,
            repository: newest.repository.clone(),
            workflow: None,
            job: None,
            sentence: Insight::store_overflow_sentence(profile, overflowed, total, p90, share),
            kind: InsightKind::StoreOverflow {
                profile: profile.to_owned(),
                overflowed,
                total,
                held: p90,
                share,
            },
            runs: over
                .iter()
                .take(LINKED)
                .map(|(run, _)| run.link())
                .collect(),
        });
    }
    found
}
