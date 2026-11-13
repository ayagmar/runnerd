//! `/`: what needs attention first, when anything does; whether CI keeps up as one row of figures; then the machine,
//! one region: the host's reading as figures, then the memory budget as lanes (a lane per slot whose block sits where
//! its memory does on the slice, a running slot's job, step, memory and CPU, a warm or idle slot one compact row, then
//! a row each for the mirror, seeds, shells, what is free and what is closed) and the slice's CPU and memory over the
//! last half hour; and beside it the work: every queued job with why it waits, the latest jobs, and the repositories
//! with pause and resume. Hovering, focusing or tapping a slot's lane, job row or repository lights up the others
//! that are the same slot. That is the order for reading and for the keyboard; a wide screen
//! places the machine beside the work with the grid, not a different order.

use std::collections::BTreeMap;

use maud::{Markup, html};
use runnerd_core::api::{
    Activity, ActivityQuery, Attention, AttentionItem, Feature, Fix, HealthKind, HeldKind,
    HeldPart, HostSample, HostSamples, Job, JobPage, JobQuery, JobState, JobSummary, JobTitle,
    ProfileStatus, QueuedJob, RepositoryStatus, Sample, SampleSeries, Savings, Section, SlotStatus,
    Status, Subject, View, WaitKind, cpus, in_cores,
};
use runnerd_core::client::{Client, ClientError};

use super::{
    Live, Page, Parts, badge, digest, heading, js, page, profile_class, repository_id, slot_id,
    time_ago, tone_class,
};
use crate::chart::{Base, Series, Sparkline, TimeChart, nice_top};
use crate::format;

/// How far back the host charts reach.
pub(crate) const HOST_WINDOW: u64 = 30 * 60;
/// How much longer the samples of the slice are kept, so the oldest stretch of a live chart stays whole.
const HOST_MARGIN: u64 = 5 * 60;
/// How far back a slot's sparkline reaches.
const SPARK_WINDOW: u64 = 5 * 60;
/// Latest jobs listed.
const RECENT: u32 = 6;
/// The most jobs `GET /v1/jobs` lists at once.
const MAX_JOBS: u32 = 500;
/// Queued jobs listed, the oldest; the history lists the rest.
const QUEUED_SHOWN: usize = 6;

/// How many queued jobs a phone shows; the stylesheet hides the rest, so the two agree.
const QUEUED_SHOWN_NARROW: usize = 3;
/// How far back the health figures look: the 24 hours up to now.
const DAY: u64 = 24 * 3600;
/// Attention items listed before the rest fold under core's "and N more".
const ATTENTION_SHOWN: usize = 4;

/// How many attention items a phone shows before the rest fold.
const ATTENTION_SHOWN_NARROW: usize = 2;

/// Where an attention item shows: on every screen, shown on a wide one and folded on a narrow one, or folded on all.
#[derive(Debug, Clone, Copy)]
enum Fold {
    Always,
    Wide,
    Narrow,
}

impl Fold {
    /// An item of the shown list, at its place in the whole list.
    fn shown(at: usize) -> Self {
        if at < ATTENTION_SHOWN_NARROW {
            Self::Always
        } else {
            Self::Wide
        }
    }

    /// An item of the folded list, at its place in the whole list.
    fn folded(at: usize) -> Self {
        if at < ATTENTION_SHOWN {
            Self::Narrow
        } else {
            Self::Always
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Always => "",
            Self::Wide => "wide",
            Self::Narrow => "narrow",
        }
    }
}

/// Everything the overview shows.
#[derive(Debug, Clone)]
pub(crate) struct Overview {
    pub(crate) now: u64,
    pub(crate) status: Status,
    /// Running jobs with their steps.
    pub(crate) running: Vec<Job>,
    /// Samples of each running job, by job ID.
    pub(crate) samples: BTreeMap<u64, Vec<Sample>>,
    pub(crate) host: HostSamples,
    pub(crate) recent: Vec<JobSummary>,
    /// The jobs started and finished in the last `DAY`, for the health figures.
    pub(crate) day: Activity,
    /// What the jobs' minutes would have cost on GitHub's runners; `None` when the history could not say.
    pub(crate) savings: Option<Savings>,
    /// What needs attention; `None` from a daemon that does not list it.
    pub(crate) attention: Option<Attention>,
}

impl Overview {
    pub(crate) async fn load(daemon: &Client, now: u64) -> Result<Overview, ClientError> {
        let status: Status = daemon.get("/v1/status").await?;
        let host = daemon
            .get(&format!(
                "/v1/host/samples?since={}",
                now.saturating_sub(HOST_WINDOW + HOST_MARGIN)
            ))
            .await?;
        let recent = read_recent(daemon, &status).await?;
        let mut overview = Overview {
            now,
            status,
            running: Vec::new(),
            samples: BTreeMap::new(),
            host,
            recent,
            day: Activity {
                since: now.saturating_sub(DAY),
                until: now,
                started: Vec::new(),
                finished: 0,
                failed: 0,
                runner_failed: 0,
            },
            savings: None,
            attention: daemon.attention().await?,
        };
        overview.reload_running(daemon).await?;
        overview.reload_day(daemon, now).await?;
        Ok(overview)
    }

    /// Reads the jobs started and finished in the day up to `now` again, and what the jobs' minutes would have cost.
    pub(crate) async fn reload_day(
        &mut self,
        daemon: &Client,
        now: u64,
    ) -> Result<(), ClientError> {
        self.day = daemon
            .get_query(
                "/v1/activity",
                &ActivityQuery {
                    since: Some(now.saturating_sub(DAY)),
                },
            )
            .await?;
        // The overview stands without them: savings the history could not read are left out.
        self.savings = daemon.get("/v1/savings").await.ok();
        Ok(())
    }

    /// Reads the running jobs again, and the samples of those not seen before.
    pub(crate) async fn reload_running(&mut self, daemon: &Client) -> Result<(), ClientError> {
        let page: JobPage = daemon
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    state: Some(JobState::Running),
                    limit: Some(100),
                    ..JobQuery::default()
                },
            )
            .await?;
        let mut running = Vec::with_capacity(page.jobs.len());
        for summary in page.jobs {
            running.push(
                daemon
                    .get::<Job>(&format!("/v1/jobs/{}", summary.id))
                    .await?,
            );
        }
        self.samples
            .retain(|id, _| running.iter().any(|job| job.summary.id == *id));
        for job in &running {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.samples.entry(job.summary.id)
            {
                let series: SampleSeries = daemon
                    .get(&format!("/v1/jobs/{}/samples", job.summary.id))
                    .await?;
                entry.insert(series.samples);
            }
        }
        self.running = running;
        Ok(())
    }

    /// Adds a live sample of a running job.
    pub(crate) fn add_job_sample(&mut self, job: u64, sample: Sample) {
        if let Some(samples) = self.samples.get_mut(&job) {
            samples.push(sample);
        }
    }

    /// Adds a live sample of the slice, dropping those older than the window and its margin.
    pub(crate) fn add_host_sample(&mut self, sample: HostSample) {
        let since = sample.at.saturating_sub(HOST_WINDOW + HOST_MARGIN);
        self.host.samples.push(sample);
        self.host.samples.retain(|sample| sample.at >= since);
    }

    fn job_in(&self, profile: &str, slot: usize) -> Option<&Job> {
        self.running.iter().find(|job| {
            job.summary
                .runner
                .as_ref()
                .is_some_and(|runner| runner.profile == profile && runner.slot == slot)
        })
    }
}

/// The live regions, by id, in the order the page has them, with their parts.
pub(crate) fn regions(overview: &Overview) -> [(&'static str, Markup, Parts); 8] {
    let region = |id, render: &dyn Fn(&Parts) -> Markup| {
        let parts = Parts::default();
        let markup = render(&parts);
        (id, markup, parts)
    };
    [
        region("health", &|_| health(overview)),
        region("attention", &|_| attention(overview)),
        region("budget", &|parts| budget(overview, parts)),
        region("slots", &|parts| slots(overview, parts)),
        region("host", &|parts| host(overview, parts)),
        region("queue", &|_| queue(overview)),
        region("recent", &|_| recent(overview)),
        region("repositories", &|_| repositories(&overview.status)),
    ]
}

/// The whole page.
pub(crate) fn render(overview: &Overview) -> Markup {
    let regions = regions(overview);
    let live = Live {
        url: "/live/overview",
        at: overview.now,
        log: None,
        regions: regions
            .iter()
            .map(|(id, markup, _)| (*id, digest(&markup.0)))
            .collect(),
    };
    let [
        health,
        attention,
        budget,
        slots,
        host,
        queue,
        recent,
        repositories,
    ] = regions;
    let main = html! {
        (heading(View::Overview.title(), Some(Page::Overview), Some(health.1)))
        (attention.1)
        div.overview {
            section.machine aria-labelledby="budget-title"
                "data-signals:_slot__ifmissing"="''" "data-signals:_repo__ifmissing"="''" {
                (budget.1)
                (slots.1)
                (host.1)
            }
            div.work {
                (queue.1)
                (recent.1)
                (repositories.1)
                @if let Some(savings) = &overview.savings {
                    p.savings { (savings.month.headline()) }
                }
            }
        }
    };
    page(
        View::Overview.title(),
        Some(Page::Overview),
        Some(&live),
        &main,
    )
}

/// Where an attention item's subject is shown, if this dashboard has a page for it.
fn subject_href(subject: &Subject) -> Option<String> {
    match subject {
        Subject::Job { id, .. } => Some(format!("/jobs/{id}")),
        Subject::Repository { name } => Some(format!(
            "/history?{}",
            serde_urlencoded::to_string([("repository", name)]).unwrap_or_default()
        )),
        Subject::Check { .. } => Some(format!(
            "{}#doctor",
            Page::of_section(Section::Config).path()
        )),
        Subject::Insight { .. } => Some(Page::of(View::Insights).path().to_owned()),
        Subject::Host | Subject::Github | Subject::Unknown => None,
    }
}

/// An attention item's fix: a command to copy, a key of the config, a view to open, or words.
fn fix(fix: &Fix) -> Markup {
    html! {
        @match fix {
            Fix::Command { command } => { code.fix-command { (command) } }
            Fix::Open { view } => { a href=(Page::of(*view).path()) { (fix.line()) } }
            Fix::Config { .. } => { a href=(Page::of_section(Section::Config).path()) { (fix.line()) } }
            Fix::Text { .. } => { (fix.line()) }
        }
    }
}

fn attention_item(item: &AttentionItem, now: u64, fold: Fold) -> Markup {
    let href = subject_href(&item.subject);
    html! {
        li class=(format!("attention-item {} {}", tone_class(item.tone), fold.class())) data-row tabindex="-1" {
            span.tone-mark aria-hidden="true" {}
            @if let Some(href) = &href {
                a.attention-title href=(href) { (item.title) }
            } @else {
                span.attention-title { (item.title) }
            }
            @if let Some(found) = &item.fix { span.attention-fix { (fix(found)) } }
            @if let Some(since) = item.since { span.attention-since { (time_ago(since, now)) } }
        }
    }
}

/// What needs attention now, most urgent first, as core orders and words it; nothing at all when nothing does. The
/// first few show, the rest fold under core's count of them.
pub(crate) fn attention(overview: &Overview) -> Markup {
    html! {
        @match &overview.attention {
            None => {
                section #attention .attention.unsupported aria-label="Needs attention" {
                    p.muted { (format::sentence(&Feature::Attention.unsupported())) "." }
                }
            }
            Some(attention) if attention.items.is_empty() => { div #attention hidden {} }
            Some(attention) => {
                section #attention .attention aria-label="Needs attention" {
                    ul.attention-list {
                        @for (at, item) in attention.items.iter().take(ATTENTION_SHOWN).enumerate() {
                            (attention_item(item, overview.now, Fold::shown(at)))
                        }
                    }
                    @if let Some(narrow) = attention.more(ATTENTION_SHOWN_NARROW) {
                        @let wide = attention.more(ATTENTION_SHOWN);
                        details.attention-more.narrow-only[wide.is_none()] {
                            summary {
                                span.narrow { (narrow) }
                                @if let Some(wide) = &wide { span.wide { (wide) } }
                            }
                            ul.attention-list {
                                @for (at, item) in attention.items.iter().enumerate().skip(ATTENTION_SHOWN_NARROW) {
                                    (attention_item(item, overview.now, Fold::folded(at)))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What an element standing for a slot does on hover, focus and touch: it selects the slot, which outlines its lane
/// and its repository's row. Only the key is kept, so each live update keeps the selection.
struct Link {
    enter: String,
    /// Clears the selection, unless another element has made its own since, as a tap does before the last one's
    /// blur.
    leave: String,
    /// `leave` for a mouse only: a finger leaves as soon as it lifts, so what a tap selected stays until the next tap
    /// or until focus moves on.
    pointer_leave: String,
    linked: String,
}

fn link(key: &str, repository: Option<&str>) -> Link {
    let key = js(key);
    Link {
        enter: format!("$_slot = {key}"),
        leave: format!("$_slot == {key} && ($_slot = '')"),
        pointer_leave: format!("evt.pointerType == 'mouse' && $_slot == {key} && ($_slot = '')"),
        linked: match repository {
            Some(repository) => format!("$_slot == {key} || $_repo == {}", js(repository)),
            None => format!("$_slot == {key}"),
        },
    }
}

/// The budget's head: what is reserved of the slice as its heading, then what is in use, of the CPUs and of the host,
/// as figures. The lanes under it are the bar.
pub(crate) fn budget(overview: &Overview, parts: &Parts) -> Markup {
    let budget = &overview.status.budget;
    let reading = budget.memory_reading();
    html! {
        div #budget .budget {
            h2 #budget-title {
                span.big { (reading.figure) }
                " " span.reading-text { (reading.text) }
            }
            (parts.html("budget-readout", html! {
                dl #budget-readout .readout {
                    @if let Some(cpu) = budget.cpu {
                        div.cpu-reserved {
                            dt.visually-hidden { "CPU" }
                            dd {
                                (cpu.line())
                                span.meter aria-hidden="true" style=(format!("--fill: {:.3}", crate::chart::ratio(cpu.in_use, cpu.budget).min(1.0))) {}
                            }
                        }
                    }
                    @for figure in budget.host_figures() {
                        div {
                            dt { (figure.label) }
                            dd {
                                (figure.value)
                                @if let Some(note) = &figure.note { span.figure-note { " " (note) } }
                            }
                        }
                    }
                }
            }))
        }
    }
}

/// Where a part of the budget sits on the slice: from where, and how much of it, each a share of the whole.
#[derive(Debug, Clone, Copy)]
struct Share {
    from: f64,
    size: f64,
}

impl Share {
    /// Its block in the lane's strip of the slice, filled to `fill` of it when a job's memory is sampled.
    fn block(self, fill: Option<(&str, f64)>, parts: &Parts) -> Markup {
        html! {
            span.lane-share aria-hidden="true" {
                span.lane-block style=(format!("--from: {:.4}; --share: {:.4}", self.from, self.size)) {
                    @if let Some((id, fill)) = fill {
                        (parts.html(id, html! { span.fill #(id) style=(format!("--fill: {fill:.3}")) {} }))
                    }
                }
            }
        }
    }
}

/// The budget's parts laid end to end over the slice, in the order core gives them: the slots, then what else holds
/// memory, what is free and what is closed.
struct Strip {
    whole: u64,
    at: u64,
}

impl Strip {
    fn take(&mut self, memory: u64) -> Share {
        let share = Share {
            from: crate::chart::ratio(self.at, self.whole),
            size: crate::chart::ratio(memory, self.whole),
        };
        self.at = self.at.saturating_add(memory);
        share
    }
}

/// The machine's lanes, which are its budget bar: a lane per slot, in config order and its profile's colour, whose
/// block sits where the slot's memory sits on the slice; then a row for the mirror, each seed container and shell,
/// what is free, and what no runner may start in. A running lane carries its job, its step, its memory and CPU over
/// the last minutes and a fill to its last sampled memory; any other row is one compact line.
pub(crate) fn slots(overview: &Overview, parts: &Parts) -> Markup {
    let status = &overview.status;
    let bar = status.budget_parts();
    let mut strip = Strip {
        whole: bar.whole.max(1),
        at: 0,
    };
    let mut shares = BTreeMap::new();
    for part in &bar.slots {
        shares.insert(
            (part.profile, part.slot),
            (strip.take(part.memory), part.memory),
        );
    }
    let held: Vec<(&HeldPart, Share)> = bar
        .held
        .iter()
        .map(|part| (part, strip.take(part.memory)))
        .collect();
    let free = bar
        .free
        .as_ref()
        .map(|part| (part, strip.take(part.memory)));
    let closed = bar
        .closed
        .as_ref()
        .map(|part| (part, strip.take(part.memory)));
    let label = format!(
        "Memory budget of {}: {}",
        status.budget.slice,
        status.budget.memory_reading().line()
    );
    html! {
        div #slots .slots {
            h2.visually-hidden { "Slots" }
            @if status.profiles.is_empty() {
                p.muted { "No profiles in runnerd.toml, so no slots." }
            }
            ol.lanes aria-label=(label) {
                @for (index, profile) in status.profiles.iter().enumerate() {
                    @for slot in &profile.slots {
                        (lane(overview, profile, slot, shares.get(&(index, slot.slot)).copied(), parts))
                    }
                }
                @for (part, share) in &held {
                    @let class = match &part.kind {
                        HeldKind::Mirror => "mirror".to_owned(),
                        HeldKind::Seed { profile } => format!("seed {}", profile_class(status, profile)),
                        HeldKind::Shell { .. } => "shell".to_owned(),
                    };
                    li class=(format!("lane part {class}")) aria-label=(format!("{} {}", format::bytes(part.memory), part.holder())) {
                        (share.block(None, parts))
                        span.lane-name { (format::bytes(part.memory)) }
                        span.lane-what { (part.name()) }
                    }
                }
                @if let Some((part, share)) = free {
                    li.lane.part.free aria-label=(part.reading.line()) {
                        (share.block(None, parts))
                        span.lane-name { (part.reading.figure) }
                        span.lane-what.lane-state { (part.reading.text) }
                    }
                }
                @if let Some((part, share)) = closed {
                    li.lane.part.closed aria-label=(part.reading.line()) {
                        (share.block(None, parts))
                        span.lane-name { (part.reading.figure) }
                        span.lane-what.lane-state { (part.reading.text) }
                    }
                }
            }
        }
    }
}

fn lane(
    overview: &Overview,
    profile: &ProfileStatus,
    slot: &SlotStatus,
    share: Option<(Share, u64)>,
    parts: &Parts,
) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let key = slot_id(&profile.name, slot.slot);
    let job = overview.job_in(&profile.name, slot.slot);
    let slot_state = Status::slot_state(profile, slot);
    let state = slot_state.word();
    let repository = slot
        .runner
        .as_ref()
        .map(|runner| runner.repository.as_str());
    let link = link(&key, repository);
    let memory = job
        .and_then(|job| overview.samples.get(&job.summary.id))
        .and_then(|samples| samples.last())
        .map(|sample| sample.memory);
    let fill_id = format!("fill-{key}");
    html! {
        li #(key) class=(format!("lane {class} {state}")) data-row
            aria-label=(format!("{} {}: {}", profile.name, slot.slot, slot_state.line()))
            data-on:pointerenter=(link.enter) data-on:focusin=(link.enter)
            data-on:pointerleave=(link.pointer_leave) data-on:focusout=(link.leave) data-class:linked=(link.linked) {
            @match share {
                Some((share, reserved)) => {
                    (share.block(memory.map(|used| (fill_id.as_str(), crate::chart::ratio(used, reserved).min(1.0))), parts))
                }
                None => { span.lane-share aria-hidden="true" {} }
            }
            span.lane-name { (profile.name) " " (slot.slot) }
            span.lane-caps { (slot_caps(profile, job)) }
            @match (&slot.runner, job) {
                (Some(_), Some(job)) if slot_state.is_running() => { (running_job(overview, profile, job, &key, parts)) }
                (Some(runner), _) => {
                    span.lane-what {
                        span.lane-state { (state) }
                        " " span.repo { (runner.repository) }
                    }
                    @let id = format!("{key}-waiting");
                    (parts.html(&id, html! { span.lane-since #(id) { (time_ago(runner.started_at, overview.now)) } }))
                }
                (None, _) => {
                    span.lane-what { span.lane-state { (state) } }
                }
            }
        }
    }
}

/// A slot's caps: its running job's container's, which a reload since may have changed in the profile, else the
/// profile's.
fn slot_caps(profile: &ProfileStatus, job: Option<&Job>) -> String {
    let limits = job.map(|job| job.limits(Some(profile)));
    let memory = limits
        .and_then(|limits| limits.memory)
        .map_or(profile.memory, |limit| limit.value);
    let cpu = limits
        .and_then(|limits| limits.cpu)
        .map_or(u64::from(profile.cpus) * 1000, |limit| limit.value);
    format!("{} · {}", format::bytes(memory), cpus(cpu))
}

fn running_job(
    overview: &Overview,
    profile: &ProfileStatus,
    job: &Job,
    key: &str,
    parts: &Parts,
) -> Markup {
    let summary = &job.summary;
    let now = overview.now;
    let started = summary
        .timeline
        .started_at
        .unwrap_or(summary.timeline.queued_at);
    // The running step, or the last one done, and its place among the job's steps, as `runnerctl watch` counts it.
    let current = job
        .steps
        .iter()
        .position(|step| step.status == "in_progress")
        .or_else(|| {
            job.steps
                .iter()
                .rposition(|step| step.status == "completed")
        })
        .map(|place| (place + 1, &job.steps[place]));
    let samples = overview
        .samples
        .get(&summary.id)
        .map_or(&[][..], Vec::as_slice);
    let memory: Vec<(u64, u64)> = samples
        .iter()
        .map(|sample| (sample.at, sample.memory))
        .collect();
    let cpu: Vec<(u64, u64)> = samples
        .iter()
        .map(|sample| (sample.at, sample.cpu))
        .collect();
    let sparkline = |points, top, class: &'static str| {
        Sparkline {
            points,
            start: started.min(now),
            end: now,
            window: SPARK_WINDOW,
            top,
            class,
            live: Some(format!("{key}-{class}")),
        }
        .render(parts)
    };
    let limits = job.limits(Some(profile));
    let last = samples.last();
    let figure = |class: &str, text: String| {
        let id = format!("{key}-{class}-now");
        parts.html(&id, html! { span.figure-now #(id) { (text) } })
    };
    let elapsed = format!("{key}-elapsed");
    html! {
        a.lane-what.lane-job href=(format!("/jobs/{}", summary.id)) {
            span.job-name { (summary.title().name) }
            " " span.repo { (summary.repository) }
        }
        span.lane-step {
            @if let Some((place, step)) = current {
                span.step-count { (place) "/" (job.steps.len()) }
                " " (step.name)
            } @else {
                "Setting up"
            }
        }
        (parts.html(&elapsed, html! {
            span.lane-since.elapsed #(elapsed) { span.visually-hidden { "Running for " } (format::duration(now.saturating_sub(started))) }
        }))
        span.lane-figure.memory {
            span.visually-hidden { "Memory " }
            (figure("memory", last.map_or("–".to_owned(), |sample| format::bytes(sample.memory))))
            (sparkline(&memory, limits.memory.map_or(profile.memory, |limit| limit.value), "memory"))
        }
        span.lane-figure.cpu {
            span.visually-hidden { "CPU " }
            (figure("cpu", last.map_or("–".to_owned(), |sample| in_cores(sample.cpu))))
            (sparkline(&cpu, limits.cpu.map_or(u64::from(profile.cpus) * 1000, |limit| limit.value), "cpu"))
        }
    }
}

/// The slice's memory and CPU over the last half hour, against `MemoryMax` and the CPUs the slots may use.
pub(crate) fn host(overview: &Overview, parts: &Parts) -> Markup {
    let status = &overview.status;
    let now = overview.now;
    let samples = &overview.host.samples;
    let memory: Vec<(u64, u64)> = samples.iter().map(|s| (s.at, s.memory)).collect();
    let cpu: Vec<(u64, u64)> = samples.iter().map(|s| (s.at, s.cpu)).collect();
    let shown = |points: &[(u64, u64)]| {
        let since = now.saturating_sub(HOST_WINDOW);
        points
            .iter()
            .filter(|point| point.0 >= since)
            .map(|point| point.1)
            .max()
            .unwrap_or(0)
    };
    let gap = overview.host.interval.max(1) * 3;
    let memory_peak = shown(&memory);
    let cpu_peak = shown(&cpu);
    let (slice, interval) = (&status.budget.slice, overview.host.interval);
    let up_to =
        |limit: Option<String>| limit.map_or(String::new(), |limit| format!(", up to its {limit}"));
    let memory_chart = TimeChart {
        title: format!(
            "Memory of {slice} over the last 30 minutes, sampled every {interval} s{}",
            up_to(
                status
                    .budget
                    .memory_max
                    .map(|max| format!("MemoryMax of {}", format::bytes(max)))
            )
        ),
        start: 0,
        end: now,
        window: Some(HOST_WINDOW),
        live: Some("host-memory".to_owned()),
        series: vec![Series {
            label: "memory".to_owned(),
            class: "memory",
            points: memory,
            area: true,
        }],
        limits: vec![],
        value: format::bytes,
        time: relative_clock,
        gap,
    };
    // The slice's own limits top the axes when they are known: the slots' memory caps and CPU limits may add up
    // past them, and a rounded-up top would draw the slice's whole as a part.
    let memory_top = status.budget.memory_max.map_or_else(
        || nice_top(memory_peak, Base::Binary),
        |max| max.max(memory_peak),
    );
    let cpu_chart = TimeChart {
        title: format!(
            "CPU of {slice} over the last 30 minutes, sampled every {interval} s{}",
            up_to(status.budget.slice_cpus.map(cpus))
        ),
        start: 0,
        end: now,
        window: Some(HOST_WINDOW),
        live: Some("host-cpu".to_owned()),
        series: vec![Series {
            label: "CPU".to_owned(),
            class: "cpu",
            points: cpu,
            area: true,
        }],
        limits: vec![],
        value: in_cores,
        time: relative_clock,
        gap,
    };
    let cpu_top = status.budget.slice_cpus.map_or_else(
        || nice_top(cpu_peak, Base::Decimal),
        |cpus| cpus.max(cpu_peak),
    );
    html! {
        div #host .host {
            h2.visually-hidden { (slice) }
            div.charts {
                div.chart-card {
                    h3 {
                        "Memory"
                        (reading(parts, "host-memory-now", &overview.host.samples.last().map_or("–".to_owned(), |s| format::bytes(s.memory)), &format::bytes(memory_peak)))
                    }
                    (memory_chart.render(memory_top, parts))
                }
                div.chart-card {
                    h3 {
                        "CPU"
                        (reading(parts, "host-cpu-now", &overview.host.samples.last().map_or("–".to_owned(), |s| in_cores(s.cpu)), &format::cores_used(cpu_peak)))
                    }
                    (cpu_chart.render(cpu_top, parts))
                }
            }
        }
    }
}

/// A chart's latest value and its peak, which change with every sample.
fn reading(parts: &Parts, id: &str, now: &str, peak: &str) -> Markup {
    parts.html(
        id,
        html! { span #(id) .reading { (now) span.muted { " · peak " (peak) } } },
    )
}

fn relative_clock(at: u64) -> String {
    format!("{} UTC", format::clock(at))
}

/// The configured repositories, with their queue and a pause or resume button, and below them a pause and a resume
/// of every one, each while some repository is not yet in that state. A single repository has only its own button.
pub(crate) fn repositories(status: &Status) -> Markup {
    let paused = status
        .repositories
        .iter()
        .filter(|repository| repository.paused)
        .count();
    let mut every = Vec::new();
    if status.repositories.len() > 1 {
        if paused < status.repositories.len() {
            every.push(("pause", "Pause all", "Pause all repositories"));
        }
        if paused > 0 {
            every.push(("resume", "Resume all", "Resume all repositories"));
        }
    }
    html! {
        section #repositories .repositories aria-labelledby="repositories-title" {
            h2 #repositories-title { "Repositories" }
            @if status.repositories.is_empty() {
                p.muted { "No repositories yet. Add a " code { "[[repositories]]" } " table to runnerd.toml." }
            }
            ul.repo-list {
                @for repository in &status.repositories {
                    (repository_row(status, repository))
                }
                @if !every.is_empty() {
                    li.all {
                        @for (action, label, name) in every {
                            button.button-quiet type="button"
                                data-indicator:_busy_all=""
                                data-attr:aria-busy="$_busy_all && 'true'"
                                data-attr:disabled="$_busy_all"
                                data-on:click=(format!("@post('/v1/repositories/{action}')"))
                                // The name starts with the label, so voice control finds it by what it shows.
                                aria-label=(name) { (label) }
                        }
                    }
                }
            }
        }
    }
}

/// One repository's row; a pause or resume replaces it. Hovering it lights up the slots its runners hold and its
/// latest jobs.
pub(crate) fn repository_row(status: &Status, repository: &RepositoryStatus) -> Markup {
    // Coloured as its first pool's profile.
    let class = profile_class(status, repository.first_profile());
    let (action, label) = if repository.paused {
        ("resume", "Resume")
    } else {
        ("pause", "Pause")
    };
    let held: Vec<String> = status
        .profiles
        .iter()
        .flat_map(|profile| {
            profile.slots.iter().filter_map(|slot| {
                slot.runner
                    .as_ref()
                    .filter(|runner| runner.repository == repository.name)
                    .map(|_| js(&slot_id(&profile.name, slot.slot)))
            })
        })
        .collect();
    let enter = format!("$_repo = {}", js(&repository.name));
    let linked = format!(
        "$_repo == {} || [{}].includes($_slot)",
        js(&repository.name),
        held.join(", ")
    );
    html! {
        li #(repository_id(&repository.name)) class=(format!("repository {class}")) data-row
            data-on:pointerenter=(enter) data-on:focusin=(enter)
            data-on:pointerleave="evt.pointerType == 'mouse' && ($_repo = '')" data-on:focusout="$_repo = ''" data-class:linked=(linked) {
            div.repo-name {
                span.swatch aria-hidden="true" {}
                a href=(format!("/history?repository={}", repository.name)) { (repository.name) }
            }
            div.repo-meta {
                span { (repository.profiles()) }
                @if repository.queued_jobs > 0 { span { (repository.queued_jobs) " queued" } }
                @if repository.paused { span.badge.neutral { "paused" } }
            }
            // One busy signal per row, so a pending pause greys out only its own button.
            @let busy = format!("_busy_{}", repository_id(&repository.name).replace('-', "_"));
            button.button-quiet type="button"
                data-indicator=(busy)
                data-attr:aria-busy=(format!("${busy} && 'true'"))
                data-attr:disabled=(format!("${busy}"))
                data-on:click=(format!("@post('/v1/repositories/{}/{action}')", repository.name))
                aria-label=(format!("{label} {}", repository.name)) { (label) }
        }
    }
}

/// The newest jobs of the history, enough that `RECENT` are left once `recent` drops the ones `status` lists queued
/// or running: a burst of queued jobs has the newest IDs.
pub(crate) async fn read_recent(
    daemon: &Client,
    status: &Status,
) -> Result<Vec<JobSummary>, ClientError> {
    let running = status
        .profiles
        .iter()
        .flat_map(|profile| &profile.slots)
        .filter(|slot| slot.runner.is_some())
        .count();
    let queued = u32::try_from(status.queued.len().saturating_add(running)).unwrap_or(u32::MAX);
    let page: JobPage = daemon
        .get_query(
            "/v1/jobs",
            &JobQuery {
                limit: Some(RECENT.saturating_add(queued).min(MAX_JOBS)),
                ..JobQuery::default()
            },
        )
        .await?;
    Ok(page.jobs)
}

/// The latest jobs that are done: the queue lists the queued ones and the lanes the running ones.
pub(crate) fn recent(overview: &Overview) -> Markup {
    let queued = &overview.status.queued;
    let latest: Vec<&JobSummary> = overview
        .recent
        .iter()
        .filter(|job| {
            !matches!(job.state, JobState::Queued | JobState::Running)
                && !queued.iter().any(|known| known.id == job.id)
        })
        .take(RECENT as usize)
        .collect();
    html! {
        section #recent .recent aria-labelledby="recent-title" {
            div.section-head {
                h2 #recent-title { "Latest jobs" }
                a href="/history" { "All history" }
            }
            @if latest.is_empty() {
                p.muted { "No jobs yet. Jobs show here once a runner of this machine takes one." }
            } @else {
                ul.job-list {
                    @for job in latest {
                        li data-row data-class:linked=(format!("$_repo == {}", js(&job.repository))) {
                            a href=(format!("/jobs/{}", job.id)) {
                                (badge(job))
                                (job_name(&job.title()))
                                span.repo { (job.repository) }
                                span.when { (time_ago(job.timeline.queued_at, overview.now)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The figures that say whether CI keeps up, in one row: slots running, jobs waiting and the oldest's wait, the waits
/// of the last day with the longest of each hour, and the failures of the last day; then whether runnerd is stopping,
/// and, from a daemon that lists no attention, GitHub's rate limit, which the attention list says otherwise.
pub(crate) fn health(overview: &Overview) -> Markup {
    let figures = overview
        .status
        .health(overview.now)
        .figures(Some(&overview.day));
    html! {
        section #health .health aria-label="Health" {
            dl.health-figures {
                @for figure in &figures {
                    div class=(tone_class(figure.tone)) {
                        dt { (figure.label) }
                        dd {
                            @match figure.kind {
                                HealthKind::Waiting => { a.figure-main href="#queue" { (figure.value) } }
                                HealthKind::Failed => { a.figure-main href="/history?outcome=failure" { (figure.value) } }
                                HealthKind::Running | HealthKind::Wait => { span.figure-main { (figure.value) } }
                            }
                            @if let Some(of) = &figure.of { span.figure-of { " " (of) } }
                        }
                        @for note in &figure.notes {
                            dd.figure-note { (note) }
                        }
                        @if figure.kind == HealthKind::Wait { (wait_hours(&overview.day)) }
                    }
                }
            }
            @if let Some(stopping) = overview.status.stopping_line() {
                p.notice.tone-warn { "runnerd is " (stopping) "; it starts no new runners." }
            }
            @if overview.attention.is_none() && let Some(limited) = overview.status.github_line() {
                p.notice.tone-warn { (limited) }
            }
        }
    }
}

/// The longest wait that started in each hour of the last day, counted back from `until` so the 24 hours are the
/// day the figures count, oldest hour first, as bars against the longest.
fn wait_hours(day: &Activity) -> Markup {
    let since = day.until.saturating_sub(DAY);
    let hours = day.wait_hours();
    let longest = hours.iter().copied().max().unwrap_or(0);
    html! {
        @if longest > 0 {
            dd.wait-hours aria-hidden="true" {
                @for (index, wait) in hours.iter().enumerate() {
                    @let start = since + index as u64 * 3600;
                    span style=(format!("--h: {:.3}", crate::chart::ratio(*wait, longest)))
                        title=(format!("From {}: longest wait {}", relative_clock(start), format::duration(*wait))) {}
                }
            }
        }
    }
}

/// Every queued job, oldest first, how long it has waited and why. A row opens the job on GitHub and lights up its
/// repository.
pub(crate) fn queue(overview: &Overview) -> Markup {
    let queued = &overview.status.queued;
    let more = queued.len().saturating_sub(QUEUED_SHOWN);
    html! {
        section #queue .queue aria-labelledby="queue-title" {
            div.section-head {
                h2 #queue-title { "Waiting" }
                // A phone shows the first three; the link to the rest shows there whenever more wait.
                @if queued.len() > QUEUED_SHOWN_NARROW {
                    a.narrow-only[more == 0] href="/history?outcome=queued" { "All " (queued.len()) " queued" }
                }
            }
            @if queued.is_empty() {
                p.muted { "Nothing waiting: every queued job has a runner." }
            } @else {
                ol.queue-list {
                    @for job in queued.iter().take(QUEUED_SHOWN) {
                        (queued_row(overview, job))
                    }
                }
            }
        }
    }
}

fn queued_row(overview: &Overview, job: &QueuedJob) -> Markup {
    let repository = js(&job.repository);
    let enter = format!("$_repo = {repository}");
    html! {
        li data-row data-class:linked=(format!("$_repo == {repository}"))
            data-on:pointerenter=(enter) data-on:focusin=(enter)
            data-on:pointerleave="evt.pointerType == 'mouse' && ($_repo = '')"
            data-on:focusout="$_repo = ''" {
            a href=(job.url.clone().unwrap_or_else(|| format!("/jobs/{}", job.id))) {
                span.waited {
                    @if job.queued_at > 0 {
                        span.visually-hidden { "Waited " }
                        (format::duration(overview.now.saturating_sub(job.queued_at)))
                    } @else {
                        "–"
                    }
                }
                (job_name(&job.title()))
                span.repo {
                    (job.place(&overview.status))
                    @if let Some(branch) = &job.branch { " · " (branch) }
                }
                @if let Some(waiting) = &job.waiting {
                    span class=(format!("reason {}", reason_class(waiting.kind()))) { (waiting.reason(job, &overview.status, overview.now)) }
                }
            }
        }
    }
}

/// How a reason is drawn: one the owner can act on, one that clears by itself, what is full, or one this client
/// does not know, drawn plain.
pub(crate) fn reason_class(kind: WaitKind) -> &'static str {
    match kind {
        WaitKind::Act => "act",
        WaitKind::Soon => "soon",
        WaitKind::Full => "full",
        WaitKind::Other => "other",
    }
}

/// A job's name after its workflow's, which reads quieter: `CI / build`, as every client names a job.
fn job_name(title: &JobTitle<'_>) -> Markup {
    html! {
        span.job-name {
            @if let Some(workflow) = title.workflow { span.workflow { (workflow) " / " } }
            (title.name)
        }
    }
}
