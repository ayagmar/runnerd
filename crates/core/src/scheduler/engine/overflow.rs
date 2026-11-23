//! Which queued jobs an overflow host leaves to the other machines for now (ADR 0162). It owns what it saw of each
//! queued job, when and in which listings, and which runners listing of each repository answered last. It reads no
//! clock and calls nothing: the engine says what each listing answered and when, and asks which jobs to hold back.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::github::QueuedJob;

/// What an overflow host saw of the queued jobs and of the runners listings.
///
/// Nothing of it survives a restart: a job a restarted daemon finds queued waits for two listings here, with a runners
/// listing between them, as a new one does, though the time since GitHub queued it still counts. A reload keeps it,
/// and a sleep forgets it, as nothing listed before the sleep holds after it. A primary keeps it too, so a reload that
/// makes the host an overflow one finds the jobs' ages known.
#[derive(Debug, Default)]
pub(super) struct Overflow {
    /// By job ID, the jobs the last listing of their repository showed.
    seen: BTreeMap<u64, Sighting>,
    /// By repository, the listing whose runners listing answered last; none while the last one failed or was refused.
    runners: BTreeMap<String, u64>,
}

/// One queued job as this host saw it.
#[derive(Debug, Clone)]
struct Sighting {
    /// `owner/name`, whose listings show it.
    repository: String,
    /// When a listing first showed it, in ms since the epoch.
    first_at: u64,
    /// The listing that first showed it.
    first: u64,
    /// The listing that last showed it.
    last: u64,
}

impl Overflow {
    /// A listing of `repository`'s queued jobs, the engine's `listing`, answered with `jobs` at `now` (ms): those it
    /// shows are seen in it, and those it no longer shows are forgotten.
    pub(super) fn queued_listed(
        &mut self,
        repository: &str,
        jobs: &[QueuedJob],
        listing: u64,
        now: u64,
    ) {
        self.seen.retain(|id, sighting| {
            sighting.repository != repository || jobs.iter().any(|job| job.id == *id)
        });
        for job in jobs {
            self.seen
                .entry(job.id)
                .and_modify(|sighting| sighting.last = sighting.last.max(listing))
                .or_insert_with(|| Sighting {
                    repository: repository.to_owned(),
                    first_at: now,
                    first: listing,
                    last: listing,
                });
        }
    }

    /// `repository`'s runners listing of the engine's `listing` answered.
    pub(super) fn runners_listed(&mut self, repository: &str, listing: u64) {
        self.runners.insert(repository.to_owned(), listing);
    }

    /// `repository`'s runners listing failed, or the rate limit refused it: what other machines registered is not
    /// known until one answers again.
    pub(super) fn runners_unknown(&mut self, repository: &str) {
        self.runners.remove(repository);
    }

    /// The host slept: nothing listed before holds.
    pub(super) fn slept(&mut self) {
        self.seen.clear();
        self.runners.clear();
    }

    /// Forgets the repositories `listed` no longer says are listed or draining.
    pub(super) fn forget_unlisted(&mut self, listed: impl Fn(&str) -> bool) {
        self.seen.retain(|_, sighting| listed(&sighting.repository));
        self.runners.retain(|repository, _| listed(repository));
    }

    /// Until when, in seconds since the epoch, an overflow host leaves `job` to the other machines at `now` (ms), with
    /// `after` its `[host] overflow_after`; `None` once it may start a runner for it. It may once GitHub queued it
    /// `after` ago, or, where GitHub did not say when, this host first saw it `after` ago; and it was listed twice
    /// here, the second time after a runners listing of its repository that began after the first. Until then it is
    /// held, and past the first condition `until` is in the past.
    pub(super) fn holds(&self, job: &QueuedJob, now: u64, after: Duration) -> Option<u64> {
        let after = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
        let Some(sighting) = self.seen.get(&job.id) else {
            // Not listed here yet: as one listed now.
            return Some(now.saturating_add(after) / 1000);
        };
        let queued = if job.created_at == 0 {
            sighting.first_at
        } else {
            job.created_at
        };
        let until = queued.saturating_add(after);
        let relisted = self
            .runners
            .get(&sighting.repository)
            .is_some_and(|&runners| sighting.first < runners && runners <= sighting.last);
        (until > now || !relisted).then_some(until / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AFTER: Duration = Duration::from_secs(45);
    const T0: u64 = 1_000_000_000;

    fn job(id: u64, created_at: u64) -> QueuedJob {
        QueuedJob {
            id,
            created_at,
            ..QueuedJob::default()
        }
    }

    /// A tick of listing `listing` at `now`: the runners of `o/r` (when `runners`) and then its queued `jobs`.
    fn tick(overflow: &mut Overflow, listing: u64, now: u64, runners: bool, jobs: &[QueuedJob]) {
        if runners {
            overflow.runners_listed("o/r", listing);
        }
        overflow.queued_listed("o/r", jobs, listing, now);
    }

    #[test]
    fn a_job_old_enough_waits_for_a_second_listing_after_a_runners_listing() {
        // Catches a start from the runners listing that began before the job was first listed (finding M3): another
        // machine's runner registered meanwhile is not in it.
        let mut overflow = Overflow::default();
        let old = job(1, T0 - 3_600_000);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&old));
        assert_eq!(
            overflow.holds(&old, T0, AFTER),
            Some((T0 - 3_600_000 + 45_000) / 1000),
            "first seen in listing 1, whose runners listing began before it"
        );
        tick(
            &mut overflow,
            2,
            T0 + 5_000,
            false,
            std::slice::from_ref(&old),
        );
        assert!(
            overflow.holds(&old, T0 + 5_000, AFTER).is_some(),
            "listed again, with no runners listing since the first"
        );
        tick(
            &mut overflow,
            3,
            T0 + 10_000,
            true,
            std::slice::from_ref(&old),
        );
        assert_eq!(overflow.holds(&old, T0 + 10_000, AFTER), None);
    }

    #[test]
    fn a_job_is_held_overflow_after_from_when_github_queued_it() {
        let mut overflow = Overflow::default();
        let young = job(1, T0);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&young));
        tick(
            &mut overflow,
            2,
            T0 + 15_000,
            true,
            std::slice::from_ref(&young),
        );
        assert_eq!(
            overflow.holds(&young, T0 + 44_999, AFTER),
            Some((T0 + 45_000) / 1000)
        );
        assert_eq!(overflow.holds(&young, T0 + 45_000, AFTER), None);
    }

    #[test]
    fn a_job_github_gave_no_time_counts_from_when_it_was_first_listed_here() {
        // Catches finding M2: `created_at` 0 read as 1970, which no hold outlasts.
        let mut overflow = Overflow::default();
        let unknown = job(1, 0);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&unknown));
        tick(
            &mut overflow,
            2,
            T0 + 15_000,
            true,
            std::slice::from_ref(&unknown),
        );
        assert_eq!(
            overflow.holds(&unknown, T0 + 15_000, AFTER),
            Some((T0 + 45_000) / 1000)
        );
        assert_eq!(overflow.holds(&unknown, T0 + 45_000, AFTER), None);
    }

    #[test]
    fn a_failed_runners_listing_holds_every_job_until_one_answers() {
        let mut overflow = Overflow::default();
        let old = job(1, T0 - 3_600_000);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&old));
        tick(
            &mut overflow,
            2,
            T0 + 5_000,
            true,
            std::slice::from_ref(&old),
        );
        assert_eq!(overflow.holds(&old, T0 + 5_000, AFTER), None);
        overflow.runners_unknown("o/r");
        assert!(
            overflow.holds(&old, T0 + 5_000, AFTER).is_some(),
            "a failed runners listing says nothing of other machines' runners"
        );
        tick(
            &mut overflow,
            3,
            T0 + 10_000,
            true,
            std::slice::from_ref(&old),
        );
        assert_eq!(overflow.holds(&old, T0 + 10_000, AFTER), None);
    }

    #[test]
    fn a_job_a_listing_no_longer_shows_starts_over_if_it_comes_back() {
        // A job that left the queue and came back, as a re-queued one after its runner went, gets its listings anew.
        let mut overflow = Overflow::default();
        let old = job(1, T0 - 3_600_000);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&old));
        tick(&mut overflow, 2, T0 + 5_000, true, &[]);
        tick(
            &mut overflow,
            3,
            T0 + 10_000,
            false,
            std::slice::from_ref(&old),
        );
        assert!(
            overflow.holds(&old, T0 + 10_000, AFTER).is_some(),
            "first seen again in listing 3"
        );
        tick(
            &mut overflow,
            4,
            T0 + 15_000,
            true,
            std::slice::from_ref(&old),
        );
        assert_eq!(overflow.holds(&old, T0 + 15_000, AFTER), None);
    }

    #[test]
    fn a_sleep_and_a_job_never_listed_hold_as_a_first_sighting() {
        let mut overflow = Overflow::default();
        let old = job(1, T0 - 3_600_000);
        assert_eq!(
            overflow.holds(&old, T0, AFTER),
            Some((T0 + 45_000) / 1000),
            "never listed here"
        );
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&old));
        tick(
            &mut overflow,
            2,
            T0 + 5_000,
            true,
            std::slice::from_ref(&old),
        );
        overflow.slept();
        tick(
            &mut overflow,
            3,
            T0 + 600_000,
            true,
            std::slice::from_ref(&old),
        );
        assert!(
            overflow.holds(&old, T0 + 600_000, AFTER).is_some(),
            "a sleep forgets the listings before it"
        );
    }

    #[test]
    fn listings_of_another_repository_neither_hold_nor_free_a_job() {
        let mut overflow = Overflow::default();
        let old = job(1, T0 - 3_600_000);
        tick(&mut overflow, 1, T0, true, std::slice::from_ref(&old));
        overflow.runners_listed("o/s", 2);
        overflow.queued_listed("o/s", &[], 2, T0 + 5_000);
        assert!(
            overflow.holds(&old, T0 + 5_000, AFTER).is_some(),
            "o/s's runners say nothing of o/r's"
        );
        overflow.forget_unlisted(|repository| repository == "o/s");
        assert_eq!(
            overflow.holds(&old, T0 + 5_000, AFTER),
            Some((T0 + 5_000 + 45_000) / 1000),
            "o/r is no longer listed, so its job is forgotten"
        );
    }
}
