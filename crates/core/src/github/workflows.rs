//! Which workflow each run belongs to and what each workflow is named, as the client learnt them (ADR 0140). GitHub
//! fills a job's `workflow_name` with its run's name, which a workflow's `run-name` changes on every run; a job's
//! workflow is its run's `workflow_id`, and that workflow's name is the one
//! `GET /repos/{owner}/{repo}/actions/workflows` gives. Owns, per repository, the runs' workflows, the last list of
//! names and its age, when a read last failed, and the runs whose job reads kept GitHub's name for want of the
//! workflow's; it says what to read next and asks GitHub nothing itself. It is pure: the caller passes the time. In
//! memory only: a restart starts empty and learns them again, and the runs owed a name then keep GitHub's.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// The most runs whose workflow is kept per repository; past it the first noted go.
const RUNS_KEPT: usize = 2048;

/// The most runs owed a name per repository; past it the first owed go, and keep GitHub's name.
const OWED_KEPT: usize = 256;

/// How long a list of names is taken as current: a renamed workflow takes its new name within this long, and one the
/// list lacked is looked for again after it.
pub(super) const NAMES_FRESH: Duration = Duration::from_secs(3600);

/// How long after a failed read of a run or of the list nothing more is read for the repository.
pub(super) const RETRY_AFTER: Duration = Duration::from_secs(60);

/// What the client learnt of each repository's workflows and runs.
#[derive(Debug)]
pub(super) struct Workflows {
    repositories: HashMap<String, Learnt>,
    fresh: Duration,
    retry: Duration,
}

impl Default for Workflows {
    fn default() -> Workflows {
        Workflows::new(NAMES_FRESH, RETRY_AFTER)
    }
}

/// One repository's. Its fields are independent: the runs read, the last list read, when a read last failed, and the
/// runs owed a name.
#[derive(Debug, Default)]
struct Learnt {
    /// Each run's workflow, `None` for a run GitHub gives none for or no longer has.
    runs: Bounded<Option<u64>>,
    listed: Option<Listed>,
    failed_at: Option<Instant>,
    owed: Bounded<()>,
}

/// One read of the repository's workflows.
#[derive(Debug)]
struct Listed {
    at: Instant,
    names: HashMap<u64, String>,
    /// Workflows asked for that this read did not list: not read again for until it is old.
    missing: HashSet<u64>,
}

/// A map of run IDs that keeps the `limit` last inserted: a run's ID says nothing of when it was seen, as a re-run
/// keeps its run's.
#[derive(Debug)]
struct Bounded<V> {
    values: HashMap<u64, V>,
    order: VecDeque<u64>,
    limit: usize,
}

impl<V> Bounded<V> {
    fn with_limit(limit: usize) -> Bounded<V> {
        Bounded {
            values: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    fn insert(&mut self, run: u64, value: V) {
        if self.values.insert(run, value).is_none() {
            self.order.push_back(run);
        }
        while self.order.len() > self.limit {
            if let Some(first) = self.order.pop_front() {
                self.values.remove(&first);
            }
        }
    }

    fn remove(&mut self, run: u64) {
        if self.values.remove(&run).is_some() {
            self.order.retain(|kept| *kept != run);
        }
    }
}

impl Default for Bounded<Option<u64>> {
    fn default() -> Self {
        Bounded::with_limit(RUNS_KEPT)
    }
}

impl Default for Bounded<()> {
    fn default() -> Self {
        Bounded::with_limit(OWED_KEPT)
    }
}

/// What resolving a run's workflow name needs next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Next {
    /// Its workflow's name.
    Named(String),
    /// GitHub gives the run no workflow, or does not list its workflow: GitHub's own name stays.
    Unnamed,
    /// The run is to be read for its workflow.
    ReadRun,
    /// The repository's workflows are to be read for this one's name.
    ReadList(u64),
    /// A read failed less than the retry delay ago: nothing is read yet.
    Waiting,
}

impl Workflows {
    /// Empty, a list of names current for `fresh` and reads waiting `retry` after one fails.
    pub(super) fn new(fresh: Duration, retry: Duration) -> Workflows {
        Workflows {
            repositories: HashMap::new(),
            fresh,
            retry,
        }
    }

    fn learnt(&mut self, repository: &str) -> &mut Learnt {
        self.repositories.entry(repository.to_owned()).or_default()
    }

    /// What resolving the workflow name of `repository`'s run `run` needs next, at `now`.
    pub(super) fn next(&self, repository: &str, run: u64, now: Instant) -> Next {
        let learnt = self.repositories.get(repository);
        let waiting = learnt
            .and_then(|learnt| learnt.failed_at)
            .is_some_and(|failed| now.saturating_duration_since(failed) < self.retry);
        let Some(&workflow) = learnt.and_then(|learnt| learnt.runs.values.get(&run)) else {
            return if waiting {
                Next::Waiting
            } else {
                Next::ReadRun
            };
        };
        let Some(workflow) = workflow else {
            return Next::Unnamed;
        };
        let listed = learnt
            .and_then(|learnt| learnt.listed.as_ref())
            .filter(|listed| now.saturating_duration_since(listed.at) < self.fresh);
        match listed {
            Some(listed) if let Some(name) = listed.names.get(&workflow) => {
                Next::Named(name.clone())
            }
            Some(listed) if listed.missing.contains(&workflow) => Next::Unnamed,
            _ if waiting => Next::Waiting,
            _ => Next::ReadList(workflow),
        }
    }

    /// Notes that `repository`'s run `run` is of `workflow`, or of none GitHub gives.
    pub(super) fn run(&mut self, repository: &str, run: u64, workflow: Option<u64>) {
        self.learnt(repository).runs.insert(run, workflow);
    }

    /// Takes `names` as `repository`'s workflows, read at `now` when looking for `asked`, and returns `asked`'s name: a
    /// renamed workflow takes its new name, and a removed one goes. `asked`, if not listed, is not read again for
    /// until the list is old.
    pub(super) fn listed(
        &mut self,
        repository: &str,
        names: HashMap<u64, String>,
        asked: u64,
        now: Instant,
    ) -> Option<String> {
        let name = names.get(&asked).cloned();
        let learnt = self.learnt(repository);
        // A list still current that lacked another workflow keeps lacking it: this read is no older.
        let mut missing = learnt
            .listed
            .take()
            .map(|listed| listed.missing)
            .unwrap_or_default();
        missing.retain(|workflow| !names.contains_key(workflow));
        if !names.contains_key(&asked) {
            missing.insert(asked);
        }
        learnt.listed = Some(Listed {
            at: now,
            names,
            missing,
        });
        learnt.failed_at = None;
        name
    }

    /// Notes that a read for `repository` failed at `now`.
    pub(super) fn failed(&mut self, repository: &str, now: Instant) {
        self.learnt(repository).failed_at = Some(now);
    }

    /// Notes that a job of `repository`'s run `run` kept GitHub's name: the run is owed its workflow's.
    pub(super) fn owe(&mut self, repository: &str, run: u64) {
        self.learnt(repository).owed.insert(run, ());
    }

    /// The runs owed a name, by repository, first owed first.
    pub(super) fn owed(&self) -> Vec<(String, Vec<u64>)> {
        let mut owed: Vec<(String, Vec<u64>)> = self
            .repositories
            .iter()
            .filter(|(_, learnt)| !learnt.owed.order.is_empty())
            .map(|(repository, learnt)| {
                (
                    repository.clone(),
                    learnt.owed.order.iter().copied().collect(),
                )
            })
            .collect();
        owed.sort();
        owed
    }

    /// Notes that `repository`'s run `run` is owed nothing more.
    pub(super) fn paid(&mut self, repository: &str, run: u64) {
        self.learnt(repository).owed.remove(run);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use super::{NAMES_FRESH, Next, RETRY_AFTER, RUNS_KEPT, Workflows};

    fn names(name: &str) -> HashMap<u64, String> {
        HashMap::from([(7, name.to_owned())])
    }

    /// The bug it catches: the run cache evicting by lowest ID, which drops an old run's re-run as soon as it is noted.
    #[test]
    fn an_old_run_noted_last_is_kept_and_the_first_noted_goes() {
        let mut workflows = Workflows::default();
        for run in 10_000..10_000 + RUNS_KEPT as u64 {
            workflows.run("o/r", run, Some(7));
        }
        workflows.run("o/r", 1, Some(7));
        let now = Instant::now();
        assert_eq!(workflows.next("o/r", 1, now), Next::ReadList(7));
        assert_eq!(workflows.next("o/r", 10_000, now), Next::ReadRun);
        assert_eq!(workflows.next("o/r", 10_001, now), Next::ReadList(7));
    }

    /// The bugs it catches: a name kept for the daemon's life, so a rename shows at a restart, and a workflow the list
    /// lacked once never looked for again.
    #[test]
    fn names_are_read_again_once_old_and_a_missing_workflow_is_looked_for_again() {
        let mut workflows = Workflows::default();
        let start = Instant::now();
        workflows.run("o/r", 1, Some(7));
        workflows.run("o/r", 2, Some(8));
        let listed = workflows.listed("o/r", names("CI"), 7, start);
        assert_eq!(listed.as_deref(), Some("CI"));
        assert_eq!(
            workflows.next("o/r", 1, start),
            Next::Named("CI".to_owned())
        );
        assert_eq!(workflows.next("o/r", 2, start), Next::ReadList(8));
        assert_eq!(workflows.listed("o/r", names("CI"), 8, start), None);
        assert_eq!(
            workflows.next("o/r", 2, start),
            Next::Unnamed,
            "read once for 8"
        );
        assert_eq!(
            workflows.next("o/r", 1, start),
            Next::Named("CI".to_owned()),
            "7 stays named"
        );
        let old = start + NAMES_FRESH;
        assert_eq!(workflows.next("o/r", 1, old), Next::ReadList(7));
        assert_eq!(workflows.next("o/r", 2, old), Next::ReadList(8));
        let both = HashMap::from([(7, "Build".to_owned()), (8, "Deploy".to_owned())]);
        assert_eq!(
            workflows.listed("o/r", both, 7, old).as_deref(),
            Some("Build")
        );
        assert_eq!(
            workflows.next("o/r", 1, old),
            Next::Named("Build".to_owned())
        );
        assert_eq!(
            workflows.next("o/r", 2, old),
            Next::Named("Deploy".to_owned())
        );
    }

    /// The bug it catches: a failing read asked again by every job read of every tick.
    #[test]
    fn after_a_failed_read_nothing_is_read_until_the_retry_delay() {
        let mut workflows = Workflows::default();
        let start = Instant::now();
        workflows.failed("o/r", start);
        assert_eq!(workflows.next("o/r", 1, start), Next::Waiting);
        assert_eq!(
            workflows.next(
                "o/r",
                1,
                start + RETRY_AFTER.saturating_sub(Duration::from_millis(1))
            ),
            Next::Waiting
        );
        assert_eq!(workflows.next("o/r", 1, start + RETRY_AFTER), Next::ReadRun);
        assert_eq!(
            workflows.next("o/x", 1, start),
            Next::ReadRun,
            "another repository reads"
        );
        workflows.run("o/r", 1, Some(7));
        assert_eq!(workflows.next("o/r", 1, start), Next::Waiting);
        let listed = workflows.listed("o/r", names("CI"), 7, start);
        assert_eq!(listed.as_deref(), Some("CI"), "a read clears the wait");
        assert_eq!(
            workflows.next("o/r", 1, start),
            Next::Named("CI".to_owned())
        );
    }
}
