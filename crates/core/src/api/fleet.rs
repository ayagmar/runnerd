//! One view over several machines' daemons (ADR 0165): a pure merge of what a client read from each, their status and
//! a page of jobs, into one list of machines and one page of jobs, each job shown once. Decides which machine's
//! record of a job is shown, how the merged page continues, and words each machine's line, a machine that could not
//! be read included. It reads nothing itself: the client asks each daemon with the same query and hands the answers
//! here.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{JobPage, JobState, JobSummary, Status, Tone};

/// What a client read from one machine: its status and a page of jobs, or why each could not be read.
#[derive(Debug, Clone)]
pub struct MachineReading {
    /// The machine's name, as the client's `machines.toml` calls it, or this machine's own.
    pub machine: String,
    /// `GET /v1/status`, or why it could not be read.
    pub status: Result<Status, String>,
    /// `GET /v1/jobs` with the fleet's query, or why it could not be read.
    pub jobs: Result<JobPage, String>,
}

/// Every machine and one page of their jobs, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fleet {
    /// Each machine, in the order read.
    pub machines: Vec<FleetMachine>,
    /// The jobs, newest first, each once.
    pub jobs: Vec<FleetJob>,
    /// Pass as `before` to every machine for the next page; `None` on the last.
    pub next: Option<u64>,
}

/// One machine of the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMachine {
    /// Its name.
    pub name: String,
    /// What was read from it.
    #[serde(flatten)]
    pub reach: Reach,
}

/// What was read from a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Reach {
    /// Its status was read.
    Read {
        /// The daemon's version.
        version: String,
        /// Its machine id; absent from a daemon before 0.1.6.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Runners running a job.
        running: u32,
        /// Slots holding a runner, busy or idle.
        held: u32,
        /// Slots over every profile.
        slots: u32,
        /// Jobs its repositories have queued.
        queued: u32,
        /// Repositories it does not start runners for.
        paused: u32,
        /// Whether the daemon is shutting down.
        shutting_down: bool,
        /// Why its jobs could not be read, when they could not: the page lacks them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jobs_unread: Option<String>,
    },
    /// Nothing could be read from it.
    Unreachable {
        /// Why, as the client worded it.
        reason: String,
    },
}

/// A job and the machine whose record of it is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetJob {
    /// The machine.
    pub machine: String,
    /// Its record of the job.
    pub job: JobSummary,
}

/// Which record of a job seen on several machines tells the most: the one that ran it, then one that knows how it
/// ended, then one whose runner it lost, then one that still waits for it, and last one that saw it run elsewhere.
fn rank(state: JobState) -> u8 {
    match state {
        JobState::Running => 4,
        JobState::Completed => 3,
        JobState::Lost => 2,
        JobState::Queued => 1,
        JobState::Elsewhere => 0,
    }
}

/// Whether `new` tells more of the job than `kept`: a higher rank, or the same with a runner where `kept` has none.
fn better(new: &JobSummary, kept: &JobSummary) -> bool {
    (rank(new.state), new.runner.is_some()) > (rank(kept.state), kept.runner.is_some())
}

impl Fleet {
    /// Merges what was read from each machine into a page of at most `limit` jobs. Every machine was asked with the
    /// same query and `before`; a job seen on several shows once, the record that knows most (running, then completed, lost, queued, elsewhere), the first machine read winning
    /// a tie. The next page starts before the oldest job shown when the page was cut or a machine has more.
    pub fn merge(readings: Vec<MachineReading>, limit: usize) -> Fleet {
        let mut machines = Vec::with_capacity(readings.len());
        let mut jobs: BTreeMap<u64, FleetJob> = BTreeMap::new();
        let mut more = false;
        for reading in readings {
            let (page, jobs_unread) = match reading.jobs {
                Ok(page) => (Some(page), None),
                Err(reason) => (None, Some(reason)),
            };
            let reach = match (reading.status, jobs_unread) {
                (Ok(status), jobs_unread) => Reach::read(&status, jobs_unread),
                (Err(reason), _) => Reach::Unreachable { reason },
            };
            machines.push(FleetMachine {
                name: reading.machine.clone(),
                reach,
            });
            let Some(page) = page else { continue };
            more |= page.next.is_some();
            for job in page.jobs {
                match jobs.get(&job.id) {
                    Some(kept) if !better(&job, &kept.job) => {}
                    _ => {
                        jobs.insert(
                            job.id,
                            FleetJob {
                                machine: reading.machine.clone(),
                                job,
                            },
                        );
                    }
                }
            }
        }
        let cut = jobs.len() > limit;
        let jobs: Vec<FleetJob> = jobs.into_values().rev().take(limit).collect();
        let next = if cut || more {
            jobs.last().map(|job| job.job.id)
        } else {
            None
        };
        Fleet {
            machines,
            jobs,
            next,
        }
    }
}

impl Fleet {
    /// Whether every machine's status and jobs were read.
    pub fn all_read(&self) -> bool {
        self.machines.iter().all(|machine| {
            matches!(
                machine.reach,
                Reach::Read {
                    jobs_unread: None,
                    ..
                }
            )
        })
    }
}

impl Reach {
    fn read(status: &Status, jobs_unread: Option<String>) -> Reach {
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let slots = status.profiles.iter().flat_map(|profile| &profile.slots);
        let runners: Vec<_> = slots
            .clone()
            .filter_map(|slot| slot.runner.as_ref())
            .collect();
        Reach::Read {
            version: status.version.clone(),
            id: status.machine.as_ref().map(|machine| machine.id.clone()),
            running: count(runners.iter().filter(|runner| runner.busy).count()),
            held: count(runners.len()),
            slots: count(slots.count()),
            queued: count(status.queued.len()),
            paused: count(
                status
                    .repositories
                    .iter()
                    .filter(|repository| repository.paused)
                    .count(),
            ),
            shutting_down: status.shutting_down,
            jobs_unread,
        }
    }
}

impl FleetMachine {
    /// What a client shows after the machine's name: `2 running, 1 queued, 3 of 8 slots held, runnerd 0.1.6`, with
    /// what is paused, shutting down or unread after it; or why it could not be read.
    pub fn summary(&self) -> String {
        match &self.reach {
            Reach::Unreachable { reason } => format!("cannot be read: {reason}"),
            Reach::Read {
                version,
                running,
                held,
                slots,
                queued,
                paused,
                shutting_down,
                jobs_unread,
                ..
            } => {
                let mut text = format!(
                    "{running} running, {queued} queued, {held} of {slots} slots held, runnerd {version}"
                );
                if *paused > 0 {
                    let repositories = if *paused == 1 {
                        "repository"
                    } else {
                        "repositories"
                    };
                    let _ = write!(text, ", {paused} {repositories} paused");
                }
                if *shutting_down {
                    text.push_str(", shutting down");
                }
                if let Some(reason) = jobs_unread {
                    let _ = write!(text, "; its jobs cannot be read: {reason}");
                }
                text
            }
        }
    }

    /// What the summary means: bad when nothing or not its jobs could be read, a warning while it shuts down or
    /// has paused repositories, else neutral.
    pub fn tone(&self) -> Tone {
        match &self.reach {
            Reach::Unreachable { .. }
            | Reach::Read {
                jobs_unread: Some(_),
                ..
            } => Tone::Bad,
            Reach::Read {
                shutting_down: true,
                ..
            }
            | Reach::Read { paused: 1.., .. } => Tone::Warn,
            Reach::Read { .. } => Tone::Neutral,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fleet, MachineReading};
    use crate::api::{JobMetrics, JobPage, JobRunner, JobState, JobSummary, LogSummary, Timeline};

    fn job(id: u64, state: JobState, runner: bool) -> JobSummary {
        let mut job = JobSummary {
            id,
            repository: "halyard/ledger".to_owned(),
            run_id: 1,
            run_attempt: 1,
            workflow: None,
            name: "test".to_owned(),
            branch: None,
            url: None,
            labels: Vec::new(),
            state,
            conclusion: None,
            runner: None,
            timeline: Timeline::default(),
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
            runner_fault: false,
            failure_cause: None,
            rerun_at: None,
            probe: None,
            slept: false,
            record_mark: None,
        };
        job.state = state;
        job.runner = runner.then(|| JobRunner {
            name: format!("runnerd-1a2b3c4d-skiff-jvm-1-{id}"),
            profile: "jvm".to_owned(),
            slot: 1,
            memory_cap: None,
            cpu_cap: None,
        });
        job
    }

    fn reading(machine: &str, jobs: Vec<JobSummary>, next: Option<u64>) -> MachineReading {
        MachineReading {
            machine: machine.to_owned(),
            status: Err("not read in this test".to_owned()),
            jobs: Ok(JobPage { jobs, next }),
        }
    }

    fn shown(fleet: &Fleet) -> Vec<(u64, &str, JobState)> {
        fleet
            .jobs
            .iter()
            .map(|job| (job.job.id, job.machine.as_str(), job.job.state))
            .collect()
    }

    /// Catches: a job one machine ran shown as `elsewhere` or `queued` because the machine that saw it so was read
    /// first, or shown twice.
    #[test]
    fn a_job_seen_on_two_machines_shows_once_as_the_one_that_knows_most() {
        let fleet = Fleet::merge(
            vec![
                reading(
                    "tern",
                    vec![
                        job(30, JobState::Elsewhere, false),
                        job(20, JobState::Queued, false),
                        job(10, JobState::Lost, true),
                    ],
                    None,
                ),
                reading(
                    "skiff",
                    vec![
                        job(30, JobState::Running, true),
                        job(20, JobState::Completed, true),
                        job(10, JobState::Queued, false),
                    ],
                    None,
                ),
            ],
            50,
        );
        assert_eq!(
            shown(&fleet),
            vec![
                (30, "skiff", JobState::Running),
                (20, "skiff", JobState::Completed),
                (10, "tern", JobState::Lost),
            ]
        );
        assert_eq!(fleet.next, None, "no machine has more");
    }

    /// Catches: a page cut to its limit skipping jobs of the machine whose jobs were cut, or a next page that never
    /// ends.
    #[test]
    fn pages_continue_before_the_oldest_job_shown_and_skip_none() {
        let tern: Vec<_> = (91..=100)
            .rev()
            .map(|id| job(id, JobState::Completed, true))
            .collect();
        let skiff: Vec<_> = (85..=94)
            .rev()
            .map(|id| job(id, JobState::Completed, true))
            .collect();
        let fleet = Fleet::merge(
            vec![
                reading("tern", tern, Some(91)),
                reading("skiff", skiff, Some(85)),
            ],
            10,
        );
        let ids: Vec<u64> = fleet.jobs.iter().map(|job| job.job.id).collect();
        assert_eq!(ids, (91..=100).rev().collect::<Vec<_>>());
        assert_eq!(
            fleet.next,
            Some(91),
            "every job above 91 is shown, so the next page starts there"
        );

        let last = Fleet::merge(
            vec![reading(
                "tern",
                vec![job(5, JobState::Completed, true)],
                None,
            )],
            10,
        );
        assert_eq!(last.next, None, "the last page has no next");
    }
}
