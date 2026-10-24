//! The TUI's state as a pure reducer: `update` takes a key, an event or an answer from the daemon and returns the
//! requests to make. It decides what is shown and asked for; making requests is `tui`'s, drawing is `draw`'s, and
//! the daemon's state comes from `watch::State`, the reducer `runnerctl watch` uses.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use runnerd_core::api::{
    Activity, Event, EventEnvelope, HostSample, Job, JobMetrics, JobPage, JobQuery, JobState,
    JobSummary, LogSummary, QueuedJob, Records, Resize, Sample, SampleSeries, Sizing, Status,
    Timeline,
};

use crate::watch;

/// Samples kept per running job: ten minutes at the daemon's two seconds.
const JOB_SAMPLES: usize = 300;
/// Host samples kept: thirty minutes.
const HOST_SAMPLES: usize = 900;
/// Log lines kept for the open job; older ones are dropped.
pub(crate) const LOG_LINES: usize = 5000;
/// Jobs per history page.
pub(crate) const HISTORY_PAGE: u32 = 100;
/// The conclusions the history filter cycles through, after "any".
const CONCLUSIONS: [&str; 3] = ["failure", "success", "cancelled"];
/// The most runs of the open job's own the job view compares it with, as the dashboard does.
pub(crate) const RUNS: u32 = 30;
/// How often the day's activity is read again while nothing starts or ends, so its 24 hours move on.
const ACTIVITY_EVERY: u64 = 60;
/// How often the queued jobs' reasons are read again while no job or slot changes: no event carries them (ADR 0044),
/// so the dashboard reads the status this often too.
const REASONS_EVERY: u64 = 10;

/// What happened: a key, something from the daemon, or the answer to a request.
#[derive(Debug)]
pub(crate) enum Msg {
    Key(KeyEvent),
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
        page: Result<JobPage, String>,
    },
    LogLine {
        job: u64,
        text: String,
    },
    Status(Status),
    Records(Records),
    Sizing(Sizing),
    /// The jobs started and finished in the last 24 hours, read by the request of this generation.
    Activity {
        generation: u64,
        activity: Activity,
    },
    /// The status read again for why each queued job waits; `None` when it could not be read.
    Reasons(Option<Box<Status>>),
    /// The answer to the open job's request for its latest runs.
    Runs {
        job: u64,
        page: Result<JobPage, String>,
    },
    /// A request succeeded; the text says what it did.
    Done(String),
    /// A request failed.
    Failed(String),
    /// A pause was refused because the repository, or one of them in a pause of all, runs a job.
    Busy(Refused),
}

/// What the job's page shows of a queued job until its record arrives.
fn queued_summary(job: &QueuedJob) -> JobSummary {
    JobSummary {
        id: job.id,
        repository: job.repository.clone(),
        run_id: job.run_id,
        run_attempt: job.run_attempt,
        workflow: job.workflow.clone(),
        name: job.name.clone(),
        branch: job.branch.clone(),
        url: job.url.clone(),
        labels: Vec::new(),
        state: JobState::Queued,
        conclusion: None,
        runner: None,
        timeline: Timeline {
            queued_at: job.queued_at,
            ..Timeline::default()
        },
        metrics: JobMetrics::default(),
        log: LogSummary::default(),
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
    /// Read the job, its samples and its log, following the log while the job runs.
    OpenJob(u64),
    /// Stop following the open job's log.
    CloseJob,
    RefreshJob(u64),
    /// Read a history page; the answer carries the generation, so a later request's answer wins.
    History {
        generation: u64,
        query: JobQuery,
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
    /// Read the jobs started and finished in the last 24 hours; the answer carries the generation, so an older read
    /// that answers late does not undo a newer one.
    Activity {
        generation: u64,
    },
    /// Read the status again for why each queued job waits.
    Reasons,
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
    Sizing,
    Job,
}

/// The overview's list that takes the selection keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
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
    pub(crate) outcome: Outcome,
}

/// Whether a flash reports something done or something that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    Failed,
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

/// The open job.
#[derive(Debug)]
pub(crate) struct JobView {
    pub(crate) job: Job,
    /// Its newest samples, oldest first; as many as the overview keeps per job.
    pub(crate) samples: VecDeque<Sample>,
    pub(crate) log: VecDeque<String>,
    /// Lines scrolled up from the end; 0 follows the log.
    pub(crate) scroll: usize,
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

    fn keep_runs(&mut self, page: JobPage) {
        let workflow = &self.job.summary.workflow;
        let mut runs: Vec<JobSummary> = page
            .jobs
            .into_iter()
            .filter(|run| &run.workflow == workflow)
            .collect();
        runs.reverse();
        self.runs = Some(runs);
    }
}

/// The history page and its filters.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub(crate) page: Option<JobPage>,
    pub(crate) repository: Option<String>,
    pub(crate) workflow: Option<String>,
    pub(crate) conclusion: Option<String>,
    /// Every workflow seen in a job, by repository, which `w` cycles through.
    workflows: BTreeSet<(String, String)>,
    /// The `before` of each page read before this one; empty on the first page.
    pub(crate) previous: Vec<Option<u64>>,
    pub(crate) before: Option<u64>,
    pub(crate) selected: usize,
    /// The last request's generation; answers to earlier ones are ignored.
    generation: u64,
}

impl History {
    /// Asks for the page the filters and `before` name. With [`Rows::Clear`], the rows shown go until the answer
    /// comes, and with them their continuation for `]`.
    fn request(&mut self, rows: Rows) -> Effect {
        self.generation += 1;
        if rows == Rows::Clear {
            self.page = None;
            self.selected = 0;
        }
        Effect::History {
            generation: self.generation,
            query: self.query(),
        }
    }

    fn query(&self) -> JobQuery {
        JobQuery {
            repository: self.repository.clone(),
            workflow: self.workflow.clone(),
            conclusion: self.conclusion.clone(),
            before: self.before,
            limit: Some(HISTORY_PAGE),
            ..JobQuery::default()
        }
    }

    /// Learns the workflows of `jobs`.
    fn learn<'a>(&mut self, jobs: impl IntoIterator<Item = &'a JobSummary>) {
        for job in jobs {
            if let Some(workflow) = &job.workflow {
                self.workflows
                    .insert((job.repository.clone(), workflow.clone()));
            }
        }
    }

    /// The workflows `w` cycles through: those of the repository filtered by, or of every repository, by name.
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
    pub(crate) slot: usize,
    pub(crate) queued: usize,
    pub(crate) repository: usize,
    pub(crate) latest: usize,
    /// The jobs started and finished in the last 24 hours, once read.
    pub(crate) activity: Option<Activity>,
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
    pub(crate) confirm: Option<Refused>,
    /// The records of failed jobs, once read.
    pub(crate) records: Option<Records>,
    pub(crate) record: usize,
    pub(crate) removal: Option<Removal>,
    /// The sizing advice, once read.
    pub(crate) sizing: Option<Sizing>,
    /// The selected profile of the sizing view.
    pub(crate) sized: usize,
    /// The change `a` asks to write, waiting for `y`.
    pub(crate) resize: Option<Resizing>,
    pub(crate) help: bool,
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
            slot: 0,
            queued: 0,
            repository: 0,
            latest: 0,
            activity_due: activity
                .as_ref()
                .map_or(0, |activity| activity.until + ACTIVITY_EVERY),
            activity_asked: 0,
            activity_shown: 0,
            reasons_due: 0,
            reasons_asked: false,
            reasons_stale: false,
            activity,
            history: History::default(),
            job: None,
            flash: None,
            confirm: None,
            records: None,
            record: 0,
            removal: None,
            sizing: None,
            sized: 0,
            resize: None,
            help: false,
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
        if let Msg::Key(key) = msg {
            return self.key(key);
        }
        // Rows of the queue and the latest jobs come and go above the selected one, as queued jobs start and others
        // finish: the selection follows its job, or stays on a row that is there when the job left.
        let queued = self.live.status.queued.get(self.queued).map(|job| job.id);
        let latest = self.live.recent.get(self.latest).map(|job| job.id);
        let effects = self.apply(msg);
        let ids: Vec<u64> = self.live.status.queued.iter().map(|job| job.id).collect();
        self.queued = follow(queued, self.queued, &ids);
        let ids: Vec<u64> = self.live.recent.iter().map(|job| job.id).collect();
        self.latest = follow(latest, self.latest, &ids);
        if self.pane == Pane::Queue && self.live.status.queued.is_empty() {
            self.pane = Pane::Slots;
        }
        effects
    }

    fn apply(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Key(key) => return self.key(key),
            Msg::Resize => {}
            Msg::Event(envelope) => return self.event(&envelope),
            Msg::Disconnected(why) => self.connection = Connection::Reconnecting(why),
            Msg::Reconnected => {
                self.connection = Connection::Live;
                if let Some(open) = &self.job {
                    // Its runs too: their end may have been missed with the job's.
                    return vec![Effect::RefreshJob(open.job.summary.id), open.runs_request()];
                }
            }
            Msg::Snapshot(live) => {
                self.live = *live;
                let running = &self.live.running;
                self.series.retain(|id, _| running.contains_key(id));
                self.history.learn(&self.live.recent);
                // Jobs may have started and ended while the stream was away, and a reasons read under way is older.
                if self.reasons_asked {
                    self.reasons_stale = true;
                }
                return vec![self.ask_activity()];
            }
            Msg::Job(job) => {
                if let Some(open) = &mut self.job
                    && open.job.summary.id == job.summary.id
                {
                    open.job = job.clone();
                }
                if job.summary.state.is_final() {
                    // Its `job.finished` was missed, as across an outage.
                    self.live.running.remove(&job.summary.id);
                    self.series.remove(&job.summary.id);
                } else {
                    self.live.refresh(job);
                }
            }
            Msg::Samples(series) => {
                if let Some(open) = &mut self.job
                    && open.job.summary.id == series.job
                {
                    open.samples.clear();
                    for sample in series.samples {
                        push(&mut open.samples, sample, JOB_SAMPLES);
                    }
                }
            }
            Msg::History { generation, page } => {
                if generation != self.history.generation {
                    return Vec::new();
                }
                match page {
                    Ok(page) => {
                        self.history.learn(&page.jobs);
                        self.history.selected =
                            self.history.selected.min(page.jobs.len().saturating_sub(1));
                        self.history.page = Some(page);
                    }
                    Err(error) => self.flash(error, Outcome::Failed),
                }
            }
            Msg::LogLine { job, text } => {
                if let Some(open) = &mut self.job
                    && open.job.summary.id == job
                {
                    push(&mut open.log, text, LOG_LINES);
                    if open.scroll > 0 {
                        open.scroll += 1;
                    }
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
            Msg::Activity {
                generation,
                activity,
            } => self.activity(generation, activity),
            Msg::Reasons(status) => return self.reasons(status.as_deref()),
            Msg::Runs { job, page } => match page {
                Ok(page) => {
                    self.history.learn(&page.jobs);
                    if let Some(open) = &mut self.job
                        && open.job.summary.id == job
                    {
                        open.keep_runs(page);
                    }
                }
                Err(error) => self.flash(error, Outcome::Failed),
            },
            Msg::Done(text) => self.flash(text, Outcome::Done),
            Msg::Failed(text) => self.flash(text, Outcome::Failed),
            Msg::Busy(refused) => self.confirm = Some(refused),
        }
        Vec::new()
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
            for job in &mut self.live.status.queued {
                job.waiting = status
                    .queued
                    .iter()
                    .find(|read| read.id == job.id)
                    .and_then(|read| read.waiting.clone());
            }
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

    fn flash(&mut self, text: String, outcome: Outcome) {
        self.flash = Some(Flash { text, outcome });
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
            Event::HostSample { sample } => {
                push(&mut self.host, sample.clone(), HOST_SAMPLES);
                if sample.at >= self.activity_due {
                    // Asked once until the answer moves it on, however many samples come before.
                    self.activity_due = sample.at + ACTIVITY_EVERY;
                    effects.push(self.ask_activity());
                }
                if sample.at >= self.reasons_due {
                    self.reasons_due = sample.at + REASONS_EVERY;
                    effects.extend(self.ask_reasons());
                }
            }
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
                if self.view == View::History && self.history.before.is_none() {
                    effects.push(self.history.request(Rows::Keep));
                }
                if self.view == View::Sizing {
                    effects.push(Effect::Sizing);
                }
            }
            Event::CacheReseeded {
                profile,
                output,
                error,
            } => {
                let text = match error {
                    Some(error) => format!("{profile}: reseed failed: {error}"),
                    None if output.trim().is_empty() => format!("{profile}: reseeded"),
                    None => format!("{profile}: {}", last_line(output)),
                };
                let outcome = if error.is_some() {
                    Outcome::Failed
                } else {
                    Outcome::Done
                };
                self.flash(text, outcome);
            }
            Event::SizingChanged { change } => {
                self.flash(
                    format!("{}: auto sizing: {}", change.profile, change.line()),
                    Outcome::Done,
                );
                if self.view == View::Sizing {
                    effects.push(Effect::Sizing);
                }
            }
            Event::RecordCreated { .. } | Event::RecordDeleted { .. }
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
            Event::JobStarted { job } if job.id == id => open.job.summary = job.clone(),
            Event::JobFinished { job } if job.id == id => {
                open.job.summary = job.clone();
                // The final record carries the steps' last state and the job's totals.
                effects.push(Effect::RefreshJob(id));
                effects.push(open.runs_request());
            }
            Event::JobStep { job, step } if *job == id => {
                watch::update_step(&mut open.job.steps, step);
            }
            Event::JobSample { job, sample } if *job == id => {
                push(&mut open.samples, sample.clone(), JOB_SAMPLES);
            }
            // The job's page says what became of its record.
            Event::RecordCreated { record } if record.job == id => {
                effects.push(Effect::RefreshJob(id));
            }
            Event::RecordDeleted { job, .. } if *job == id => effects.push(Effect::RefreshJob(id)),
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
        if let Some(effects) = self.answer(key.code) {
            return effects;
        }
        if self.help {
            self.help = false;
            return match key.code {
                KeyCode::Char('q') => vec![Effect::Quit],
                _ => Vec::new(),
            };
        }
        match key.code {
            KeyCode::Char('q') => return vec![Effect::Quit],
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1' | 'o') => return self.show(View::Overview),
            KeyCode::Char('2' | 'h') => return self.show(View::History),
            KeyCode::Char('3') => return self.show(View::Records),
            KeyCode::Char('4') => return self.show(View::Sizing),
            KeyCode::Esc if self.view == View::Job => return self.show(self.back),
            KeyCode::Tab => self.next_pane(Direction::Forward),
            KeyCode::BackTab => self.next_pane(Direction::Backward),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.step(10),
            KeyCode::PageUp => self.step(-10),
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(open) = &mut self.job {
                    open.scroll = 0;
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(open) = &mut self.job {
                    open.scroll = open.log.len();
                }
            }
            KeyCode::Enter => {
                if let Some(id) = self.selected_job() {
                    return self.open(id);
                }
            }
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
            code if self.view == View::History => return self.history_key(code),
            code if self.view == View::Records => self.records_key(code),
            KeyCode::Char('a') if self.view == View::Sizing => {
                self.resize = self.selected_resize();
            }
            _ => {}
        }
        Vec::new()
    }

    /// The answer to a question in the footer, which any key ends: `y` does what it asked. `None` when none was asked.
    fn answer(&mut self, code: KeyCode) -> Option<Vec<Effect>> {
        let yes = code == KeyCode::Char('y');
        let effect = if let Some(refused) = self.confirm.take() {
            Effect::Pause {
                repository: refused.repository,
                force: true,
            }
        } else if let Some(removal) = self.removal.take() {
            Effect::RemoveRecords(removal)
        } else {
            Effect::Resize(self.resize.take()?)
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
            KeyCode::Char('d') => self.removal = self.selected_job().map(Removal::Job),
            KeyCode::Char('D') if any => self.removal = Some(Removal::All),
            _ => {}
        }
    }

    /// The keys only the history takes: its filters and pages.
    fn history_key(&mut self, code: KeyCode) -> Vec<Effect> {
        match code {
            KeyCode::Char('c') => {
                self.history.conclusion = cycle(
                    self.history.conclusion.as_deref(),
                    CONCLUSIONS.iter().copied(),
                );
                return self.first_page();
            }
            KeyCode::Char('s') => {
                let names = self
                    .live
                    .status
                    .repositories
                    .iter()
                    .map(|r| r.name.as_str());
                self.history.repository = cycle(self.history.repository.as_deref(), names);
                // A workflow the repository has not run would show nothing.
                if let Some(workflow) = self.history.workflow.take()
                    && self.history.workflows().contains(workflow.as_str())
                {
                    self.history.workflow = Some(workflow);
                }
                return self.first_page();
            }
            KeyCode::Char('w') => {
                let workflows = self.history.workflows();
                let next = cycle(self.history.workflow.as_deref(), workflows.iter().copied());
                self.history.workflow = next;
                return self.first_page();
            }
            KeyCode::Char(']') => {
                if let Some(next) = self.history.page.as_ref().and_then(|page| page.next) {
                    self.history.previous.push(self.history.before);
                    self.history.before = Some(next);
                    return vec![self.history.request(Rows::Clear)];
                }
            }
            KeyCode::Char('[') => {
                if let Some(before) = self.history.previous.pop() {
                    self.history.before = before;
                    return vec![self.history.request(Rows::Clear)];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn show(&mut self, view: View) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.view == View::Job && view != View::Job {
            self.job = None;
            effects.push(Effect::CloseJob);
        }
        if view == View::History && self.view != View::History {
            effects.push(self.history.request(Rows::Keep));
        }
        if view == View::Records && self.view != View::Records {
            effects.push(Effect::Records);
        }
        if view == View::Sizing && self.view != View::Sizing {
            effects.push(Effect::Sizing);
        }
        self.view = view;
        effects
    }

    fn first_page(&mut self) -> Vec<Effect> {
        self.history.previous.clear();
        self.history.before = None;
        vec![self.history.request(Rows::Clear)]
    }

    /// Opens a job; its record, samples and log arrive as messages.
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
        let Some(summary) = summary else {
            return Vec::new();
        };
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
        self.job = Some(JobView {
            job: Job {
                admission: None,
                summary,
                steps,
                record: None,
            },
            samples: self.series.get(&id).cloned().unwrap_or_default(),
            log: VecDeque::new(),
            scroll: 0,
            runs: None,
        });
        if self.view != View::Job {
            self.back = self.view;
        }
        self.view = View::Job;
        effects.push(Effect::OpenJob(id));
        if let Some(open) = &self.job {
            effects.push(open.runs_request());
        }
        effects
    }

    /// Jobs on screen outside the slots: the latest ones and the history page.
    fn jobs_listed(&self) -> impl Iterator<Item = &JobSummary> {
        self.live
            .recent
            .iter()
            .chain(self.history.page.iter().flat_map(|page| page.jobs.iter()))
    }

    fn step(&mut self, by: isize) {
        let slots = self.slots().count();
        let (selected, len) = match (self.view, self.pane) {
            (View::Job, _) => {
                if let Some(open) = &mut self.job {
                    open.scroll = open.scroll.saturating_add_signed(-by).min(open.log.len());
                }
                return;
            }
            (View::History, _) => (
                &mut self.history.selected,
                self.history.page.as_ref().map_or(0, |page| page.jobs.len()),
            ),
            (View::Records, _) => (
                &mut self.record,
                self.records.as_ref().map_or(0, |r| r.records.len()),
            ),
            (View::Sizing, _) => (
                &mut self.sized,
                self.sizing.as_ref().map_or(0, |s| s.profiles.len()),
            ),
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
    }

    /// Every slot in config order, with its profile's position.
    pub(crate) fn slots(
        &self,
    ) -> impl Iterator<
        Item = (
            usize,
            &runnerd_core::api::ProfileStatus,
            &runnerd_core::api::SlotStatus,
        ),
    > {
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
                .page
                .as_ref()
                .and_then(|page| page.jobs.get(self.history.selected))
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
            (View::History | View::Records | View::Sizing, _) => None,
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

    /// Moves to the next of the overview's panes, in the order they are drawn, past the queue while it is empty.
    fn next_pane(&mut self, direction: Direction) {
        const ORDER: [Pane; 4] = [Pane::Slots, Pane::Queue, Pane::Repositories, Pane::Latest];
        let at = ORDER
            .iter()
            .position(|pane| *pane == self.pane)
            .unwrap_or(0);
        let step = match direction {
            Direction::Forward => 1,
            Direction::Backward => ORDER.len() - 1,
        };
        let mut next = (at + step) % ORDER.len();
        if ORDER[next] == Pane::Queue && self.live.status.queued.is_empty() {
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

/// The value after `current` in `values`, then none, then the first again.
fn cycle<'a>(current: Option<&str>, mut values: impl Iterator<Item = &'a str>) -> Option<String> {
    match current {
        None => values.next(),
        Some(current) => values.skip_while(|value| *value != current).nth(1),
    }
    .map(str::to_owned)
}

fn last_line(text: &str) -> &str {
    text.trim().lines().last().unwrap_or_default()
}
