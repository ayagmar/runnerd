//! What one tick knows of the jobs it follows from GitHub's answers in that tick (ADR 0180): the jobs its listings'
//! run pages showed besides the queued ones, and the jobs it read since, each answer replacing what the tick knew of
//! the job before. It owns that value and decides which of it may stand in for a job read; it must not call GitHub,
//! the store or log: `history` reads the jobs it cannot answer and records what GitHub says.

use std::collections::BTreeMap;

use crate::github::JobDetails;

/// The tick's answers about jobs, by job ID.
///
/// It lives for one tick: `list` makes it from that tick's listings, and the tick drops it once it followed its jobs.
/// Nothing of it survives the tick, so neither a restart nor a reload sees any of it, and a repository not listed this
/// tick has nothing in it.
#[derive(Debug, Default)]
pub(super) struct Observed(BTreeMap<u64, Observation>);

/// What the tick knows of one job.
#[derive(Debug)]
struct Observation {
    /// The repository whose listing or read gave it.
    repository: String,
    /// GitHub's details, from the tick's listing of the repository or a read of the job after it.
    details: JobDetails,
    /// Whether the history recorded these details this tick.
    recorded: Recorded,
}

/// Whether the history recorded a job's details this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recorded {
    /// It did not: a listing showed them, or a read whose answer was not a runner's here.
    No,
    /// It did, as these details.
    Yes,
}

/// What a closing job's wait is answered by, short of a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Closing {
    /// GitHub said this tick that it completed, in these details, which are not recorded yet.
    Completed(Box<JobDetails>),
    /// GitHub said this tick that it completed, and the history recorded it so: nothing is to be written again.
    Recorded,
}

impl Observed {
    /// The jobs a listing of `repository` this tick showed besides the queued ones, each complete for its status
    /// (`github::JobListing::observed`).
    pub(super) fn listed(&mut self, repository: &str, observed: Vec<JobDetails>) {
        for details in observed {
            self.0.insert(
                details.id,
                Observation {
                    repository: repository.to_owned(),
                    details,
                    recorded: Recorded::No,
                },
            );
        }
    }

    /// GitHub answered a read of job `details.id` of `repository` this tick, after anything the tick knew of it:
    /// these details stand for it from now on, recorded as `recorded` says.
    pub(super) fn read(&mut self, repository: &str, details: &JobDetails, recorded: Recorded) {
        self.0.insert(
            details.id,
            Observation {
                repository: repository.to_owned(),
                details: details.clone(),
                recorded,
            },
        );
    }

    /// What stands for a read of `id` of `repository` that left the queue and that no runner here is known to have
    /// taken: anything GitHub said of it this tick, as the queue is all that listing absence says.
    pub(super) fn for_queued(&self, repository: &str, id: u64) -> Option<JobDetails> {
        self.of(repository, id)
            .map(|observation| observation.details.clone())
    }

    /// What stands for a read of `id` of `repository`, which this machine's `runner` took: only details that name that
    /// runner, which GitHub gives once it took the job, so none from before the take, nor any whose runner is not
    /// known yet.
    pub(super) fn for_runner(&self, repository: &str, id: u64, runner: &str) -> Option<JobDetails> {
        self.of(repository, id)
            .filter(|observation| observation.details.runner_name.as_deref() == Some(runner))
            .map(|observation| observation.details.clone())
    }

    /// What stands for a read of closing `id` of `repository`: only that it completed, which nothing changes after.
    /// Any other status may have moved on since, so the job is read.
    pub(super) fn for_closing(&self, repository: &str, id: u64) -> Option<Closing> {
        let observation = self
            .of(repository, id)
            .filter(|observation| observation.details.status == "completed")?;
        Some(match observation.recorded {
            Recorded::No => Closing::Completed(Box::new(observation.details.clone())),
            Recorded::Yes => Closing::Recorded,
        })
    }

    fn of(&self, repository: &str, id: u64) -> Option<&Observation> {
        self.0
            .get(&id)
            .filter(|observation| observation.repository == repository)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(id: u64, status: &str, runner: Option<&str>) -> JobDetails {
        JobDetails {
            id,
            run_id: 1,
            run_attempt: 1,
            name: "build".to_owned(),
            workflow_name: Some("ci".to_owned()),
            head_branch: None,
            html_url: None,
            labels: Vec::new(),
            created_at: None,
            status: status.to_owned(),
            conclusion: (status == "completed").then(|| "success".to_owned()),
            started_at: None,
            completed_at: None,
            runner_name: runner.map(str::to_owned),
            steps: Vec::new(),
        }
    }

    #[test]
    fn a_job_runner_reads_stand_only_for_the_runner_github_names() {
        let mut observed = Observed::default();
        observed.listed(
            "o/r",
            vec![
                details(1, "in_progress", Some("runnerd-a")),
                details(2, "waiting", None),
            ],
        );
        assert!(
            observed.for_runner("o/r", 1, "runnerd-a").is_some(),
            "job 1's details name runnerd-a, so they stand for its read"
        );
        assert_eq!(observed.for_runner("o/r", 1, "runnerd-b"), None);
        assert_eq!(observed.for_runner("o/r", 2, "runnerd-a"), None);
        assert_eq!(observed.for_runner("o/s", 1, "runnerd-a"), None);
        assert_eq!(
            observed.for_queued("o/r", 2).map(|job| job.status),
            Some("waiting".to_owned())
        );
        assert_eq!(observed.for_queued("o/r", 3), None, "never shown");
    }

    #[test]
    fn a_closing_job_stands_answered_only_once_completed_and_is_written_once() {
        let mut observed = Observed::default();
        observed.listed("o/r", vec![details(1, "in_progress", Some("runnerd-a"))]);
        assert_eq!(observed.for_closing("o/r", 1), None, "may have moved on");
        let completed = details(1, "completed", Some("runnerd-a"));
        observed.read("o/r", &completed, Recorded::No);
        assert_eq!(
            observed.for_closing("o/r", 1),
            Some(Closing::Completed(Box::new(completed.clone())))
        );
        observed.read("o/r", &completed, Recorded::Yes);
        assert_eq!(observed.for_closing("o/r", 1), Some(Closing::Recorded));
        // A later read replaces what the tick knew, recorded or not.
        observed.read("o/r", &details(1, "in_progress", None), Recorded::No);
        assert_eq!(observed.for_closing("o/r", 1), None);
    }
}
