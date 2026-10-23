//! The TUI's state as a pure reducer: `update` takes a key, an event or an answer from the daemon and returns the
//! requests to make. It decides what is shown and asked for; making requests is `tui`'s, drawing is `draw`'s, and
//! the daemon's state comes from `watch::State`, the reducer `runnerctl watch` uses.

use std::collections::{BTreeMap, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use runnerd_core::api::{
    Event, EventEnvelope, HostSample, Job, JobMetrics, JobPage, JobQuery, JobState, JobSummary,
    LogSummary, QueuedJob, Sample, SampleSeries, Status, Timeline,
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

/// A row of the latest jobs pane.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LatestRow<'app> {
    Queued(&'app QueuedJob),
    Finished(&'app JobSummary),
}

impl LatestRow<'_> {
    fn id(self) -> u64 {
        match self {
            LatestRow::Queued(job) => job.id,
            LatestRow::Finished(job) => job.id,
        }
    }

    fn repository(&self) -> &str {
        match self {
            LatestRow::Queued(job) => &job.repository,
            LatestRow::Finished(job) => &job.repository,
        }
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
    /// Give the terminal back and stop, as Ctrl-Z does in a shell.
    Suspend,
    Quit,
}

/// The screen shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Overview,
    History,
    Job,
}

/// The overview's list that takes the selection keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    Slots,
    Repositories,
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
}

/// The history page and its filters.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub(crate) page: Option<JobPage>,
    pub(crate) repository: Option<String>,
    pub(crate) conclusion: Option<String>,
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
            conclusion: self.conclusion.clone(),
            before: self.before,
            limit: Some(HISTORY_PAGE),
            ..JobQuery::default()
        }
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
    pub(crate) repository: usize,
    pub(crate) latest: usize,
    pub(crate) history: History,
    pub(crate) job: Option<JobView>,
    pub(crate) flash: Option<Flash>,
    pub(crate) confirm: Option<Refused>,
    pub(crate) help: bool,
}

impl App {
    pub(crate) fn new(live: watch::State, host: Vec<HostSample>, series: Vec<SampleSeries>) -> App {
        let mut app = App {
            live,
            series: BTreeMap::new(),
            host: VecDeque::new(),
            connection: Connection::Live,
            view: View::Overview,
            back: View::Overview,
            pane: Pane::Slots,
            slot: 0,
            repository: 0,
            latest: 0,
            history: History::default(),
            job: None,
            flash: None,
            confirm: None,
            help: false,
        };
        for sample in host {
            push(&mut app.host, sample, HOST_SAMPLES);
        }
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
        // Rows of the latest jobs come and go above the selected one, as queued jobs start: the selection follows its
        // job, or stays on a row that is there when the job left.
        let selected = self.latest_rows().nth(self.latest).map(LatestRow::id);
        let effects = self.apply(msg);
        let rows = self.latest_rows().count();
        self.latest = selected
            .and_then(|id| self.latest_rows().position(|row| row.id() == id))
            .unwrap_or(self.latest)
            .min(rows.saturating_sub(1));
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
                    return vec![Effect::RefreshJob(open.job.summary.id)];
                }
            }
            Msg::Snapshot(live) => {
                self.live = *live;
                let running = &self.live.running;
                self.series.retain(|id, _| running.contains_key(id));
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
            Msg::Done(text) => self.flash(text, Outcome::Done),
            Msg::Failed(text) => self.flash(text, Outcome::Failed),
            Msg::Busy(refused) => self.confirm = Some(refused),
        }
        Vec::new()
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
            Event::HostSample { sample } => push(&mut self.host, sample.clone(), HOST_SAMPLES),
            Event::JobFinished { job } => {
                self.series.remove(&job.id);
                if self.view == View::History && self.history.before.is_none() {
                    effects.push(self.history.request(Rows::Keep));
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
            Event::ProfileChanged { .. } | Event::ProfileRemoved { .. } => {
                // A reload can take away the selected slot; the selection stays on one that is there.
                self.slot = self.slot.min(self.slots().count().saturating_sub(1));
            }
            _ => {}
        }
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
            }
            Event::JobStep { job, step } if *job == id => {
                watch::update_step(&mut open.job.steps, step);
            }
            Event::JobSample { job, sample } if *job == id => {
                push(&mut open.samples, sample.clone(), JOB_SAMPLES);
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
        if let Some(refused) = self.confirm.take() {
            return match key.code {
                KeyCode::Char('y') => vec![Effect::Pause {
                    repository: refused.repository,
                    force: true,
                }],
                _ => Vec::new(),
            };
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
            KeyCode::Esc if self.view == View::Job => return self.show(self.back),
            KeyCode::Tab => self.pane = next_pane(self.pane, Direction::Forward),
            KeyCode::BackTab => self.pane = next_pane(self.pane, Direction::Backward),
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
            _ => {}
        }
        Vec::new()
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
            job: Job { summary, steps },
            samples: self.series.get(&id).cloned().unwrap_or_default(),
            log: VecDeque::new(),
            scroll: 0,
        });
        if self.view != View::Job {
            self.back = self.view;
        }
        self.view = View::Job;
        effects.push(Effect::OpenJob(id));
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
        let latest = self.latest_rows().count();
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
            (View::Overview, Pane::Slots) => (&mut self.slot, slots),
            (View::Overview, Pane::Repositories) => {
                (&mut self.repository, self.live.status.repositories.len())
            }
            (View::Overview, Pane::Latest) => (&mut self.latest, latest),
        };
        *selected = selected
            .saturating_add_signed(by)
            .min(len.saturating_sub(1));
    }

    /// The latest jobs pane's rows: the queued jobs, oldest first, then the last finished ones.
    pub(crate) fn latest_rows(&self) -> impl Iterator<Item = LatestRow<'_>> {
        let queued = self.live.status.queued.iter().map(LatestRow::Queued);
        queued.chain(self.live.recent.iter().map(LatestRow::Finished))
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
            (View::Overview, Pane::Slots) => {
                let (_, profile, slot) = self.slots().nth(self.slot)?;
                self.live
                    .job_in(&profile.name, slot.slot)
                    .map(|job| job.summary.id)
            }
            (View::Overview, Pane::Latest) => {
                self.latest_rows().nth(self.latest).map(LatestRow::id)
            }
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
            (View::History, _) => None,
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
            (View::Overview, Pane::Latest) => self
                .latest_rows()
                .nth(self.latest)
                .map(|row| row.repository().to_owned()),
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

fn next_pane(pane: Pane, direction: Direction) -> Pane {
    match (pane, direction) {
        (Pane::Slots, Direction::Forward) | (Pane::Latest, Direction::Backward) => {
            Pane::Repositories
        }
        (Pane::Repositories, Direction::Forward) | (Pane::Slots, Direction::Backward) => {
            Pane::Latest
        }
        (Pane::Latest, Direction::Forward) | (Pane::Repositories, Direction::Backward) => {
            Pane::Slots
        }
    }
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
