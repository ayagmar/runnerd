//! What the last listing of one repository's queued jobs showed: nothing known, the jobs it listed, those jobs gone
//! stale once a stop ended the listings, or why the listings fail. It owns that value and must not call GitHub or log:
//! `tick` lists the jobs and words what came of each listing in the log; `api::ListingFailure` words a failure for
//! clients.

use std::time::SystemTime;

use crate::api;
use crate::github::{self, QueuedJob};

/// A repository's queued jobs as its listings last showed them.
///
/// Nothing of it survives a restart: every repository starts unknown, so a failure is forgotten. A reload keeps it for
/// a repository the config still lists; one it removes drains with no listing, and one added back starts unknown.
#[derive(Debug, Default)]
pub(super) struct Listing(Shown);

#[derive(Debug, Default)]
enum Shown {
    /// No listing has answered since the start, or the rate limit stopped the last one.
    #[default]
    Unknown,
    /// The listings fail, the first of this streak at `since`, the last for `problem`.
    Failed {
        since: SystemTime,
        problem: api::ListingProblem,
    },
    /// The jobs the last listing answered with.
    Listed(Vec<QueuedJob>),
    /// The jobs the last listing answered with before a stop ended the listings: what was queued then, not now.
    Stale(Vec<QueuedJob>),
}

impl Listing {
    /// A listing answered with `jobs`, possibly none.
    pub(super) fn answer(&mut self, jobs: Vec<QueuedJob>) {
        self.0 = Shown::Listed(jobs);
    }

    /// The rate limit stopped a listing: what was queued is no longer known, and nothing says the listing fails.
    pub(super) fn fail(&mut self) {
        self.0 = Shown::Unknown;
    }

    /// A listing failed at `at` with `error`: what was queued is no longer known. A failure after a failure keeps when
    /// the streak began. The rate limit is no failure of the listing: `fail`.
    pub(super) fn fail_with(&mut self, error: &github::Error, at: SystemTime) {
        let problem = problem(error);
        self.0 = match &self.0 {
            Shown::Failed { since, .. } => Shown::Failed {
                since: *since,
                problem,
            },
            Shown::Unknown | Shown::Listed(_) | Shown::Stale(_) => {
                Shown::Failed { since: at, problem }
            }
        };
    }

    /// Why the listings fail, while they do, as clients read it.
    pub(super) fn failure(&self) -> Option<api::ListingFailure> {
        match &self.0 {
            Shown::Failed { since, problem } => Some(api::ListingFailure {
                since: crate::time::ms(*since) / 1000,
                problem: problem.clone(),
            }),
            Shown::Unknown | Shown::Listed(_) | Shown::Stale(_) => None,
        }
    }

    /// A stop ended the listings: the jobs listed last go stale.
    pub(super) fn stop(&mut self) {
        if let Shown::Listed(jobs) = &mut self.0 {
            self.0 = Shown::Stale(std::mem::take(jobs));
        }
    }

    /// Replaces the jobs and keeps whether they are listed, as the listing's jobs alone used to be written.
    #[cfg(test)]
    pub(super) fn replace_jobs(&mut self, jobs: Vec<QueuedJob>) {
        self.0 = match &self.0 {
            Shown::Listed(_) => Shown::Listed(jobs),
            Shown::Unknown | Shown::Failed { .. } | Shown::Stale(_) => Shown::Stale(jobs),
        };
    }

    /// The jobs the last listing answered with, unless it failed or a stop made them stale.
    pub(super) fn listed(&self) -> Option<&[QueuedJob]> {
        match &self.0 {
            Shown::Listed(jobs) => Some(jobs),
            Shown::Unknown | Shown::Failed { .. } | Shown::Stale(_) => None,
        }
    }

    /// Whether the last listing answered and no stop made its jobs stale.
    pub(super) fn is_listed(&self) -> bool {
        self.listed().is_some()
    }

    /// Whether the last listing answered without job `id`: an unknown or stale listing says nothing of it.
    pub(super) fn no_longer_shows(&self, id: u64) -> bool {
        self.listed()
            .is_some_and(|jobs| !jobs.iter().any(|job| job.id == id))
    }

    /// The jobs the last listing that answered showed, stale or not; none once a listing failed.
    pub(super) fn jobs(&self) -> &[QueuedJob] {
        match &self.0 {
            Shown::Listed(jobs) | Shown::Stale(jobs) => jobs,
            Shown::Unknown | Shown::Failed { .. } => &[],
        }
    }
}

/// What a failed listing tells clients of `error`: its class and GitHub's status, never GitHub's text, which could
/// echo what was sent.
fn problem(error: &github::Error) -> api::ListingProblem {
    match error {
        github::Error::Token(_) => api::ListingProblem::NoToken,
        github::Error::Status { status, .. } if status.as_u16() == 401 => {
            api::ListingProblem::Unauthorized
        }
        github::Error::Status { status, .. } if matches!(status.as_u16(), 403 | 404 | 410) => {
            api::ListingProblem::Refused {
                status: status.as_u16(),
            }
        }
        github::Error::Status { status, .. } => api::ListingProblem::Failed {
            status: status.as_u16(),
        },
        github::Error::Http { .. } => api::ListingProblem::Unreachable,
        github::Error::Decode { .. } => api::ListingProblem::Unexpected,
        // `tick` takes the rate limit apart before; were one to come here, nothing more is known.
        github::Error::RateLimited { .. } => api::ListingProblem::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64) -> QueuedJob {
        QueuedJob {
            id,
            run_id: id,
            name: format!("job {id}"),
            labels: Vec::new(),
            created_at: 0,
            run_attempt: 1,
            workflow: None,
            branch: None,
            url: None,
        }
    }

    fn ids(jobs: &[QueuedJob]) -> Vec<u64> {
        jobs.iter().map(|job| job.id).collect()
    }

    #[test]
    fn a_repository_never_listed_shows_no_jobs_and_is_not_listed() {
        let listing = Listing::default();
        assert_eq!(listing.listed().map(ids), None);
        assert!(listing.jobs().is_empty(), "nothing is known yet");
        assert!(
            !listing.no_longer_shows(1),
            "an unknown listing says nothing of job 1"
        );
    }

    #[test]
    fn an_empty_listing_is_listed_and_drops_every_job() {
        let mut listing = Listing::default();
        listing.answer(Vec::new());
        assert_eq!(listing.listed().map(ids), Some(Vec::new()));
        assert!(
            listing.no_longer_shows(1),
            "an empty listing no longer shows job 1"
        );
    }

    #[test]
    fn a_listing_that_fails_after_one_that_answered_forgets_its_jobs() {
        let mut listing = Listing::default();
        listing.answer(vec![job(1)]);
        assert!(!listing.no_longer_shows(1), "the answer shows job 1");
        assert!(listing.no_longer_shows(2), "the answer does not show job 2");
        listing.fail();
        assert_eq!(listing.listed().map(ids), None);
        assert!(listing.jobs().is_empty(), "a failed listing keeps no job");
        assert!(
            !listing.no_longer_shows(2),
            "a failed listing says nothing of job 2"
        );
    }

    fn bad_gateway() -> github::Error {
        github::Error::Status {
            method: reqwest::Method::GET,
            path: "/repos/o/r/actions/runs".to_owned(),
            status: reqwest::StatusCode::BAD_GATEWAY,
            message: "upstream said: token ghp_secret".to_owned(),
        }
    }

    #[test]
    fn failures_keep_when_they_began_until_an_answer_or_a_rate_limit_ends_them() {
        let start = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let later = start + std::time::Duration::from_secs(60);
        let mut listing = Listing::default();
        assert_eq!(listing.failure(), None, "never listed is no failure");
        listing.fail_with(&bad_gateway(), start);
        listing.fail_with(&github::Error::Token("gh is logged out".to_owned()), later);
        assert_eq!(
            listing.failure(),
            Some(api::ListingFailure {
                since: 1000,
                problem: api::ListingProblem::NoToken,
            }),
            "the streak's start, the last failure's class, no text of it"
        );
        assert!(listing.jobs().is_empty(), "a failed listing keeps no job");
        listing.answer(vec![job(1)]);
        assert_eq!(listing.failure(), None, "an answer clears it");
        listing.fail_with(&bad_gateway(), later);
        listing.fail();
        assert_eq!(
            listing.failure(),
            None,
            "a rate limit says nothing of a failure"
        );
        listing.fail_with(&bad_gateway(), later);
        assert_eq!(
            listing.failure().map(|failure| failure.since),
            Some(1060),
            "a new streak starts over"
        );
    }

    #[test]
    fn a_stop_keeps_the_jobs_listed_last_but_no_longer_lists_them() {
        let mut listing = Listing::default();
        listing.answer(vec![job(1), job(2)]);
        listing.stop();
        assert_eq!(listing.listed().map(ids), None);
        assert!(!listing.is_listed(), "a stopped listing is not listed");
        assert_eq!(ids(listing.jobs()), [1, 2]);
        assert!(
            !listing.no_longer_shows(3),
            "a stale listing says nothing of job 3"
        );
        listing.stop();
        assert_eq!(ids(listing.jobs()), [1, 2], "a second stop keeps them");
    }

    #[test]
    fn a_stop_after_a_failed_listing_shows_nothing() {
        let mut listing = Listing::default();
        listing.answer(vec![job(1)]);
        listing.fail();
        listing.stop();
        assert!(
            listing.jobs().is_empty(),
            "nothing was known when the stop came"
        );
    }

    #[test]
    fn a_listing_after_a_stop_lists_again() {
        let mut listing = Listing::default();
        listing.answer(vec![job(1)]);
        listing.stop();
        listing.answer(vec![job(2)]);
        assert_eq!(listing.listed().map(ids), Some(vec![2]));
    }

    /// The two fields the listing replaces, with the writes `tick` made to them.
    #[derive(Default)]
    struct Fields {
        queued: Vec<QueuedJob>,
        listed: bool,
    }

    #[derive(Debug, Clone)]
    enum Step {
        Answer(Vec<u64>),
        Fail,
        Stop,
        /// A restart, or a reload that removes the repository and adds it back: a fresh listing.
        Restart,
    }

    fn step(at: u64) -> Step {
        match at % 7 {
            0 | 1 => Step::Answer((0..at % 3).map(|id| id + at % 5).collect()),
            2 => Step::Answer(Vec::new()),
            3 => Step::Fail,
            4 | 5 => Step::Stop,
            _ => Step::Restart,
        }
    }

    #[test]
    fn a_generated_sequence_reads_as_the_two_fields_did() {
        for seed in 0..64_u64 {
            let mut listing = Listing::default();
            let mut fields = Fields::default();
            let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let mut steps = Vec::new();
            for _ in 0..200 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let next = step(state >> 33);
                steps.push(next.clone());
                match next {
                    Step::Answer(answered) => {
                        let jobs: Vec<QueuedJob> = answered.into_iter().map(job).collect();
                        listing.answer(jobs.clone());
                        fields.queued = jobs;
                        fields.listed = true;
                    }
                    Step::Fail => {
                        listing.fail();
                        fields.queued = Vec::new();
                        fields.listed = false;
                    }
                    Step::Stop => {
                        listing.stop();
                        fields.listed = false;
                    }
                    Step::Restart => {
                        listing = Listing::default();
                        fields = Fields::default();
                    }
                }
                let listed = fields.listed.then(|| ids(&fields.queued));
                assert_eq!(listing.listed().map(ids), listed, "after {steps:?}");
                assert_eq!(ids(listing.jobs()), ids(&fields.queued), "after {steps:?}");
                for id in 0..8 {
                    assert_eq!(
                        listing.no_longer_shows(id),
                        fields.listed && !fields.queued.iter().any(|job| job.id == id),
                        "job {id} after {steps:?}"
                    );
                }
            }
        }
    }
}
