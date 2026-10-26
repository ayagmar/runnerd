//! Records of failed jobs as clients read and remove them: a record is a snapshot of a failed job's runner, kept as
//! a Docker image so `runnerctl shell` can open a shell where the job failed. What the job history keeps about a
//! job's record once it is gone, and the queries that remove records and open shells. Sizes are bytes, times are
//! seconds since the Unix epoch.

use serde::{Deserialize, Serialize};

/// One record: the runner of a failed job as the job left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The failed job's ID.
    pub job: u64,
    /// `owner/name` on GitHub.
    pub repository: String,
    /// The profile the job ran in.
    pub profile: String,
    /// The workflow's name, when the history knew it.
    pub workflow: Option<String>,
    /// The job's name in the workflow, when the history knew it.
    pub name: Option<String>,
    /// When it was made.
    pub created_at: u64,
    /// The disk it takes on its own: what removing it frees.
    pub bytes: u64,
    /// The Docker image, `runnerd-record:<job>`.
    pub image: String,
}

/// The limits on records, from `[records]` in the config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordLimits {
    /// The most disk all records may take together.
    pub max_total: u64,
    /// The most records kept.
    pub max_count: u32,
    /// How long a record is kept, in seconds.
    pub max_age: u64,
    /// The disk Docker must keep free: no record is made that would leave less.
    pub min_free: u64,
}

/// `GET /v1/records`: every record, oldest first, and the limits they are kept within.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records {
    /// Oldest first.
    pub records: Vec<Record>,
    /// The limits.
    pub limits: RecordLimits,
    /// The jobs whose record a shell is open in.
    pub shells: Vec<u64>,
}

impl Records {
    /// The disk all records take together.
    pub fn bytes(&self) -> u64 {
        self.records.iter().map(|record| record.bytes).sum()
    }
}

/// Why a record was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordRemoval {
    /// A newer record would have passed `max_count`.
    Count,
    /// A newer record would have passed `max_total`.
    Size,
    /// It was older than `max_age`.
    Age,
    /// Someone removed it.
    User,
}

impl std::fmt::Display for RecordRemoval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RecordRemoval::Count => "removed to keep within max_count",
            RecordRemoval::Size => "removed to keep within max_total",
            RecordRemoval::Age => "removed past max_age",
            RecordRemoval::User => "removed by hand",
        })
    }
}

/// What the job history keeps about a failed job's record, after the record is gone too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordNote {
    /// When the record was made; `None` when it was not.
    pub created_at: Option<u64>,
    /// Its size when made.
    pub bytes: Option<u64>,
    /// Why no record was made, such as too little free disk.
    pub skipped: Option<String>,
    /// When it was removed.
    pub removed_at: Option<u64>,
    /// Why it was removed.
    pub removed_by: Option<RecordRemoval>,
}

/// `DELETE /v1/records` query parameters: exactly one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveRecordsQuery {
    /// Remove every record.
    #[serde(default)]
    pub all: bool,
    /// Remove the records older than this, such as `2d` or `12h`.
    pub older_than: Option<String>,
}

/// `DELETE /v1/records…`: what was removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedRecords {
    /// The jobs whose records were removed.
    pub jobs: Vec<u64>,
    /// The disk they took.
    pub bytes: u64,
}

/// `POST /v1/records/{job}/shell` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellQuery {
    /// The step whose command the shell shows, by GitHub's number; default the step that failed.
    pub step: Option<u32>,
    /// The terminal's width in columns.
    pub cols: Option<u16>,
    /// The terminal's height in rows.
    pub rows: Option<u16>,
}

/// The `Upgrade` protocol of `POST /v1/records/{job}/shell`: after `101 Switching Protocols` the connection carries
/// the shell's terminal, raw bytes both ways.
pub const SHELL_PROTOCOL: &str = "runnerd-tty";
