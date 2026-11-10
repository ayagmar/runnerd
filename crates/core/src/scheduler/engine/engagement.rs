//! What a slot's runner is doing as far as this daemon knows: why it is there, whether GitHub's listing shows it busy,
//! the job a capture or the history named for it, that job as admission tells jobs apart, and a forced pause's
//! deadline for it. It owns that value and must not call Docker, GitHub or the store, nor log: the engine reads
//! those and tells it what each said.

use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use crate::admission::Key;

use super::Purpose;

/// How a slot's runner came to be there, and what that says of its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// This daemon started it, for `purpose`, and behind the barrier of job `behind` if it passed one (ADR 0131).
    Started {
        purpose: Purpose,
        behind: Option<u64>,
    },
    /// A restart found its container running and adopted it.
    Adopted(Adoption),
}

/// What the runners listings showed of an adopted runner since its adoption, which says whether its job started
/// under this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adoption {
    /// No listing showed it waiting with no job named: it may have taken its job while no daemon ran, and the
    /// capture that names it after the restart does not say when.
    Unknown,
    /// A listing showed it waiting with no job named: the job it takes next starts under this daemon. A capture names
    /// a job before a listing that lags it shows the runner busy, so a listing that shows it idle once its job is
    /// named says nothing.
    Waited,
}

/// What GitHub's runners listings showed of a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listed {
    /// Idle: the last listing showed it waiting for a job.
    Idle,
    /// Adopted, and no listing answered for it yet. It counts as busy, but no listing showed it so: the first that
    /// does may still show the job it took queued. Only `adopted` makes it.
    Unlisted,
    /// Busy, as the listing before the last already showed it.
    Busy,
    /// Busy since the listing or the answer that `Mark` says: the queued jobs listings of its repository after it may
    /// still show the job it took, as GitHub lists one queued for a few seconds after. Only a listing of those that
    /// began after the mark, and at least `poll_interval` after it, ends it (`queued_listed`); a later runners listing
    /// that shows it busy again leaves the mark as it was.
    NewlyBusy(Mark),
}

/// A listing of GitHub's, as the claims on listed jobs count it: its number, counted from 1 since the engine's start,
/// and when it began on Tokio's clock. A claim is marked with the listing begun last when it was made: a runner first
/// seen busy, with the runners listing that showed it, or the one before GitHub said so; a runner gone, with the one
/// before it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    pub(super) listing: u64,
    pub(super) at: Instant,
}

impl Mark {
    /// Whether a queued jobs listing `self` ends a claim marked with `claim`: it is a later listing, begun at least
    /// `floor` after the claim's, so GitHub's lag in taking a job off the queued listing has as long as when every
    /// listing came one `poll_interval` after the last, whatever the listings' pace (ADR 0128).
    pub(super) fn ends(self, claim: Mark, floor: Duration) -> bool {
        self.listing > claim.listing && self.at >= claim.at + floor
    }
}

/// A job named for a runner, with when this daemon's capture found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TakenJob {
    id: u64,
    /// When this daemon's capture found it; `None` for one only the history named, after a restart.
    found_at: Option<SystemTime>,
}

/// A slot's runner's engagement: whether it holds a job, which, and how it came to be in its slot.
///
/// All four combinations of listed busy and a job named are meant: the runners listing lags a capture that names a
/// job, and a capture lags a listing that shows the runner busy. The key is beside the job, not in it: admission keys
/// a job by the history's record, which a capture that names the job again does not change.
///
/// A restart rebuilds it in `adopted`: adopted and not yet listed, counted busy until a listing says otherwise, with
/// the job its history row names, a forced pause's deadline as its row kept it, and no key until `find_keys` reads its
/// record. Whether a listing showed it newly busy or waiting, when its job was found and the barrier it started behind
/// are lost: every barrier after a restart matures after it, so a backfill would not count an adopted runner anyway. A
/// reload does not touch it, so a newly busy runner keeps its mark through one.
#[derive(Debug, Clone)]
pub(super) struct Engagement {
    origin: Origin,
    /// What GitHub's last runners listing showed of it, or what GitHub said since.
    listing: Listed,
    /// The job its capture or its history row named.
    job: Option<TakenJob>,
    /// That job as admission tells jobs apart, once the history had its record.
    key: Option<Key>,
    /// When a forced pause that cancelled its run removes it if it still runs.
    cancel_by: Option<Instant>,
}

impl Engagement {
    /// A runner this daemon starts for `purpose`, behind the barrier of job `behind` if its plan passed one: idle,
    /// with no job.
    pub(super) fn started(purpose: Purpose, behind: Option<u64>) -> Self {
        Self {
            origin: Origin::Started { purpose, behind },
            listing: Listed::Idle,
            job: None,
            key: None,
            cancel_by: None,
        }
    }

    /// A runner a restart adopts, running the job its history row names, if any: counted busy until a listing says
    /// otherwise. `cancel_by` is the deadline a forced pause before the restart gave it, as its row kept it.
    pub(super) fn adopted(job: Option<u64>, cancel_by: Option<Instant>) -> Self {
        Self {
            origin: Origin::Adopted(Adoption::Unknown),
            listing: Listed::Unlisted,
            job: job.map(|id| TakenJob { id, found_at: None }),
            key: None,
            cancel_by,
        }
    }

    /// A listing of its repository's queued jobs begun at `listing` answered: it is no longer newly busy if that
    /// listing ends its mark (`Mark::ends`).
    pub(super) fn queued_listed(&mut self, listing: Mark, floor: Duration) {
        if let Listed::NewlyBusy(mark) = self.listing
            && listing.ends(mark, floor)
        {
            self.listing = Listed::Busy;
        }
    }

    /// A runners listing that `mark` says showed it busy or idle. The first listing that shows an adopted runner busy
    /// shows it newly busy, though adoption counted it busy already. One that shows a newly busy runner busy again
    /// keeps its mark. One that shows an adopted runner waiting with no job named says its next job starts under this
    /// daemon.
    pub(super) fn listed(&mut self, busy: bool, mark: Mark) {
        self.listing = match (busy, self.listing) {
            (false, _) => Listed::Idle,
            (true, Listed::Idle | Listed::Unlisted) => Listed::NewlyBusy(mark),
            (true, listed @ (Listed::Busy | Listed::NewlyBusy(_))) => listed,
        };
        if let Origin::Adopted(adoption) = &mut self.origin
            && !busy
            && self.job.is_none()
        {
            *adoption = Adoption::Waited;
        }
    }

    /// GitHub said it took a job, as a removal of it refused, when `mark` says: newly busy unless a listing showed it
    /// busy already, and a newly busy one keeps its mark. One no listing answered for yet stays so, for the first that
    /// shows it busy to mark it newly busy.
    pub(super) fn kept_busy(&mut self, mark: Mark) {
        self.listing = match self.listing {
            Listed::Idle => Listed::NewlyBusy(mark),
            listed @ (Listed::Busy | Listed::NewlyBusy(_) | Listed::Unlisted) => listed,
        };
    }

    /// Its capture found `job` at `at`. Its key stays: the history's record of the job says what it is.
    pub(super) fn found(&mut self, job: u64, at: SystemTime) {
        self.job = Some(TakenJob {
            id: job,
            found_at: Some(at),
        });
    }

    /// The job its worker log names, once it stopped, for a runner no capture named a job for.
    pub(super) fn named_by_worker_log(&mut self, job: u64) {
        if !self.runs(job) {
            self.job = Some(TakenJob {
                id: job,
                found_at: None,
            });
        }
    }

    /// The history has a record of its job, which admission tells apart by `key`.
    pub(super) fn keyed(&mut self, key: Key) {
        self.key = Some(key);
    }

    /// A forced pause cancelled its run: it is removed if it still runs at `deadline`.
    pub(super) fn cancelled(&mut self, deadline: Instant) {
        self.cancel_by = Some(deadline);
    }

    /// Whether GitHub's last runners listing showed it busy, or GitHub said so since; an adopted runner no listing
    /// answered for yet counts as busy. Alone it says what the listing says, which lags a capture that named a job:
    /// `engaged` is whether it holds one.
    pub(super) fn listed_busy(&self) -> bool {
        self.listing != Listed::Idle
    }

    /// The job its capture or its history row named; a listing that shows it idle does not unname it.
    pub(super) fn named_job(&self) -> Option<u64> {
        self.job.map(|job| job.id)
    }

    /// Whether `job` is the one named for it.
    pub(super) fn runs(&self, job: u64) -> bool {
        self.named_job() == Some(job)
    }

    /// Whether it holds a job as far as this daemon knows: the listing shows it busy, or a capture named its job
    /// first.
    pub(super) fn engaged(&self) -> bool {
        self.listed_busy() || self.job.is_some()
    }

    /// Whether it may be running one of its pool's listed jobs that no capture named yet: the listings of its
    /// repository until `queued_listed` ends it can still show that job queued.
    pub(super) fn claims_a_listed_job(&self) -> bool {
        matches!(self.listing, Listed::NewlyBusy(_)) && self.job.is_none()
    }

    /// Whether a detaching stop removes it: no job named and either listed idle or adopted, as adoption counts a
    /// runner busy until a listing says otherwise. One started here that a listing shows busy is left to its job.
    pub(super) fn goes_when_detaching(&self) -> bool {
        self.job.is_none() && (!self.listed_busy() || !self.was_started_here())
    }

    /// Whether this daemon started it: a restart adopted it otherwise.
    pub(super) fn was_started_here(&self) -> bool {
        self.purpose().is_some()
    }

    /// Why this daemon started it; `None` for one a restart adopted.
    pub(super) fn purpose(&self) -> Option<Purpose> {
        match self.origin {
            Origin::Started { purpose, .. } => Some(purpose),
            Origin::Adopted(_) => None,
        }
    }

    /// When this daemon's capture found its job, if that job started under this daemon: `None` for an adopted runner
    /// no listing showed waiting with no job named, whose job may have started before the restart.
    pub(super) fn job_found_here(&self) -> Option<SystemTime> {
        let started_here = match self.origin {
            Origin::Started { .. } | Origin::Adopted(Adoption::Waited) => true,
            Origin::Adopted(Adoption::Unknown) => false,
        };
        self.job
            .and_then(|job| job.found_at)
            .filter(|_| started_here)
    }

    /// The owner of the barrier it started behind; `None` for one started otherwise or adopted by a restart.
    pub(super) fn behind(&self) -> Option<u64> {
        match self.origin {
            Origin::Started { behind, .. } => behind,
            Origin::Adopted(_) => None,
        }
    }

    /// Its job as admission tells jobs apart, once the history had its record.
    pub(super) fn key(&self) -> Option<&Key> {
        self.key.as_ref()
    }

    /// Whether a forced pause cancelled its run and it still runs past the deadline that gave it to exit.
    pub(super) fn cancel_overdue(&self, now: Instant) -> bool {
        self.cancel_by.is_some_and(|deadline| deadline <= now)
    }
}

#[cfg(test)]
impl Engagement {
    /// When its capture found its job; `None` before.
    pub(super) fn job_found_at(&self) -> Option<SystemTime> {
        self.job.and_then(|job| job.found_at)
    }

    /// As if its capture had found its job at `at`.
    pub(super) fn found_job_at(&mut self, at: SystemTime) {
        if let Some(job) = &mut self.job {
            job.found_at = Some(at);
        }
    }

    /// As if its capture had found its job `by` earlier.
    pub(super) fn age_job_start(&mut self, by: Duration) {
        if let Some(TakenJob {
            found_at: Some(at), ..
        }) = &mut self.job
        {
            *at -= by;
        }
    }

    /// As a listing shows it busy, with no job named.
    pub(super) fn set_listed_busy(&mut self) {
        self.count_busy();
    }

    /// As if it had taken `job`, which a listing shows; when a capture found a job stays.
    pub(super) fn has_taken(&mut self, job: u64) {
        self.job = Some(TakenJob {
            id: job,
            found_at: self.job.and_then(|job| job.found_at),
        });
        self.count_busy();
    }

    /// Busy, newly or not as it was: an idle one becomes busy, not newly.
    fn count_busy(&mut self) {
        if self.listing == Listed::Idle {
            self.listing = Listed::Busy;
        }
    }

    /// As if a listing showed it idle before any job was named, nor its key known.
    pub(super) fn unname(&mut self) {
        self.job = None;
        self.key = None;
        self.listing = Listed::Idle;
    }

    pub(super) fn set_key(&mut self, key: Key) {
        self.key = Some(key);
    }

    /// As if it was first seen busy `by` earlier.
    pub(super) fn age_mark(&mut self, by: Duration) {
        if let Listed::NewlyBusy(mark) = &mut self.listing
            && let Some(at) = mark.at.checked_sub(by)
        {
            mark.at = at;
        }
    }

    /// Whether a forced pause cancelled its run.
    pub(super) fn is_cancelling(&self) -> bool {
        self.cancel_by.is_some()
    }

    /// As if its forced pause's grace ran out.
    pub(super) fn expire_cancel(&mut self, now: Instant) {
        if self.cancel_by.is_some() {
            self.cancel_by = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_secs(15);

    /// Listing `listing`, begun `secs` after `start`.
    fn mark(start: Instant, listing: u64, secs: u64) -> Mark {
        Mark {
            listing,
            at: start + Duration::from_secs(secs),
        }
    }

    /// A runner this daemon started, which listing 1 shows busy before any capture names its job.
    fn newly_busy(start: Instant) -> Engagement {
        let mut engagement = Engagement::started(Purpose::Job, None);
        engagement.listed(true, mark(start, 1, 0));
        assert!(engagement.claims_a_listed_job(), "listing 1 marks it");
        engagement
    }

    #[test]
    fn runners_listings_that_show_it_busy_again_leave_its_mark_as_it_was() {
        // Its repository is listed every 5 s while active, its runners every third probe.
        let start = Instant::now();
        let mut engagement = newly_busy(start);
        for (listing, secs) in [(2, 5), (3, 10)] {
            engagement.listed(true, mark(start, listing, secs));
            engagement.queued_listed(mark(start, listing, secs), POLL);
            assert!(
                engagement.claims_a_listed_job(),
                "listing {listing}, {secs} s after the mark, may still show its job"
            );
        }
        engagement.listed(true, mark(start, 4, 15));
        engagement.queued_listed(mark(start, 4, 15), POLL);
        assert!(
            !engagement.claims_a_listed_job(),
            "the first listing a poll interval after the first mark ends the claim"
        );
        assert!(engagement.listed_busy(), "it stays busy");
    }

    #[test]
    fn only_a_later_listing_ends_the_claim_however_late_it_answers() {
        let start = Instant::now();
        let mut engagement = newly_busy(start);
        // Listing 1's queued jobs answer long after it began, which is what counts.
        engagement.queued_listed(mark(start, 1, 60), POLL);
        assert!(
            engagement.claims_a_listed_job(),
            "the listing that marked it may still show its job"
        );
    }

    #[test]
    fn github_saying_a_newly_busy_runner_is_busy_keeps_its_mark() {
        let start = Instant::now();
        let mut engagement = newly_busy(start);
        engagement.kept_busy(mark(start, 3, 10));
        engagement.queued_listed(mark(start, 4, 15), POLL);
        assert!(
            !engagement.claims_a_listed_job(),
            "the claim ends a poll interval after the first mark, not the refused removal"
        );
    }
}
