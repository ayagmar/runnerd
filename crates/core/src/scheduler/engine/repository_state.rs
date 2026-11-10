//! What the engine keeps of one listed repository: its last listing of queued jobs, when it is listed next, and per
//! pool, the health that holds the pool's runners back and what the pool's next job waits for. It owns those values and must not call
//! GitHub or Docker, nor log: the engine decides what each listing, failure and plan does to them.

use std::collections::BTreeMap;

use super::listing::Listing;
use super::listing_pace::ListingPace;
use super::pool_health::PoolHealth;
use crate::scheduler::Waits;

/// A repository's state, shared by its pools but for their health and what they wait for.
///
/// Nothing of it survives a restart. A reload keeps it for a repository the config still lists, but drops the health
/// of a pool whose runners change and the waits of a pool gone; a repository it removes drains with no listing, and
/// one it adds back starts afresh, due at once.
#[derive(Debug, Default)]
pub(super) struct RepositoryState {
    /// Its queued jobs as its last listing showed them.
    listing: Listing,
    /// When it is listed next (ADR 0128).
    pace: ListingPace,
    /// Its pools by profile, each kept while it has a health or a wait.
    pools: BTreeMap<String, PoolState>,
}

/// One pool's two halves, each with its own lifetime: a wait that ends keeps the health, and a reload that drops the
/// health keeps the wait.
#[derive(Debug, Default)]
struct PoolState {
    /// The pool's runners held back after they failed: a broken pool does not hold back the others (ADR 0058). Made
    /// by the first runner that leaves or fails, kept until a reload changes the pool's runners.
    health: Option<PoolHealth>,
    /// What the pool's next job has waited for and since when (ADR 0096): from the first plan of a tick or a pass
    /// that held it, not one a status runs, until a runner of the pool is in its slot, the pool may no longer start
    /// one, or a listing that succeeded no longer shows the job. A plan that holds it for a slot or for the other
    /// resource does not make it younger, nor does a listing that failed. With them, whether a barrier has served the
    /// pool (ADR 0098), until a listing that succeeded shows its queue empty or the pool may no longer start a runner.
    waits: Option<Waits>,
}

impl PoolState {
    fn is_empty(&self) -> bool {
        self.health.is_none() && self.waits.is_none()
    }

    fn health_mut(&mut self) -> &mut PoolHealth {
        self.health.get_or_insert_default()
    }

    fn forget_health(&mut self) {
        self.health = None;
    }

    fn waits_mut(&mut self) -> Option<&mut Waits> {
        self.waits.as_mut()
    }

    fn waits_entry(&mut self) -> &mut Waits {
        self.waits.get_or_insert_default()
    }

    fn end_waits(&mut self) {
        self.waits = None;
    }

    #[cfg(test)]
    fn health_if_any(&mut self) -> Option<&mut PoolHealth> {
        self.health.as_mut()
    }
}

impl RepositoryState {
    /// Its queued jobs as its last listing showed them.
    pub(super) fn listing(&self) -> &Listing {
        &self.listing
    }

    /// Its listing, for a listing's answer or a stop.
    pub(super) fn listing_mut(&mut self) -> &mut Listing {
        &mut self.listing
    }

    /// When it is listed next.
    pub(super) fn pace(&self) -> &ListingPace {
        &self.pace
    }

    /// When it is listed next, for a probe of it, a start or a release of its runners, or its listing's answer.
    pub(super) fn pace_mut(&mut self) -> &mut ListingPace {
        &mut self.pace
    }

    /// The health of its pool in `profile`, if a runner there left or failed since.
    pub(super) fn health(&self, profile: &str) -> Option<&PoolHealth> {
        self.pools.get(profile)?.health.as_ref()
    }

    /// The health of its pool in `profile`, made healthy if it has none.
    pub(super) fn health_mut(&mut self, profile: &str) -> &mut PoolHealth {
        self.pools
            .entry(profile.to_owned())
            .or_default()
            .health_mut()
    }

    /// Drops the health of each pool `keep` does not keep; their waits stay.
    pub(super) fn forget_health(&mut self, mut keep: impl FnMut(&str) -> bool) {
        for (profile, pool) in &mut self.pools {
            if !keep(profile) {
                pool.forget_health();
            }
        }
        self.pools.retain(|_, pool| !pool.is_empty());
    }

    /// What its pool in `profile` waits for, if anything.
    pub(super) fn waits(&self, profile: &str) -> Option<&Waits> {
        self.pools.get(profile)?.waits.as_ref()
    }

    /// What its pool in `profile` waits for, if anything, to change.
    pub(super) fn waits_mut(&mut self, profile: &str) -> Option<&mut Waits> {
        self.pools.get_mut(profile)?.waits_mut()
    }

    /// What its pool in `profile` waits for, from nothing if it waits for nothing yet.
    pub(super) fn waits_entry(&mut self, profile: &str) -> &mut Waits {
        self.pools
            .entry(profile.to_owned())
            .or_default()
            .waits_entry()
    }

    /// Its pool in `profile` waits for nothing; its health stays.
    pub(super) fn end_waits(&mut self, profile: &str) {
        if let Some(pool) = self.pools.get_mut(profile) {
            pool.end_waits();
            if pool.is_empty() {
                self.pools.remove(profile);
            }
        }
    }

    /// Ends the waits of each pool `keep` does not keep; their health stays.
    pub(super) fn forget_waits(&mut self, mut keep: impl FnMut(&str) -> bool) {
        for (profile, pool) in &mut self.pools {
            if !keep(profile) {
                pool.end_waits();
            }
        }
        self.pools.retain(|_, pool| !pool.is_empty());
    }

    /// What each of its pools that waits waits for.
    pub(super) fn all_waits(&self) -> impl Iterator<Item = &Waits> {
        self.pools.values().filter_map(|pool| pool.waits.as_ref())
    }

    /// What each of its pools that waits waits for, to change.
    pub(super) fn all_waits_mut(&mut self) -> impl Iterator<Item = &mut Waits> {
        self.pools.values_mut().filter_map(PoolState::waits_mut)
    }

    /// Each pool's health, to change.
    #[cfg(test)]
    pub(super) fn all_health_mut(&mut self) -> impl Iterator<Item = &mut PoolHealth> {
        self.pools.values_mut().filter_map(PoolState::health_if_any)
    }
}
