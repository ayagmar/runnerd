//! What `GET /v1/jobs` searches, filters and sorts by (ADR 0135). The semantics are here, without the history: the
//! store implements them in SQL, the demo's fake daemon and the fixtures in memory with [`JobQuery::page`], and a
//! store test holds the two to the same pages.
//!
//! Only the default order, newest first by ID, pages, with `before`: a job's ID never changes. Any other order is one
//! page of the first `limit` jobs, read in one snapshot, with how many the filters admit: a sort key such as a wait
//! changes while a job runs, so no continuation of a sorted list could promise to show each job once.

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
/// labels as the list shows them ([`row_bytes`]): a page ends before the job that would pass it. A job's text is cut
/// to [`JOB_TEXT_MAX`] and [`JOB_LABELS_MAX`], so even a page's first job is far below it.
pub const JOBS_PAGE_BYTES: u64 = 1 << 20;

/// The most bytes of a job's repository, workflow, name, branch and URL a list shows, each, to the last whole
/// character; past them, the text is cut and the job is in `JobList::shortened`.
pub const JOB_TEXT_MAX: usize = 1024;

/// The most bytes of a job's labels, as stored (a JSON array of strings), a list shows; past them it shows none,
/// and the job is in `JobList::shortened`.
pub const JOB_LABELS_MAX: usize = 4096;

/// What a sorted list of jobs is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobSort {
    /// When GitHub queued it, in the order of the jobs' IDs, which GitHub gives as it queues them: the default
    /// order.
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

/// What became of a failed job's record, as a list marks it (`JobSummary::record_mark`): what the history notes,
/// which `RecordNote::state` reads in full on the job's page. Docker's images are the inventory (`GET /v1/records`);
/// a record removed outside runnerd reads removed once runnerd lists the images and finds it gone.
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
            RecordState::Removed { .. } | RecordState::Gone { .. } => RecordMark::Removed,
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

impl RecordFilter {
    /// Every filter, in the order a client offers them.
    pub const ALL: [RecordFilter; 4] = [
        RecordFilter::Any,
        RecordFilter::Kept,
        RecordFilter::Removed,
        RecordFilter::Skipped,
    ];

    /// What a client offers it as: `any record`, then the marks' own words.
    pub fn words(self) -> &'static str {
        match self {
            RecordFilter::Any => "any record",
            RecordFilter::Kept => "record kept",
            RecordFilter::Removed => "record removed",
            RecordFilter::Skipped => "no record",
        }
    }
}

/// An order a list of jobs can be read in: what it is sorted by, and which way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOrder {
    /// What it is sorted by.
    pub sort: JobSort,
    /// Which way.
    pub order: SortOrder,
}

impl JobOrder {
    /// The default order, newest first: the one that pages.
    pub const NEWEST: JobOrder = JobOrder {
        sort: JobSort::Queued,
        order: SortOrder::Desc,
    };

    /// Every order, in the order a client offers them: newest first, then each figure largest first, then the
    /// reverse of each.
    pub const ALL: [JobOrder; 8] = [
        JobOrder::NEWEST,
        JobOrder::of_sort(JobSort::Wait, SortOrder::Desc),
        JobOrder::of_sort(JobSort::Run, SortOrder::Desc),
        JobOrder::of_sort(JobSort::Memory, SortOrder::Desc),
        JobOrder::of_sort(JobSort::Queued, SortOrder::Asc),
        JobOrder::of_sort(JobSort::Wait, SortOrder::Asc),
        JobOrder::of_sort(JobSort::Run, SortOrder::Asc),
        JobOrder::of_sort(JobSort::Memory, SortOrder::Asc),
    ];

    const fn of_sort(sort: JobSort, order: SortOrder) -> JobOrder {
        JobOrder { sort, order }
    }

    /// The order `query` asks for.
    pub fn of(query: &JobQuery) -> JobOrder {
        JobOrder {
            sort: query.sort.unwrap_or(JobSort::Queued),
            order: query.order.unwrap_or_default(),
        }
    }

    /// Whether it is the default order, which pages.
    pub fn pages(self) -> bool {
        self == JobOrder::NEWEST
    }

    /// Its words: `newest first`, `longest wait first`.
    pub fn words(self) -> &'static str {
        match (self.sort, self.order) {
            (JobSort::Queued, SortOrder::Desc) => "newest first",
            (JobSort::Queued, SortOrder::Asc) => "oldest first",
            (JobSort::Wait, SortOrder::Desc) => "longest wait first",
            (JobSort::Wait, SortOrder::Asc) => "shortest wait first",
            (JobSort::Run, SortOrder::Desc) => "longest run first",
            (JobSort::Run, SortOrder::Asc) => "shortest run first",
            (JobSort::Memory, SortOrder::Desc) => "largest memory peak first",
            (JobSort::Memory, SortOrder::Asc) => "smallest memory peak first",
        }
    }

    /// `query` read in this order: its sort and order set, `None` for the default's, and `before` dropped where it
    /// does not page.
    pub fn apply(self, query: &mut JobQuery) {
        let default = self.pages();
        query.sort = (!default).then_some(self.sort);
        query.order = (!default).then_some(self.order);
        if !default {
            query.before = None;
        }
    }
}

/// How far back a list of jobs reaches, by when GitHub queued them, as every client offers it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JobRange {
    /// The last 24 hours.
    Day,
    /// The last 7 days.
    Week,
    /// The last 30 days.
    Month,
    /// Every job the history keeps.
    #[default]
    All,
}

impl JobRange {
    /// Every range, shortest first.
    pub const ALL: [JobRange; 4] = [
        JobRange::Day,
        JobRange::Week,
        JobRange::Month,
        JobRange::All,
    ];

    /// Its words: `last 24 hours`, `all kept`.
    pub fn words(self) -> &'static str {
        match self {
            JobRange::Day => "last 24 hours",
            JobRange::Week => "last 7 days",
            JobRange::Month => "last 30 days",
            JobRange::All => "all kept",
        }
    }

    /// `JobQuery::since` for it at `now`: none for every job.
    pub fn since(self, now: u64) -> Option<u64> {
        let days = match self {
            JobRange::Day => 1,
            JobRange::Week => 7,
            JobRange::Month => 30,
            JobRange::All => return None,
        };
        Some(now.saturating_sub(days * 24 * 3600))
    }

    /// The timeline of it at `now`: its window, at most the [`super::TIMELINE_WINDOW_MAX`] a timeline shows.
    pub fn timeline(self, now: u64) -> super::TimelineQuery {
        let longest = now.saturating_sub(super::TIMELINE_WINDOW_MAX);
        super::TimelineQuery {
            since: Some(self.since(now).map_or(longest, |since| since.max(longest))),
            until: None,
        }
    }
}

impl JobQuery {
    /// Checks a query before it is read: a search of 1 to [`JOB_SEARCH_MAX`] bytes without control characters,
    /// `before` only in the default order, and no `cursor`. Returns the reason to refuse it, worded for its user.
    pub fn check(&self) -> Result<(), String> {
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
        if self.cursor.is_some() {
            return Err(
                "a sorted list is one page, its first jobs; narrow it with a search or the filters"
                    .to_owned(),
            );
        }
        if self.before.is_some() && !self.pages() {
            return Err(
                "before pages the list newest first; a sorted list is one page, which a search or the filters \
                 narrow"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Whether it lists in the default order, newest first by ID, the one order that pages, with `before` and
    /// `next`.
    pub fn pages(&self) -> bool {
        self.sort.is_none_or(|sort| sort == JobSort::Queued)
            && self.order.unwrap_or_default() == SortOrder::Desc
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

    /// Puts `jobs` in its order: by ID for `queued`, else by its key, unknown last, ties by ID.
    fn order(&self, jobs: &mut [&JobSummary]) {
        let order = self.order.unwrap_or_default();
        let by_id = |a: &JobSummary, b: &JobSummary| match order {
            SortOrder::Desc => b.id.cmp(&a.id),
            SortOrder::Asc => a.id.cmp(&b.id),
        };
        match self.sort.unwrap_or(JobSort::Queued) {
            JobSort::Queued => jobs.sort_by(|a, b| by_id(a, b)),
            sort => jobs.sort_by(|a, b| {
                let keyed = match (sort.key(a), sort.key(b)) {
                    (Some(a_key), Some(b_key)) => match order {
                        SortOrder::Desc => b_key.cmp(&a_key),
                        SortOrder::Asc => a_key.cmp(&b_key),
                    },
                    (a_key, b_key) => a_key.is_none().cmp(&b_key.is_none()),
                };
                keyed.then_with(|| by_id(a, b))
            }),
        }
    }

    /// The page this query reads of `jobs`, a whole history held in memory, as the store reads it from the
    /// database: for the demo's fake daemon and tests. Refuses what [`check`](JobQuery::check) refuses.
    pub fn page(&self, jobs: &[JobSummary]) -> Result<JobList, String> {
        self.check()?;
        let mut listed: Vec<&JobSummary> = jobs
            .iter()
            .filter(|job| self.admits(job))
            .filter(|job| self.before.is_none_or(|before| job.id < before))
            .collect();
        self.order(&mut listed);
        let size = usize::try_from(self.page_size()).unwrap_or(usize::MAX);
        let mut page = Vec::new();
        let mut shortened = Vec::new();
        let mut bytes = 0;
        for job in listed.iter().take(size) {
            let (shown, cut) = listed_text(job);
            let row = row_bytes(&shown);
            if !page.is_empty() && bytes + row > JOBS_PAGE_BYTES {
                break;
            }
            bytes += row;
            if cut {
                shortened.push(shown.id);
            }
            page.push(shown);
        }
        let total = u64::try_from(listed.len()).unwrap_or(u64::MAX);
        let more = listed.len() > page.len();
        Ok(self.answer(page, more, total, shortened))
    }

    /// A page of `jobs`, in order, of the `total` the filters admit, `shortened` those whose text was cut: with the
    /// next page's `next` when `more` follow in the default order, and `total` in any other.
    pub fn answer(
        &self,
        jobs: Vec<JobSummary>,
        more: bool,
        total: u64,
        shortened: Vec<u64>,
    ) -> JobList {
        let pages = self.pages();
        let next = jobs.last().filter(|_| more && pages).map(|last| last.id);
        let mut applied = self.clone();
        applied.limit = Some(self.page_size());
        applied.order = Some(self.order.unwrap_or_default());
        JobList {
            page: JobPage { jobs, next },
            total: (!pages).then_some(total),
            shortened,
            applied: Some(applied),
        }
    }
}

/// `text` cut to [`JOB_TEXT_MAX`] bytes, to its last whole character.
fn cut(text: &str) -> &str {
    &text[..text.floor_char_boundary(JOB_TEXT_MAX)]
}

/// `job` as a list shows it, as the store reads it: each text cut to [`JOB_TEXT_MAX`] bytes, labels past
/// [`JOB_LABELS_MAX`] left out. And whether any was.
fn listed_text(job: &JobSummary) -> (JobSummary, bool) {
    let cut_option = |text: &Option<String>| text.as_deref().map(|text| cut(text).to_owned());
    let labels_fit = labels_bytes(&job.labels) <= JOB_LABELS_MAX;
    let listed = JobSummary {
        repository: cut(&job.repository).to_owned(),
        name: cut(&job.name).to_owned(),
        workflow: cut_option(&job.workflow),
        branch: cut_option(&job.branch),
        url: cut_option(&job.url),
        labels: if labels_fit {
            job.labels.clone()
        } else {
            Vec::new()
        },
        ..job.clone()
    };
    let shortened = !labels_fit || row_bytes(&listed) != row_bytes(job);
    (listed, shortened)
}

/// The bytes of `labels` as the store keeps them: a JSON array of strings, as `serde_json` writes it.
fn labels_bytes(labels: &[String]) -> usize {
    2 + labels.len().saturating_sub(1)
        + labels
            .iter()
            .map(|label| json_string_len(label))
            .sum::<usize>()
}

/// The bytes of a job's text the page budget counts, as the store counts them in its columns.
pub fn row_bytes(job: &JobSummary) -> u64 {
    let text = |text: Option<&str>| text.map_or(0, str::len);
    u64::try_from(
        job.repository.len()
            + text(job.workflow.as_deref())
            + job.name.len()
            + text(job.branch.as_deref())
            + text(job.url.as_deref())
            + labels_bytes(&job.labels),
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

    /// What a client says above a list an older daemon answered without reading its search, sort or record filter:
    /// that it cannot, and what the list is instead. `None` when the daemon read them.
    pub fn unread_words(&self) -> Option<String> {
        (!self.understood()).then(|| {
            format!(
                "{}; this is every job, newest first",
                crate::api::Feature::JobSearch.unsupported()
            )
        })
    }

    /// [`JobList::shown_words`] as a client says it above the list, with what to do about the rest: `50 of 1234 jobs,
    /// longest wait first; a search or the filters narrow it`.
    pub fn shown_line(&self) -> Option<String> {
        self.shown_words()
            .map(|shown| format!("{shown}; a search or the filters narrow it"))
    }

    /// What a client says of a page with no jobs.
    pub fn none_words() -> &'static str {
        "no jobs match"
    }

    /// What a sorted page shows of its list when it is not all of it: `50 of 1234 jobs, longest wait first`.
    /// `None` in the default order, which pages, and when the page holds every job.
    pub fn shown_words(&self) -> Option<String> {
        let total = self.total?;
        let shown = u64::try_from(self.page.jobs.len()).unwrap_or(u64::MAX);
        if shown >= total {
            return None;
        }
        let query = self.applied.as_ref()?;
        Some(format!(
            "{shown} of {total} jobs, {}",
            JobOrder::of(query).words()
        ))
    }
}

// The tests read the wire format, JSON, which only the host and client features read.
#[cfg(test)]
#[cfg(any(feature = "host", feature = "client"))]
mod tests;
