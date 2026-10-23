//! The job history: a SQLite database of jobs, their runners, steps, samples and logs, the events clients resume
//! from, cache sizes and the paused repositories. Owns the schema and its migrations, the one writer and the event
//! broadcast. Writes are messages to a writer thread, so the engine never waits on the disk; reads run on Tokio's
//! blocking pool. It decides nothing about scheduling and stores no secret: no token, no JIT config, no runner
//! `_diag` log.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};
use tracing::warn;

use crate::api::{self, Event, EventEnvelope};
use crate::config::History;
use crate::github::{JobDetails, QueuedJob};
use crate::sync::lock;

mod read;
mod write;

/// The schema's migrations, in order; the database's `user_version` is how many ran.
const MIGRATIONS: &[&str] = &[
    include_str!("store/migrations/0001_history.sql"),
    include_str!("store/migrations/0002_runner_caps.sql"),
    include_str!("store/migrations/0003_paused.sql"),
];

/// Bytes of log lines sent and not yet stored, at most: a job's capture waits beyond it, so a noisy job or a slow
/// disk holds back that job's log, not the daemon's memory.
pub(crate) const LOG_BACKLOG: u64 = 8 << 20;

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
    /// Bytes without page cache.
    pub memory: u64,
    /// Bytes received since the container started.
    pub network_received: u64,
    /// Bytes sent since the container started.
    pub network_sent: u64,
}

/// The last measurement of the cache volumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measured {
    /// When it was taken (seconds); none before the first measurement.
    pub at: Option<u64>,
    /// Each volume's size in bytes, by name.
    pub bytes: BTreeMap<String, u64>,
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
    RunnerRemoved {
        runner: String,
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
    Event(Event),
    Paused {
        repositories: Vec<String>,
        /// Whether it committed.
        done: oneshot::Sender<bool>,
    },
    Prune {
        now: u64,
        history: History,
    },
    Flush(oneshot::Sender<()>),
}

/// The job history. Cloning shares it.
#[derive(Debug, Clone)]
pub struct Store {
    writes: mpsc::UnboundedSender<Write>,
    reader: Arc<Mutex<Connection>>,
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
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let (writes, receiver) = mpsc::unbounded_channel();
        let thread_events = events.clone();
        std::thread::Builder::new()
            .name("runnerd-store".to_owned())
            .spawn(move || write::run(db, receiver, &thread_events))?;
        Ok(Store {
            writes,
            reader: Arc::new(Mutex::new(reader)),
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
        self.send(Write::RunnerRemoved {
            runner: runner.to_owned(),
            at,
        });
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
        self.send(Write::Samples(samples));
    }

    /// Records a sample of the slice.
    pub fn host_sample(&self, sample: api::HostSample) {
        self.send(Write::HostSample(sample));
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
        self.send(Write::Event(event));
    }

    /// Records the paused repositories, all of them, in place of the ones recorded before. The write is sent at
    /// once, in order with every other; the future says whether it committed.
    pub fn paused(&self, repositories: Vec<String>) -> impl Future<Output = bool> + use<> {
        let (done, committed) = oneshot::channel();
        self.send(Write::Paused { repositories, done });
        async move { committed.await.unwrap_or(false) }
    }

    /// Deletes what is older than `history` keeps.
    pub fn prune(&self, now: u64, history: &History) {
        self.send(Write::Prune {
            now,
            history: history.clone(),
        });
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

    /// The repositories as the stored events last announced them, so a restarted daemon announces only what
    /// changed since, including the repositories its config no longer has. Blocks on the database: for startup.
    pub fn announced_repositories(&self) -> Result<Vec<api::RepositoryStatus>, StoreError> {
        Ok(read::announced_repositories(&lock(&self.reader))?)
    }

    /// The repositories recorded paused, for the engine to start with. Blocks on the database: for startup.
    pub fn paused_repositories(&self) -> Result<Vec<String>, StoreError> {
        Ok(read::paused(&lock(&self.reader))?)
    }

    /// The slice's samples since `since` (seconds), oldest first.
    pub async fn host_samples(&self, since: u64) -> Result<Vec<api::HostSample>, StoreError> {
        self.read(move |db| read::host_samples(db, since)).await
    }

    /// Finished jobs per repository and bucket.
    pub async fn stats(&self, query: api::StatsQuery, now: u64) -> Result<api::Stats, StoreError> {
        self.read(move |db| read::stats(db, &query, now)).await
    }

    /// The last measurement of every volume, with its time (seconds), by name.
    pub async fn cache_sizes_now(&self) -> Result<Measured, StoreError> {
        self.read(read::latest_cache_sizes).await
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

    /// Where a job's log capture continues.
    pub async fn log_position(&self, job: u64) -> Result<LogPosition, StoreError> {
        self.read(move |db| read::log_position(db, job)).await
    }

    /// How far the mirror's log was counted.
    pub async fn mirror_log_until(&self) -> Result<MirrorPosition, StoreError> {
        self.read(read::mirror_log_until).await
    }
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
