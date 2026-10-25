//! The job history: a SQLite database of jobs, their runners, steps, samples and logs, the events clients resume
//! from, cache sizes, the paused repositories and where the notifier got to. Owns the schema and its migrations, the one writer and the event
//! broadcast. Writes are messages to a writer thread, so the engine never waits on the disk; reads and pruning run
//! on Tokio's blocking pool, pruning on its own connection, a batch at a time. It decides nothing about scheduling
//! and stores no secret: no token, no JIT config, no runner `_diag` log.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};
use tracing::warn;

use crate::api::{self, Event, EventEnvelope};
use crate::config::History;
use crate::github::{JobDetails, QueuedJob};
use crate::observe::metrics::Events;
use crate::scheduler::warm;
use crate::sizing;
use crate::sync::lock;

mod prune;
mod read;
mod write;

/// The schema's migrations, in order; the database's `user_version` is how many ran.
const MIGRATIONS: &[&str] = &[
    include_str!("store/migrations/0001_history.sql"),
    include_str!("store/migrations/0002_runner_caps.sql"),
    include_str!("store/migrations/0003_paused.sql"),
    include_str!("store/migrations/0004_prune.sql"),
    include_str!("store/migrations/0005_announced_profiles.sql"),
    include_str!("store/migrations/0006_records.sql"),
    include_str!("store/migrations/0007_admission.sql"),
    include_str!("store/migrations/0008_memory_without_page_cache.sql"),
    include_str!("store/migrations/0009_auto_sizing.sql"),
    include_str!("store/migrations/0010_auto_sizing_cpus.sql"),
    include_str!("store/migrations/0011_notify.sql"),
    include_str!("store/migrations/0012_build_cache.sql"),
    include_str!("store/migrations/0013_host_memory.sql"),
];

/// Bytes of log lines sent and not yet stored, at most: a job's capture waits beyond it, so a noisy job or a slow
/// disk holds back that job's log, not the daemon's memory.
pub(crate) const LOG_BACKLOG: u64 = 8 << 20;

/// Host samples the writer holds before it stores them in one transaction: a minute's worth. One transaction per
/// sample wrote about 300 MB a day to an idle daemon's WAL. A crash loses the samples held.
const HOST_SAMPLE_BATCH: usize = 30;

/// How many events a slow client may fall behind before its stream ends; it reconnects with `Last-Event-ID`.
const EVENT_BUFFER: usize = 4096;

/// Why the database cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A newer runnerd wrote the database.
    #[error(
        "the job history database is version {found}, newer than the {supported} this runnerd reads; \
         run a newer runnerd or move the database aside"
    )]
    Newer {
        /// The database's version.
        found: u32,
        /// The newest version this build knows.
        supported: u32,
    },
    /// The directory or file cannot be created.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A blocking read did not finish.
    #[error("a database read stopped: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// The database's `-wal` file holds changes not yet in it, and this user cannot open the database the way that
    /// reads them.
    #[error(
        "{} holds changes not yet in the job history, which this user cannot read; run runnerd doctor as the user \
         the daemon runs as",
        .0.display()
    )]
    Unread(PathBuf),
}

/// A runner container as the history records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerRecord {
    /// The runner's name, also its container's.
    pub name: String,
    /// `owner/name`.
    pub repository: String,
    /// The profile it runs in.
    pub profile: String,
    /// 1-based.
    pub slot: usize,
    /// The Docker container ID.
    pub container: String,
    /// When the container started, in milliseconds since the epoch.
    pub started_at: u64,
    /// The container's memory cap in bytes.
    pub memory_cap: Option<u64>,
    /// The container's CPU limit in millicores.
    pub cpu_cap: Option<u64>,
    /// How it was admitted; `None` for one a restart adopted without a record of it.
    pub admission: Option<api::Admission>,
}

/// One sample of one runner's container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleRecord {
    /// The runner.
    pub runner: String,
    /// When, in milliseconds since the epoch.
    pub at: u64,
    /// Millicores over the interval before it.
    pub cpu: u64,
    /// CPU time so far, in microseconds.
    pub cpu_usage: u64,
    /// Bytes without page cache (`observe::metrics::Usage::memory`).
    pub memory: u64,
    /// Bytes received since the container started.
    pub network_received: u64,
    /// Bytes sent since the container started.
    pub network_sent: u64,
    /// The container's `memory.events` so far.
    pub events: Events,
}

/// The last measurement of the cache volumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measured {
    /// When it was taken (seconds); none before the first measurement.
    pub at: Option<u64>,
    /// Each volume's size in bytes, by name.
    pub bytes: BTreeMap<String, u64>,
}

/// The profiles and repositories as the stored events last announced them: what clients resuming after those
/// events have.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announced {
    /// The last `profile.changed` of each profile not removed since, by name.
    pub profiles: Vec<api::ProfileStatus>,
    /// The last `repository.changed` of each repository not removed since, by name.
    pub repositories: Vec<api::RepositoryStatus>,
}

/// A job the history has not closed, as the engine needs it after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenJob {
    /// GitHub's job ID.
    pub id: u64,
    /// `owner/name`.
    pub repository: String,
    /// `Queued` or `Running`.
    pub state: api::JobState,
    /// Its runner, once one took it.
    pub runner: Option<String>,
    /// When its runner was removed, in milliseconds since the epoch.
    pub runner_removed_at: Option<u64>,
    /// The workflow run it belongs to.
    pub run_id: u64,
    /// 1 for the run's first attempt.
    pub run_attempt: u32,
    /// The workflow's name, when known.
    pub workflow: Option<String>,
    /// When GitHub queued it, in milliseconds since the epoch.
    pub queued_at: u64,
    /// The labels it asked for, which tell its repository's pool.
    pub labels: Vec<String>,
}

/// Where a job's log capture continues: the runner's page file and the byte offset in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPosition {
    /// The page number, from 1.
    pub page: u64,
    /// Bytes of that page already stored as lines.
    pub offset: u64,
}

/// How far the mirror's log was counted: the time of the last response counted, in nanoseconds since the epoch as
/// Docker logs it, and how many responses logged at exactly that time were counted, so the next read counts the
/// ones it was too early to see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirrorPosition {
    /// Nanoseconds since the epoch.
    pub at: u64,
    /// Responses logged at `at` already counted.
    pub seen: u64,
}

/// What the writer thread is asked to do, in order.
#[derive(Debug)]
enum Write {
    Queued {
        repository: String,
        job: QueuedJob,
    },
    RunnerStarted(RunnerRecord),
    RunnerAdmission {
        runner: String,
        admission: api::Admission,
    },
    RunnerOomKilled(String),
    HostWaited {
        jobs: Vec<u64>,
        ms: u64,
    },
    BuildCache {
        runner: String,
        counts: api::BuildCacheCounts,
    },
    RunnerRemoved {
        runner: String,
        at: u64,
    },
    RunnersGone {
        running: Vec<String>,
        at: u64,
    },
    Claimed {
        job: u64,
        run_id: u64,
        runner: String,
        at: u64,
    },
    Details {
        details: JobDetails,
    },
    Elsewhere {
        details: JobDetails,
    },
    Lost {
        job: u64,
        at: u64,
    },
    Log {
        job: u64,
        lines: Vec<String>,
        /// Where the capture resumes once these lines are stored; `None` for all but the last part of a batch.
        position: Option<LogPosition>,
        max_bytes: u64,
        /// The lines' share of `LOG_BACKLOG`, given back once they are stored.
        _backlog: OwnedSemaphorePermit,
    },
    Samples(Vec<SampleRecord>),
    HostSample(api::HostSample),
    MirrorBytes {
        bytes: Vec<(String, u64)>,
        until: MirrorPosition,
        /// Whether the count committed.
        done: oneshot::Sender<bool>,
    },
    CacheSizes {
        at: u64,
        sizes: BTreeMap<String, u64>,
    },
    /// Boxed: an event with a job's summary is many times the other writes' size.
    Event(Box<Event>),
    RecordMade(api::Record),
    RecordSkipped {
        job: u64,
        why: String,
    },
    RecordRemoved {
        job: u64,
        at: u64,
        by: api::RecordRemoval,
    },
    Paused {
        repositories: Vec<String>,
        /// Whether it committed.
        done: oneshot::Sender<bool>,
    },
    SizingIntent {
        change: api::SizingChange,
        /// Its row, once it committed.
        done: oneshot::Sender<Option<i64>>,
    },
    SizingResolved {
        id: i64,
        resolution: Resolution,
        /// Whether it committed.
        done: oneshot::Sender<bool>,
    },
    Notify {
        change: NotifyChange,
        /// Whether it committed.
        done: oneshot::Sender<bool>,
    },
    Flush(oneshot::Sender<()>),
}

/// What the notifier records.
#[derive(Debug)]
enum NotifyChange {
    /// Sets a target's keys and removes others.
    Record {
        target: String,
        set: Vec<(String, u64)>,
        remove: Vec<String>,
    },
    /// Forgets every target but these.
    Keep(Vec<String>),
}

/// What became of a pending auto sizing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The config file has it: announced as `sizing.changed`.
    Done,
    /// The file was not written.
    Failed,
    /// Left pending, and the config no longer says whether it was written: it keeps its interval and stays listed,
    /// with what it replaced.
    Uncertain,
}

/// Auto sizing as the history keeps it: when each profile last changed, and changes not yet known to be written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoSizing {
    /// The latest pending or done change of each profile, in seconds, however old.
    pub last: BTreeMap<String, u64>,
    /// Changes recorded before their file was written and not resolved since, by row.
    pub pending: Vec<(i64, api::SizingChange)>,
}

/// The job history. Cloning shares it.
#[derive(Debug, Clone)]
pub struct Store {
    writes: mpsc::UnboundedSender<Write>,
    reader: Arc<Mutex<Connection>>,
    /// Taken with `try_lock`, so one prune runs at a time; one that panicked midway left rows the next one deletes.
    pruner: Arc<Mutex<Connection>>,
    /// Set by `close`: a running prune stops after its batch, and none starts.
    closed: Arc<AtomicBool>,
    /// Host samples sent and not yet stored, oldest first; reads include them. The writer removes them once stored.
    // Locked with `sync::lock`: every critical section is a push, a copy or a drain.
    held: Arc<Mutex<Vec<api::HostSample>>>,
    /// The last host sample sent, for admission to read without a query.
    latest_host: Arc<Mutex<Option<api::HostSample>>>,
    /// The last reading of the host's memory, which is not stored.
    latest_host_memory: Arc<Mutex<Option<api::HostMemory>>>,
    /// Each running runner's last sample, for admission to read without a query.
    latest_samples: Arc<Mutex<BTreeMap<String, SampleRecord>>>,
    events: broadcast::Sender<Arc<EventEnvelope>>,
    backlog: Arc<Semaphore>,
}

impl Store {
    /// Opens the database at `path`, creating it and its directory (mode 0700) if missing, and migrates it to the
    /// latest version. Refuses a database a newer runnerd wrote.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        // Owner-only, as SQLite then makes its `-wal` and `-shm` files: logs may say more than the owner wants shared.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let mut db = Connection::open(path)?;
        configure(&db)?;
        migrate(&mut db)?;
        let reader = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&reader)?;
        let pruner = Connection::open(path)?;
        configure(&pruner)?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (writes, receiver) = mpsc::unbounded_channel();
        let thread_events = events.clone();
        let held = Arc::new(Mutex::new(Vec::new()));
        let thread_held = held.clone();
        std::thread::Builder::new()
            .name("runnerd-store".to_owned())
            .spawn(move || write::run(db, receiver, &thread_events, &thread_held))?;
        Ok(Store {
            writes,
            reader: Arc::new(Mutex::new(reader)),
            pruner: Arc::new(Mutex::new(pruner)),
            closed: Arc::new(AtomicBool::new(false)),
            held,
            latest_host: Arc::new(Mutex::new(None)),
            latest_host_memory: Arc::new(Mutex::new(None)),
            latest_samples: Arc::new(Mutex::new(BTreeMap::new())),
            events,
            backlog: Arc::new(Semaphore::new(
                usize::try_from(LOG_BACKLOG).unwrap_or(usize::MAX),
            )),
        })
    }

    fn send(&self, write: Write) {
        // Fails only once the writer thread is gone, after a SQLite error it logged.
        let _ = self.writes.send(write);
    }

    /// Records a queued job the repository's runners may run; announces it the first time.
    pub fn queued(&self, repository: &str, job: &QueuedJob) {
        self.send(Write::Queued {
            repository: repository.to_owned(),
            job: job.clone(),
        });
    }

    /// Records a runner container that started, or one an earlier daemon started and this one adopted.
    pub fn runner_started(&self, runner: RunnerRecord) {
        self.send(Write::RunnerStarted(runner));
    }

    /// Records that a runner's container and registration were removed at `at` (ms).
    pub fn runner_removed(&self, runner: &str, at: u64) {
        lock(&self.latest_samples).remove(runner);
        self.send(Write::RunnerRemoved {
            runner: runner.to_owned(),
            at,
        });
    }

    /// Records every runner the history holds open, except the `running` ones, removed at `at` (ms): what recovery
    /// found gone, so it stops counting as holding a slot.
    pub fn runners_gone(&self, running: Vec<String>, at: u64) {
        lock(&self.latest_samples).retain(|runner, _| running.contains(runner));
        self.send(Write::RunnersGone { running, at });
    }

    /// Records that a runner took a job at `at` (ms).
    pub fn claimed(&self, job: u64, run_id: u64, runner: &str, at: u64) {
        self.send(Write::Claimed {
            job,
            run_id,
            runner: runner.to_owned(),
            at,
        });
    }

    /// Records a running job's progress and steps as GitHub reports them; announces changed steps.
    pub fn details(&self, details: JobDetails) {
        self.send(Write::Details { details });
    }

    /// Closes a queued job that left the queue without a runner of this daemon.
    pub fn elsewhere(&self, details: JobDetails) {
        self.send(Write::Elsewhere { details });
    }

    /// Closes a job whose container is gone and that GitHub did not report completed in time.
    pub fn lost(&self, job: u64, at: u64) {
        self.send(Write::Lost { job, at });
    }

    /// Appends log lines to a job, up to `max_bytes` per job, and records where the capture continues. Waits while
    /// `LOG_BACKLOG` bytes of lines are waiting to be stored; a larger batch goes in parts, each waiting its turn.
    pub async fn log(&self, job: u64, lines: Vec<String>, position: LogPosition, max_bytes: u64) {
        let size = |line: &String| line.len() as u64 + 1;
        let mut lines = lines.into_iter().map(|mut line| {
            // No line takes the whole backlog; the capture already cuts lines far shorter.
            if size(&line) > LOG_BACKLOG {
                let mut end = usize::try_from(LOG_BACKLOG / 2).unwrap_or(usize::MAX);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                line.truncate(end);
            }
            line
        });
        let mut next = lines.next();
        loop {
            let (mut part, mut bytes) = (Vec::new(), 0);
            while let Some(line) = next.take_if(|line| bytes + size(line) <= LOG_BACKLOG) {
                bytes += size(&line);
                part.push(line);
                next = lines.next();
            }
            let last = next.is_none();
            let share = u32::try_from(bytes.max(1)).unwrap_or(u32::MAX);
            // The semaphore is never closed.
            let Ok(backlog) = self.backlog.clone().acquire_many_owned(share).await else {
                return;
            };
            self.send(Write::Log {
                job,
                lines: part,
                position: last.then_some(position),
                max_bytes,
                _backlog: backlog,
            });
            if last {
                return;
            }
        }
    }

    /// Records samples of runner containers.
    pub fn samples(&self, samples: Vec<SampleRecord>) {
        let mut latest = lock(&self.latest_samples);
        for sample in &samples {
            latest.insert(sample.runner.clone(), sample.clone());
        }
        drop(latest);
        self.send(Write::Samples(samples));
    }

    /// Records a sample of the slice and sends it to every subscriber at once; it is stored with the next ones, a
    /// minute's worth in one transaction, or at `flush`.
    pub fn host_sample(&self, sample: api::HostSample) {
        *lock(&self.latest_host) = Some(sample.clone());
        lock(&self.held).push(sample.clone());
        self.send(Write::HostSample(sample));
    }

    /// A runner's last sample, stored or not; `None` before its first sample or once it is removed.
    pub fn latest_sample(&self, runner: &str) -> Option<SampleRecord> {
        lock(&self.latest_samples).get(runner).cloned()
    }

    /// Records what a runner reserves now, when measured admission changed it after the start.
    pub fn runner_admission(&self, runner: &str, admission: api::Admission) {
        self.send(Write::RunnerAdmission {
            runner: runner.to_owned(),
            admission,
        });
    }

    /// Records that Docker saw the kernel kill a process in a runner's container for memory, which its samples may
    /// have missed: one in the last interval before the container stopped.
    pub fn runner_oom_killed(&self, runner: &str) {
        self.send(Write::RunnerOomKilled(runner.to_owned()));
    }

    /// Adds what the shared build cache served a runner and took from it, and records whether its job could write.
    pub fn build_cache(&self, runner: &str, counts: api::BuildCacheCounts) {
        self.send(Write::BuildCache {
            runner: runner.to_owned(),
            counts,
        });
    }

    /// Keeps the latest reading of the host's memory for admission and the status; it is not stored.
    pub fn host_memory(&self, memory: api::HostMemory) {
        *lock(&self.latest_host_memory) = Some(memory);
    }

    /// The latest reading of the host's memory; `None` before the first.
    pub fn latest_host_memory(&self) -> Option<api::HostMemory> {
        lock(&self.latest_host_memory).clone()
    }

    /// Adds `ms` to how long each of `jobs` waited because the host's memory could not take its runner, which
    /// sizing does not read as a wait for a slot.
    pub fn host_waited(&self, jobs: Vec<u64>, ms: u64) {
        self.send(Write::HostWaited { jobs, ms });
    }

    /// The last host sample sent, stored or not; `None` before the first.
    pub fn latest_host_sample(&self) -> Option<api::HostSample> {
        lock(&self.latest_host).clone()
    }

    /// Adds bytes the mirror served to runners, counted from its log up to `until`, and says whether that was
    /// stored: if not, the caller counts the same lines again.
    pub async fn mirror_bytes(&self, bytes: Vec<(String, u64)>, until: MirrorPosition) -> bool {
        let (done, committed) = oneshot::channel();
        self.send(Write::MirrorBytes { bytes, until, done });
        committed.await.unwrap_or(false)
    }

    /// Records volume sizes in bytes, by name, measured at `at` (ms).
    pub fn cache_sizes(&self, at: u64, sizes: BTreeMap<String, u64>) {
        self.send(Write::CacheSizes { at, sizes });
    }

    /// Stores a durable event, or sends a live one, to every subscriber.
    pub fn event(&self, event: Event) {
        self.send(Write::Event(Box::new(event)));
    }

    /// Records that a failed job's runner was kept as `record`, and announces it.
    pub fn record_made(&self, record: api::Record) {
        self.send(Write::RecordMade(record));
    }

    /// Records why a failed job's runner was not kept.
    pub fn record_skipped(&self, job: u64, why: String) {
        self.send(Write::RecordSkipped { job, why });
    }

    /// Records that a job's record was removed at `at` (ms), and why, and announces it.
    pub fn record_removed(&self, job: u64, at: u64, by: api::RecordRemoval) {
        self.send(Write::RecordRemoved { job, at, by });
    }

    /// Records the paused repositories, all of them, in place of the ones recorded before. The write is sent at
    /// once, in order with every other; the future says whether it committed.
    pub fn paused(&self, repositories: Vec<String>) -> impl Future<Output = bool> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::Paused { repositories, done });
        async move { committed.await.unwrap_or(false) }
    }

    /// Records a change auto sizing is about to write, as pending, before the file is, synced to disk. The future
    /// has its row once it committed, `None` if it did not: then nothing may be written.
    pub fn sizing_intent(
        &self,
        change: api::SizingChange,
    ) -> impl Future<Output = Option<i64>> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::SizingIntent { change, done });
        async move { committed.await.ok().flatten() }
    }

    /// Records what became of a pending change, synced to disk; a done one is announced as `sizing.changed` in the
    /// same transaction. The future says whether it committed.
    pub fn sizing_resolved(
        &self,
        id: i64,
        resolution: Resolution,
    ) -> impl Future<Output = bool> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::SizingResolved {
            id,
            resolution,
            done,
        });
        async move { committed.await.unwrap_or(false) }
    }

    /// Records what the notifier delivered to a target: sets `set`'s keys and removes `remove`'s, in one
    /// transaction. The future says whether it committed.
    pub fn notify_record(
        &self,
        target: &str,
        set: Vec<(String, u64)>,
        remove: Vec<String>,
    ) -> impl Future<Output = bool> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::Notify {
            change: NotifyChange::Record {
                target: target.to_owned(),
                set,
                remove,
            },
            done,
        });
        async move { committed.await.unwrap_or(false) }
    }

    /// Forgets what the notifier kept for every target but `targets`. The future says whether it committed.
    pub fn notify_keep(&self, targets: Vec<String>) -> impl Future<Output = bool> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::Notify {
            change: NotifyChange::Keep(targets),
            done,
        });
        async move { committed.await.unwrap_or(false) }
    }

    /// Deletes what is older than `history` keeps, at `now` (ms), off the writer, in batches the writer's
    /// transactions fit between. A prune while the last one runs does nothing. The handle finishes with it; dropping
    /// it lets it run.
    pub fn prune(&self, now: u64, history: &History) -> tokio::task::JoinHandle<()> {
        let pruner = self.pruner.clone();
        let closed = self.closed.clone();
        let history = history.clone();
        tokio::task::spawn_blocking(move || {
            let db = match pruner.try_lock() {
                Ok(db) => db,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            };
            let db = prune::Db {
                connection: &db,
                stop: &closed,
            };
            if !db.stop.load(Ordering::Relaxed) {
                prune::run(&db, now, &history);
            }
        })
    }

    /// Stops a running prune after its batch, and any later one before it starts, so a shutdown does not wait for
    /// one. What a stopped prune left, the next daemon's deletes. Writes go on as before.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    /// Waits until every write sent before is done.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        self.send(Write::Flush(done));
        let _ = wait.await;
    }

    /// Every event from now on, durable or live. Subscribe before reading what was stored, then skip what the read
    /// returned, and nothing is missed or sent twice.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EventEnvelope>> {
        self.events.subscribe()
    }

    async fn read<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let reader = self.reader.clone();
        // A panic during a read leaves the connection usable: reads change nothing.
        Ok(tokio::task::spawn_blocking(move || query(&lock(&reader))).await??)
    }

    /// One page of jobs, newest first.
    pub async fn jobs(&self, query: api::JobQuery) -> Result<api::JobPage, StoreError> {
        self.read(move |db| read::jobs(db, &query)).await
    }

    /// A job with its steps.
    pub async fn job(&self, id: u64) -> Result<Option<api::Job>, StoreError> {
        self.read(move |db| read::job(db, id)).await
    }

    /// A job's samples, oldest first; `None` if there is no such job.
    pub async fn job_samples(
        &self,
        id: u64,
        interval: Duration,
    ) -> Result<Option<api::SampleSeries>, StoreError> {
        self.read(move |db| read::job_samples(db, id, interval))
            .await
    }

    /// Up to `limit` log lines of a job from line `from`, with their numbers.
    pub async fn log_lines(
        &self,
        job: u64,
        from: u64,
        limit: u64,
    ) -> Result<Vec<(u64, String)>, StoreError> {
        self.read(move |db| read::log_lines(db, job, from, limit))
            .await
    }

    /// Up to `limit` durable events after `after`, oldest first.
    pub async fn events_after(
        &self,
        after: u64,
        limit: u64,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        self.read(move |db| read::events_after(db, after, limit))
            .await
    }

    /// The changes auto sizing wrote since `since` (ms), oldest first.
    pub async fn sizing_changes(&self, since: u64) -> Result<Vec<api::SizingChange>, StoreError> {
        self.read(move |db| read::sizing_changes(db, since)).await
    }

    /// When each profile last changed, and the changes still pending.
    pub async fn auto_sizing(&self) -> Result<AutoSizing, StoreError> {
        self.read(read::auto_sizing).await
    }

    /// The profiles and repositories as the stored events last announced them, so a restarted daemon announces
    /// only what changed since, including what its config no longer has. Blocks on the database: for startup.
    pub fn announced(&self) -> Result<Announced, StoreError> {
        Ok(read::announced(&lock(&self.reader))?)
    }

    /// The repositories recorded paused, for the engine to start with. Blocks on the database: for startup.
    pub fn paused_repositories(&self) -> Result<Vec<String>, StoreError> {
        Ok(read::paused(&lock(&self.reader))?)
    }

    /// The slice's samples since `since` (seconds), oldest first, the ones not yet stored included.
    pub async fn host_samples(&self, since: u64) -> Result<Vec<api::HostSample>, StoreError> {
        // Copied before the read: the writer removes a sample only once it is stored, so each one is in the copy,
        // the read, or both.
        let held = lock(&self.held).clone();
        let mut samples = self.read(move |db| read::host_samples(db, since)).await?;
        let stored = samples.last().map_or(0, |sample| sample.at);
        samples.extend(
            held.into_iter()
                .filter(|sample| sample.at >= since && sample.at > stored),
        );
        Ok(samples)
    }

    /// The jobs runnerd's runners started and finished from `since` until `now` (seconds).
    pub async fn activity(&self, since: u64, now: u64) -> Result<api::Activity, StoreError> {
        self.read(move |db| read::activity(db, since, now)).await
    }

    /// Finished jobs per repository and bucket.
    pub async fn stats(&self, query: api::StatsQuery, now: u64) -> Result<api::Stats, StoreError> {
        self.read(move |db| read::stats(db, &query, now)).await
    }

    /// What sizing reads: the finished jobs queued at or after `since` (ms) and the runners since.
    pub async fn sizing(&self, since: u64) -> Result<sizing::History, StoreError> {
        self.read(move |db| read::sizing(db, since)).await
    }

    /// The jobs completed or lost that were queued at or after `since` (ms), as insights read them.
    pub async fn insights(&self, since: u64) -> Result<Vec<crate::insights::Run>, StoreError> {
        self.read(move |db| read::insights(db, since)).await
    }

    /// The newest durable event's ID; 0 before the first.
    pub async fn last_event(&self) -> Result<u64, StoreError> {
        self.read(read::last_event).await
    }

    /// What the notifier kept for each target, by target, then key.
    pub async fn notify_state(
        &self,
    ) -> Result<BTreeMap<String, BTreeMap<String, u64>>, StoreError> {
        self.read(read::notify_state).await
    }

    /// The jobs queued at or after `since` (ms), whatever became of them, as warm runners learn from them.
    pub async fn warm(&self, since: u64) -> Result<Vec<warm::Seen>, StoreError> {
        self.read(move |db| read::warm(db, since)).await
    }

    /// The last measurement of every volume, with its time (seconds), by name.
    pub async fn cache_sizes_now(&self) -> Result<Measured, StoreError> {
        self.read(read::latest_cache_sizes).await
    }

    /// What the build cache served and took from the runners started since `since` (ms), summed.
    pub async fn build_cache_since(&self, since: u64) -> Result<api::BuildCacheCounts, StoreError> {
        self.read(move |db| read::build_cache_since(db, since))
            .await
    }

    /// Every volume measurement since `since` (seconds).
    pub async fn cache_history(&self, since: u64) -> Result<Vec<api::CacheSize>, StoreError> {
        self.read(move |db| read::cache_history(db, since)).await
    }

    /// The jobs not yet closed, for the engine after a restart.
    pub async fn open_jobs(&self) -> Result<Vec<OpenJob>, StoreError> {
        self.read(read::open_jobs).await
    }

    /// The job a runner took, if the history knows it.
    pub async fn runner_job(&self, runner: &str) -> Result<Option<u64>, StoreError> {
        let runner = runner.to_owned();
        self.read(move |db| read::runner_job(db, &runner)).await
    }

    /// How a runner was admitted, with what it reserves since, if the history knows it.
    pub async fn runner_admitted(
        &self,
        runner: &str,
    ) -> Result<Option<api::Admission>, StoreError> {
        let runner = runner.to_owned();
        self.read(move |db| read::runner_admission(db, &runner))
            .await
    }

    /// Where a job's log capture continues.
    pub async fn log_position(&self, job: u64) -> Result<LogPosition, StoreError> {
        self.read(move |db| read::log_position(db, job)).await
    }

    /// How far the mirror's log was counted.
    pub async fn mirror_log_until(&self) -> Result<MirrorPosition, StoreError> {
        self.read(read::mirror_log_until).await
    }
}

/// What sizing reads from the database at `path`, opened read-only and not migrated, as `runnerd doctor` reads it
/// while no daemon runs; `None` when there is no database yet. In a directory it may write, SQLite keeps the
/// `-wal` and `-shm` files a read of a WAL database needs next to it; in one it may not, it reads the file as it is,
/// or refuses when the `-wal` holds changes the file does not have yet.
pub fn sizing_at(path: &Path, since: u64) -> Result<Option<sizing::History>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let read = |path: &str, flags| -> Result<sizing::History, StoreError> {
        let db = Connection::open_with_flags(path, flags)?;
        db.busy_timeout(Duration::from_secs(5))?;
        sizing_of(&db, since)
    };
    let name = path.to_string_lossy();
    match read(&name, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Err(StoreError::Sqlite(error))
            if matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ReadOnly | rusqlite::ErrorCode::CannotOpen)
            ) =>
        {
            // No `-shm` can be made: read the file alone, unless the `-wal` holds what it lacks.
            let mut wal = path.as_os_str().to_owned();
            wal.push("-wal");
            let wal = PathBuf::from(wal);
            if std::fs::metadata(&wal).is_ok_and(|wal| wal.len() > 0) {
                return Err(StoreError::Unread(wal));
            }
            let uri = format!(
                "file:{}?mode=ro&immutable=1",
                name.replace('%', "%25")
                    .replace('?', "%3f")
                    .replace('#', "%23")
            );
            read(
                &uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
        }
        history => history,
    }
    .map(Some)
}

/// What sizing reads from a database of any version this build knows, without migrating it.
fn sizing_of(db: &Connection, since: u64) -> Result<sizing::History, StoreError> {
    let found: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX);
    if found > supported {
        return Err(StoreError::Newer { found, supported });
    }
    if found == 0 {
        return Ok(sizing::History::default());
    }
    // Columns sizing reads that later versions added: before them, a temporary view gives each runner none.
    let added = match found {
        1 => {
            "NULL AS memory_cap, NULL AS cpu_cap, NULL AS admission, NULL AS oom_kills, NULL AS ooms, \
             NULL AS memory_measure"
        }
        2..=6 => "NULL AS admission, NULL AS oom_kills, NULL AS ooms, NULL AS memory_measure",
        7 => "NULL AS ooms, NULL AS memory_measure",
        _ => "",
    };
    if found < 13 {
        db.execute_batch("CREATE TEMP VIEW jobs AS SELECT *, 0 AS host_waited FROM main.jobs")?;
    }
    if !added.is_empty() {
        db.execute_batch(&format!(
            "CREATE TEMP VIEW runners AS SELECT *, {added} FROM main.runners"
        ))?;
    }
    Ok(read::sizing(db, since)?)
}

/// WAL, so the API reads while the writer writes; a busy timeout instead of an immediate error.
fn configure(db: &Connection) -> rusqlite::Result<()> {
    db.pragma_update(None, "journal_mode", "wal")?;
    db.pragma_update(None, "synchronous", "normal")?;
    db.pragma_update(None, "foreign_keys", "on")?;
    db.busy_timeout(Duration::from_secs(5))
}

/// Runs the migrations the database lacks, each in its own transaction with its new version.
fn migrate(db: &mut Connection) -> Result<(), StoreError> {
    let found: u32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX);
    if found > supported {
        return Err(StoreError::Newer { found, supported });
    }
    for (version, migration) in (1..).zip(MIGRATIONS).skip(found as usize) {
        let transaction = db.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", version)?;
        transaction.commit()?;
    }
    Ok(())
}

/// Logs a failed write; the writer goes on with the next one.
fn logged<T>(what: &str, result: rusqlite::Result<T>) -> Option<T> {
    result
        .map_err(|error| warn!("job history: cannot {what}: {error}"))
        .ok()
}

#[cfg(test)]
mod tests;
