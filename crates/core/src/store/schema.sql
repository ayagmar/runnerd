-- The job history at schema version 19, which `Store::open` creates in a new database. Times are milliseconds since
-- the Unix epoch. A database from version 15 on is brought here by `store::MIGRATIONS`, an older or newer one is
-- refused (ADR 0073); a change to this schema adds a step there, its columns appended where `ALTER TABLE` puts them.

-- One row per GitHub job a runner of this daemon may run, from the moment it is seen queued.
CREATE TABLE jobs (
    id INTEGER PRIMARY KEY,
    repository TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    run_attempt INTEGER NOT NULL,
    workflow TEXT,
    name TEXT NOT NULL,
    branch TEXT,
    url TEXT,
    labels TEXT NOT NULL,
    state TEXT NOT NULL,
    conclusion TEXT,
    -- Set while GitHub reports the job completed; the record is final once its runner is removed too.
    github_completed INTEGER NOT NULL DEFAULT 0,
    runner TEXT,
    queued_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    -- Totals and peaks, kept once the record is final; computed from samples before that.
    cpu_time INTEGER,
    cpu_peak INTEGER,
    cpu_average INTEGER,
    memory_peak INTEGER,
    memory_average INTEGER,
    network_received INTEGER,
    network_sent INTEGER,
    log_lines INTEGER NOT NULL DEFAULT 0,
    log_bytes INTEGER NOT NULL DEFAULT 0,
    log_truncated INTEGER NOT NULL DEFAULT 0,
    -- How far the log capture got: the runner's page file and the byte offset after the last stored line.
    log_page INTEGER NOT NULL DEFAULT 1,
    log_offset INTEGER NOT NULL DEFAULT 0,
    -- How long it waited because the host's memory could not take its runner (ADR 0055), which sizing does not read
    -- as a wait for a slot.
    host_waited INTEGER NOT NULL DEFAULT 0,
    -- How much of its wait passed while no runnerd ran, as the daemon that started after measured it from its
    -- history's last sign of life (ADR 0081); waits for a slot leave it out. 0 in rows from before schema 16.
    unseen INTEGER NOT NULL DEFAULT 0,
    -- What held its CPU back, kept once the record is final as the totals above are (ADR 0079): the periods of its
    -- CPU quota in which it had work and those it was throttled in, and the milliseconds a task of it waited for a
    -- CPU (`cpu.pressure`). NULL where the kernel kept none, and in rows from before schema 17.
    cpu_periods INTEGER,
    cpu_throttled INTEGER,
    cpu_stalled INTEGER,
    -- How long it waited with no slot of its profile free while the slice's memory, the host's and the CPU budget had
    -- room for its runner: what one more slot would have saved it (ADR 0090). 0 in rows from before schema 18.
    slot_waited INTEGER NOT NULL DEFAULT 0,
    -- When runnerd first listed it queued (ADR 0092): its first listing's answer, kept across later listings and
    -- restarts. NULL for a job no listing showed queued, such as one a warm runner took first, and in rows from
    -- before schema 19.
    seen_at INTEGER
);
CREATE INDEX jobs_by_repository ON jobs (repository, id);
CREATE INDEX jobs_by_state ON jobs (state);
CREATE INDEX jobs_by_queued_at ON jobs (queued_at);

-- One row per runner container this daemon started.
CREATE TABLE runners (
    name TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    profile TEXT NOT NULL,
    slot INTEGER NOT NULL,
    container TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    removed_at INTEGER,
    mirror_bytes INTEGER NOT NULL DEFAULT 0,
    -- The caps its container ran with, which a reload may change in its profile since: bytes, millicores.
    memory_cap INTEGER,
    cpu_cap INTEGER,
    -- How it was admitted (`api::Admission` as JSON), and how many times the kernel killed a process in its
    -- container for memory (`memory.events` `oom_kill`).
    admission TEXT,
    oom_kills INTEGER,
    -- 'without_page_cache': its samples leave out all page cache on the file LRU lists. NULL in rows an older
    -- runnerd wrote, whose samples counted active page cache (ADR 0037).
    memory_measure TEXT,
    -- How many times its memory reached a limit with nothing left to reclaim (`memory.events` `oom`), NULL while
    -- never read.
    ooms INTEGER,
    -- What the shared build cache served it and took from it (ADR 0052), and whether its job could write to it (1)
    -- or only read (0); NULL for a runner without the build cache.
    build_cache_hits INTEGER NOT NULL DEFAULT 0,
    build_cache_misses INTEGER NOT NULL DEFAULT 0,
    build_cache_read_bytes INTEGER NOT NULL DEFAULT 0,
    build_cache_written_bytes INTEGER NOT NULL DEFAULT 0,
    build_cache_refused INTEGER NOT NULL DEFAULT 0,
    build_cache_writer INTEGER,
    -- How many times its sockets were throttled for memory pressure (`memory.events` `sock_throttled`), NULL while
    -- never read.
    sock_throttled INTEGER,
    -- How many times its own limit failed reclaim (`memory.events.local` `oom`), the largest value sampled, NULL
    -- while never read. Unlike `ooms` it leaves out a limit a descendant cgroup hit (ADR 0070).
    local_ooms INTEGER,
    -- Why it started (ADR 0092): 'job' for a queued job; 'warm' for one of its repository's configured warm runners,
    -- 'burst' for one of the jobs its runs are expected to queue still. NULL for one a restart adopted, and in rows
    -- from before schema 19.
    purpose TEXT,
    -- Why it was removed while idle (`scheduler::StopReason::as_str`); NULL for one that ended, was killed or is
    -- still up, and in rows from before schema 19.
    stopped_for TEXT
);

CREATE TABLE steps (
    job INTEGER NOT NULL,
    number INTEGER NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    conclusion TEXT,
    started_at INTEGER,
    completed_at INTEGER,
    PRIMARY KEY (job, number)
) WITHOUT ROWID;

-- cpu: millicores over the interval before the sample; cpu_usage: the container's CPU time so far, in
-- microseconds; network: bytes since the container started; cpu_periods, cpu_throttled: `cpu.stat`'s `nr_periods`
-- and `nr_throttled` so far, cpu_stalled: `cpu.pressure`'s `some` `total` so far, in microseconds, each NULL where
-- the kernel keeps none.
CREATE TABLE samples (
    runner TEXT NOT NULL,
    at INTEGER NOT NULL,
    cpu INTEGER NOT NULL,
    cpu_usage INTEGER NOT NULL,
    memory INTEGER NOT NULL,
    network_received INTEGER NOT NULL,
    network_sent INTEGER NOT NULL,
    cpu_periods INTEGER,
    cpu_throttled INTEGER,
    cpu_stalled INTEGER,
    PRIMARY KEY (runner, at)
) WITHOUT ROWID;

CREATE TABLE host_samples (
    at INTEGER PRIMARY KEY,
    cpu INTEGER NOT NULL,
    memory INTEGER NOT NULL
);

CREATE TABLE log_lines (
    job INTEGER NOT NULL,
    line INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (job, line)
) WITHOUT ROWID;

-- Durable events. AUTOINCREMENT: an ID is never reused, even after every event was pruned.
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX events_by_at ON events (at);

CREATE TABLE cache_sizes (
    at INTEGER NOT NULL,
    name TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (at, name)
) WITHOUT ROWID;

-- mirror_log_until: the Docker timestamp (ns) of the last mirror log line counted; mirror_log_seen: how many lines
-- logged at exactly that time were counted; opened_at: when a daemon last opened the history, a sign of life that
-- ends a downtime (ADR 0081).
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

-- The repositories paused through the API, so a restart keeps them paused. A reload that removes a repository from
-- the config removes its row.
CREATE TABLE paused (
    repository TEXT PRIMARY KEY NOT NULL
);

-- Each repository's latest `repository.*` event, which pruning keeps and a restarted daemon reads
-- (`read::announced`).
CREATE TABLE announced (
    repository TEXT PRIMARY KEY NOT NULL,
    event INTEGER NOT NULL
) WITHOUT ROWID;

-- Each profile's latest `profile.*` event, as `announced` does for repositories, so a restarted daemon announces a
-- profile its config no longer has.
CREATE TABLE announced_profiles (
    profile TEXT PRIMARY KEY NOT NULL,
    event INTEGER NOT NULL
) WITHOUT ROWID;

-- What the history keeps about a failed job's record (`api::RecordNote`): when it was made and its size, or why
-- none was made; when it was removed and why. Pruned with its job.
CREATE TABLE records (
    job INTEGER PRIMARY KEY NOT NULL,
    created_at INTEGER,
    bytes INTEGER,
    skipped TEXT,
    removed_at INTEGER,
    removed_by TEXT
);

-- Each change auto sizing made or meant to make to a profile's `memory`, `slots` and `cpus` (`api::SizingChange`),
-- written before the config file is: 'pending' until the file is written ('done') or is not ('failed'). The
-- interval between changes counts from the last pending or done one. `from_cpus` and `to_cpus` are NULL in changes
-- that left them, and in rows an older runnerd wrote. Never pruned: a few rows a day at most.
CREATE TABLE auto_sizing (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile TEXT NOT NULL,
    at INTEGER NOT NULL,
    from_memory INTEGER NOT NULL,
    from_slots INTEGER NOT NULL,
    to_memory INTEGER NOT NULL,
    to_slots INTEGER NOT NULL,
    reason TEXT NOT NULL,
    state TEXT NOT NULL,
    from_cpus INTEGER,
    to_cpus INTEGER
);
CREATE INDEX auto_sizing_by_profile ON auto_sizing (profile, at);

-- What the notifier keeps per `[notify]` target (ADR 0051): `event`, the last durable event delivered; `digest`,
-- when the last digest went; `insight:<key>`, when an insight told of was last seen to hold.
CREATE TABLE notify (
    target TEXT NOT NULL,
    key TEXT NOT NULL,
    value INTEGER NOT NULL,
    PRIMARY KEY (target, key)
) WITHOUT ROWID;

-- Each repository's visibility as GitHub last gave it, and when, for pricing jobs at GitHub's rates (ADR 0072).
-- Rows stay when a repository leaves the config, as its jobs stay in the history.
CREATE TABLE repositories (
    name TEXT PRIMARY KEY NOT NULL,
    visibility TEXT NOT NULL,
    checked_at INTEGER NOT NULL
) WITHOUT ROWID;

-- One row: the history holds every final job that ended since `jobs_since`. Pruning raises it to its cutoff before
-- it deletes job records. It starts when the database does.
CREATE TABLE history_kept (
    jobs_since INTEGER NOT NULL
);
INSERT INTO history_kept (jobs_since) VALUES (CAST(strftime('%s', 'now') AS INTEGER) * 1000);
