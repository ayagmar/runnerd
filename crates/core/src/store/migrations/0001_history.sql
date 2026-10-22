-- Version 1: job history, samples, logs, events and cache sizes. Times are milliseconds since the Unix epoch.

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
    log_offset INTEGER NOT NULL DEFAULT 0
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
    mirror_bytes INTEGER NOT NULL DEFAULT 0
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
-- microseconds; network: bytes since the container started.
CREATE TABLE samples (
    runner TEXT NOT NULL,
    at INTEGER NOT NULL,
    cpu INTEGER NOT NULL,
    cpu_usage INTEGER NOT NULL,
    memory INTEGER NOT NULL,
    network_received INTEGER NOT NULL,
    network_sent INTEGER NOT NULL,
    PRIMARY KEY (runner, at)
) WITHOUT ROWID;
CREATE INDEX samples_by_at ON samples (at);

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

-- mirror_log_until: the Docker timestamp of the last mirror log line counted.
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;
