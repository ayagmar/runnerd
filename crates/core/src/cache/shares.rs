//! How the slots' Docker stores split what they may keep together (docs/adr/0054, docs/adr/0156): each profile's
//! slots get a share that follows what its stores held after their jobs, within the same total, never under a floor.
//! Also reads what a runner's image reported of its slot's store once the runner exited. Pure: the engine reads the
//! history and the container's log and passes them in; it never measures a store or changes a share itself.

use std::collections::BTreeMap;

use crate::config::Config;

/// What a runner image's entrypoint prints once the runner exited (`image/entrypoint.sh`), followed by
/// `held=<bytes> share=<GiB>`, or by `unmeasured` when its job's cleanup measured nothing.
pub const STORE_LINE: &str = "runnerd: docker store ";

/// What one job's cleanup found its slot's Docker store holding, as its runner's container log says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreReport {
    /// Bytes the store held once the job's containers and volumes were gone, before the build cache gave way.
    pub held: u64,
    /// The share the slot had then, in GiB, as the cleanup held the store to it.
    pub share_gib: u64,
}

impl StoreReport {
    /// Whether the store held more than its share, so the cleanup removed more than the build layers no recent job
    /// used: the next job pulls and builds again what went.
    pub fn overflowed(&self) -> bool {
        self.held > self.share_gib << 30
    }
}

/// What the last `STORE_LINE` of a runner container's `logs` reports. `None` when there is none, as from an image
/// before it, or when the last says `unmeasured`: the line the entrypoint prints after every process of the job
/// ended is the last, so a line a job printed before it never counts.
pub fn store_report(logs: &str) -> Option<StoreReport> {
    let last = logs
        .lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix(STORE_LINE))?;
    let mut held = None;
    let mut share = None;
    for field in last.split(' ') {
        match field.split_once('=') {
            Some(("held", value)) => held = value.parse().ok(),
            Some(("share", value)) => share = value.parse().ok(),
            _ => return None,
        }
    }
    Some(StoreReport {
        held: held?,
        share_gib: share?,
    })
}

/// One job's report as the history keeps it, for `needs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreUse {
    /// The profile its runner ran in.
    pub profile: String,
    /// What the store held after the job, in bytes (`StoreReport::held`).
    pub held: u64,
}

/// The reports `needs` reads per profile, the most recent.
pub const NEED_RUNS: usize = 100;

/// The fewest reports a profile needs before its share follows them; with fewer it keeps the equal share.
pub const NEED_MIN_RUNS: usize = 5;

/// What each profile's slots need of their Docker stores: the 90th percentile, by nearest rank, of what they held
/// after each of the profile's last `NEED_RUNS` jobs, from `uses`, newest first. A profile with fewer than
/// `NEED_MIN_RUNS` reports is left out.
pub fn needs(uses: &[StoreUse]) -> BTreeMap<String, u64> {
    let mut by_profile: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for used in uses {
        let held = by_profile.entry(&used.profile).or_default();
        if held.len() < NEED_RUNS {
            held.push(used.held);
        }
    }
    by_profile
        .into_iter()
        .filter(|(_, held)| held.len() >= NEED_MIN_RUNS)
        .map(|(profile, mut held)| {
            held.sort_unstable();
            let rank = (held.len() * 9).div_ceil(10).max(1);
            (profile.to_owned(), held[rank - 1])
        })
        .collect()
}

/// How far a profile's need moves before its share follows, in percent of the need it had: each lower share empties
/// the stores of the slots that start under it, so a need that wanders by a few percent from one read to the next
/// keeps its share.
const STEADY_PERCENT: u64 = 10;

/// The needs to split by, from the `previous` ones and those just read: a profile's previous need while the new one
/// is within `STEADY_PERCENT` of it, its new one otherwise. A profile no longer read has no need.
pub fn steady(
    previous: &BTreeMap<String, u64>,
    fresh: BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    fresh
        .into_iter()
        .map(|(profile, need)| match previous.get(&profile) {
            Some(&before) if need.abs_diff(before) <= before / 100 * STEADY_PERCENT => {
                (profile, before)
            }
            _ => (profile, need),
        })
        .collect()
}

/// The least share a slot gets, as a part of the equal share: a quarter of it, at least 1 GiB, at most all of it. So
/// a profile whose stores hold little still keeps room for a job unlike its last ones, and the floors of every slot
/// never add up to more than the total.
const FLOOR_PART: u64 = 4;

/// What a share asks for above what the stores held, in percent: a store that holds exactly its share is emptied by
/// the next job that adds a layer.
const HEADROOM_PERCENT: u64 = 125;

/// The most one slot's Docker Engine keeps, in GiB.
pub(super) const SLOT_MAX_GIB: u64 = crate::api::DOCKER_SLOT_MAX >> 30;

const GIB: u64 = 1 << 30;

/// Each configured profile's slot share of `disk`, the bytes the slots' Docker stores may keep together, in whole
/// GiB as `image/layers.sh` takes it. A profile `needs` has no figure for gets the equal share of every slot, as
/// before runnerd measured the stores. The rest of `disk` goes to the measured profiles' slots in proportion to
/// their needs and a quarter more, each between its floor and `SLOT_MAX_GIB`; what the cap leaves unused stays
/// unused. A profile's floor is what it asks for, rounded up to whole GiB, as far as the equal share in whole GiB, and
/// never under a quarter of the equal share (`FLOOR_PART`): a profile whose stores fitted the equal split never gets
/// less than they hold, so a profile that grows takes room only from those that hold less than their share. Each
/// share is rounded to whole GiB without the shares adding up to more than `disk`, and is at least 1 GiB, as the
/// equal split's always was.
pub fn slot_shares(
    disk: u64,
    config: &Config,
    needs: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let slots: u64 = config.profiles.values().map(|p| u64::from(p.slots)).sum();
    let equal = disk / slots.max(1);
    let floor = (equal / FLOOR_PART).max(GIB).min(equal);
    let equal_gib = equal / GIB * GIB;
    let cap = SLOT_MAX_GIB * GIB;
    let mut shares = BTreeMap::new();
    let mut measured = Vec::new();
    let mut left = disk;
    for (name, profile) in &config.profiles {
        let count = u64::from(profile.slots);
        match needs.get(name) {
            Some(need) if count > 0 => {
                let want = u128::from(*need) * u128::from(HEADROOM_PERCENT) / 100;
                let want = u64::try_from(want).unwrap_or(u64::MAX);
                let own = want.div_ceil(GIB).saturating_mul(GIB).min(equal_gib);
                measured.push((name, count, want, own.max(floor)));
            }
            _ => {
                left = left.saturating_sub(equal.saturating_mul(count));
                shares.insert(name.clone(), equal);
            }
        }
    }
    let fill = |scale: f64| -> u64 {
        measured
            .iter()
            .map(|(_, count, want, floor)| count * spread(*want, scale, *floor, cap))
            .sum()
    };
    // The largest scale whose shares fit in what is left: `fill` only grows with it.
    let (mut low, mut high) = (0.0_f64, 1.0_f64);
    while fill(high) <= left && high < 1e12 {
        low = high;
        high *= 2.0;
    }
    for _ in 0..64 {
        let mid = f64::midpoint(low, high);
        if fill(mid) <= left {
            low = mid;
        } else {
            high = mid;
        }
    }
    // Whole GiB, rounded down, then a GiB more for the profiles that lost most to the rounding while what is left
    // holds one for each of their slots.
    let mut spare = left;
    let mut rounded: Vec<(&String, u64, u64, u64)> = measured
        .iter()
        .map(|(name, count, want, floor)| {
            let bytes = spread(*want, low, *floor, cap);
            spare = spare.saturating_sub(count * (bytes / GIB * GIB));
            (*name, *count, bytes / GIB, bytes % GIB)
        })
        .collect();
    rounded.sort_by_key(|(name, _, _, lost)| (std::cmp::Reverse(*lost), *name));
    for (name, count, gib, lost) in rounded {
        let more = lost > 0 && gib < SLOT_MAX_GIB && count * GIB <= spare;
        if more {
            spare -= count * GIB;
        }
        shares.insert(name.clone(), (gib + u64::from(more)) * GIB);
    }
    shares
        .into_iter()
        .map(|(name, bytes)| (name, (bytes / GIB).clamp(1, SLOT_MAX_GIB)))
        .collect()
}

/// One measured slot's share at `scale`, in bytes.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a share in bytes is far under 2^53, and the result is clamped to the cap"
)]
fn spread(want: u64, scale: f64, floor: u64, cap: u64) -> u64 {
    ((want as f64 * scale) as u64).clamp(floor, cap.max(floor))
}

#[cfg(test)]
mod tests;
