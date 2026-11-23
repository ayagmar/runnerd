//! What each profile's slots need of their Docker stores, read from the history on a slow cadence (ADR 0156), and
//! the share each runner it starts gets of what the stores may keep together. Only `Shares`' methods here write it.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::cache::{self, shares};
use crate::config::Config;

/// How often the needs are read again: a share changes what the slot's next cleanup removes, so it moves slowly.
pub(super) const EVERY: Duration = Duration::from_secs(3600);

/// How far back the reports are read, in ms: a week of a profile's jobs, of which `shares::NEED_RUNS` count.
pub(super) const WINDOW_MS: u64 = 7 * 24 * 3600 * 1000;

/// Each profile's need, and when it was last read. Nothing survives a restart, which reads the history again before
/// the first runner starts; a reload keeps it, as the history it came from is the same, and the shares follow the
/// new config's slots from the next runner on.
#[derive(Debug, Default)]
pub(super) struct Shares {
    /// Bytes each profile's slots held after their jobs (`shares::needs`); a profile not in it keeps the equal share.
    needs: BTreeMap<String, u64>,
    /// When the history was last read; `None` before the first read.
    read_at: Option<Instant>,
}

impl Shares {
    /// Whether the history is due to be read again at `now`.
    pub(super) fn due(&self, now: Instant) -> bool {
        self.read_at
            .is_none_or(|at| now.saturating_duration_since(at) >= EVERY)
    }

    /// Takes in what the history said at `now`: `uses` newest first, or `None` when it could not be read, which
    /// keeps the needs as they were and tries again at the next tick. A need that moved little keeps its value
    /// (`shares::steady`), so a share does not drop and rise again from one read to the next.
    pub(super) fn read(&mut self, uses: Option<&[shares::StoreUse]>, now: Instant) {
        if let Some(uses) = uses {
            self.needs = shares::steady(&self.needs, shares::needs(uses));
            self.read_at = Some(now);
        }
    }

    /// The share in GiB of a slot of `profile` in `disk`, the bytes the stores may keep together.
    pub(super) fn gib(&self, disk: u64, config: &Config, profile: &str) -> u64 {
        shares::slot_shares(disk, config, &self.needs)
            .get(profile)
            .copied()
            .unwrap_or_else(|| {
                let slots = config.profiles.values().map(|p| u64::from(p.slots)).sum();
                cache::docker_budget_gib(disk, slots)
            })
    }
}
