//! What runnerd asks GitHub, counted by class (each a finite endpoint, ADR 0179) and by what GitHub answered, and the rate limit the last answer gave,
//! with the words of the log line that reports them. It decides nothing: the pace of the reads (ADR 0127) is the
//! scheduler's, from `Spend`, and a count names a class, never a repository, a path or the token. Nothing here survives a restart, and a
//! config reload does not touch it: a window starts when the client is made, or when the last line was written.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};

use crate::api::duration;

/// How long a window of counts runs before its line is logged. The line comes with the first request after it.
pub(super) const WINDOW: Duration = Duration::from_hours(1);

/// What a request asked for, as the engine's own work: the requests of one class cost about the same and are made for
/// the same reason. The first six are the engine's polls and runners; the rest are what a line sums as `other` and
/// lists one by one beside it, each a finite endpoint, never a repository, a number or a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A repository's runs, queued or in progress.
    Runs,
    /// The jobs of one run.
    Jobs,
    /// A repository's registered runners.
    Runners,
    /// One job by ID: the engine reads each running job's steps this way, and the history reads others.
    JobRead,
    /// A runner's JIT registration.
    Registration,
    /// A runner's deletion.
    Delete,
    /// One run read for who started it, as the build cache decides whether a job may write (ADR 0052).
    Trigger,
    /// One run read for anything else: its workflow (ADR 0140), or its attempt for a job action (ADR 0134).
    Run,
    /// One pull request, read for who opened it and its head and base (ADR 0053).
    PullRequest,
    /// One branch's head, read for whether it has a job's commit.
    Branch,
    /// Two commits compared, for whether a branch has a job's commit.
    Compare,
    /// A repository's details: its default branch and visibility.
    Repository,
    /// A repository's workflows, for their names (ADR 0140).
    Workflows,
    /// A branch's ref, read by the action archives (ADR 0147).
    ArchiveRef,
    /// A commit's tree, read by the action archives.
    ArchiveTree,
    /// A file's blob, read by the action archives.
    ArchiveBlob,
    /// The jobs of one attempt of a run, read before a re-run (ADR 0134).
    AttemptJobs,
    /// A run's cancellation.
    Cancel,
    /// A re-run of a job or of a run's failed jobs.
    Rerun,
    /// Anything no class above names.
    Other,
}

impl Class {
    /// Every class, in the order a line lists them.
    pub const ALL: [Class; 20] = [
        Class::Runs,
        Class::Jobs,
        Class::Runners,
        Class::JobRead,
        Class::Registration,
        Class::Delete,
        Class::Trigger,
        Class::Run,
        Class::PullRequest,
        Class::Branch,
        Class::Compare,
        Class::Repository,
        Class::Workflows,
        Class::ArchiveRef,
        Class::ArchiveTree,
        Class::ArchiveBlob,
        Class::AttemptJobs,
        Class::Cancel,
        Class::Rerun,
        Class::Other,
    ];

    /// How many classes a line lists on their own; those after them it sums as `other`.
    const OWN: usize = 6;

    fn label(self) -> &'static str {
        match self {
            Class::Runs => "runs",
            Class::Jobs => "jobs",
            Class::Runners => "runners",
            Class::JobRead => "job reads",
            Class::Registration => "registrations",
            Class::Delete => "deletes",
            Class::Trigger => "run triggers",
            Class::Run => "run reads",
            Class::PullRequest => "pull requests",
            Class::Branch => "branches",
            Class::Compare => "compares",
            Class::Repository => "repositories",
            Class::Workflows => "workflows",
            Class::ArchiveRef => "archive refs",
            Class::ArchiveTree => "archive trees",
            Class::ArchiveBlob => "archive blobs",
            Class::AttemptJobs => "attempt jobs",
            Class::Cancel => "cancels",
            Class::Rerun => "re-runs",
            Class::Other => "unclassified",
        }
    }

    /// The class of a request, from its method and its path under the API's base. A run read for its trigger has
    /// the path of any other run read: its caller names it `Trigger`.
    pub(super) fn of(method: &Method, path: &str) -> Class {
        let path = path.split_once('?').map_or(path, |(path, _)| path);
        let segments: Vec<&str> = path.split('/').skip(1).collect();
        match (method.as_str(), segments.as_slice()) {
            ("GET", ["repos", _, _, "actions", "runs"]) => Class::Runs,
            ("GET", ["repos", _, _, "actions", "runs", _, "jobs"]) => Class::Jobs,
            ("GET", ["repos", _, _, "actions", "runners"]) => Class::Runners,
            ("GET", ["repos", _, _, "actions", "jobs", _]) => Class::JobRead,
            ("POST", ["repos", _, _, "actions", "runners", "generate-jitconfig"]) => {
                Class::Registration
            }
            ("DELETE", ["repos", _, _, "actions", "runners", _]) => Class::Delete,
            ("GET", ["repos", _, _, "actions", "runs", _]) => Class::Run,
            ("GET", ["repos", _, _, "pulls", _]) => Class::PullRequest,
            // A branch's name may hold `/`.
            ("GET", ["repos", _, _, "branches", _, ..]) => Class::Branch,
            ("GET", ["repos", _, _, "compare", ..]) => Class::Compare,
            ("GET", ["repos", _, _]) => Class::Repository,
            ("GET", ["repos", _, _, "actions", "workflows"]) => Class::Workflows,
            ("GET", ["repos", _, _, "git", "ref", ..]) => Class::ArchiveRef,
            ("GET", ["repos", _, _, "git", "trees", _]) => Class::ArchiveTree,
            ("GET", ["repos", _, _, "git", "blobs", _]) => Class::ArchiveBlob,
            ("GET", ["repos", _, _, "actions", "runs", _, "attempts", _, "jobs"]) => {
                Class::AttemptJobs
            }
            ("POST", ["repos", _, _, "actions", "runs", _, "cancel"]) => Class::Cancel,
            (
                "POST",
                ["repos", _, _, "actions", "jobs", _, "rerun"]
                | ["repos", _, _, "actions", "runs", _, "rerun-failed-jobs"],
            ) => Class::Rerun,
            _ => Class::Other,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// How many requests of one class got each kind of answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Answered with anything but the two below: the answer counts against the primary limit.
    pub fresh: u64,
    /// 304: the answer GitHub kept for the request's `ETag` was still current.
    pub unchanged: u64,
    /// 403 or 429: a rate limit, primary or secondary, or a token that may not do this.
    pub refused: u64,
}

impl Tally {
    /// Every request of the class.
    pub fn total(&self) -> u64 {
        self.fresh + self.unchanged + self.refused
    }
}

/// The primary limit's last reading: GitHub sends it with every answer, 304s included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests an hour.
    pub limit: u64,
    /// Requests left before `reset`.
    pub remaining: u64,
    /// When the count starts over.
    pub reset: SystemTime,
}

impl RateLimit {
    /// The `core` resource's reading in `headers`, if they carry one: the `search` and `graphql` resources have
    /// their own, which say nothing about the requests made here.
    fn read(headers: &HeaderMap) -> Option<RateLimit> {
        let resource = headers
            .get("x-ratelimit-resource")
            .and_then(|value| value.to_str().ok());
        if resource.is_some_and(|resource| resource != "core") {
            return None;
        }
        Some(RateLimit {
            limit: super::header_number(headers, "x-ratelimit-limit")?,
            remaining: super::header_number(headers, "x-ratelimit-remaining")?,
            reset: SystemTime::UNIX_EPOCH
                + Duration::from_secs(super::header_number(headers, "x-ratelimit-reset")?),
        })
    }
}

/// What the scheduler paces GitHub's reads by: the limit as the last answer gave it, and how many registrations and
/// deletes were sent since the client was made, a count no window's line starts over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spend {
    /// The primary limit's last reading, if any answer carried one.
    pub rate_limit: Option<RateLimit>,
    /// Registrations and deletes sent, whatever GitHub answered.
    pub mutations: u64,
}

/// What a window counted, and the limit's last reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    tallies: [Tally; Class::ALL.len()],
    rate_limit: Option<RateLimit>,
    /// How long the window ran.
    window: Duration,
}

impl Report {
    /// The requests of `class`.
    pub fn tally(&self, class: Class) -> Tally {
        self.tallies[class.index()]
    }

    /// Whether no request was counted.
    pub fn is_empty(&self) -> bool {
        Class::ALL
            .iter()
            .all(|class| self.tally(*class).total() == 0)
    }

    /// The limit as the last answer of the client's life said, if any did: kept across windows.
    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.rate_limit
    }

    /// How long the window ran.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Every request of the classes a line sums as `other`.
    pub fn other(&self) -> Tally {
        Class::ALL[Class::OWN..]
            .iter()
            .map(|class| self.tally(*class))
            .fold(Tally::default(), |sum, tally| Tally {
                fresh: sum.fresh + tally.fresh,
                unchanged: sum.unchanged + tally.unchanged,
                refused: sum.refused + tally.refused,
            })
    }

    /// The log line: `GitHub requests in the last 1h 00m: runs 480 (120 fresh, 360 unchanged), … , other 40 (30
    /// fresh, 10 unchanged) [run triggers 25 (5 fresh, 20 unchanged), repositories 15 (…)]; rate limit 4200 of 5000
    /// left, resets in 31m 12s`. The bracket lists each class `other` sums that was asked at all; a refused count
    /// shows only once it is not 0.
    pub fn line(&self, now: SystemTime) -> String {
        let mut line = format!(
            "GitHub requests in the last {}: ",
            duration(self.window.as_secs())
        );
        for (nth, class) in Class::ALL[..Class::OWN].iter().enumerate() {
            if nth > 0 {
                line.push_str(", ");
            }
            push_tally(&mut line, class.label(), self.tally(*class));
        }
        line.push_str(", ");
        push_tally(&mut line, "other", self.other());
        let asked: Vec<Class> = Class::ALL[Class::OWN..]
            .iter()
            .copied()
            .filter(|class| self.tally(*class).total() > 0)
            .collect();
        if !asked.is_empty() {
            line.push_str(" [");
            for (nth, class) in asked.into_iter().enumerate() {
                if nth > 0 {
                    line.push_str(", ");
                }
                push_tally(&mut line, class.label(), self.tally(class));
            }
            line.push(']');
        }
        match self.rate_limit {
            Some(limit) => {
                let _ = write!(
                    line,
                    "; rate limit {} of {} left, ",
                    limit.remaining, limit.limit
                );
                match limit.reset.duration_since(now) {
                    Ok(left) => {
                        let _ = write!(line, "resets in {}", duration(left.as_secs()));
                    }
                    Err(_) => line.push_str("reset passed"),
                }
            }
            None => line.push_str("; rate limit not read"),
        }
        line
    }
}

/// `label 12 (10 fresh, 2 unchanged)`, with `, 1 refused` before the bracket once any was.
fn push_tally(line: &mut String, label: &str, tally: Tally) {
    let _ = write!(
        line,
        "{label} {} ({} fresh, {} unchanged",
        tally.total(),
        tally.fresh,
        tally.unchanged
    );
    if tally.refused > 0 {
        let _ = write!(line, ", {} refused", tally.refused);
    }
    line.push(')');
}

/// The counts of the window now running.
pub(super) struct Counter {
    report: Report,
    /// Registrations and deletes since the client was made.
    mutations: u64,
    began: Instant,
    /// How long a window runs.
    window: Duration,
}

impl Counter {
    pub(super) fn new(now: Instant, window: Duration) -> Counter {
        Counter {
            report: Report {
                tallies: [Tally::default(); Class::ALL.len()],
                rate_limit: None,
                window: Duration::ZERO,
            },
            mutations: 0,
            began: now,
            window,
        }
    }

    /// A response to a request of `class`, and the limit it carried.
    pub(super) fn record(&mut self, class: Class, status: StatusCode, headers: &HeaderMap) {
        if matches!(class, Class::Registration | Class::Delete) {
            self.mutations += 1;
        }
        let tally = &mut self.report.tallies[class.index()];
        match status {
            StatusCode::NOT_MODIFIED => tally.unchanged += 1,
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => tally.refused += 1,
            _ => tally.fresh += 1,
        }
        // A refused token gets the unauthenticated reading, which is not the account's.
        if status != StatusCode::UNAUTHORIZED
            && let Some(limit) = RateLimit::read(headers)
        {
            self.report.rate_limit = Some(limit);
        }
    }

    /// What the scheduler paces by, now.
    pub(super) fn spend(&self) -> Spend {
        Spend {
            rate_limit: self.report.rate_limit,
            mutations: self.mutations,
        }
    }

    /// What the window counted until `now`, without ending it.
    pub(super) fn peek(&self, now: Instant) -> Report {
        Report {
            window: now.saturating_duration_since(self.began),
            ..self.report.clone()
        }
    }

    /// The report of the window if it ran its time; the next one starts.
    pub(super) fn due(&mut self, now: Instant) -> Option<Report> {
        if now.saturating_duration_since(self.began) < self.window {
            return None;
        }
        Some(self.take(now))
    }

    /// The report of the window until `now`; the next one starts empty, with the limit still as last read.
    pub(super) fn take(&mut self, now: Instant) -> Report {
        let report = self.peek(now);
        self.report.tallies = [Tally::default(); Class::ALL.len()];
        self.began = now;
        report
    }
}
