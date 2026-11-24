//! The writer thread: applies writes in the order they were sent, closes job records once both GitHub and the
//! engine are done with them, and announces what changed. A change and the durable events it announces commit in
//! one transaction and are broadcast only after, so a subscriber that reads the stored events after subscribing
//! misses none, and a crash never keeps a change without its event. Telemetry sent close together, log lines and
//! samples, commits in one transaction, each write in its own savepoint, in the order it was sent among every other
//! write.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::outline::Outlining;
use super::read::{self, secs};
use super::{
    Downtime, Downtimes, HOST_SAMPLE_BATCH, LOG_BACKLOG, LogPosition, MirrorPosition, Resolution,
    RunnerRecord, SampleRecord, Telemetry, Write, logged,
};
use crate::api::{self, Event, EventEnvelope};
use crate::github::{JobDetails, JobStep, NamedRun, QueuedJob};
use crate::record;
use crate::sync::lock;
use crate::time;

/// How long after the host woke GitHub may still fail a job for its sleep, in ms (ADR 0159): it fails a job whose
/// runner went silent while the host slept, or once the runner reconnects to a session that has ended. A job that
/// ended later survived the sleep.
pub(super) const SLEPT_GRACE_MS: u64 = 5 * 60 * 1000;

/// Makes an event its repository's latest in `announced`.
const ANNOUNCE_REPOSITORY: &str =
    "INSERT OR REPLACE INTO announced (repository, event) VALUES (?1, ?2)";
/// Makes an event its profile's latest in `announced_profiles`.
const ANNOUNCE_PROFILE: &str =
    "INSERT OR REPLACE INTO announced_profiles (profile, event) VALUES (?1, ?2)";

/// How long the writer gathers telemetry after the first of a batch before it commits them: a noisy runner's log
/// comes every 200 ms and every runner's samples every 2 s, each once a transaction of its own that rewrote the same
/// pages of `jobs`, `runners` and the indices. Live views see log lines and samples this much later at most. Any
/// other write ends the wait at once, so nothing else waits for it.
const TELEMETRY_WINDOW: Duration = Duration::from_secs(1);

/// Telemetry writes one transaction holds at most.
const TELEMETRY_MAX: usize = 256;

/// Log bytes one transaction holds at most: half of what may wait, so a capture that waits on the backlog is let go
/// without the window's wait.
const TELEMETRY_LOG_BYTES: u64 = LOG_BACKLOG / 2;

/// How far ahead of the second a runner is named for a lease of names reaches (seconds): the history keeps the lease's
/// end, synced to disk, once, and names runners up to it with no write. A restart names past it.
pub(super) const NAME_LEASE_SECS: u64 = 300;

/// What `Store::open` hands its writer.
pub(super) struct Started {
    /// The writer's connection.
    pub(super) db: Connection,
    /// The time before this daemon when no runnerd ran, if any.
    pub(super) downtimes: Downtimes,
    /// This daemon's row in `sessions`, which it notes each host sample it stores in.
    pub(super) session: i64,
}

/// The downtime a job's wait went through (ms): all of it, and the part before its first listing.
#[derive(Debug, Clone, Copy)]
struct Unseen {
    unseen: u64,
    unlisted: u64,
}

pub(super) fn run(
    started: Started,
    mut queue: mpsc::UnboundedReceiver<Write>,
    events: &broadcast::Sender<Arc<EventEnvelope>>,
    held: &Mutex<Vec<api::HostSample>>,
) {
    let mut writer = Writer {
        db: started.db,
        downtimes: started.downtimes,
        session: started.session,
        events,
        held,
        leased: 0,
    };
    // The window's clock: the writer thread runs no runtime of its own. Without one, it commits what is already sent.
    let clock = logged_io(
        "time the telemetry window",
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build(),
    );
    let mut next = None;
    while let Some(write) = next.take().or_else(|| queue.blocking_recv()) {
        match write {
            Write::Telemetry(first) => {
                let gathered = gather(&mut queue, first, clock.as_ref());
                next = gathered.next;
                writer.telemetry(gathered.batch);
            }
            write => writer.apply(write),
        }
    }
    writer.store_host_samples();
}

/// Logs an I/O error; `None` then.
fn logged_io<T>(what: &str, result: std::io::Result<T>) -> Option<T> {
    result
        .map_err(|error| tracing::warn!("job history: cannot {what}: {error}"))
        .ok()
}

/// A batch of telemetry, and the write after it that ended it, if one did.
struct Gathered {
    batch: Vec<Telemetry>,
    next: Option<Write>,
}

/// The telemetry sent after `first` within [`TELEMETRY_WINDOW`], up to [`TELEMETRY_MAX`] writes and
/// [`TELEMETRY_LOG_BYTES`] of log: the first other write ends it, and is the next one applied, so every write keeps its
/// place in the order they were sent. Without `clock`, only what is already sent.
fn gather(
    queue: &mut mpsc::UnboundedReceiver<Write>,
    first: Telemetry,
    clock: Option<&tokio::runtime::Runtime>,
) -> Gathered {
    let until = Instant::now() + TELEMETRY_WINDOW;
    let mut log_bytes = first.log_bytes();
    let mut batch = vec![first];
    while batch.len() < TELEMETRY_MAX && log_bytes < TELEMETRY_LOG_BYTES {
        let received = match queue.try_recv() {
            Ok(write) => Some(write),
            Err(mpsc::error::TryRecvError::Disconnected) => None,
            Err(mpsc::error::TryRecvError::Empty) => {
                let left = until.saturating_duration_since(Instant::now());
                match clock {
                    // The timer is made inside the runtime, which drives it.
                    Some(clock) if !left.is_zero() => clock
                        .block_on(async { tokio::time::timeout(left, queue.recv()).await })
                        .ok()
                        .flatten(),
                    _ => None,
                }
            }
        };
        match received {
            Some(Write::Telemetry(telemetry)) => {
                log_bytes += telemetry.log_bytes();
                batch.push(telemetry);
            }
            next => return Gathered { batch, next },
        }
    }
    Gathered { batch, next: None }
}

impl Telemetry {
    /// The bytes of log it stores.
    fn log_bytes(&self) -> u64 {
        match self {
            Telemetry::Log { lines, .. } => lines.iter().map(|line| line.len() as u64 + 1).sum(),
            Telemetry::Samples(_) | Telemetry::HostSample(_) => 0,
        }
    }
}

struct Writer<'store> {
    db: Connection,
    /// The time before this daemon when no runnerd ran and each time the host slept since, which a job queued before
    /// one first records waited through.
    downtimes: Downtimes,
    /// This daemon's row in `sessions`, a new one after each sleep.
    session: i64,
    events: &'store broadcast::Sender<Arc<EventEnvelope>>,
    /// `Store::held`.
    held: &'store Mutex<Vec<api::HostSample>>,
    /// The end of the lease of runner names this writer took (`runner_named`), a second the history keeps synced to
    /// disk: names before it need no write. 0 before the first; a restart takes a new lease past the stored one.
    leased: u64,
}

/// An event a batch of telemetry sends once it commits.
enum Outgoing {
    /// A stored write announced it: sent only if the batch commits.
    Stored(EventEnvelope),
    /// A host sample, which is not stored with the batch: sent whether or not it commits.
    Live(EventEnvelope),
}

/// The events a change announces once it commits.
type Announced = Vec<Event>;

fn timestamp(text: Option<&str>) -> Option<u64> {
    text.and_then(time::parse_ms)
}

/// How `records.removed_by` stores a removal: its name in the API.
fn removal_name(by: api::RecordRemoval) -> String {
    serde_json::to_value(by)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl Writer<'_> {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kind of write, each a call; split, it would only move the match"
    )]
    fn apply(&mut self, write: Write) {
        match write {
            Write::Downtime { from, until } => self.slept(Downtime { from, until }),
            Write::Queued {
                repository,
                job,
                seen_at,
            } => {
                self.change("record a queued job", |announced| {
                    self.queued(&repository, &job, seen_at, announced)
                });
            }
            Write::RunnerStarted(runner) => self.runner_started(&runner),
            Write::RunnerAdmission { runner, admission } => self.admission(&runner, admission),
            Write::RunnerOomKilled(runner) => self.runner_oom_killed(&runner),
            Write::RunnerProbe { runner, probe } => self.runner_probe(&runner, probe),
            Write::RunnerDockerStore { runner, report } => {
                self.runner_docker_store(&runner, report);
            }
            Write::RunnerStoppedFor { runner, reason } => self.runner_stopped_for(&runner, reason),
            Write::RunnerCancelBy { runner, at } => self.runner_cancel_by(&runner, at),
            Write::HostWaited { jobs, ms } => self.host_waited(&jobs, ms),
            Write::SlotWaited { jobs, ms } => self.slot_waited(&jobs, ms),
            Write::CausesWaited(waits) => {
                self.change("record what jobs waited for", |_| {
                    self.causes_waited(&waits)
                });
            }
            Write::Reran { jobs, at } => self.reran(&jobs, at),
            Write::BuildCache { runner, counts } => self.build_cache(&runner, &counts),
            Write::RunnerRemoved { runner, at } => {
                self.change("record a removed runner", |announced| {
                    self.runner_removed(&runner, at, announced)
                });
            }
            Write::RunnersGone { running, at } => {
                self.change("record removed runners", |announced| {
                    self.runners_gone(&running, at, announced)
                });
            }
            Write::Claimed {
                job,
                run_id,
                runner,
                at,
            } => {
                self.change("record a claimed job", |announced| {
                    self.claimed(job, run_id, &runner, at, announced)
                });
            }
            Write::Details { details } => {
                self.change("record a job's progress", |announced| {
                    self.details(&details, announced)
                });
            }
            Write::WorkflowNamed(named) => {
                self.change("name a run's workflow", |_| self.workflow_named(&named));
            }
            Write::Elsewhere { details } => {
                self.change("close a job that ran elsewhere", |announced| {
                    self.elsewhere(&details, announced)
                });
            }
            Write::Unanswered { job, at } => {
                self.change("close a job GitHub no longer answers for", |announced| {
                    self.unanswered(job, at, announced)
                });
            }
            Write::Lost { job, at } => {
                self.change("close a lost job", |announced| {
                    self.close(job, "lost", Some(at), announced)
                });
            }
            Write::Telemetry(telemetry) => self.telemetry(vec![telemetry]),
            Write::CgroupsGone { runners, at } => {
                self.change("record runners' cgroups gone", |_| {
                    for runner in &runners {
                        self.db.execute(
                            "UPDATE runners SET cgroup_gone_at = coalesce(cgroup_gone_at, ?2) WHERE name = ?1",
                            params![runner, at],
                        )?;
                        self.disk_again(runner)?;
                    }
                    Ok(())
                });
            }
            Write::DiskWindow(window) => {
                self.change("record a window of disk writes", |_| {
                    self.db.execute(
                        "INSERT OR REPLACE INTO disk_windows (start, end_at, device, store, drive, slice, \
                         daemon, commits, owners) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            window.start,
                            window.end,
                            window.device,
                            window.store,
                            window.drive,
                            window.slice,
                            window.daemon,
                            window.commits,
                            window
                                .owners
                                .and_then(|owners| serde_json::to_string(&owners).ok())
                        ],
                    )?;
                    Ok(())
                });
            }
            Write::MirrorBytes { bytes, until, done } => {
                let _ = done.send(self.mirror_bytes(&bytes, until));
            }
            Write::CacheSizes { at, sizes } => self.cache_sizes(at, sizes),
            Write::Event(event) => {
                self.change("store an event", |announced| {
                    announced.push(*event);
                    Ok(())
                });
            }
            Write::RecordMade(record) => self.record_made(record),
            Write::RecordSkipped { job, why } => self.record_skipped(job, &why),
            Write::RecordRemoved { job, at, by } => self.record_removed(job, at, by),
            Write::RecordsListed { jobs, at } => self.records_listed(&jobs, at),
            Write::Visibility {
                repository,
                visibility,
                at,
            } => {
                self.change("record a repository's visibility", |_| {
                    self.db.execute(
                        "INSERT INTO repositories (name, visibility, checked_at) VALUES (?1, ?2, ?3) \
                         ON CONFLICT (name) DO UPDATE SET visibility = ?2, checked_at = ?3",
                        params![repository, visibility_text(visibility), at],
                    )?;
                    Ok(())
                });
            }
            Write::Paused { repositories, done } => {
                let _ = done.send(self.paused(&repositories));
            }
            Write::SizingIntent { change, done } => {
                let _ = done.send(self.sizing_intent(&change));
            }
            Write::SizingResolved {
                id,
                resolution,
                done,
            } => self.sizing_resolved(id, resolution, done),
            Write::Notify { change, done } => self.notify(change, done),
            Write::RunnerNamed { secs, done } => {
                let _ = done.send(self.runner_named(secs));
            }
            Write::ClaimRerun { attempt, at, done } => {
                let _ = done.send(self.claim_rerun(&attempt, at));
            }
            Write::ReleaseRerun(attempt) => self.release_rerun(&attempt),
            Write::Flush(done) => {
                self.store_host_samples();
                let _ = done.send(());
            }
        }
    }

    fn record_made(&self, record: api::Record) {
        self.change("record a job's record", |announced| {
            self.db.execute(
                "INSERT OR REPLACE INTO records (job, created_at, bytes) VALUES (?1, ?2, ?3)",
                params![record.job, record.created_at * 1000, record.bytes],
            )?;
            announced.push(Event::RecordCreated { record });
            Ok(())
        });
    }

    fn record_skipped(&self, job: u64, why: &str) {
        self.change("record a skipped record", |announced| {
            self.db.execute(
                "INSERT OR REPLACE INTO records (job, skipped) VALUES (?1, ?2)",
                params![job, why],
            )?;
            announced.push(Event::RecordSkipped { job });
            Ok(())
        });
    }

    fn record_removed(&self, job: u64, at: u64, by: api::RecordRemoval) {
        self.change("record a removed record", |announced| {
            self.db.execute(
                "INSERT INTO records (job, removed_at, removed_by) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (job) DO UPDATE SET removed_at = ?2, removed_by = ?3",
                params![job, at, removal_name(by)],
            )?;
            announced.push(Event::RecordDeleted {
                job,
                removed_by: by,
            });
            Ok(())
        });
    }

    /// Notes as gone each record noted kept whose job is not in `listed`, the records Docker has, and announces it;
    /// one noted gone that is in `listed` is kept again.
    /// After every write sent before, so a record runnerd made or removed is noted as such first.
    fn records_listed(&self, listed: &[u64], at: u64) {
        self.change("note the records gone from Docker", |announced| {
            // One noted gone that Docker lists again is kept again: its image is there, and a shell opens in it.
            for job in listed {
                self.db.execute(
                    "UPDATE records SET gone_at = NULL WHERE job = ?1 AND gone_at IS NOT NULL",
                    [job],
                )?;
            }
            let kept: Vec<u64> = self
                .db
                .prepare(
                    "SELECT job FROM records WHERE created_at IS NOT NULL AND skipped IS NULL \
                     AND removed_at IS NULL AND gone_at IS NULL ORDER BY job",
                )?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for job in kept.into_iter().filter(|job| !listed.contains(job)) {
                self.db.execute(
                    "UPDATE records SET gone_at = ?2 WHERE job = ?1",
                    params![job, at],
                )?;
                announced.push(Event::RecordGone { job });
            }
            Ok(())
        });
    }

    /// The host slept through `downtime` (ADR 0159): the jobs queued now leave it out of their waits, and those
    /// recorded later that were queued before it will; the jobs running now ran across it. The held host samples are
    /// stored in the session that ends where the sleep began, and a new session begins where it ended. If the
    /// change fails, the jobs keep their waits as they were, and the session goes on.
    fn slept(&mut self, downtime: Downtime) {
        self.store_host_samples();
        let session = std::cell::Cell::new(None);
        self.change("record that the host slept", |_| {
            downtime.credit_queued(&self.db)?;
            self.db.execute(
                "UPDATE jobs SET slept_from = ?1, slept_ended_by = ?2 WHERE state = 'running'",
                params![
                    downtime.from,
                    downtime.until.saturating_add(SLEPT_GRACE_MS)
                ],
            )?;
            // Samples the sampler took after the wake and before this write are the new session's.
            self.db.execute(
                "UPDATE sessions SET sampled_at = min(coalesce(sampled_at, ?2), ?2) WHERE id = ?1",
                params![self.session, downtime.from],
            )?;
            self.db.execute(
                "INSERT INTO sessions (started_at, sampled_at) SELECT ?1, max(at) FROM host_samples \
                 WHERE at >= ?1",
                [downtime.until],
            )?;
            session.set(Some(self.db.last_insert_rowid()));
            // A sign of life after the sleep, so a daemon that starts next does not count the sleep as its downtime
            // again, should no sample from after the wake be stored before it stops.
            self.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('opened_at', max(?1, coalesce((SELECT value \
                 FROM meta WHERE key = 'opened_at'), 0)))",
                [downtime.until],
            )?;
            // So a daemon that starts next knows which runners it adopts lived through this sleep.
            self.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('slept_from', ?1)",
                [downtime.from],
            )?;
            Ok(())
        });
        if let Some(session) = session.get() {
            self.downtimes.push(downtime);
            self.session = session;
        }
    }

    /// Stores the held host samples in one transaction, and notes the latest in this daemon's session: runnerd was
    /// watching in it up to then. Then lets go of them. If that fails they are held for the next try, ten batches at
    /// most: older ones are dropped.
    fn store_host_samples(&self) {
        let samples = lock(self.held).clone();
        if samples.is_empty() {
            return;
        }
        let stored = self.change("record host samples", |_| {
            for sample in &samples {
                self.db.execute(
                    "INSERT OR REPLACE INTO host_samples (at, cpu, memory) VALUES (?1, ?2, ?3)",
                    params![sample.at * 1000, sample.cpu, sample.memory],
                )?;
            }
            if let Some(latest) = samples.iter().map(|sample| sample.at).max() {
                self.db.execute(
                    "UPDATE sessions SET sampled_at = max(coalesce(sampled_at, 0), ?2) WHERE id = ?1",
                    params![self.session, latest * 1000],
                )?;
            }
            Ok(())
        });
        let mut held = lock(self.held);
        let done = if stored {
            samples.len()
        } else {
            held.len().saturating_sub(HOST_SAMPLE_BATCH * 10)
        };
        let len = held.len();
        held.drain(..done.min(len));
    }

    fn runner_started(&self, runner: &RunnerRecord) {
        logged(
            "record a runner",
            self.db.execute(
                "INSERT OR IGNORE INTO runners (name, repository, profile, slot, container, started_at, \
                 memory_cap, cpu_cap, admission, memory_measure, purpose) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'without_page_cache', ?10)",
                params![
                    runner.name,
                    runner.repository,
                    runner.profile,
                    runner.slot,
                    runner.container,
                    runner.started_at,
                    runner.memory_cap,
                    runner.cpu_cap,
                    runner
                        .admission
                        .and_then(|admission| serde_json::to_string(&admission).ok()),
                    runner.purpose
                ],
            ),
        );
    }

    /// One measurement of every volume, in one transaction: all of it or none.
    fn cache_sizes(&self, at: u64, sizes: BTreeMap<String, u64>) {
        self.change("record cache sizes", |_| {
            for (name, bytes) in sizes {
                self.db.execute(
                    "INSERT OR REPLACE INTO cache_sizes (at, name, bytes) VALUES (?1, ?2, ?3)",
                    params![at, name, bytes],
                )?;
            }
            Ok(())
        });
    }

    fn admission(&self, runner: &str, admission: api::Admission) {
        logged(
            "record what a runner reserves",
            self.db.execute(
                "UPDATE runners SET admission = ?2 WHERE name = ?1",
                params![runner, serde_json::to_string(&admission).ok()],
            ),
        );
    }

    fn runner_oom_killed(&self, runner: &str) {
        logged(
            "record a kill for memory",
            self.db.execute(
                "UPDATE runners SET oom_kills = max(coalesce(oom_kills, 0), 1) WHERE name = ?1",
                [runner],
            ),
        );
    }

    fn runner_docker_store(&self, runner: &str, report: crate::cache::shares::StoreReport) {
        logged(
            "record what a slot's Docker store held",
            self.db.execute(
                "UPDATE runners SET docker_held = ?2, docker_share = ?3 WHERE name = ?1",
                params![runner, report.held, report.share_gib],
            ),
        );
    }

    fn runner_probe(&self, runner: &str, probe: api::RunnerProbe) {
        let Some(word) = read::probe_word(probe) else {
            return;
        };
        logged(
            "record a runner's probe",
            self.db.execute(
                "UPDATE runners SET probe = ?2 WHERE name = ?1",
                [runner, word],
            ),
        );
    }

    fn runner_stopped_for(&self, runner: &str, reason: &str) {
        logged(
            "record why a runner stopped",
            self.db.execute(
                "UPDATE runners SET stopped_for = ?2 WHERE name = ?1",
                params![runner, reason],
            ),
        );
    }

    fn runner_cancel_by(&self, runner: &str, at: u64) {
        logged(
            "record a cancelled runner's deadline",
            self.db.execute(
                "UPDATE runners SET cancel_by = ?2 WHERE name = ?1",
                params![runner, at],
            ),
        );
    }

    /// In one transaction, as every multi-row write: one commit, not one per job.
    fn host_waited(&self, jobs: &[u64], ms: u64) {
        self.change("record a wait for the host's memory", |_| {
            for job in jobs {
                self.db.execute(
                    "UPDATE jobs SET host_waited = host_waited + ?2 WHERE id = ?1",
                    params![job, ms],
                )?;
            }
            Ok(())
        });
    }

    fn slot_waited(&self, jobs: &[u64], ms: u64) {
        self.change("record a wait for a slot", |_| {
            for job in jobs {
                self.db.execute(
                    "UPDATE jobs SET slot_waited = slot_waited + ?2 WHERE id = ?1",
                    params![job, ms],
                )?;
            }
            Ok(())
        });
    }

    /// Adds `waits` to their jobs' `wait_causes`: one read and one write of each job's row.
    fn causes_waited(&self, waits: &[super::CauseWait]) -> rusqlite::Result<()> {
        let mut by_job: BTreeMap<u64, Vec<&super::CauseWait>> = BTreeMap::new();
        for wait in waits.iter().filter(|wait| wait.ms > 0) {
            by_job.entry(wait.job).or_default().push(wait);
        }
        for (job, waits) in by_job {
            let stored: Option<Option<String>> = self
                .db
                .query_row("SELECT wait_causes FROM jobs WHERE id = ?1", [job], |row| {
                    row.get(0)
                })
                .optional()?;
            // No row, or one from before schema 26: nothing to add to.
            let Some(Some(stored)) = stored else {
                continue;
            };
            let mut causes: BTreeMap<String, u64> =
                serde_json::from_str(&stored).unwrap_or_default();
            for wait in waits {
                let counted = causes.entry(wait.cause.word().to_owned()).or_insert(0);
                *counted = counted.saturating_add(wait.ms);
            }
            let causes = serde_json::to_string(&causes)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            self.db.execute(
                "UPDATE jobs SET wait_causes = ?2 WHERE id = ?1",
                params![job, causes],
            )?;
        }
        Ok(())
    }

    fn reran(&self, jobs: &[u64], at: u64) {
        self.change("record an automatic re-run", |_| {
            for job in jobs {
                self.db.execute(
                    "UPDATE jobs SET rerun_at = ?2 WHERE id = ?1 AND rerun_at IS NULL",
                    params![job, at],
                )?;
            }
            Ok(())
        });
    }

    fn build_cache(&self, runner: &str, counts: &api::BuildCacheCounts) {
        logged(
            "record the build cache's counts",
            self.db.execute(
                "UPDATE runners SET build_cache_hits = build_cache_hits + ?2, \
                 build_cache_misses = build_cache_misses + ?3, \
                 build_cache_read_bytes = build_cache_read_bytes + ?4, \
                 build_cache_written_bytes = build_cache_written_bytes + ?5, \
                 build_cache_refused = build_cache_refused + ?6, \
                 build_cache_writer = max(coalesce(build_cache_writer, 0), ?7) WHERE name = ?1",
                params![
                    runner,
                    counts.hits,
                    counts.misses,
                    counts.read_bytes,
                    counts.written_bytes,
                    counts.refused,
                    counts.writer
                ],
            ),
        );
    }

    /// Applies a batch of telemetry in one transaction, each write in a savepoint of its own: one that fails is rolled
    /// back and logged, and the others are kept. The events they announce are sent in order once it commits; the host
    /// samples' live ones whether or not it does. The held host samples are stored after, a minute's worth at a time.
    fn telemetry(&self, batch: Vec<Telemetry>) {
        let mut outgoing = Vec::new();
        let mut backlogs = Vec::new();
        let mut store_host = false;
        let result = (|| {
            let transaction = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            for telemetry in batch {
                match telemetry {
                    Telemetry::Log {
                        job,
                        lines,
                        position,
                        max_bytes,
                        backlog,
                    } => {
                        let stored = self.savepoint("store log lines", |announced| {
                            self.log(job, lines, position, max_bytes, announced)
                        })?;
                        outgoing.extend(stored.into_iter().flatten().map(Outgoing::Stored));
                        // Its share of the backlog is given back once its transaction is done.
                        backlogs.push(backlog);
                    }
                    Telemetry::Samples(samples) => {
                        let stored = self.savepoint("record samples", |announced| {
                            self.samples(&samples, announced)
                        })?;
                        outgoing.extend(stored.into_iter().flatten().map(Outgoing::Stored));
                    }
                    Telemetry::HostSample(sample) => {
                        // Live: no ID to wait for.
                        outgoing.push(Outgoing::Live(EventEnvelope {
                            id: None,
                            at: secs(time::now_ms()),
                            event: Event::HostSample { sample },
                        }));
                        store_host |= lock(self.held).len() >= HOST_SAMPLE_BATCH;
                    }
                }
            }
            transaction.commit()
        })();
        let committed = logged("record telemetry", result).is_some();
        drop(backlogs);
        for outgoing in outgoing {
            let envelope = match outgoing {
                Outgoing::Stored(envelope) if committed => envelope,
                Outgoing::Stored(_) => continue,
                Outgoing::Live(envelope) => envelope,
            };
            // No subscriber is not an error.
            let _ = self.events.send(Arc::new(envelope));
        }
        if store_host {
            self.store_host_samples();
        }
    }

    /// Makes a change and stores the durable events it announces in a savepoint of the open transaction. `Ok(None)`
    /// when the change failed, logged and rolled back; an error when the savepoint itself cannot be used, which
    /// leaves the transaction to roll back whole.
    fn savepoint(
        &self,
        what: &str,
        change: impl FnOnce(&mut Announced) -> rusqlite::Result<()>,
    ) -> rusqlite::Result<Option<Vec<EventEnvelope>>> {
        self.db.execute_batch("SAVEPOINT telemetry")?;
        let made = (|| {
            let mut announced = Vec::new();
            change(&mut announced)?;
            announced
                .into_iter()
                .map(|event| self.store_event(event))
                .collect::<rusqlite::Result<Vec<_>>>()
        })();
        match made {
            Ok(envelopes) => {
                self.db.execute_batch("RELEASE telemetry")?;
                Ok(Some(envelopes))
            }
            Err(error) => {
                self.db
                    .execute_batch("ROLLBACK TO telemetry; RELEASE telemetry")?;
                logged::<()>(what, Err(error));
                Ok(None)
            }
        }
    }

    /// Makes a change and stores the durable events it announces in one transaction, then sends them, with their
    /// IDs, to every subscriber. On any error nothing is kept or sent. Returns whether it committed.
    fn change(
        &self,
        what: &str,
        change: impl FnOnce(&mut Announced) -> rusqlite::Result<()>,
    ) -> bool {
        let result = (|| {
            // Immediate, so a busy database is waited for: a deferred transaction that read first fails at once.
            let transaction = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
            let mut announced = Vec::new();
            change(&mut announced)?;
            let envelopes = announced
                .into_iter()
                .map(|event| self.store_event(event))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            transaction.commit()?;
            Ok(envelopes)
        })();
        let Some(envelopes) = logged(what, result) else {
            return false;
        };
        for envelope in envelopes {
            // No subscriber is not an error.
            let _ = self.events.send(Arc::new(envelope));
        }
        true
    }

    /// Stores an event if it is durable, giving it its ID. A repository's or profile's event becomes
    /// its latest in `announced` or `announced_profiles`.
    fn store_event(&self, event: Event) -> rusqlite::Result<EventEnvelope> {
        let at = time::now_ms();
        let id = if event.is_durable() {
            let body = serde_json::to_string(&event)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            self.db.execute(
                "INSERT INTO events (at, body) VALUES (?1, ?2)",
                params![at, body],
            )?;
            let id = self.db.last_insert_rowid();
            let latest = match &event {
                Event::RepositoryChanged { repository } => {
                    Some((ANNOUNCE_REPOSITORY, &repository.name))
                }
                Event::RepositoryRemoved { repository } => Some((ANNOUNCE_REPOSITORY, repository)),
                Event::ProfileChanged { profile } => Some((ANNOUNCE_PROFILE, &profile.name)),
                Event::ProfileRemoved { profile } => Some((ANNOUNCE_PROFILE, profile)),
                _ => None,
            };
            if let Some((upsert, name)) = latest {
                self.db.execute(upsert, params![name, id])?;
            }
            u64::try_from(id).ok()
        } else {
            None
        };
        Ok(EventEnvelope {
            id,
            at: secs(at),
            event,
        })
    }

    fn queued(
        &self,
        repository: &str,
        job: &QueuedJob,
        seen_at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let labels = serde_json::to_string(&job.labels).unwrap_or_else(|_| "[]".to_owned());
        let queued_at = if job.created_at == 0 {
            time::now_ms()
        } else {
            job.created_at
        };
        let unseen = self.downtimes.of(queued_at);
        let inserted = self.db.execute(
            "INSERT INTO jobs (id, repository, run_id, run_attempt, workflow, name, branch, url, labels, state, \
             queued_at, unseen, seen_at, wait_causes, unseen_unlisted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11, ?12, '{}', ?11) \
             ON CONFLICT (id) DO NOTHING",
            params![
                job.id,
                repository,
                job.run_id,
                job.run_attempt,
                job.workflow,
                job.name,
                job.branch,
                job.url,
                labels,
                queued_at,
                unseen,
                seen_at
            ],
        )?;
        if inserted == 0 {
            // A runner's capture may have claimed it first: the listing still saw it queued.
            self.db.execute(
                "UPDATE jobs SET seen_at = ?2 WHERE id = ?1 AND seen_at IS NULL \
                 AND (started_at IS NULL OR started_at >= ?2)",
                params![job.id, seen_at],
            )?;
        } else if let Some(job) = read::summary(&self.db, job.id)? {
            announced.push(Event::JobQueued { job });
        }
        Ok(())
    }

    fn claimed(
        &self,
        job: u64,
        run_id: u64,
        runner: &str,
        at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        // A job the daemon never saw queued: GitHub's listing lagged, or it was queued before the daemon started.
        let inserted = self.db.execute(
            "INSERT INTO jobs (id, repository, run_id, run_attempt, name, labels, state, runner, queued_at, \
             started_at, wait_causes) SELECT ?1, repository, ?2, 1, '', '[]', 'running', name, ?3, ?3, '{}' FROM runners \
             WHERE name = ?4 ON CONFLICT (id) DO NOTHING",
            params![job, run_id, at, runner],
        )?;
        // A runner here took a job `unanswered` closed, as one GitHub refused while its token lost the repository and
        // queued again once it was restored: the job is this machine's (ADR 0109).
        let updated = self.db.execute(
            "UPDATE jobs SET state = 'running', runner = ?2, started_at = coalesce(started_at, ?3), \
             finished_at = NULL, elsewhere_runner = NULL WHERE id = ?1 AND (state = 'queued' \
             OR (state = 'elsewhere' AND conclusion IS NULL AND started_at IS NULL \
             AND finished_at IS NOT NULL))",
            params![job, runner, at],
        )?;
        if (inserted == 1 || updated == 1)
            && let Some(job) = read::summary(&self.db, job)?
        {
            announced.push(Event::JobStarted { job });
        }
        Ok(())
    }

    fn workflow_named(&self, named: &NamedRun) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE jobs SET workflow = ?3 WHERE repository = ?1 AND run_id = ?2",
            params![named.repository, named.run_id, named.workflow],
        )?;
        Ok(())
    }

    fn details(&self, details: &JobDetails, announced: &mut Announced) -> rusqlite::Result<()> {
        let completed = details.status == "completed";
        let labels = serde_json::to_string(&details.labels).unwrap_or_else(|_| "[]".to_owned());
        let started_at = timestamp(details.started_at.as_deref());
        let queued_at = timestamp(details.created_at.as_deref());
        let captured_first = self.captured_first(details.id, queued_at, started_at)?;
        // A runner that outlived the last daemon may have started it while none ran: what the downtime gave it
        // after that start was no wait. Once, when GitHub's start first says so.
        let after_start = match started_at {
            _ if captured_first.is_some() || self.downtimes.is_empty() => 0,
            Some(started_at) => self
                .db
                .query_row(
                    "SELECT queued_at, started_at FROM jobs WHERE id = ?1",
                    [details.id],
                    |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?)),
                )
                .optional()?
                .map_or(0, |(queued_at, known)| {
                    self.downtimes.after(queued_at, started_at, known)
                }),
            None => 0,
        };
        let updated = self.db.execute(
            "UPDATE jobs SET run_id = ?2, run_attempt = ?3, name = ?4, workflow = ?5, branch = ?6, url = ?7, \
             labels = ?8, queued_at = coalesce(?9, queued_at), started_at = coalesce(?10, started_at), \
             finished_at = CASE WHEN ?11 THEN coalesce(?12, finished_at) ELSE finished_at END, \
             conclusion = ?13, github_completed = ?11, \
             unseen = coalesce(?15, max(unseen - ?14, 0)), unseen_unlisted = coalesce(?16, unseen_unlisted) \
             WHERE id = ?1 AND state = 'running'",
            params![
                details.id,
                details.run_id,
                details.run_attempt,
                details.name,
                details.workflow_name,
                details.head_branch,
                details.html_url,
                labels,
                queued_at,
                started_at,
                completed,
                timestamp(details.completed_at.as_deref()),
                details.conclusion,
                after_start,
                captured_first.map(|unseen| unseen.unseen),
                captured_first.map(|unseen| unseen.unlisted),
            ],
        )?;
        if updated != 1 {
            return Ok(());
        }
        for step in &details.steps {
            if let Some(step) = self.step(details.id, step)? {
                announced.push(Event::JobStep {
                    job: details.id,
                    step,
                });
            }
        }
        self.close_if_done(details.id, announced)
    }

    /// The downtime a job a runner's capture recorded first waited through, once GitHub's times for it first come:
    /// its row took the capture's time as its queueing and its start, so the restart gave it none (ADR 0154). The
    /// part before its first listing is all of it unless a listing saw it. `None` for any other row, or once its
    /// queueing is known, so a later read gives none again.
    fn captured_first(
        &self,
        job: u64,
        queued_at: Option<u64>,
        started_at: Option<u64>,
    ) -> rusqlite::Result<Option<Unseen>> {
        let Some(queued_at) = queued_at.filter(|_| !self.downtimes.is_empty()) else {
            return Ok(None);
        };
        let row = self
            .db
            .query_row(
                "SELECT started_at, seen_at FROM jobs WHERE id = ?1 AND state = 'running' \
                 AND queued_at = started_at AND unseen = 0",
                [job],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?)),
            )
            .optional()?;
        Ok(row.map(|(captured, seen_at)| {
            let started_at = started_at.unwrap_or(captured);
            let listed = seen_at.map_or(started_at, |seen_at| seen_at.min(started_at));
            Unseen {
                unseen: self.downtimes.during(queued_at, started_at),
                unlisted: self.downtimes.during(queued_at, listed),
            }
        }))
    }

    /// Stores a step; returns it if it is new or changed.
    fn step(&self, job: u64, step: &JobStep) -> rusqlite::Result<Option<api::Step>> {
        let stored = api::Step {
            number: step.number,
            name: step.name.clone(),
            status: step.status.clone(),
            conclusion: step.conclusion.clone(),
            started_at: timestamp(step.started_at.as_deref()).map(secs),
            completed_at: timestamp(step.completed_at.as_deref()).map(secs),
        };
        let changed = self.db.execute(
            "INSERT INTO steps (job, number, name, status, conclusion, started_at, completed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (job, number) DO UPDATE SET \
             name = excluded.name, status = excluded.status, conclusion = excluded.conclusion, \
             started_at = excluded.started_at, completed_at = excluded.completed_at \
             WHERE (name, status, conclusion, started_at, completed_at) IS NOT \
             (excluded.name, excluded.status, excluded.conclusion, excluded.started_at, excluded.completed_at)",
            params![
                job,
                step.number,
                step.name,
                step.status,
                step.conclusion,
                timestamp(step.started_at.as_deref()),
                timestamp(step.completed_at.as_deref()),
            ],
        )?;
        Ok((changed == 1).then_some(stored))
    }

    fn runner_removed(
        &self,
        runner: &str,
        at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        self.db.execute(
            "UPDATE runners SET removed_at = ?2 WHERE name = ?1 AND removed_at IS NULL",
            params![runner, at],
        )?;
        let job: Option<u64> = self
            .db
            .query_row(
                "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                [runner],
                |row| row.get(0),
            )
            .optional()?;
        match job {
            Some(job) => self.close_if_done(job, announced),
            None => Ok(()),
        }
    }

    fn runners_gone(
        &self,
        running: &[String],
        at: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let open: Vec<String> = self
            .db
            .prepare_cached("SELECT name FROM runners WHERE removed_at IS NULL")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for runner in open.iter().filter(|runner| !running.contains(runner)) {
            self.runner_removed(runner, at, announced)?;
        }
        Ok(())
    }

    /// Closes a running job once GitHub reports it completed and its runner is removed.
    /// A job GitHub completed closes once its runner is gone. A runner the history never recorded, as Docker failed to
    /// inspect it, leaves no removal to wait for.
    fn close_if_done(&self, job: u64, announced: &mut Announced) -> rusqlite::Result<()> {
        let done = self
            .db
            .query_row(
                "SELECT j.github_completed AND (r.name IS NULL OR r.removed_at IS NOT NULL) FROM jobs j \
                 LEFT JOIN runners r ON r.name = j.runner WHERE j.id = ?1 AND j.state = 'running'",
                [job],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        if done == Some(true) {
            self.close(job, "completed", None, announced)?;
        }
        Ok(())
    }

    /// Keeps a job's metrics and makes its record final.
    fn close(
        &self,
        job: u64,
        state: &str,
        finished_at: Option<u64>,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let metrics = read::metrics(&self.db, job)?;
        let closed = self.db.execute(
            "UPDATE jobs SET state = ?2, finished_at = coalesce(finished_at, ?3), cpu_time = ?4, \
             cpu_peak = ?5, cpu_average = ?6, memory_peak = ?7, memory_average = ?8, \
             network_received = ?9, network_sent = ?10, cpu_periods = ?11, cpu_throttled = ?12, \
             cpu_stalled = ?13, disk_written = ?14, disk_write_ios = ?15, disk_dirty = ?16, disk_measure = ?17 \
             WHERE id = ?1 AND state = 'running'",
            params![
                job,
                state,
                finished_at,
                metrics.cpu_time,
                metrics.cpu_peak,
                metrics.cpu_average,
                metrics.memory_peak,
                metrics.memory_average,
                metrics.network_received,
                metrics.network_sent,
                metrics.cpu_periods,
                metrics.cpu_throttled,
                metrics.cpu_stalled,
                metrics.disk.map(|disk| disk.bytes),
                metrics.disk.map(|disk| disk.ios),
                metrics.disk.and_then(|disk| disk.dirty),
                metrics.disk.map(|disk| read::measure_word(disk.measure)),
            ],
        )?;
        if closed == 1 {
            self.read_runner_fault(job)?;
        }
        if closed == 1
            && let Some(job) = read::summary(&self.db, job)?
        {
            announced.push(Event::JobFinished { job });
        }
        Ok(())
    }

    /// Works out again what the final jobs of a runner whose end the sampler saw wrote (`read::disk_written`): the
    /// engine may close a job before the sampler's next pass finds its cgroup gone.
    fn disk_again(&self, runner: &str) -> rusqlite::Result<()> {
        let jobs: Vec<(u64, Option<u64>)> = self
            .db
            .prepare_cached(
                "SELECT id, started_at FROM jobs WHERE runner = ?1 AND state IN ('completed', 'lost')",
            )?
            .query_map([runner], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (job, started) in jobs {
            let disk = read::disk_written(&self.db, runner, started.unwrap_or(0))?;
            self.db.execute(
                "UPDATE jobs SET disk_written = ?2, disk_write_ios = ?3, disk_dirty = ?4, disk_measure = ?5 \
                 WHERE id = ?1",
                params![
                    job,
                    disk.map(|disk| disk.bytes),
                    disk.map(|disk| disk.ios),
                    disk.and_then(|disk| disk.dirty),
                    disk.map(|disk| read::measure_word(disk.measure)),
                ],
            )?;
        }
        Ok(())
    }

    /// Keeps whether a job that failed was failed by its runner (`record::runner_fault`), and what else its log says
    /// failed it (`record::failure_cause`), from its log, which is complete once the record is final.
    fn read_runner_fault(&self, job: u64) -> rusqlite::Result<()> {
        let Some(job) = read::job(&self.db, job)? else {
            return Ok(());
        };
        let summary = &job.summary;
        if summary.state != api::JobState::Completed || !summary.is_failure() {
            return Ok(());
        }
        let log = read::log_text(&self.db, summary.id)?;
        if record::runner_fault(&job, &log) {
            self.db.execute(
                "UPDATE jobs SET runner_fault = 1 WHERE id = ?1",
                [summary.id],
            )?;
        }
        if let Some(cause) = record::failure_cause(&job, &log).and_then(read::cause_word) {
            self.db.execute(
                "UPDATE jobs SET failure_cause = ?2 WHERE id = ?1",
                params![summary.id, cause],
            )?;
        }
        Ok(())
    }

    fn unanswered(&self, job: u64, at: u64, announced: &mut Announced) -> rusqlite::Result<()> {
        let closed = self.db.execute(
            "UPDATE jobs SET state = 'elsewhere', finished_at = ?2 WHERE id = ?1 AND state = 'queued'",
            params![job, at],
        )?;
        if closed == 1
            && let Some(job) = read::summary(&self.db, job)?
        {
            announced.push(Event::JobFinished { job });
        }
        Ok(())
    }

    fn elsewhere(&self, details: &JobDetails, announced: &mut Announced) -> rusqlite::Result<()> {
        // GitHub gives a job no runner took, as one a newer push cancelled while queued, a start time: its creation.
        // Its runner's name is null, or empty for a job GitHub skipped. The name of the one that took it is kept, so
        // the history says which machine ran it (ADR 0163), as one line of plain text (`Machine::runner_text`); a name
        // with nothing left of it is none.
        let runner = details
            .runner_name
            .as_deref()
            .and_then(api::Machine::runner_text);
        let started_at = runner
            .as_ref()
            .and_then(|_| timestamp(details.started_at.as_deref()));
        let closed = self.db.execute(
            "UPDATE jobs SET state = 'elsewhere', conclusion = ?2, started_at = ?3, finished_at = ?4, \
             elsewhere_runner = ?5 WHERE id = ?1 AND state = 'queued'",
            params![
                details.id,
                details.conclusion,
                started_at,
                timestamp(details.completed_at.as_deref()),
                runner,
            ],
        )?;
        if closed == 1
            && let Some(job) = read::summary(&self.db, details.id)?
        {
            announced.push(Event::JobFinished { job });
        }
        Ok(())
    }

    fn log(
        &self,
        job: u64,
        lines: Vec<String>,
        position: Option<LogPosition>,
        max_bytes: u64,
        announced: &mut Announced,
    ) -> rusqlite::Result<()> {
        let (mut count, mut bytes, mut truncated): (u64, u64, bool) = self.db.query_row(
            "SELECT log_lines, log_bytes, log_truncated FROM jobs WHERE id = ?1",
            [job],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut outlining = None;
        for line in lines {
            if truncated {
                break;
            }
            let size = line.len() as u64 + 1;
            if bytes + size > max_bytes {
                truncated = true;
                break;
            }
            self.db.execute(
                "INSERT OR IGNORE INTO log_lines (job, line, text) VALUES (?1, ?2, ?3)",
                params![job, count, line],
            )?;
            let outlining = match &mut outlining {
                Some(outlining) => outlining,
                None => outlining.insert(Outlining::load(&self.db, job, count)?),
            };
            outlining.line(count, &line)?;
            announced.push(Event::LogLine {
                job,
                line: count,
                text: line,
            });
            count += 1;
            bytes += size;
        }
        if let Some(outlining) = outlining {
            outlining.save()?;
        }
        self.db.execute(
            "UPDATE jobs SET log_lines = ?2, log_bytes = ?3, log_truncated = ?4, \
             log_page = coalesce(?5, log_page), log_offset = coalesce(?6, log_offset) WHERE id = ?1",
            params![
                job,
                count,
                bytes,
                truncated,
                position.map(|position| position.page),
                position.map(|position| position.offset)
            ],
        )?;
        Ok(())
    }

    fn samples(&self, samples: &[SampleRecord], announced: &mut Announced) -> rusqlite::Result<()> {
        for sample in samples {
            self.db.execute(
                "INSERT OR REPLACE INTO samples (runner, at, cpu, cpu_usage, memory, network_received, \
                 network_sent, cpu_periods, cpu_throttled, cpu_stalled, written, write_ios, dirty) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    sample.runner,
                    sample.at,
                    sample.cpu,
                    sample.cpu_usage,
                    sample.memory,
                    sample.network_received,
                    sample.network_sent,
                    sample.cpu_held.periods,
                    sample.cpu_held.throttled,
                    sample.cpu_held.stalled,
                    sample.written.map(|written| written.bytes),
                    sample.written.map(|written| written.ios),
                    sample.dirty,
                ],
            )?;
            // Each counter only rises, and a row is written only when it does: an unknown one (NULL) becomes the first
            // count read, 0 included, which says the kernel counts it; the same count again writes nothing.
            if let Some(ooms) = sample.events.oom {
                self.db.execute(
                    "UPDATE runners SET ooms = ?2 WHERE name = ?1 AND coalesce(ooms, -1) < ?2",
                    params![sample.runner, ooms],
                )?;
            }
            if let Some(ooms) = sample.events.local_oom {
                self.db.execute(
                    "UPDATE runners SET local_ooms = ?2 WHERE name = ?1 AND coalesce(local_ooms, -1) < ?2",
                    params![sample.runner, ooms],
                )?;
            }
            if let Some(throttled) = sample.events.sock_throttled {
                self.db.execute(
                    "UPDATE runners SET sock_throttled = ?2 WHERE name = ?1 AND coalesce(sock_throttled, -1) < ?2",
                    params![sample.runner, throttled],
                )?;
            }
            if let Some(kills) = sample.events.oom_kill.filter(|kills| *kills > 0) {
                self.db.execute(
                    "UPDATE runners SET oom_kills = ?2 WHERE name = ?1 AND coalesce(oom_kills, 0) < ?2",
                    params![sample.runner, kills],
                )?;
            }
            let job: Option<u64> = self
                .db
                .query_row(
                    "SELECT id FROM jobs WHERE runner = ?1 AND state = 'running'",
                    [&sample.runner],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(job) = job {
                announced.push(Event::JobSample {
                    job,
                    sample: api::Sample {
                        at: secs(sample.at),
                        cpu: sample.cpu,
                        memory: sample.memory,
                        network_received: sample.network_received,
                        network_sent: sample.network_sent,
                    },
                });
            }
        }
        Ok(())
    }

    /// Replaces the paused repositories.
    fn paused(&self, repositories: &[String]) -> bool {
        self.change("record the paused repositories", |_| {
            self.db.execute("DELETE FROM paused", [])?;
            for repository in repositories {
                self.db
                    .execute("INSERT INTO paused (repository) VALUES (?1)", [repository])?;
            }
            Ok(())
        })
    }

    /// Records what the notifier did, and sends whether it committed.
    fn notify(&self, change: super::NotifyChange, done: oneshot::Sender<bool>) {
        let committed = match change {
            super::NotifyChange::Record {
                target,
                set,
                remove,
            } => self.notify_record(&target, &set, &remove),
            super::NotifyChange::Keep(targets) => self.notify_keep(&targets),
        };
        let _ = done.send(committed);
    }

    fn notify_record(&self, target: &str, set: &[(String, u64)], remove: &[String]) -> bool {
        self.change("record what was notified", |_| {
            for (key, value) in set {
                self.db.execute(
                    "INSERT OR REPLACE INTO notify (target, key, value) VALUES (?1, ?2, ?3)",
                    params![target, key, value],
                )?;
            }
            for key in remove {
                self.db.execute(
                    "DELETE FROM notify WHERE target = ?1 AND key = ?2",
                    params![target, key],
                )?;
            }
            Ok(())
        })
    }

    fn notify_keep(&self, targets: &[String]) -> bool {
        self.change("forget removed notification targets", |_| {
            let kept: Vec<String> = self
                .db
                .prepare_cached("SELECT DISTINCT target FROM notify")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for gone in kept.iter().filter(|target| !targets.contains(target)) {
                self.db
                    .execute("DELETE FROM notify WHERE target = ?1", [gone])?;
            }
            Ok(())
        })
    }

    /// Runs `write` with its commits synced to disk (`synchronous = FULL`), for what must outlast a power failure:
    /// an auto sizing change recorded before the config file is written and synced, what became of it, and the second
    /// a runner was named for. `None` when the database will not sync them.
    fn durably<T>(&self, write: impl FnOnce() -> T) -> Option<T> {
        logged(
            "sync commits to disk",
            self.db.pragma_update(None, "synchronous", "full"),
        )?;
        let written = write();
        logged(
            "stop syncing every commit",
            self.db.pragma_update(None, "synchronous", "normal"),
        );
        Some(written)
    }

    fn sizing_intent(&self, change: &api::SizingChange) -> Option<i64> {
        self.durably(|| self.record_intent(change)).flatten()
    }

    fn record_intent(&self, change: &api::SizingChange) -> Option<i64> {
        let mut id = None;
        self.change("record an auto sizing change", |_| {
            self.db.execute(
                "INSERT INTO auto_sizing (profile, at, from_memory, from_slots, to_memory, to_slots, reason, state, \
                 from_cpus, to_cpus) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
                params![
                    change.profile,
                    change.at.saturating_mul(1000),
                    change.from.memory,
                    change.from.slots,
                    change.to.memory,
                    change.to.slots,
                    change.reason,
                    change.from.cpus,
                    change.to.cpus
                ],
            )?;
            id = Some(self.db.last_insert_rowid());
            Ok(())
        })
        .then_some(id)
        .flatten()
    }

    fn sizing_resolved(&self, id: i64, resolution: Resolution, done: oneshot::Sender<bool>) {
        let resolved = self
            .durably(|| self.resolve(id, resolution))
            .unwrap_or(false);
        let _ = done.send(resolved);
    }

    fn resolve(&self, id: i64, resolution: Resolution) -> bool {
        self.change("resolve an auto sizing change", |announced| {
            let state = match resolution {
                Resolution::Done => "done",
                Resolution::Failed => "failed",
                Resolution::Uncertain => "uncertain",
            };
            self.db.execute(
                "UPDATE auto_sizing SET state = ?2 WHERE id = ?1 AND state = 'pending'",
                params![id, state],
            )?;
            if resolution == Resolution::Done {
                let change = self.db.query_row(
                    "SELECT * FROM auto_sizing WHERE id = ?1",
                    [id],
                    read::sizing_change,
                )?;
                announced.push(Event::SizingChanged { change });
            }
            Ok(())
        })
    }

    /// Whether the history keeps, synced to disk, a second at or past `secs`: a name a power failure took back would
    /// be given again after it. A name before the end of this writer's lease needs no write; one at or past it takes
    /// a new lease, [`NAME_LEASE_SECS`] past `secs`, as `runner_named` in `meta`, which never goes back. That is the
    /// latest second a runner was named for as every runnerd reads it, so a restart, of this build or an older one,
    /// names past the lease, and a clock stepped back never names a second inside it again.
    fn runner_named(&mut self, secs: u64) -> bool {
        if secs < self.leased {
            return true;
        }
        let until = secs.saturating_add(NAME_LEASE_SECS);
        let leased = self
            .durably(|| {
                self.change("lease runner names", |_| {
                    self.db.execute(
                        "INSERT OR REPLACE INTO meta (key, value) VALUES ('runner_named', \
                         max(?1, coalesce((SELECT value FROM meta WHERE key = 'runner_named'), 0)))",
                        [until],
                    )?;
                    Ok(())
                })
            })
            .unwrap_or(false);
        if leased {
            self.leased = until;
        }
        leased
    }

    /// Claims an automatic re-run's attempt, synced to disk, and drops claims older than a day.
    fn claim_rerun(&self, attempt: &super::run::ClaimKey, at: u64) -> super::Claim {
        let mut inserted = 0;
        let committed = self
            .durably(|| {
                self.change("claim an automatic re-run", |_| {
                    self.db.execute(
                        "DELETE FROM rerun_claims WHERE claimed_at < ?1",
                        [at.saturating_sub(super::run::CLAIM_KEPT_MS)],
                    )?;
                    inserted = self.db.execute(
                        "INSERT OR IGNORE INTO rerun_claims (repository, run_id, attempt, claimed_at) \
                         VALUES (?1, ?2, ?3, ?4)",
                        params![attempt.repository, attempt.run, attempt.attempt, at],
                    )?;
                    Ok(())
                })
            })
            .unwrap_or(false);
        match (committed, inserted) {
            (false, _) => super::Claim::Unstored,
            (true, 0) => super::Claim::Taken,
            (true, _) => super::Claim::Claimed,
        }
    }

    fn release_rerun(&self, attempt: &super::run::ClaimKey) {
        logged(
            "release an automatic re-run's claim",
            self.db.execute(
                "DELETE FROM rerun_claims WHERE repository = ?1 AND run_id = ?2 AND attempt = ?3",
                params![attempt.repository, attempt.run, attempt.attempt],
            ),
        );
    }

    /// Counts a batch of the mirror's log and where it ends, together: counting part of it, or counting it without
    /// moving past it, would count it again after a restart.
    fn mirror_bytes(&self, bytes: &[(String, u64)], until: MirrorPosition) -> bool {
        self.change("count mirror bytes", |_| {
            for (runner, bytes) in bytes {
                self.db.execute(
                    "UPDATE runners SET mirror_bytes = mirror_bytes + ?2 WHERE name = ?1",
                    params![runner, bytes],
                )?;
            }
            self.db.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('mirror_log_until', ?1), ('mirror_log_seen', ?2)",
                [until.at, until.seen],
            )?;
            Ok(())
        })
    }
}

/// A visibility as the `repositories` table keeps it, GitHub's word for it.
fn visibility_text(visibility: api::Visibility) -> &'static str {
    match visibility {
        api::Visibility::Public => "public",
        api::Visibility::Private => "private",
        api::Visibility::Internal => "internal",
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "tests: a database that cannot open fails the test"
)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use rusqlite::Connection;
    use tokio::sync::{broadcast, mpsc, oneshot};

    use super::{Downtimes, TELEMETRY_MAX, TELEMETRY_WINDOW, Telemetry, Write, Writer, gather};
    use crate::testdir::TestDir;

    fn telemetry() -> Write {
        Write::Telemetry(Telemetry::Samples(Vec::new()))
    }

    fn clock() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// The bug it catches: a batch of telemetry gathering past a write sent between its parts, as a flush or a
    /// durable change, which would then be applied out of the order it was sent in.
    #[test]
    fn a_batch_of_telemetry_ends_at_the_first_other_write_and_that_write_comes_next() {
        let (sender, mut queue) = mpsc::unbounded_channel();
        let (done, _) = oneshot::channel();
        for write in [telemetry(), telemetry(), Write::Flush(done), telemetry()] {
            sender.send(write).unwrap();
        }
        let Some(Write::Telemetry(first)) = queue.try_recv().ok() else {
            unreachable!("the first write is telemetry");
        };
        let gathered = gather(&mut queue, first, Some(&clock()));
        assert_eq!(gathered.batch.len(), 2);
        assert!(
            matches!(gathered.next, Some(Write::Flush(_))),
            "the flush is applied next"
        );
        assert!(
            matches!(queue.try_recv(), Ok(Write::Telemetry(_))),
            "the telemetry after the flush waits its turn"
        );
    }

    /// The bug it catches: a flush, a runner's name or a durable change waiting out the telemetry window behind log
    /// lines sent just before it.
    #[test]
    fn another_write_ends_the_window_at_once() {
        let (sender, mut queue) = mpsc::unbounded_channel();
        let sent = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let (done, _) = oneshot::channel();
            sender.send(telemetry()).unwrap();
            sender.send(Write::Flush(done)).unwrap();
        });
        let started = Instant::now();
        let gathered = gather(&mut queue, Telemetry::Samples(Vec::new()), Some(&clock()));
        sent.join().unwrap();
        assert_eq!(
            gathered.batch.len(),
            2,
            "the telemetry sent in the window joins"
        );
        assert!(matches!(gathered.next, Some(Write::Flush(_))));
        assert!(
            started.elapsed() < TELEMETRY_WINDOW,
            "the flush did not wait: {:?}",
            started.elapsed()
        );
    }

    /// The bug it catches: a batch that never ends while telemetry keeps coming, or one past its bounds.
    #[test]
    fn a_batch_ends_at_its_size_or_when_nothing_else_comes() {
        let (sender, mut queue) = mpsc::unbounded_channel();
        for _ in 0..TELEMETRY_MAX + 5 {
            sender.send(telemetry()).unwrap();
        }
        let gathered = gather(&mut queue, Telemetry::Samples(Vec::new()), None);
        assert_eq!(gathered.batch.len(), TELEMETRY_MAX);
        assert!(gathered.next.is_none(), "the rest stays queued");

        let (_sender, mut queue) = mpsc::unbounded_channel::<Write>();
        let started = Instant::now();
        let gathered = gather(&mut queue, Telemetry::Samples(Vec::new()), Some(&clock()));
        assert_eq!(gathered.batch.len(), 1);
        assert!(
            started.elapsed() >= TELEMETRY_WINDOW,
            "a lone write waits the window for company"
        );
    }

    fn synchronous(db: &Connection) -> u8 {
        db.pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_durable_write_syncs_its_commits_and_the_writer_goes_back_to_normal_after() {
        let dir = TestDir::new("store-write");
        let db = Connection::open(dir.0.join("runnerd.db")).unwrap();
        super::super::configure(&db).unwrap();
        let (events, _) = broadcast::channel(1);
        let held = Mutex::new(Vec::new());
        let writer = Writer {
            db,
            downtimes: Downtimes::default(),
            session: 0,
            events: &events,
            held: &held,
            leased: 0,
        };
        // 2 is FULL, 1 NORMAL.
        assert_eq!(writer.durably(|| synchronous(&writer.db)), Some(2));
        assert_eq!(synchronous(&writer.db), 1);
    }
}
