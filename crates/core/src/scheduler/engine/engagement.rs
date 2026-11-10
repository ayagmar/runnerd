//! What a slot's runner is doing as far as this daemon knows: why it is there, whether GitHub's listing shows it busy,
//! the job a capture or the history named for it, that job as admission tells jobs apart, and a forced pause's
//! deadline for it. It owns that value and must not call Docker, GitHub or the store, nor log: the engine reads
//! those and tells it what each said.

use std::time::SystemTime;

use crate::admission::Key;

use super::Purpose;

/// How a slot's runner came to be there, and what that says of its job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// This daemon started it, for this.
    Started(Purpose),
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
    /// Busy since the last listing, or since GitHub said so after it: the queued jobs listing after it may still show
    /// the job it took, as GitHub lists one queued for a few seconds after.
    NewlyBusy,
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
/// record. Whether a listing showed it newly busy or waiting and when its job was found are lost. A reload does not
/// touch it.
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
    cancel_by: Option<tokio::time::Instant>,
}

impl Engagement {
    /// A runner this daemon starts for `purpose`: idle, with no job.
    pub(super) fn started(purpose: Purpose) -> Self {
        Self {
            origin: Origin::Started(purpose),
            listing: Listed::Idle,
            job: None,
            key: None,
            cancel_by: None,
        }
    }

    /// A runner a restart adopts, running the job its history row names, if any: counted busy until a listing says
    /// otherwise. `cancel_by` is the deadline a forced pause before the restart gave it, as its row kept it.
    pub(super) fn adopted(job: Option<u64>, cancel_by: Option<tokio::time::Instant>) -> Self {
        Self {
            origin: Origin::Adopted(Adoption::Unknown),
            listing: Listed::Unlisted,
            job: job.map(|id| TakenJob { id, found_at: None }),
            key: None,
            cancel_by,
        }
    }

    /// A runners listing is done, whether or not it answered for this runner: the job it may have taken is no
    /// longer listed queued, or that listing showed the job named since.
    pub(super) fn runners_listed(&mut self) {
        if self.listing == Listed::NewlyBusy {
            self.listing = Listed::Busy;
        }
    }

    /// A runners listing showed it busy or idle. Called after `runners_listed`, for the runners it answered for. The
    /// first listing that shows an adopted runner busy shows it newly busy, though adoption counted it busy already.
    /// One that shows an adopted runner waiting with no job named says its next job starts under this daemon.
    pub(super) fn listed(&mut self, busy: bool) {
        self.listing = match (busy, self.listing) {
            (false, _) => Listed::Idle,
            (true, Listed::Idle | Listed::Unlisted | Listed::NewlyBusy) => Listed::NewlyBusy,
            (true, Listed::Busy) => Listed::Busy,
        };
        if let Origin::Adopted(adoption) = &mut self.origin
            && !busy
            && self.job.is_none()
        {
            *adoption = Adoption::Waited;
        }
    }

    /// GitHub said it took a job, as a removal of it refused: newly busy unless a listing showed it busy already. One
    /// no listing answered for yet stays so, for the first that shows it busy to mark it newly busy.
    pub(super) fn kept_busy(&mut self) {
        self.listing = match self.listing {
            Listed::Idle => Listed::NewlyBusy,
            Listed::Busy | Listed::NewlyBusy => Listed::Busy,
            Listed::Unlisted => Listed::Unlisted,
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
    pub(super) fn cancelled(&mut self, deadline: tokio::time::Instant) {
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

    /// Whether it may be running one of its pool's listed jobs that no capture named yet: the listing that first
    /// showed it busy can still show that job queued. A listing clears this (`runners_listed`), so a pass between two
    /// listings keeps counting it.
    pub(super) fn claims_a_listed_job(&self) -> bool {
        self.listing == Listed::NewlyBusy && self.job.is_none()
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
            Origin::Started(purpose) => Some(purpose),
            Origin::Adopted(_) => None,
        }
    }

    /// When this daemon's capture found its job, if that job started under this daemon: `None` for an adopted runner
    /// no listing showed waiting with no job named, whose job may have started before the restart.
    pub(super) fn job_found_here(&self) -> Option<SystemTime> {
        let started_here = match self.origin {
            Origin::Started(_) | Origin::Adopted(Adoption::Waited) => true,
            Origin::Adopted(Adoption::Unknown) => false,
        };
        self.job
            .and_then(|job| job.found_at)
            .filter(|_| started_here)
    }

    /// Its job as admission tells jobs apart, once the history had its record.
    pub(super) fn key(&self) -> Option<&Key> {
        self.key.as_ref()
    }

    /// Whether a forced pause cancelled its run and it still runs past the deadline that gave it to exit.
    pub(super) fn cancel_overdue(&self, now: tokio::time::Instant) -> bool {
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
    pub(super) fn age_job_start(&mut self, by: std::time::Duration) {
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

    /// Whether a forced pause cancelled its run.
    pub(super) fn is_cancelling(&self) -> bool {
        self.cancel_by.is_some()
    }

    /// As if its forced pause's grace ran out.
    pub(super) fn expire_cancel(&mut self, now: tokio::time::Instant) {
        if self.cancel_by.is_some() {
            self.cancel_by = Some(now);
        }
    }
}
