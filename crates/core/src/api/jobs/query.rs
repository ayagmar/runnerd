//! What `GET /v1/jobs` searches, filters and sorts by, and the cursor that pages a sorted list (ADR 0135). The
//! semantics are here, without the history: the store implements them in SQL, the demo's fake daemon and the
//! fixtures in memory with [`JobQuery::page`], and a store test holds the two to the same pages.
//!
//! A sort's key only ever goes from unknown to known (a job starts, ends, or its record becomes final), and unknown
//! keys sort last in both directions; so a pass through the pages never shows a job twice, and a job whose key
//! becomes known behind the cursor is not shown by that pass.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::{JobList, JobPage, JobQuery, JobSummary};
use crate::api::{RecordNote, RecordState, Tone};

/// The longest search, `JobQuery::q`, in bytes.
pub const JOB_SEARCH_MAX: usize = 100;

/// Jobs a page holds unless `limit` says otherwise.
pub const JOBS_PAGE_DEFAULT: u32 = 50;

/// Jobs a page holds at most.
pub const JOBS_PAGE_MAX: u32 = 500;

/// The text a page holds at most, in bytes, counted over each job's repository, workflow, name, branch, URL and
/// labels as stored: the job that reaches it is the page's last, and the page has a next one.
pub const JOBS_PAGE_BYTES: u64 = 1 << 20;

/// What a sorted list of jobs is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobSort {
    /// When GitHub queued it.
    Queued,
    /// How long it waited for a runner, as its page says it ([`JobSummary::waited`]): from queued to started, in
    /// whole seconds. Unknown until it starts, so a job still queued sorts with the unknown ones.
    Wait,
    /// How long it ran ([`JobSummary::ran`]) once its record is final; unknown while it runs.
    Run,
    /// Its memory peak once its record is final; unknown while it runs, whose peak still grows.
    Memory,
}

impl JobSort {
    /// Its name in a query and a cursor.
    fn name(self) -> &'static str {
        match self {
            JobSort::Queued => "queued",
            JobSort::Wait => "wait",
            JobSort::Run => "run",
            JobSort::Memory => "memory",
        }
    }

    fn named(name: &str) -> Option<JobSort> {
        [
            JobSort::Queued,
            JobSort::Wait,
            JobSort::Run,
            JobSort::Memory,
        ]
        .into_iter()
        .find(|sort| sort.name() == name)
    }

    /// `job`'s key: seconds, or bytes for memory; `None` while unknown.
    pub fn key(self, job: &JobSummary) -> Option<u64> {
        let timeline = &job.timeline;
        match self {
            JobSort::Queued => Some(timeline.queued_at),
            JobSort::Wait => timeline
                .started_at
                .map(|started| started.saturating_sub(timeline.queued_at)),
            JobSort::Run if job.state.is_final() => {
                let (started, finished) = (timeline.started_at?, timeline.finished_at?);
                Some(finished.saturating_sub(started))
            }
            JobSort::Memory if job.state.is_final() => job.metrics.memory_peak,
            JobSort::Run | JobSort::Memory => None,
        }
    }
}

/// Which way a sorted list goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortOrder {
    /// Largest or latest first.
    #[default]
    Desc,
    /// Smallest or earliest first.
    Asc,
}

impl SortOrder {
    fn name(self) -> &'static str {
        match self {
            SortOrder::Desc => "desc",
            SortOrder::Asc => "asc",
        }
    }
}

/// `JobQuery::record`: jobs by what became of their record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordFilter {
    /// Every job the history notes a record for: kept, removed or not kept.
    Any,
    /// Those whose record is kept.
    Kept,
    /// Those whose record was made, then removed.
    Removed,
    /// Those whose record was not made.
    Skipped,
}

impl RecordFilter {
    /// Whether a job with `mark` passes.
    pub fn admits(self, mark: Option<RecordMark>) -> bool {
        match (self, mark) {
            (RecordFilter::Any, mark) => mark.is_some(),
            (RecordFilter::Kept, Some(RecordMark::Kept))
            | (RecordFilter::Removed, Some(RecordMark::Removed))
            | (RecordFilter::Skipped, Some(RecordMark::Skipped)) => true,
            _ => false,
        }
    }
}

/// What became of a failed job's record, as a list marks it (`JobSummary::record`): what the history notes, which
/// `RecordNote::state` reads in full on the job's page. Docker's images are the inventory (`GET /v1/records`); a
/// record removed outside runnerd still reads kept here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordMark {
    /// It is kept: a shell can open in it.
    Kept,
    /// It was made, then removed.
    Removed,
    /// None was made.
    Skipped,
    /// A newer daemon's mark this version does not know.
    #[serde(other)]
    Unknown,
}

impl RecordMark {
    /// The mark of a record the history notes as `note`, in `RecordNote::state`'s precedence.
    pub fn of(note: &RecordNote) -> RecordMark {
        match note.state() {
            RecordState::Skipped { .. } => RecordMark::Skipped,
            RecordState::Removed { .. } => RecordMark::Removed,
            RecordState::Kept { .. } => RecordMark::Kept,
        }
    }

    /// Its words: `record kept`, `record removed` or `no record`; `None` for one this version does not know.
    pub fn words(self) -> Option<&'static str> {
        match self {
            RecordMark::Kept => Some("record kept"),
            RecordMark::Removed => Some("record removed"),
            RecordMark::Skipped => Some("no record"),
            RecordMark::Unknown => None,
        }
    }

    /// What it means: a kept record is a fact to act on, the rest are background.
    pub fn tone(self) -> Tone {
        match self {
            RecordMark::Kept => Tone::Neutral,
            RecordMark::Removed | RecordMark::Skipped | RecordMark::Unknown => Tone::Quiet,
        }
    }
}

/// Where a sorted list's next page starts: after the job with `key` and `id`, in the list `filter` names. A client
/// passes it on as `JobQuery::cursor` and reads nothing in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobCursor {
    /// The sort it pages.
    pub sort: JobSort,
    /// Its direction.
    pub order: SortOrder,
    /// [`JobQuery::filter_hash`] of the query it pages.
    pub filter: u64,
    /// The last job's key; `None` while unknown.
    pub key: Option<u64>,
    /// The last job's ID.
    pub id: u64,
}

/// A cursor's version, its first part: a later version that pages another way changes it, and this version refuses
/// a cursor it cannot read.
const CURSOR_VERSION: &str = "j1";

impl JobCursor {
    /// The cursor `JobPage::cursor` carries: `j1.wait.desc.<hash>.<key or ->.<id>`.
    pub fn encode(&self) -> String {
        let key = self
            .key
            .map_or_else(|| "-".to_owned(), |key| key.to_string());
        format!(
            "{CURSOR_VERSION}.{}.{}.{:016x}.{key}.{}",
            self.sort.name(),
            self.order.name(),
            self.filter,
            self.id
        )
    }

    /// Reads a cursor [`encode`](JobCursor::encode) wrote; `None` for any other text.
    pub fn parse(text: &str) -> Option<JobCursor> {
        let mut parts = text.split('.');
        let mut next = || parts.next();
        if next()? != CURSOR_VERSION {
            return None;
        }
        let sort = JobSort::named(next()?)?;
        let order = match next()? {
            "desc" => SortOrder::Desc,
            "asc" => SortOrder::Asc,
            _ => return None,
        };
        let filter = u64::from_str_radix(next()?, 16).ok()?;
        let key = match next()? {
            "-" => None,
            key => Some(key.parse().ok()?),
        };
        let id = next()?.parse().ok()?;
        next().is_none().then_some(JobCursor {
            sort,
            order,
            filter,
            key,
            id,
        })
    }

    /// Whether `job` comes after it in its list.
    fn precedes(&self, job: &JobSummary) -> bool {
        let key = self.sort.key(job);
        let after_id = match self.order {
            SortOrder::Desc => job.id < self.id,
            SortOrder::Asc => job.id > self.id,
        };
        match (self.key, key) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => after_id,
            (Some(last), Some(key)) => match self.order {
                SortOrder::Desc => key < last || (key == last && after_id),
                SortOrder::Asc => key > last || (key == last && after_id),
            },
        }
    }
}

/// FNV-1a, 64 bits: a hash that stays the same across builds and platforms, unlike `std`'s.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl JobQuery {
    /// Checks a query before it is read: a search of 1 to [`JOB_SEARCH_MAX`] bytes without control characters, a
    /// cursor only with the sort, direction and filters it was made for, and `before` only without a sort. Returns
    /// the cursor, or the reason to refuse the query, worded for its user.
    pub fn check(&self) -> Result<Option<JobCursor>, String> {
        if let Some(q) = &self.q {
            if q.is_empty() || q.len() > JOB_SEARCH_MAX {
                return Err(format!(
                    "a search is 1 to {JOB_SEARCH_MAX} bytes; leave q out to list every job"
                ));
            }
            if q.chars().any(char::is_control) {
                return Err("a search has no control characters".to_owned());
            }
        }
        match (self.sort, self.before, &self.cursor) {
            (Some(_), Some(_), _) => Err(
                "before pages the list by ID; a sorted list pages with the cursor its page gives"
                    .to_owned(),
            ),
            (None, _, Some(_)) => {
                Err("a cursor pages a sorted list; give the sort it was made for".to_owned())
            }
            (Some(sort), None, Some(cursor)) => {
                let read = JobCursor::parse(cursor)
                    .ok_or_else(|| "the cursor is not one this daemon made".to_owned())?;
                if read.sort != sort
                    || read.order != self.order.unwrap_or_default()
                    || read.filter != self.filter_hash()
                {
                    return Err(
                        "the cursor is of another sort or search; start from the first page"
                            .to_owned(),
                    );
                }
                Ok(Some(read))
            }
            (_, _, None) => Ok(None),
        }
    }

    /// A hash of what the query filters by, which a cursor carries so that it pages only the list it was made for.
    pub fn filter_hash(&self) -> u64 {
        // Each field absent, or its length and text, so no two filters write the same bytes.
        let mut canonical = String::new();
        let mut field = |value: Option<String>| match value {
            Some(value) => {
                let _ = write!(canonical, "{}:{value};", value.len());
            }
            None => canonical.push_str("-;"),
        };
        field(self.repository.clone());
        field(self.state.map(|state| format!("{state:?}")));
        field(self.conclusion.clone());
        field(self.workflow.clone());
        field(self.name.clone());
        field(self.since.map(|since| since.to_string()));
        field(self.until.map(|until| until.to_string()));
        field(self.q.clone());
        field(self.branch.clone());
        field(self.record.map(|record| format!("{record:?}")));
        fnv1a(canonical.as_bytes())
    }

    /// How many jobs a page holds: `limit`, within 1 and [`JOBS_PAGE_MAX`].
    pub fn page_size(&self) -> u32 {
        self.limit
            .unwrap_or(JOBS_PAGE_DEFAULT)
            .clamp(1, JOBS_PAGE_MAX)
    }

    /// Whether `job` passes its filters: not its paging.
    pub fn admits(&self, job: &JobSummary) -> bool {
        let equal = |want: &Option<String>, have: Option<&str>| {
            want.as_deref().is_none_or(|want| have == Some(want))
        };
        let queued = job.timeline.queued_at;
        equal(&self.repository, Some(&job.repository))
            && self.state.is_none_or(|state| state == job.state)
            && equal(&self.conclusion, job.conclusion.as_deref())
            && equal(&self.workflow, job.workflow.as_deref())
            && equal(&self.name, Some(&job.name))
            && equal(&self.branch, job.branch.as_deref())
            && self.since.is_none_or(|since| queued >= since)
            && self.until.is_none_or(|until| queued < until)
            && self
                .record
                .is_none_or(|record| record.admits(job.record_mark))
            && self.q.as_deref().is_none_or(|q| {
                let q = q.to_ascii_lowercase();
                [
                    Some(job.name.as_str()),
                    job.workflow.as_deref(),
                    job.branch.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|text| text.to_ascii_lowercase().contains(&q))
            })
    }

    /// The page this query reads of `jobs`, a whole history held in memory, as the store reads it from the
    /// database: for the demo's fake daemon and tests. Refuses what [`check`](JobQuery::check) refuses.
    pub fn page(&self, jobs: &[JobSummary]) -> Result<JobList, String> {
        let cursor = self.check()?;
        let mut listed: Vec<&JobSummary> = jobs.iter().filter(|job| self.admits(job)).collect();
        match self.sort {
            None => {
                listed.sort_by_key(|job| std::cmp::Reverse(job.id));
                listed.retain(|job| self.before.is_none_or(|before| job.id < before));
            }
            Some(sort) => {
                let order = self.order.unwrap_or_default();
                listed.sort_by(|a, b| {
                    let (a_key, b_key) = (sort.key(a), sort.key(b));
                    let keyed = match (a_key, b_key) {
                        (Some(a_key), Some(b_key)) => match order {
                            SortOrder::Desc => b_key.cmp(&a_key),
                            SortOrder::Asc => a_key.cmp(&b_key),
                        },
                        (a_key, b_key) => a_key.is_none().cmp(&b_key.is_none()),
                    };
                    keyed.then_with(|| match order {
                        SortOrder::Desc => b.id.cmp(&a.id),
                        SortOrder::Asc => a.id.cmp(&b.id),
                    })
                });
                if let Some(cursor) = cursor {
                    listed.retain(|job| cursor.precedes(job));
                }
            }
        }
        let size = usize::try_from(self.page_size()).unwrap_or(usize::MAX);
        let mut page = Vec::new();
        let mut bytes = 0;
        for job in &listed {
            if page.len() == size || (bytes >= JOBS_PAGE_BYTES && !page.is_empty()) {
                break;
            }
            bytes += row_bytes(job);
            page.push((*job).clone());
        }
        let more = listed.len() > page.len();
        Ok(self.answer(page, more))
    }

    /// A page of `jobs`, in order, with the next page's `next` or `cursor` when `more` follow.
    pub fn answer(&self, jobs: Vec<JobSummary>, more: bool) -> JobList {
        let last = jobs.last().filter(|_| more);
        let (next, cursor) = match (self.sort, last) {
            (None, Some(last)) => (Some(last.id), None),
            (Some(sort), Some(last)) => (
                None,
                Some(
                    JobCursor {
                        sort,
                        order: self.order.unwrap_or_default(),
                        filter: self.filter_hash(),
                        key: sort.key(last),
                        id: last.id,
                    }
                    .encode(),
                ),
            ),
            (_, None) => (None, None),
        };
        let mut applied = self.clone();
        applied.limit = Some(self.page_size());
        applied.order = self.sort.map(|_| self.order.unwrap_or_default());
        JobList {
            page: JobPage { jobs, next },
            cursor,
            applied: Some(applied),
        }
    }
}

/// The bytes of a job's text the page budget counts, as the store counts them in its columns.
pub fn row_bytes(job: &JobSummary) -> u64 {
    let text = |text: Option<&str>| text.map_or(0, str::len);
    // The labels as the store keeps them: a JSON array of strings, as `serde_json` writes it.
    let labels = 2
        + job.labels.len().saturating_sub(1)
        + job
            .labels
            .iter()
            .map(|label| json_string_len(label))
            .sum::<usize>();
    u64::try_from(
        job.repository.len()
            + text(job.workflow.as_deref())
            + job.name.len()
            + text(job.branch.as_deref())
            + text(job.url.as_deref())
            + labels,
    )
    .unwrap_or(u64::MAX)
}

/// The length of `text` as a JSON string, quotes included, escaped as `serde_json` escapes it.
fn json_string_len(text: &str) -> usize {
    2 + text
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            control if u32::from(control) < 0x20 => 6,
            other => other.len_utf8(),
        })
        .sum::<usize>()
}

impl JobList {
    /// Whether the daemon read the query's search, sort and record filter: one older than them leaves out
    /// `applied`, ignores them and lists every job, which a client says is not supported rather than a result.
    pub fn understood(&self) -> bool {
        self.applied.is_some()
    }
}

// The tests read the wire format, JSON, which only the host and client features read.
#[cfg(test)]
#[cfg(any(feature = "host", feature = "client"))]
mod tests;
