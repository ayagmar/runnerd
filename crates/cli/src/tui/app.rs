//! The TUI's state as a pure reducer: `update` takes a key, an event or an answer from the daemon and returns the
//! requests to make. It decides what is shown and asked for; making requests is `tui`'s, drawing is `draw`'s, and
//! the daemon's state comes from `watch::State`, the reducer `runnerctl watch` uses.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use runnerd_core::api::{
    self, ActionPreview, ActionQuery, ActionResult, Activity, Attention, Caches, ConfigCheck,
    ConfigFile, Doctor, Event, EventEnvelope, Fix, HostSample, Insights, Job, JobAction,
    JobComparison, JobList, JobMetrics, JobOrder, JobQuery, JobRange, JobState, JobSummary,
    LineRange, LogOutline, LogSearch, LogSearchQuery, LogSummary, QueuedJob, RecordFilter, Records,
    Resize, Sample, SampleSeries, Savings, Sizing, SlotTimeline, Status, StyledLine, Subject,
    Timeline,
};

use super::log::{LogView, Way};
use super::mouse::{List, Target};
use super::overlay::{Choice, Input, Overlay, Pick, Picker, Typing, Value};
use super::prompts::{Prompt, Prompts};
use super::reading::{Reading, Unread};
use crate::watch;

/// Samples kept per running job: ten minutes at the daemon's two seconds.
const JOB_SAMPLES: usize = 300;
/// Host samples kept: thirty minutes.
const HOST_SAMPLES: usize = 900;
/// Log lines kept for the open job; older ones are dropped.
pub(crate) const LOG_LINES: usize = 5000;
/// Jobs per history page.
pub(crate) const HISTORY_PAGE: u32 = 100;
/// The conclusions the outcome filter offers, after "any".
const CONCLUSIONS: [&str; 3] = ["failure", "success", "cancelled"];
/// The most runs of the open job's own the job view compares it with, as the dashboard does.
pub(crate) const RUNS: u32 = 30;
/// How often the day's activity is read again while nothing starts or ends, so its 24 hours move on.
const ACTIVITY_EVERY: u64 = 60;
/// How often the queued jobs' reasons are read again while no job or slot changes: no event carries them (ADR 0044),
/// so the dashboard reads the status this often too.
const REASONS_EVERY: u64 = 10;
/// How often the insights are read again: they change with finished jobs, slowly, and each read goes over two weeks of
/// history.
const INSIGHTS_EVERY: u64 = 300;
/// How often what needs attention is read again with no `attention.changed` event: the bound ADR 0132 gives clients.
const ATTENTION_EVERY: u64 = 30;
/// Matches a page of a log search holds.
pub(crate) const SEARCH_PAGE: u32 = 200;

/// What happened: a key, something from the daemon, or the answer to a request.
#[derive(Debug)]
pub(crate) enum Msg {
    Key(KeyEvent),
    /// A click on what the last frame drew there.
    Click(Target),
    /// The mouse wheel turned: down with a positive count of rows.
    Wheel(isize),
    /// The terminal changed size; only the drawing changes.
    Resize,
    Event(EventEnvelope),
    /// The event stream failed for good; the feed is reconnecting.
    Disconnected(String),
    /// The feed is reading events again, resumed after the last one it read.
    Reconnected,
    /// The daemon's state read again, after a reconnect with no event to resume after.
    Snapshot(Box<watch::State>),
    Job(Job),
    Samples(SampleSeries),
    /// The answer to the history request of this generation.
    History {
        generation: u64,
        list: Result<JobList, String>,
    },
    /// The answer to the timeline request of this generation.
    Timeline {
        generation: u64,
        timeline: Result<SlotTimeline, Unread>,
    },
    /// The open job's log outline.
    Outline {
        job: u64,
        outline: Result<LogOutline, Unread>,
    },
    /// Lines of the open job's log, read or followed.
    Lines {
        job: u64,
        lines: Vec<StyledLine>,
    },
    /// A page of a search of the open job's log.
    Found {
        job: u64,
        query: String,
        found: Result<LogSearch, Unread>,
    },
    /// The open job beside its baseline.
    Compared {
        job: u64,
        comparison: Result<JobComparison, Unread>,
    },
    /// What a cancel or re-run would do, before anything is sent.
    Preview(Box<ActionPreview>),
    /// How GitHub took a cancel or re-run.
    Acted(ActionResult),
    Status(Status),
    Records(Records),
    Sizing(Sizing),
    Caches(Result<Caches, Unread>),
    Doctor(Result<Doctor, Unread>),
    Config(Result<ConfigRead, Unread>),
    /// What needs attention (`GET /v1/attention`).
    Attention(Result<Attention, Unread>),
    /// The jobs started and finished in the last 24 hours, read by the request of this generation.
    Activity {
        generation: u64,
        activity: Activity,
    },
    /// The status read again for why each queued job waits; `None` when it could not be read.
    Reasons(Option<Box<Status>>),
    /// What the history says (`GET /v1/insights`).
    Insights(Box<Insights>),
    /// What the jobs' minutes would have cost on GitHub's runners (`GET /v1/savings`).
    Savings(Box<Savings>),
    /// The answer to the open job's request for its latest runs.
    Runs {
        job: u64,
        list: Result<JobList, String>,
    },
    /// A request succeeded; the text says what it did.
    Done(String),
    /// A request failed.
    Failed(String),
    /// A pause was refused because the repository, or one of them in a pause of all, runs a job.
    Busy(Refused),
}

/// The daemon's config file and what its check says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigRead {
    pub(crate) file: ConfigFile,
    pub(crate) check: ConfigCheck,
}

/// What the job's page shows of a queued job until its record arrives.
fn queued_summary(job: &QueuedJob) -> JobSummary {
    JobSummary {
        repository: job.repository.clone(),
        run_id: job.run_id,
        run_attempt: job.run_attempt,
        workflow: job.workflow.clone(),
        branch: job.branch.clone(),
        url: job.url.clone(),
        timeline: Timeline {
            queued_at: job.queued_at,
            ..Timeline::default()
        },
        ..stub_summary(job.id, &job.repository, &job.name)
    }
}

/// What the job's page shows of a job known only by its ID, repository and name until its record arrives.
fn stub_summary(id: u64, repository: &str, name: &str) -> JobSummary {
    JobSummary {
        id,
        repository: repository.to_owned(),
        run_id: 0,
        run_attempt: 0,
        workflow: None,
        name: name.to_owned(),
        branch: None,
        url: None,
        labels: Vec::new(),
        state: JobState::Queued,
        conclusion: None,
        runner: None,
        timeline: Timeline::default(),
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
        runner_fault: false,
        record_mark: None,
    }
}

/// A pause the daemon refused because jobs run, waiting for `y` to force it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    /// `None` for every repository.
    pub(crate) repository: Option<String>,
    /// The daemon's text, naming the jobs.
    pub(crate) message: String,
    /// How many jobs a forced pause cancels.
    pub(crate) jobs: usize,
}

/// A request `update` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Read the job and its samples, its log's outline, and its last lines, following them while the job runs.
    OpenJob(u64),
    /// Stop following the open job's log.
    CloseJob,
    RefreshJob(u64),
    /// Read the open job's outline again, as its steps moved on.
    Outline(u64),
    /// Read these lines of the open job's log.
    Lines {
        job: u64,
        range: LineRange,
    },
    /// Search the open job's log.
    Search {
        job: u64,
        query: LogSearchQuery,
    },
    /// Read the open job beside its baseline.
    Compare(u64),
    /// Ask to cancel or re-run a job: without a confirmation, what it would do; with one, do it.
    Act {
        job: u64,
        action: JobAction,
        query: ActionQuery,
    },
    /// Read a list of jobs; the answer carries the generation, so a later request's answer wins. The range's start
    /// is set when the request is made, from the clock.
    History {
        generation: u64,
        query: JobQuery,
        range: JobRange,
    },
    /// Read what each slot ran over the range; its answer carries the generation too.
    Timeline {
        generation: u64,
        range: JobRange,
    },
    Pause {
        /// `None` for every repository.
        repository: Option<String>,
        force: bool,
    },
    Resume {
        /// `None` for every repository.
        repository: Option<String>,
    },
    Reseed {
        profile: String,
    },
    /// Read the records of failed jobs.
    Records,
    RemoveRecords(Removal),
    /// Read the sizing advice.
    Sizing,
    /// Read the cache volumes.
    Caches,
    /// Empty a cache volume.
    Empty(String),
    /// Read the doctor's checks.
    Doctor,
    /// Read the daemon's config and check it.
    Config,
    /// Read what needs attention.
    Attention,
    /// Read the jobs started and finished in the last 24 hours; the answer carries the generation, so an older read
    /// that answers late does not undo a newer one.
    Activity {
        generation: u64,
    },
    /// Read the status again for why each queued job waits.
    Reasons,
    /// Read the insights, and what the jobs' minutes would have cost, again.
    Insights,
    /// Read the open job's latest runs: the same repository's, workflow's and job name's.
    Runs {
        job: u64,
        query: JobQuery,
    },
    /// Write a profile's new memory and slots into the daemon's config.
    Resize(Resizing),
    /// Give the terminal back and stop, as Ctrl-Z does in a shell.
    Suspend,
    Quit,
}

/// A profile's advised memory and slots, as the sizing view showed them when `a` asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resizing {
    pub(crate) profile: String,
    pub(crate) resize: Resize,
}

/// Records `d` or `D` asks to remove, waiting for `y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Removal {
    /// The record of this job.
    Job(u64),
    All,
}

/// The screen shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Overview,
    History,
    Records,
    Insights,
    Sizing,
    Caches,
    Settings,
    Job,
}

impl View {
    /// The views the TUI has, in core's order, each with core's name for it: tab `n` and key `n` open the `n`th, on
    /// its first section; the key again opens its next. A view or section added to core fails to compile here until
    /// it is drawn.
    pub(crate) fn tabs() -> Vec<(api::View, View)> {
        View::tabs_of(&api::View::ALL)
    }

    /// [`View::tabs`] for `order`, a list of core's views.
    pub(crate) fn tabs_of(order: &[api::View]) -> Vec<(api::View, View)> {
        order
            .iter()
            .filter_map(|view| Some((*view, *View::of(*view).first()?)))
            .collect()
    }

    /// The TUI's views of core's `view`, in the order of its sections.
    pub(crate) fn of(view: api::View) -> Vec<View> {
        use api::{Section, View as Core};
        let drawn = |section: &Section| match section {
            Section::History => View::History,
            Section::Records => View::Records,
            Section::Sizing => View::Sizing,
            Section::Caches => View::Caches,
            Section::Config => View::Settings,
        };
        match view {
            Core::Overview => vec![View::Overview],
            Core::Insights => vec![View::Insights],
            Core::Jobs | Core::Capacity | Core::Settings => {
                view.sections().iter().map(drawn).collect()
            }
        }
    }

    /// Core's view it is part of; none for a job's.
    pub(crate) fn group(self) -> Option<api::View> {
        use api::View as Core;
        match self {
            View::Overview => Some(Core::Overview),
            View::Insights => Some(Core::Insights),
            View::Job => None,
            section => section.section().map(api::Section::view),
        }
    }

    /// Core's name of its section, for a view that is one.
    pub(crate) fn section(self) -> Option<api::Section> {
        use api::Section;
        match self {
            View::History => Some(Section::History),
            View::Records => Some(Section::Records),
            View::Sizing => Some(Section::Sizing),
            View::Caches => Some(Section::Caches),
            View::Settings => Some(Section::Config),
            View::Overview | View::Insights | View::Job => None,
        }
    }
}

/// The overview's list that takes the selection keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    /// What needs attention.
    Attention,
    Slots,
    /// The queued jobs, each with why it waits.
    Queue,
    Repositories,
    /// The latest finished jobs.
    Latest,
}

/// Whether events arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Connection {
    Live,
    Reconnecting(String),
}

/// A one-line message in the footer, until the next key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Flash {
    pub(crate) text: String,
    pub(crate) tone: api::Tone,
}

/// What a history request does with the rows shown until its answer comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rows {
    /// They are the same page, refreshed.
    Keep,
    /// They are from other filters or another page.
    Clear,
}

/// Which way tab moves between the overview's panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// What the job's page shows under its header: its log, or it beside its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panel {
    Log,
    Compare,
}

/// The open job.
#[derive(Debug)]
pub(crate) struct JobView {
    pub(crate) job: Job,
    /// Its newest samples, oldest first; as many as the overview keeps per job.
    pub(crate) samples: VecDeque<Sample>,
    pub(crate) log: LogView,
    pub(crate) panel: Panel,
    /// It beside its baseline, once read.
    pub(crate) comparison: Reading<JobComparison>,
    /// The latest runs of the same job, oldest first, this one among them; `None` until read.
    pub(crate) runs: Option<Vec<JobSummary>>,
}

impl JobView {
    /// The request for its latest runs. Without a workflow the jobs API matches every workflow's job of that name,
    /// so the answer is filtered again by [`JobView::keep_runs`].
    fn runs_request(&self) -> Effect {
        let summary = &self.job.summary;
        Effect::Runs {
            job: summary.id,
            query: JobQuery {
                repository: Some(summary.repository.clone()),
                workflow: summary.workflow.clone(),
                name: Some(summary.name.clone()),
                limit: Some(RUNS),
                ..JobQuery::default()
            },
        }
    }

    fn keep_runs(&mut self, list: JobList) {
        let workflow = &self.job.summary.workflow;
        let mut runs: Vec<JobSummary> = list
            .page
            .jobs
            .into_iter()
            .filter(|run| &run.workflow == workflow)
            .collect();
        runs.reverse();
        self.runs = Some(runs);
    }

    /// The requests for the log's lines the rows lack.
    fn wanted(&mut self) -> Vec<Effect> {
        let job = self.job.summary.id;
        self.log
            .wants()
            .into_iter()
            .map(|range| Effect::Lines { job, range })
            .collect()
    }
}

/// How the jobs view shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lens {
    /// A table, a page at a time.
    List,
    /// What each slot ran over the range.
    Timeline,
}

/// The jobs view: the list read, what it is searched and filtered by, and the timeline of its range.
#[derive(Debug)]
pub(crate) struct History {
    pub(crate) list: Option<JobList>,
    /// The text searched for in names, workflows and branches.
    pub(crate) search: Option<String>,
    pub(crate) repository: Option<String>,
    pub(crate) workflow: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) record: Option<RecordFilter>,
    pub(crate) order: JobOrder,
    pub(crate) range: JobRange,
    pub(crate) lens: Lens,
    pub(crate) timeline: Reading<SlotTimeline>,
    /// Every workflow seen in a job, by repository, to pick from.
    workflows: BTreeSet<(String, String)>,
    /// Every branch seen in a job, to pick from.
    branches: BTreeSet<String>,
    /// The `before` of each page read before this one; empty on the first page.
    pub(crate) previous: Vec<Option<u64>>,
    pub(crate) before: Option<u64>,
    pub(crate) selected: usize,
    /// The last request's generation; answers to earlier ones are ignored.
    generation: u64,
    timeline_generation: u64,
}

impl Default for History {
    fn default() -> Self {
        History {
            list: None,
            search: None,
            repository: None,
            workflow: None,
            branch: None,
            conclusion: None,
            record: None,
            order: JobOrder::NEWEST,
            range: JobRange::default(),
            lens: Lens::List,
            timeline: Reading::default(),
            workflows: BTreeSet::new(),
            branches: BTreeSet::new(),
            previous: Vec::new(),
            before: None,
            selected: 0,
            generation: 0,
            timeline_generation: 0,
        }
    }
}

impl History {
    /// Asks for the list the filters and `before` name, and in the timeline lens the timeline of the range. With
    /// [`Rows::Clear`], the rows shown go until the answer comes, and with them their continuation for `]`.
    fn request(&mut self, rows: Rows) -> Vec<Effect> {
        self.generation += 1;
        if rows == Rows::Clear {
            self.list = None;
            self.selected = 0;
        }
        let mut effects = vec![Effect::History {
            generation: self.generation,
            query: self.query(),
            range: self.range,
        }];
        if self.lens == Lens::Timeline {
            effects.push(self.ask_timeline());
        }
        effects
    }

    fn ask_timeline(&mut self) -> Effect {
        self.timeline_generation += 1;
        Effect::Timeline {
            generation: self.timeline_generation,
            range: self.range,
        }
    }

    pub(crate) fn query(&self) -> JobQuery {
        let mut query = JobQuery {
            repository: self.repository.clone(),
            workflow: self.workflow.clone(),
            branch: self.branch.clone(),
            conclusion: self.conclusion.clone(),
            record: self.record,
            q: self.search.clone(),
            before: self.before,
            limit: Some(HISTORY_PAGE),
            ..JobQuery::default()
        };
        self.order.apply(&mut query);
        query
    }

    /// Whether any filter or search narrows the list.
    pub(crate) fn narrowed(&self) -> bool {
        self.search.is_some()
            || self.repository.is_some()
            || self.workflow.is_some()
            || self.branch.is_some()
            || self.conclusion.is_some()
            || self.record.is_some()
            || self.range != JobRange::All
    }

    /// Learns the workflows and branches of `jobs`.
    fn learn<'a>(&mut self, jobs: impl IntoIterator<Item = &'a JobSummary>) {
        for job in jobs {
            if let Some(workflow) = &job.workflow {
                self.workflows
                    .insert((job.repository.clone(), workflow.clone()));
            }
            if let Some(branch) = &job.branch {
                self.branches.insert(branch.clone());
            }
        }
    }

    /// The workflows to pick from: those of the repository filtered by, or of every repository, by name.
    fn workflows(&self) -> BTreeSet<&str> {
        self.workflows
            .iter()
            .filter(|(repository, _)| {
                self.repository
                    .as_ref()
                    .is_none_or(|filter| filter == repository)
            })
            .map(|(_, workflow)| workflow.as_str())
            .collect()
    }

    /// The jobs listed.
    pub(crate) fn jobs(&self) -> &[JobSummary] {
        self.list
            .as_ref()
            .map_or(&[], |list| list.page.jobs.as_slice())
    }
}

/// Everything the screen shows.
#[derive(Debug)]
pub(crate) struct App {
    pub(crate) live: watch::State,
    /// Recent samples of each running job, oldest first.
    pub(crate) series: BTreeMap<u64, VecDeque<Sample>>,
    /// Recent samples of the slice, oldest first.
    pub(crate) host: VecDeque<HostSample>,
    pub(crate) connection: Connection,
    pub(crate) view: View,
    /// The view `Esc` returns to from a job.
    back: View,
    pub(crate) pane: Pane,
    pub(crate) notice: usize,
    pub(crate) slot: usize,
    pub(crate) queued: usize,
    pub(crate) repository: usize,
    pub(crate) latest: usize,
    /// What needs attention, once read.
    pub(crate) attention: Reading<Attention>,
    /// The host sample time from which what needs attention is read again, unless an event says it changed first.
    attention_due: u64,
    /// The jobs started and finished in the last 24 hours, once read.
    pub(crate) activity: Option<Activity>,
    /// What the history says; `None` until read.
    pub(crate) insights: Option<Insights>,
    pub(crate) insight: usize,
    /// What the jobs' minutes would have cost on GitHub's runners; `None` until read.
    pub(crate) savings: Option<Savings>,
    /// The host sample time from which the insights are read again.
    insights_due: u64,
    /// The host sample time from which the activity is read again, unless a job starts or ends first.
    activity_due: u64,
    /// The generation of the last activity request, and of the read shown.
    activity_asked: u64,
    activity_shown: u64,
    /// The queued jobs' reasons: from which host sample time they are read again, whether a read is under way, and
    /// whether something changed since it began, so another follows it.
    reasons_due: u64,
    reasons_asked: bool,
    reasons_stale: bool,
    pub(crate) history: History,
    pub(crate) job: Option<JobView>,
    pub(crate) flash: Option<Flash>,
    /// The questions the footer asks.
    pub(crate) prompts: Prompts,
    /// What is open over the view.
    pub(crate) overlay: Overlay,
    /// The records of failed jobs, once read.
    pub(crate) records: Option<Records>,
    pub(crate) record: usize,
    /// The sizing advice, once read.
    pub(crate) sizing: Option<Sizing>,
    /// The selected profile of the sizing view.
    pub(crate) sized: usize,
    pub(crate) caches: Reading<Caches>,
    pub(crate) cache: usize,
    pub(crate) doctor: Reading<Doctor>,
    pub(crate) config: Reading<ConfigRead>,
    pub(crate) check: usize,
}

impl App {
    pub(crate) fn new(
        live: watch::State,
        activity: Option<Activity>,
        host: Vec<HostSample>,
        series: Vec<SampleSeries>,
    ) -> App {
        let mut app = App {
            live,
            series: BTreeMap::new(),
            host: VecDeque::new(),
            connection: Connection::Live,
            view: View::Overview,
            back: View::Overview,
            pane: Pane::Slots,
            notice: 0,
            slot: 0,
            queued: 0,
            repository: 0,
            latest: 0,
            attention: Reading::default(),
            attention_due: 0,
            activity_due: activity
                .as_ref()
                .map_or(0, |activity| activity.until + ACTIVITY_EVERY),
            activity_asked: 0,
            activity_shown: 0,
            reasons_due: 0,
            reasons_asked: false,
            reasons_stale: false,
            activity,
            insights: None,
            insight: 0,
            savings: None,
            insights_due: 0,
            history: History::default(),
            job: None,
            flash: None,
            prompts: Prompts::default(),
            overlay: Overlay::None,
            records: None,
            record: 0,
            sizing: None,
            sized: 0,
            caches: Reading::default(),
            cache: 0,
            doctor: Reading::default(),
            config: Reading::default(),
            check: 0,
        };
        for sample in host {
            push(&mut app.host, sample, HOST_SAMPLES);
        }
        app.history.learn(&app.live.recent);
        for series in series {
            let kept = app.series.entry(series.job).or_default();
            for sample in series.samples {
                push(kept, sample, JOB_SAMPLES);
            }
        }
        app
    }

    pub(crate) fn update(&mut self, msg: Msg) -> Vec<Effect> {
        // Rows of the queue and the latest jobs come and go above the selected one, as queued jobs start and others
        // finish: the selection follows its job, or stays on a row that is there when the job left.
        let queued = self.live.status.queued.get(self.queued).map(|job| job.id);
        let latest = self.live.recent.get(self.latest).map(|job| job.id);
        let notice = self.notices().get(self.notice).map(|item| item.key.clone());
        let effects = self.apply(msg);
        let ids: Vec<u64> = self.live.status.queued.iter().map(|job| job.id).collect();
        self.queued = follow(queued, self.queued, &ids);
        let ids: Vec<u64> = self.live.recent.iter().map(|job| job.id).collect();
        self.latest = follow(latest, self.latest, &ids);
        let keys: Vec<&str> = self
            .notices()
            .iter()
            .map(|item| item.key.as_str())
            .collect();
        self.notice = keys
            .iter()
            .position(|key| Some(*key) == notice.as_deref())
            .unwrap_or(self.notice)
            .min(keys.len().saturating_sub(1));
        if self.pane == Pane::Queue && self.live.status.queued.is_empty()
            || self.pane == Pane::Attention && self.notices().is_empty()
        {
            self.pane = Pane::Slots;
        }
        effects
    }

    /// The items that need attention, as read; none before the first read or from an older daemon.
    pub(crate) fn notices(&self) -> &[api::AttentionItem] {
        self.attention
            .get()
            .map_or(&[], |attention| attention.items.as_slice())
    }

    fn apply(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Click(target) => return self.click(target),
            Msg::Wheel(rows) => {
                if let Overlay::Pick(picker) = &mut self.overlay {
                    picker.step(rows);
                } else {
                    return self.step(rows);
                }
            }
            Msg::Resize => {}
            Msg::Event(envelope) => return self.event(&envelope),
            Msg::Disconnected(why) => self.connection = Connection::Reconnecting(why),
            Msg::Reconnected => return self.reconnected(),
            Msg::Snapshot(live) => return self.snapshot(*live),
            Msg::Job(job) => self.job_read(job),
            Msg::Samples(series) => self.samples(series),
            Msg::History { generation, list } => self.listed(generation, list),
            Msg::Timeline {
                generation,
                timeline,
            } => {
                if generation == self.history.timeline_generation {
                    self.history.timeline.take(timeline);
                }
            }
            Msg::Outline { job, outline } => {
                if let Some(open) = self.open_job(job) {
                    open.log.take_outline(outline);
                    return open.wanted();
                }
            }
            Msg::Lines { job, lines } => {
                if let Some(open) = self.open_job(job) {
                    open.log.take_lines(lines);
                }
            }
            Msg::Found { job, query, found } => return self.found(job, &query, found),
            Msg::Compared { job, comparison } => {
                if let Some(open) = self.open_job(job) {
                    open.comparison.take(comparison);
                }
            }
            Msg::Preview(preview) => self.prompts.push(Prompt::Action(preview)),
            Msg::Acted(result) => {
                let tone = result.tone();
                let id = result.job;
                self.flash(result.message, tone);
                if let Some(open) = self.open_job(id) {
                    return vec![Effect::RefreshJob(id), open.runs_request()];
                }
            }
            Msg::Status(status) => self.live.status = status,
            Msg::Records(records) => {
                self.record = self.record.min(records.records.len().saturating_sub(1));
                self.records = Some(records);
            }
            Msg::Sizing(sizing) => {
                self.sized = self.sized.min(sizing.profiles.len().saturating_sub(1));
                self.sizing = Some(sizing);
            }
            Msg::Caches(caches) => {
                self.caches.take(caches);
                let volumes = self.caches.get().map_or(0, |caches| caches.volumes.len());
                self.cache = self.cache.min(volumes.saturating_sub(1));
            }
            Msg::Doctor(doctor) => {
                self.doctor.take(doctor);
                let checks = self.doctor.get().map_or(0, |doctor| doctor.checks.len());
                self.check = self.check.min(checks.saturating_sub(1));
            }
            Msg::Config(config) => self.config.take(config),
            Msg::Attention(attention) => self.attention.take(attention),
            Msg::Activity {
                generation,
                activity,
            } => self.activity(generation, activity),
            Msg::Reasons(status) => return self.reasons(status.as_deref()),
            Msg::Insights(insights) => {
                self.insight = self.insight.min(insights.insights.len().saturating_sub(1));
                self.insights = Some(*insights);
            }
            Msg::Savings(savings) => self.savings = Some(*savings),
            Msg::Runs { job, list } => match list {
                Ok(list) => {
                    self.history.learn(&list.page.jobs);
                    if let Some(open) = self.open_job(job) {
                        open.keep_runs(list);
                    }
                }
                Err(error) => self.flash(error, api::Tone::Bad),
            },
            Msg::Done(text) => self.flash(text, api::Tone::Good),
            Msg::Failed(text) => self.flash(text, api::Tone::Bad),
            Msg::Busy(refused) => self.prompts.push(Prompt::Force(refused)),
        }
        Vec::new()
    }

    /// Back on the stream: what may have changed while it was away is read again.
    fn reconnected(&mut self) -> Vec<Effect> {
        self.connection = Connection::Live;
        let mut effects = vec![Effect::Attention];
        if let Some(open) = &self.job {
            // Its runs too: their end may have been missed with the job's.
            effects.extend([Effect::RefreshJob(open.job.summary.id), open.runs_request()]);
        }
        effects
    }

    /// The daemon's state, read whole.
    fn snapshot(&mut self, live: watch::State) -> Vec<Effect> {
        self.live = live;
        let running = &self.live.running;
        self.series.retain(|id, _| running.contains_key(id));
        self.history.learn(&self.live.recent);
        // Jobs may have started and ended while the stream was away, and a reasons read under way is older.
        if self.reasons_asked {
            self.reasons_stale = true;
        }
        vec![self.ask_activity()]
    }

    /// A page of the jobs list, unless a newer one was asked since.
    fn listed(&mut self, generation: u64, list: Result<JobList, String>) {
        if generation != self.history.generation {
            return;
        }
        match list {
            Ok(list) => {
                self.history.learn(&list.page.jobs);
                self.history.selected = self
                    .history
                    .selected
                    .min(list.page.jobs.len().saturating_sub(1));
                self.history.list = Some(list);
            }
            Err(error) => self.flash(error, api::Tone::Bad),
        }
    }

    /// The open job's samples, read whole.
    fn samples(&mut self, series: SampleSeries) {
        if let Some(open) = &mut self.job
            && open.job.summary.id == series.job
        {
            open.samples.clear();
            for sample in series.samples {
                open.job.observe(&sample);
                push(&mut open.samples, sample, JOB_SAMPLES);
            }
        }
    }

    /// A page of the open job's log search.
    fn found(&mut self, job: u64, query: &str, found: Result<LogSearch, Unread>) -> Vec<Effect> {
        let Some(open) = self.open_job(job) else {
            return Vec::new();
        };
        match found {
            Ok(page) => {
                if page.matches.is_empty()
                    && open.log.found().is_some_and(|found| found.count() == 0)
                {
                    self.flash(format!("no line holds \"{query}\""), api::Tone::Quiet);
                    return Vec::new();
                }
                open.log.take_found(query, page);
                return open.wanted();
            }
            Err(Unread::Unsupported) => {
                open.log.clear_search();
                self.flash(
                    format!("log search: {}", api::NOT_SUPPORTED),
                    api::Tone::Quiet,
                );
            }
            Err(Unread::Failed(why)) => {
                open.log.clear_search();
                self.flash(why, api::Tone::Bad);
            }
            Err(Unread::Waiting) => {}
        }
        Vec::new()
    }

    /// The open job, when it is `id`.
    fn open_job(&mut self, id: u64) -> Option<&mut JobView> {
        self.job.as_mut().filter(|open| open.job.summary.id == id)
    }

    /// A job read again: the open job's page and the running jobs take it.
    fn job_read(&mut self, job: Job) {
        if let Some(open) = &mut self.job
            && open.job.summary.id == job.summary.id
        {
            open.job.reread(job.clone());
        }
        if job.summary.state.is_final() {
            // Its `job.finished` was missed, as across an outage.
            self.live.running.remove(&job.summary.id);
            self.series.remove(&job.summary.id);
        } else {
            self.live.refresh(job);
        }
    }

    /// Takes the reasons from a status read again, unless something changed while it was read: then they may be
    /// wrong, so they are left out and read again. Only the reasons: the queue itself is kept from events, which may
    /// be newer than the read.
    fn reasons(&mut self, status: Option<&Status>) -> Vec<Effect> {
        self.reasons_asked = false;
        if std::mem::take(&mut self.reasons_stale) {
            return self.ask_reasons().into_iter().collect();
        }
        if let Some(status) = status {
            self.live.take_reasons(status);
        }
        Vec::new()
    }

    /// A job or slot changed, so every reason may be wrong (a job behind one that started, a slot that freed): none
    /// shows until a status read after the change says them again.
    fn reasons_changed(&mut self) -> Option<Effect> {
        for job in &mut self.live.status.queued {
            job.waiting = None;
        }
        self.ask_reasons()
    }

    /// Shows the day's activity, unless a newer read already answered.
    fn activity(&mut self, generation: u64, activity: Activity) {
        if generation > self.activity_shown {
            self.activity_shown = generation;
            self.activity_due = activity.until + ACTIVITY_EVERY;
            self.activity = Some(activity);
        }
    }

    /// Asks for the day's activity again.
    fn ask_activity(&mut self) -> Effect {
        self.activity_asked += 1;
        Effect::Activity {
            generation: self.activity_asked,
        }
    }

    /// Asks for the queued jobs' reasons, or, while a read is under way, for another once it answers: a burst of
    /// events makes one read at a time.
    fn ask_reasons(&mut self) -> Option<Effect> {
        if self.reasons_asked {
            self.reasons_stale = true;
            return None;
        }
        self.reasons_asked = true;
        Some(Effect::Reasons)
    }

    fn flash(&mut self, text: String, tone: api::Tone) {
        self.flash = Some(Flash { text, tone });
    }

    /// A host sample: it also paces the reads that are not pushed.
    fn host_sample(&mut self, sample: &HostSample) -> Vec<Effect> {
        let mut effects = Vec::new();
        push(&mut self.host, sample.clone(), HOST_SAMPLES);
        if sample.at >= self.activity_due {
            // Asked once until the answer moves it on, however many samples come before.
            self.activity_due = sample.at + ACTIVITY_EVERY;
            effects.push(self.ask_activity());
        }
        if sample.at >= self.insights_due {
            self.insights_due = sample.at + INSIGHTS_EVERY;
            effects.push(Effect::Insights);
        }
        if sample.at >= self.attention_due {
            self.attention_due = sample.at + ATTENTION_EVERY;
            effects.push(Effect::Attention);
        }
        if sample.at >= self.reasons_due {
            self.reasons_due = sample.at + REASONS_EVERY;
            effects.extend(self.ask_reasons());
        }
        effects
    }

    /// A profile's cache reseeded, or failing to.
    fn reseeded(&mut self, profile: &str, output: &str, error: Option<&str>) {
        let done = api::reseeded(profile, error);
        let text = match last_line(output) {
            last if error.is_none() && !last.is_empty() => format!("{done}: {last}"),
            _ => done,
        };
        let tone = if error.is_some() {
            api::Tone::Bad
        } else {
            api::Tone::Good
        };
        self.flash(text, tone);
    }

    fn event(&mut self, envelope: &EventEnvelope) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(id) = self.live.apply(envelope) {
            effects.push(Effect::RefreshJob(id));
        }
        match &envelope.event {
            Event::JobSample { job, sample } => {
                if self.live.running.contains_key(job) {
                    push(
                        self.series.entry(*job).or_default(),
                        sample.clone(),
                        JOB_SAMPLES,
                    );
                }
            }
            Event::HostSample { sample } => effects.extend(self.host_sample(sample)),
            Event::AttentionChanged { .. } => effects.push(Effect::Attention),
            Event::JobQueued { .. } | Event::SlotChanged { .. } => {
                effects.extend(self.reasons_changed());
            }
            Event::JobStarted { .. } => {
                effects.push(self.ask_activity());
                effects.extend(self.reasons_changed());
            }
            Event::JobFinished { job } => {
                self.series.remove(&job.id);
                self.history.learn([job]);
                effects.push(self.ask_activity());
                effects.extend(self.reasons_changed());
                if self.view == View::History
                    && self.history.before.is_none()
                    && self.history.order.pages()
                {
                    effects.extend(self.history.request(Rows::Keep));
                }
                if self.view == View::Sizing {
                    effects.push(Effect::Sizing);
                }
            }
            Event::CacheReseeded {
                profile,
                output,
                error,
            } => self.reseeded(profile, output, error.as_deref()),
            Event::RunnerFailed { failure } => self.flash(
                format!("{}: {}", failure.repository, failure.error),
                api::Tone::Bad,
            ),
            Event::SizingChanged { change } => {
                self.flash(
                    format!("{}: auto sizing: {}", change.profile, change.line()),
                    api::Tone::Good,
                );
                if self.view == View::Sizing {
                    effects.push(Effect::Sizing);
                }
            }
            Event::RecordCreated { .. }
            | Event::RecordDeleted { .. }
            | Event::RecordGone { .. }
                if self.view == View::Records =>
            {
                effects.push(Effect::Records);
            }
            Event::ProfileChanged { .. } | Event::ProfileRemoved { .. } => {
                // A reload can take away the selected slot; the selection stays on one that is there.
                self.slot = self.slot.min(self.slots().count().saturating_sub(1));
                if self.view == View::Sizing {
                    effects.push(Effect::Sizing);
                }
            }
            _ => {}
        }
        effects.extend(self.job_event(envelope));
        effects
    }

    /// What an event changes on the open job's page.
    fn job_event(&mut self, envelope: &EventEnvelope) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(open) = &mut self.job else {
            return effects;
        };
        let id = open.job.summary.id;
        match &envelope.event {
            Event::JobStarted { job } if job.id == id => {
                open.job.summary = job.clone();
                // The started job's record says how its runner was admitted.
                effects.push(Effect::RefreshJob(id));
            }
            Event::JobFinished { job } if job.id == id => {
                open.job.summary = job.clone();
                // The final record carries the steps' last state and the job's totals, the outline its failure.
                effects.extend([Effect::RefreshJob(id), Effect::Outline(id)]);
                effects.push(open.runs_request());
            }
            Event::JobStep { job, step } if *job == id => {
                let started = open
                    .job
                    .steps
                    .iter()
                    .find(|known| known.number == step.number)
                    .map(|known| known.started_at);
                watch::update_step(&mut open.job.steps, step);
                // A step that starts or ends moves the outline's parts.
                if started != Some(step.started_at) || step.completed_at.is_some() {
                    effects.push(Effect::Outline(id));
                }
            }
            Event::JobSample { job, sample } if *job == id => {
                open.job.observe(sample);
                push(&mut open.samples, sample.clone(), JOB_SAMPLES);
            }
            // The job's page says what became of its record.
            Event::RecordCreated { record } if record.job == id => {
                effects.push(Effect::RefreshJob(id));
            }
            Event::RecordDeleted { job, .. }
            | Event::RecordGone { job }
            | Event::RecordSkipped { job }
                if *job == id =>
            {
                effects.push(Effect::RefreshJob(id));
            }
            _ => {}
        }
        effects
    }

    fn key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            // Raw mode turns off the terminal's own signals for these keys.
            match key.code {
                KeyCode::Char('c') => return vec![Effect::Quit],
                KeyCode::Char('z') => return vec![Effect::Suspend],
                _ => {}
            }
        }
        self.flash = None;
        if let Overlay::Input(_) = self.overlay {
            return self.typed(key.code);
        }
        if let Some(effects) = self.answer(key.code) {
            return effects;
        }
        if let Some(effects) = self.overlay_key(key.code) {
            return effects;
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            KeyCode::Char('o') => return self.show(View::Overview),
            KeyCode::Char('h') => return self.show(View::History),
            KeyCode::Char('!') => return self.show_attention(),
            KeyCode::Char(digit @ '1'..='9') => {
                let tab = usize::from(u8::try_from(digit).unwrap_or(b'1') - b'1');
                return self.tab(tab);
            }
            KeyCode::Esc => return self.escape(),
            KeyCode::Tab => self.next_pane(Direction::Forward),
            KeyCode::BackTab => self.next_pane(Direction::Backward),
            KeyCode::Down | KeyCode::Char('j') => return self.step(1),
            KeyCode::Up | KeyCode::Char('k') => return self.step(-1),
            KeyCode::PageDown => return self.step(10),
            KeyCode::PageUp => return self.step(-10),
            KeyCode::Enter if self.view != View::Job => return self.enter(),
            KeyCode::Char('p') => {
                if let Some(repository) = self.selected_repository() {
                    return vec![Effect::Pause {
                        repository: Some(repository),
                        force: false,
                    }];
                }
            }
            KeyCode::Char('r') => {
                if let Some(repository) = self.selected_repository() {
                    return vec![Effect::Resume {
                        repository: Some(repository),
                    }];
                }
            }
            KeyCode::Char('P') => {
                return vec![Effect::Pause {
                    repository: None,
                    force: false,
                }];
            }
            KeyCode::Char('U') => return vec![Effect::Resume { repository: None }],
            KeyCode::Char('R') => {
                if let Some(profile) = self.selected_profile() {
                    return vec![Effect::Reseed { profile }];
                }
            }
            code if self.view == View::Job => return self.job_key(code),
            code if self.view == View::History => return self.history_key(code),
            code if self.view == View::Records => self.records_key(code),
            KeyCode::Char('a') if self.view == View::Sizing => {
                if let Some(resizing) = self.selected_resize() {
                    self.prompts.push(Prompt::Resize(resizing));
                }
            }
            KeyCode::Char('d') if self.view == View::Caches => {
                if let Some(volume) = self
                    .caches
                    .get()
                    .and_then(|caches| caches.volumes.get(self.cache))
                {
                    self.prompts.push(Prompt::Empty(volume.name.clone()));
                }
            }
            KeyCode::Char('u') if self.view == View::Settings => {
                return vec![Effect::Doctor, Effect::Config];
            }
            _ => {}
        }
        Vec::new()
    }

    /// A key while help or a picker is over the view: `None` when it goes on to the view.
    fn overlay_key(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        match std::mem::take(&mut self.overlay) {
            Overlay::None => {}
            Overlay::Help => {
                return Some(match code {
                    KeyCode::Char('q') => vec![Effect::Quit],
                    _ => Vec::new(),
                });
            }
            Overlay::Pick(mut picker) => {
                match code {
                    KeyCode::Down | KeyCode::Char('j') => picker.step(1),
                    KeyCode::Up | KeyCode::Char('k') => picker.step(-1),
                    KeyCode::Enter | KeyCode::Char(' ') => return Some(self.choose(&picker)),
                    KeyCode::Char('q') => return Some(vec![Effect::Quit]),
                    // Any other key closes it.
                    _ => return Some(Vec::new()),
                }
                self.overlay = Overlay::Pick(picker);
                return Some(Vec::new());
            }
            Overlay::Input(input) => self.overlay = Overlay::Input(input),
        }
        None
    }

    /// Tab `tab`, from 0: its view, or again on it, its next section.
    fn tab(&mut self, tab: usize) -> Vec<Effect> {
        let Some((group, first)) = View::tabs().get(tab).copied() else {
            return Vec::new();
        };
        let sections = View::of(group);
        let next = sections
            .iter()
            .position(|view| *view == self.view)
            .and_then(|at| sections.get((at + 1) % sections.len()))
            .copied()
            .unwrap_or(first);
        self.show(next)
    }

    /// The overview with what needs attention selected.
    fn show_attention(&mut self) -> Vec<Effect> {
        let effects = self.show(View::Overview);
        if !self.notices().is_empty() {
            self.pane = Pane::Attention;
        }
        effects
    }

    /// `Esc`: out of a search, a comparison or a job; on the jobs view, its search goes.
    fn escape(&mut self) -> Vec<Effect> {
        match self.view {
            View::Job => {
                if let Some(open) = &mut self.job {
                    if open.panel == Panel::Compare {
                        open.panel = Panel::Log;
                        return Vec::new();
                    }
                    if open.log.found().is_some() {
                        open.log.clear_search();
                        return Vec::new();
                    }
                }
                self.show(self.back)
            }
            View::History if self.history.search.is_some() => {
                self.history.search = None;
                self.first_page()
            }
            _ => Vec::new(),
        }
    }

    /// The keys of a job's page: its log's search, the comparison, and cancel and re-run.
    fn job_key(&mut self, code: KeyCode) -> Vec<Effect> {
        let offered = self.live.status.job_actions;
        let Some(open) = &mut self.job else {
            return Vec::new();
        };
        let id = open.job.summary.id;
        match code {
            KeyCode::Char('G') | KeyCode::End => open.log.end(),
            KeyCode::Char('g') | KeyCode::Home => {
                open.log.start();
                return open.wanted();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                open.log.toggle();
                return open.wanted();
            }
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input(Input {
                    typing: Typing::Log,
                    text: String::new(),
                });
            }
            KeyCode::Char('n') => {
                open.log.next_match(Way::Forward);
                let mut effects = open.wanted();
                effects.extend(self.more_matches());
                return effects;
            }
            KeyCode::Char('N') => {
                open.log.next_match(Way::Backward);
                return open.wanted();
            }
            KeyCode::Char('c') => {
                open.panel = match open.panel {
                    Panel::Log => Panel::Compare,
                    Panel::Compare => Panel::Log,
                };
                if open.panel == Panel::Compare && open.comparison.get().is_none() {
                    return vec![Effect::Compare(id)];
                }
            }
            KeyCode::Char(key @ ('x' | 'a' | 'A')) => {
                let action = match key {
                    'x' => JobAction::Cancel,
                    'a' => JobAction::Rerun,
                    _ => JobAction::RerunFailed,
                };
                if let Some(why) = offered.why_not() {
                    self.flash(why.to_owned(), api::Tone::Quiet);
                    return Vec::new();
                }
                return vec![Effect::Act {
                    job: id,
                    action,
                    query: ActionQuery::default(),
                }];
            }
            _ => {}
        }
        Vec::new()
    }

    /// The next page of the log search when the match shown is its last read.
    fn more_matches(&self) -> Option<Effect> {
        let open = self.job.as_ref()?;
        let found = open.log.found()?;
        let next = found.next_page()?;
        Some(Effect::Search {
            job: open.job.summary.id,
            query: search_query(found.query(), Some(next)),
        })
    }

    /// A key while a line is typed: text goes in, `Enter` searches, `Esc` drops it.
    fn typed(&mut self, code: KeyCode) -> Vec<Effect> {
        let Overlay::Input(input) = &mut self.overlay else {
            return Vec::new();
        };
        match code {
            KeyCode::Char(character) => {
                input.text.push(character);
                return Vec::new();
            }
            KeyCode::Backspace => {
                input.text.pop();
                return Vec::new();
            }
            KeyCode::Enter => {}
            _ => {
                self.overlay = Overlay::None;
                return Vec::new();
            }
        }
        let Overlay::Input(input) = std::mem::take(&mut self.overlay) else {
            return Vec::new();
        };
        let text = input.text.trim().to_owned();
        match input.typing {
            Typing::Jobs => {
                self.history.search = (!text.is_empty()).then_some(text);
                self.first_page()
            }
            Typing::Log => {
                let Some(open) = &mut self.job else {
                    return Vec::new();
                };
                if text.is_empty() {
                    open.log.clear_search();
                    return Vec::new();
                }
                open.log.search(text.clone());
                vec![Effect::Search {
                    job: open.job.summary.id,
                    query: search_query(&text, None),
                }]
            }
        }
    }

    /// The answer to a question in the footer, which any key ends: `y` does what it asked. `None` when none was asked.
    fn answer(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        let yes = code == KeyCode::Char('y');
        let effect = match self.prompts.answer()? {
            Prompt::Force(refused) => Effect::Pause {
                repository: refused.repository,
                force: true,
            },
            Prompt::Remove(removal) => Effect::RemoveRecords(removal),
            Prompt::Resize(resizing) => Effect::Resize(resizing),
            Prompt::Empty(volume) => Effect::Empty(volume),
            Prompt::Action(preview) => {
                // A preview without a confirmation could not read the run: nothing to send.
                let Some(confirm) = preview.confirm.clone() else {
                    return Some(Vec::new());
                };
                Effect::Act {
                    job: preview.job,
                    action: preview.action,
                    query: ActionQuery {
                        force: true,
                        confirm: Some(confirm),
                        failed: preview.action == JobAction::RerunFailed,
                    },
                }
            }
        };
        Some(if yes { vec![effect] } else { Vec::new() })
    }

    /// The selected profile's advised memory and slots, when they differ from its own.
    fn selected_resize(&self) -> Option<Resizing> {
        let profile = self.sizing.as_ref()?.profiles.get(self.sized)?;
        Some(Resizing {
            profile: profile.name.clone(),
            resize: profile.advice.change()?,
        })
    }

    /// The keys only the records take: `d` and `D` ask to remove them.
    fn records_key(&mut self, code: KeyCode) {
        let any = self.records.as_ref().is_some_and(|r| !r.records.is_empty());
        match code {
            KeyCode::Char('d') => {
                if let Some(job) = self.selected_job() {
                    self.prompts.push(Prompt::Remove(Removal::Job(job)));
                }
            }
            KeyCode::Char('D') if any => self.prompts.push(Prompt::Remove(Removal::All)),
            _ => {}
        }
    }

    /// The keys only the jobs view takes: its search, filters, order, range, lens and pages.
    fn history_key(&mut self, code: KeyCode) -> Vec<Effect> {
        match code {
            KeyCode::Char('/') => {
                self.overlay = Overlay::Input(Input {
                    typing: Typing::Jobs,
                    text: self.history.search.clone().unwrap_or_default(),
                });
            }
            KeyCode::Char('f') => self.pick(Pick::Filters),
            KeyCode::Char('s') => self.pick(Pick::Order),
            KeyCode::Char('t') => self.pick(Pick::Range),
            KeyCode::Char('x') if self.history.narrowed() => {
                let history = &mut self.history;
                history.search = None;
                history.repository = None;
                history.workflow = None;
                history.branch = None;
                history.conclusion = None;
                history.record = None;
                history.range = JobRange::All;
                return self.first_page();
            }
            KeyCode::Char('l') => {
                self.history.lens = match self.history.lens {
                    Lens::List => Lens::Timeline,
                    Lens::Timeline => Lens::List,
                };
                if self.history.lens == Lens::Timeline {
                    return vec![self.history.ask_timeline()];
                }
            }
            KeyCode::Char(']') => {
                if let Some(next) = self.history.list.as_ref().and_then(|list| list.page.next) {
                    self.history.previous.push(self.history.before);
                    self.history.before = Some(next);
                    return self.history.request(Rows::Clear);
                }
            }
            KeyCode::Char('[') => {
                if let Some(before) = self.history.previous.pop() {
                    self.history.before = before;
                    return self.history.request(Rows::Clear);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Opens the list of `pick`'s options, on the one set now.
    fn pick(&mut self, pick: Pick) {
        let history = &self.history;
        let any = |set: &Option<String>| set.clone().unwrap_or_else(|| "any".to_owned());
        let texts = |values: Vec<String>, set: &Option<String>| {
            let mut choices = vec![Choice::of("any", Value::Text(None))];
            choices.extend(
                values
                    .into_iter()
                    .map(|value| Choice::of(value.clone(), Value::Text(Some(value)))),
            );
            (choices, Value::Text(set.clone()))
        };
        let (choices, current) = match pick {
            Pick::Filters => {
                use api::Column;
                let filter = |column: Column, pick: Pick, note: String| Choice {
                    note,
                    ..Choice::of(column.title(), Value::Filter(pick))
                };
                let record = history.record.map_or("any", RecordFilter::words);
                let choices = vec![
                    filter(Column::Outcome, Pick::Outcome, any(&history.conclusion)),
                    filter(
                        Column::Repository,
                        Pick::Repository,
                        any(&history.repository),
                    ),
                    filter(Column::Workflow, Pick::Workflow, any(&history.workflow)),
                    filter(Column::Branch, Pick::Branch, any(&history.branch)),
                    filter(Column::Record, Pick::Record, record.to_owned()),
                    filter(
                        Column::Queued,
                        Pick::Range,
                        history.range.words().to_owned(),
                    ),
                ];
                (choices, Value::Filter(Pick::Outcome))
            }
            Pick::Outcome => texts(
                CONCLUSIONS
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
                &history.conclusion,
            ),
            Pick::Repository => texts(
                self.live
                    .status
                    .repositories
                    .iter()
                    .map(|repository| repository.name.clone())
                    .collect(),
                &history.repository,
            ),
            Pick::Workflow => texts(
                history.workflows().into_iter().map(str::to_owned).collect(),
                &history.workflow,
            ),
            Pick::Branch => texts(history.branches.iter().cloned().collect(), &history.branch),
            Pick::Record => {
                let mut choices = vec![Choice::of("any", Value::Record(None))];
                choices.extend(
                    RecordFilter::ALL
                        .into_iter()
                        .map(|filter| Choice::of(filter.words(), Value::Record(Some(filter)))),
                );
                (choices, Value::Record(history.record))
            }
            Pick::Range => (
                JobRange::ALL
                    .into_iter()
                    .map(|range| Choice::of(range.words(), Value::Range(range)))
                    .collect(),
                Value::Range(history.range),
            ),
            Pick::Order => (
                JobOrder::ALL
                    .into_iter()
                    .map(|order| Choice::of(order.words(), Value::Order(order)))
                    .collect(),
                Value::Order(history.order),
            ),
        };
        self.overlay = Overlay::Pick(Picker::new(pick, choices, &current));
    }

    /// Does what the option the list is on says: opens a filter's list, or sets it and reads the first page.
    fn choose(&mut self, picker: &Picker) -> Vec<Effect> {
        let Some(choice) = picker.chosen() else {
            return Vec::new();
        };
        let history = &mut self.history;
        match (&choice.value, picker.pick) {
            (Value::Filter(pick), _) => {
                self.pick(*pick);
                return Vec::new();
            }
            (Value::Text(value), Pick::Outcome) => history.conclusion.clone_from(value),
            (Value::Text(value), Pick::Repository) => {
                history.repository.clone_from(value);
                // A workflow the repository has not run would show nothing.
                if let Some(workflow) = history.workflow.take()
                    && history.workflows().contains(workflow.as_str())
                {
                    history.workflow = Some(workflow);
                }
            }
            (Value::Text(value), Pick::Workflow) => history.workflow.clone_from(value),
            (Value::Text(value), Pick::Branch) => history.branch.clone_from(value),
            (Value::Record(record), _) => history.record = *record,
            (Value::Range(range), _) => history.range = *range,
            (Value::Order(order), _) => history.order = *order,
            (Value::Text(_), Pick::Filters | Pick::Record | Pick::Range | Pick::Order) => {}
        }
        self.first_page()
    }

    fn show(&mut self, view: View) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.view == View::Job && view != View::Job {
            self.job = None;
            effects.push(Effect::CloseJob);
        }
        if view != self.view {
            match view {
                View::History => effects.extend(self.history.request(Rows::Keep)),
                View::Records => effects.push(Effect::Records),
                View::Sizing => effects.push(Effect::Sizing),
                View::Caches => effects.push(Effect::Caches),
                View::Settings => effects.extend([Effect::Doctor, Effect::Config]),
                View::Insights if self.insights.is_none() => effects.push(Effect::Insights),
                View::Overview | View::Insights | View::Job => {}
            }
        }
        self.view = view;
        effects
    }

    fn first_page(&mut self) -> Vec<Effect> {
        self.history.previous.clear();
        self.history.before = None;
        self.history.request(Rows::Clear)
    }

    /// `Enter`: opens the selected job, or where the selected attention item points.
    fn enter(&mut self) -> Vec<Effect> {
        if self.view == View::Overview && self.pane == Pane::Attention {
            return self.follow_notice();
        }
        match self.selected_job() {
            Some(id) => self.open(id),
            None => Vec::new(),
        }
    }

    /// Goes where the selected attention item points: its job, repository, check or insight, or the view its fix
    /// names.
    fn follow_notice(&mut self) -> Vec<Effect> {
        let Some(item) = self.notices().get(self.notice).cloned() else {
            return Vec::new();
        };
        match &item.subject {
            Subject::Job {
                id,
                repository,
                name,
            } => {
                let effects = self.open(*id);
                if !effects.is_empty() {
                    return effects;
                }
                return self.open_summary(stub_summary(*id, repository, name));
            }
            Subject::Repository { name } => {
                if let Some(at) = self
                    .live
                    .status
                    .repositories
                    .iter()
                    .position(|repository| &repository.name == name)
                {
                    self.repository = at;
                    self.pane = Pane::Repositories;
                    return Vec::new();
                }
            }
            Subject::Check { name } => {
                let effects = self.show(View::Settings);
                if let Some(at) = self
                    .doctor
                    .get()
                    .and_then(|doctor| doctor.checks.iter().position(|check| &check.name == name))
                {
                    self.check = at;
                }
                return effects;
            }
            Subject::Insight { key } => {
                let effects = self.show(View::Insights);
                if let Some(at) = self.insights.as_ref().and_then(|insights| {
                    insights
                        .insights
                        .iter()
                        .position(|insight| &insight.key == key)
                }) {
                    self.insight = at;
                }
                return effects;
            }
            Subject::Host | Subject::Github | Subject::Unknown => {}
        }
        match item.fix {
            Some(Fix::Open { view }) => View::of(view)
                .first()
                .copied()
                .map(|view| self.show(view))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// A click: on a tab, its view; on the header's count, what needs attention; on a row, it is selected, and on
    /// the row already selected, opened, as `Enter` does.
    fn click(&mut self, target: Target) -> Vec<Effect> {
        self.flash = None;
        if let Overlay::Pick(picker) = &mut self.overlay {
            if let Target::Choice(at) = target {
                picker.point(at);
                let picker = picker.clone();
                self.overlay = Overlay::None;
                return self.choose(&picker);
            }
            self.overlay = Overlay::None;
            return Vec::new();
        }
        if self.overlay != Overlay::None {
            self.overlay = Overlay::None;
            return Vec::new();
        }
        let (list, row) = match target {
            Target::Tab(tab) => return self.tab(tab),
            Target::Attention => return self.show_attention(),
            Target::Choice(_) => return Vec::new(),
            Target::Row(list, row) => (list, row),
        };
        if list == List::Log {
            let Some(open) = &mut self.job else {
                return Vec::new();
            };
            if open.log.cursor() == Some(row) {
                open.log.toggle();
            } else {
                open.log.point(row);
            }
            return open.wanted();
        }
        let pane = match list {
            List::Attention => Some(Pane::Attention),
            List::Slots => Some(Pane::Slots),
            List::Queue => Some(Pane::Queue),
            List::Repositories => Some(Pane::Repositories),
            List::Latest => Some(Pane::Latest),
            _ => None,
        };
        let same_pane = pane.is_none_or(|pane| pane == self.pane);
        if let Some(pane) = pane {
            self.pane = pane;
        }
        let selected = match list {
            List::Attention => &mut self.notice,
            List::Slots => &mut self.slot,
            List::Queue => &mut self.queued,
            List::Repositories => &mut self.repository,
            List::Latest => &mut self.latest,
            List::History => &mut self.history.selected,
            List::Records => &mut self.record,
            List::Insights => &mut self.insight,
            List::Sizing => &mut self.sized,
            List::Caches => &mut self.cache,
            List::Checks => &mut self.check,
            List::Log => return Vec::new(),
        };
        let again = same_pane && *selected == row;
        *selected = row;
        if again {
            return self.enter();
        }
        Vec::new()
    }

    /// Opens a job; its record, samples, outline and log arrive as messages.
    fn open(&mut self, id: u64) -> Vec<Effect> {
        let summary = self
            .live
            .running
            .get(&id)
            .map(|job| job.summary.clone())
            .or_else(|| self.jobs_listed().find(|job| job.id == id).cloned())
            .or_else(|| {
                let queued = self.live.status.queued.iter().find(|job| job.id == id)?;
                Some(queued_summary(queued))
            });
        match summary {
            Some(summary) => self.open_summary(summary),
            None => Vec::new(),
        }
    }

    fn open_summary(&mut self, summary: JobSummary) -> Vec<Effect> {
        let id = summary.id;
        let mut effects = Vec::new();
        if self.job.is_some() {
            effects.push(Effect::CloseJob);
        }
        let steps = self
            .live
            .running
            .get(&id)
            .map(|job| job.steps.clone())
            .unwrap_or_default();
        let mut open = JobView {
            job: Job {
                admission: None,
                summary,
                steps,
                record: None,
            },
            samples: self.series.get(&id).cloned().unwrap_or_default(),
            log: LogView::default(),
            panel: Panel::Log,
            comparison: Reading::default(),
            runs: None,
        };
        for sample in &open.samples {
            open.job.observe(sample);
        }
        if self.view != View::Job {
            self.back = self.view;
        }
        self.view = View::Job;
        effects.push(Effect::OpenJob(id));
        effects.push(open.runs_request());
        self.job = Some(open);
        effects
    }

    /// Jobs on screen outside the slots: the latest ones and the jobs listed.
    fn jobs_listed(&self) -> impl Iterator<Item = &JobSummary> {
        self.live.recent.iter().chain(self.history.jobs().iter())
    }

    fn step(&mut self, by: isize) -> Vec<Effect> {
        let slots = self.slots().count();
        let notices = self.notices().len();
        let listed = self.history.jobs().len();
        let (selected, len) = match (self.view, self.pane) {
            (View::Job, _) => {
                if let Some(open) = &mut self.job {
                    open.log.step(by);
                    return open.wanted();
                }
                return Vec::new();
            }
            (View::History, _) => (&mut self.history.selected, listed),
            (View::Records, _) => (
                &mut self.record,
                self.records.as_ref().map_or(0, |r| r.records.len()),
            ),
            (View::Sizing, _) => (
                &mut self.sized,
                self.sizing.as_ref().map_or(0, |s| s.profiles.len()),
            ),
            (View::Insights, _) => (
                &mut self.insight,
                self.insights.as_ref().map_or(0, |i| i.insights.len()),
            ),
            (View::Caches, _) => (
                &mut self.cache,
                self.caches.get().map_or(0, |c| c.volumes.len()),
            ),
            (View::Settings, _) => (
                &mut self.check,
                self.doctor.get().map_or(0, |d| d.checks.len()),
            ),
            (View::Overview, Pane::Attention) => (&mut self.notice, notices),
            (View::Overview, Pane::Slots) => (&mut self.slot, slots),
            (View::Overview, Pane::Queue) => (&mut self.queued, self.live.status.queued.len()),
            (View::Overview, Pane::Repositories) => {
                (&mut self.repository, self.live.status.repositories.len())
            }
            (View::Overview, Pane::Latest) => (&mut self.latest, self.live.recent.len()),
        };
        *selected = selected
            .saturating_add_signed(by)
            .min(len.saturating_sub(1));
        Vec::new()
    }

    /// Every slot in config order, with its profile's position.
    pub(crate) fn slots(
        &self,
    ) -> impl Iterator<Item = (usize, &api::ProfileStatus, &api::SlotStatus)> {
        self.live
            .status
            .profiles
            .iter()
            .enumerate()
            .flat_map(|(index, profile)| {
                profile.slots.iter().map(move |slot| (index, profile, slot))
            })
    }

    /// The job `Enter` opens.
    fn selected_job(&self) -> Option<u64> {
        match (self.view, self.pane) {
            (View::History, _) => self
                .history
                .jobs()
                .get(self.history.selected)
                .map(|job| job.id),
            (View::Records, _) => self
                .records
                .as_ref()
                .and_then(|records| records.records.get(self.record))
                .map(|record| record.job),
            (View::Overview, Pane::Slots) => {
                let (_, profile, slot) = self.slots().nth(self.slot)?;
                self.live
                    .job_in(&profile.name, slot.slot)
                    .map(|job| job.summary.id)
            }
            (View::Overview, Pane::Queue) => {
                self.live.status.queued.get(self.queued).map(|job| job.id)
            }
            (View::Overview, Pane::Latest) => self.live.recent.get(self.latest).map(|job| job.id),
            _ => None,
        }
    }

    /// The repository `p` and `r` act on.
    fn selected_repository(&self) -> Option<String> {
        match (self.view, self.pane) {
            (View::Job, _) => self
                .job
                .as_ref()
                .map(|open| open.job.summary.repository.clone()),
            (
                View::History
                | View::Records
                | View::Sizing
                | View::Insights
                | View::Caches
                | View::Settings,
                _,
            )
            | (View::Overview, Pane::Attention) => None,
            (View::Overview, Pane::Repositories) => self
                .live
                .status
                .repositories
                .get(self.repository)
                .map(|repository| repository.name.clone()),
            (View::Overview, Pane::Slots) => {
                let (_, _, slot) = self.slots().nth(self.slot)?;
                slot.runner.as_ref().map(|runner| runner.repository.clone())
            }
            (View::Overview, Pane::Queue) => self
                .live
                .status
                .queued
                .get(self.queued)
                .map(|job| job.repository.clone()),
            (View::Overview, Pane::Latest) => self
                .live
                .recent
                .get(self.latest)
                .map(|job| job.repository.clone()),
        }
    }

    /// The profile `R` reseeds.
    fn selected_profile(&self) -> Option<String> {
        match (self.view, self.pane) {
            (View::Job, _) => self
                .job
                .as_ref()
                .and_then(|open| open.job.summary.runner.as_ref())
                .map(|runner| runner.profile.clone()),
            (View::Overview, Pane::Slots) => self
                .slots()
                .nth(self.slot)
                .map(|(_, profile, _)| profile.name.clone()),
            _ => None,
        }
    }

    /// Moves to the next of the overview's panes, in the order they are drawn, past an empty one.
    fn next_pane(&mut self, direction: Direction) {
        const ORDER: [Pane; 5] = [
            Pane::Attention,
            Pane::Slots,
            Pane::Queue,
            Pane::Repositories,
            Pane::Latest,
        ];
        let empty = |pane: Pane| match pane {
            Pane::Attention => self.notices().is_empty(),
            Pane::Queue => self.live.status.queued.is_empty(),
            Pane::Slots | Pane::Repositories | Pane::Latest => false,
        };
        let at = ORDER
            .iter()
            .position(|pane| *pane == self.pane)
            .unwrap_or(0);
        let step = match direction {
            Direction::Forward => 1,
            Direction::Backward => ORDER.len() - 1,
        };
        let mut next = (at + step) % ORDER.len();
        while empty(ORDER[next]) {
            next = (next + step) % ORDER.len();
        }
        self.pane = ORDER[next];
    }

    /// The profile's position in config order, which picks its colour.
    pub(crate) fn profile_index(&self, profile: &str) -> Option<usize> {
        self.live
            .status
            .profiles
            .iter()
            .position(|known| known.name == profile)
    }
}

/// A search of a log's messages for `text`, from line `from`.
fn search_query(text: &str, from: Option<u64>) -> LogSearchQuery {
    LogSearchQuery {
        q: text.to_owned(),
        from,
        limit: Some(SEARCH_PAGE),
        context: Some(0),
        follow: false,
    }
}

fn push<T>(queue: &mut VecDeque<T>, item: T, keep: usize) {
    queue.push_back(item);
    while queue.len() > keep {
        queue.pop_front();
    }
}

/// Where a selection goes when the rows change: to `selected`'s row while it is there, else the same row, kept
/// within the rows.
fn follow(selected: Option<u64>, row: usize, rows: &[u64]) -> usize {
    selected
        .and_then(|id| rows.iter().position(|candidate| *candidate == id))
        .unwrap_or(row)
        .min(rows.len().saturating_sub(1))
}

fn last_line(text: &str) -> &str {
    text.trim().lines().last().unwrap_or_default()
}
