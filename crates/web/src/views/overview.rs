//! `/`: the four figures that say whether CI keeps up, at a size that reads from across a desk, with what needs
//! attention as one line beside the title that opens to the list; then the budget as lanes, which are the slots
//! themselves: a lane per slot as wide as its share of the slice, filled to what its job uses now and carrying the
//! job's name, step, time and sparklines when wide enough, then a lane each for the mirror, seeds, shells, what is free
//! and what the host holds, each labelled with its figure. Under them the work: the oldest queued jobs with why each
//! waits and a link to `/queue`, which lists every one (ADR 0174), the latest jobs, the repositories with pause and
//! resume, and the slice's CPU and memory over the last half hour.
//! A wide container lays the lanes side by side as one bar; a narrow one stacks them as rows, each over a strip of
//! the slice with its block where it sits, so the blocks read down the rows as the bar reads across. Hovering,
//! focusing or tapping a slot's lane, job row or repository lights up the others that are the same slot. That is the
//! order for reading and for the keyboard on every screen.

use crate::views::Here;
use std::collections::BTreeMap;

use super::local_pages::LocalPages;

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
/// Queued jobs listed, the oldest; the queue's page lists every one.
const QUEUED_SHOWN: usize = 6;

/// How many queued jobs a phone shows; the stylesheet hides the rest, so the two agree.
const QUEUED_SHOWN_NARROW: usize = 3;
/// How far back the health figures look: the 24 hours up to now.
const DAY: u64 = 24 * 3600;
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
    /// The queued jobs the history holds, which have a page here.
    pub(crate) local: LocalPages,
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
            local: LocalPages::default(),
        };
        overview.local = LocalPages::read(daemon, &overview.status).await;
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
        region("queue", &|parts| queue(overview, parts)),
        region("recent", &|_| recent(overview)),
        region("repositories", &|_| {
            repositories_noting(&overview.status, overview.attention.as_ref())
        }),
    ]
}

/// The whole page.
pub(crate) fn render(overview: &Overview, here: Here<'_>) -> Markup {
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
        (heading(View::Overview.title(), Some(Page::Overview), Some(attention.1)))
        div.overview "data-signals:_slot__ifmissing"="''" "data-signals:_repo__ifmissing"="''" {
            (health.1)
            section.machine aria-labelledby="budget-title" {
                (budget.1)
                (slots.1)
            }
            div.work {
                (queue.1)
                (recent.1)
                (repositories.1)
                (host.1)
            }
            @if let Some(savings) = &overview.savings {
                p.savings { (savings.month.headline()) }
            }
        }
    };
    page(
        View::Overview.title(),
        here,
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
            Fix::Open { view } => { a.hit href=(Page::of(*view).path()) { (fix.line()) } }
            Fix::Config { .. } => { a.hit href=(Page::of_section(Section::Config).path()) { (fix.line()) } }
            Fix::Text { .. } => { (fix.line()) }
        }
    }
}

fn attention_item(item: &AttentionItem, now: u64) -> Markup {
    let href = subject_href(&item.subject);
    html! {
        li class=(format!("attention-item {}", tone_class(item.tone))) data-row tabindex="-1" {
            span.tone-mark aria-hidden="true" {}
            @if let Some(href) = &href {
                a.attention-title.hit href=(href) { (item.title) }
            } @else {
                span.attention-title { (item.title) }
            }
            @if let Some(found) = &item.fix { span.attention-fix { (fix(found)) } }
            @if let Some(since) = item.since { span.attention-since { (time_ago(since, now)) } }
        }
    }
}

/// What needs attention now, as one line: how many items, the most urgent one's title, and core's count of the
/// rest; it opens to every item, most urgent first, as core orders and words them. Nothing at all when nothing does.
pub(crate) fn attention(overview: &Overview) -> Markup {
    html! {
        @match &overview.attention {
            None => {
                section #attention .attention.unsupported aria-label="Needs attention" {
                    p.muted { (format::sentence(&Feature::Attention.unsupported())) "." }
                }
            }
            Some(attention) => {
                @match attention.items.first() {
                    None => { div #attention hidden {} }
                    Some(worst) => {
                        // The signal holds whether it is open, so a live update, which replaces the region, keeps it
                        // so. A click on the summary sets the signal and nothing else: a native toggle as well would
                        // be undone by the signal in the same task. Without JavaScript the native fold opens.
                        details #attention class=(format!("attention {}", tone_class(worst.tone)))
                            "data-signals:_attention__ifmissing"="false" data-attr:open="$_attention" {
                            summary "data-on:click__prevent"="$_attention = !$_attention" {
                                span.visually-hidden { "Needs attention: " }
                                span.attention-count { span.tone-mark aria-hidden="true" {} (attention.count()) }
                                span.attention-worst { (worst.title) }
                                @if let Some(more) = attention.more(1) { span.attention-rest { (more) } }
                            }
                            ul.attention-list {
                                @for item in &attention.items {
                                    (attention_item(item, overview.now))
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

/// The budget's head: what is reserved of the slice as its heading, then the CPUs reserved and the host's figures,
/// small. The lanes under it are the bar.
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

/// Where a lane sits on the slice: from where, and how much of it, each a share of the whole.
#[derive(Debug, Clone, Copy)]
struct Share {
    from: f64,
    size: f64,
}

impl Share {
    /// The lane's style: where its block starts on the slice and how much of it the block takes.
    fn style(self) -> String {
        format!("--from: {:.4}; --share: {:.4}", self.from, self.size)
    }
}

/// The lane's block, filled to `fill` of it when a job's memory is sampled: a new sample animates a transform, never
/// the layout.
fn block(fill: Option<(&str, f64)>, parts: &Parts) -> Markup {
    html! {
        span.lane-block aria-hidden="true" {
            @if let Some((id, fill)) = fill {
                (parts.html(id, html! { span.fill #(id) style=(format!("--fill: {fill:.3}")) {} }))
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

/// The machine's lanes, which are its budget bar: a lane per slot, in config order and its profile's colour, as wide
/// as the slot's memory on the slice; then a lane for the mirror, each seed container and shell, what is free, and
/// what no runner may start in, each its figure over its short name, core's whole sentence in its title. A running
/// lane carries its job, its step, its time and its memory and CPU over the last minutes, and fills to its last
/// sampled memory; a lane too narrow for that shows less, down to its name alone.
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
    // The lanes that hold memory make the bar; below the room they need, they are rows.
    let barred = bar.slots.iter().filter(|part| part.memory > 0).count()
        + held.len()
        + usize::from(free.is_some())
        + usize::from(closed.is_some());
    html! {
        div #slots .slots {
            h2.visually-hidden { "Slots" }
            @if status.profiles.is_empty() {
                p.muted { "No profiles in runnerd.toml, so no slots." }
            }
            div.lanes-fit style=(format!("--fit: {:.4}", lanes_fit(barred))) {
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
                        @let line = format!("{} {}", format::bytes(part.memory), part.holder());
                        li class=(format!("lane part {class}")) style=(share.style()) aria-label=(line) title=(line) {
                            (block(None, parts))
                            span.lane-size { (super::block_size(part.memory)) }
                            span.lane-name { (part.name()) }
                        }
                    }
                    @if let Some((part, share)) = free {
                        li.lane.part.free style=(share.style()) aria-label=(part.reading.line()) title=(part.reading.line()) {
                            (block(None, parts))
                            span.lane-size { (part.reading.figure) }
                            span.lane-name { (part.name()) }
                        }
                    }
                    @if let Some((part, share)) = closed {
                        li.lane.part.closed style=(share.style()) aria-label=(part.reading.line()) title=(part.reading.line()) {
                            (block(None, parts))
                            span.lane-size { (part.reading.figure) }
                            span.lane-name { (part.kind.name()) }
                        }
                    }
                }
            }
        }
    }
}

/// How wide, in rem, each lane in the bar must be able to get for the bar to be worth drawing: more than its 1.75rem
/// floor, where it is a sliver with its name on its side and the proportions are gone; and the gap between lanes.
const LANE_FLOOR: f64 = 2.5;
const LANE_GAP: f64 = 0.125;
/// The narrowest container that lays the lanes out as a bar, in rem: the stylesheet's `@container slots` bound.
const BAR_FROM: f64 = 44.0;

/// How much of the slots' width the bar's container query sees, so the lanes turn into rows where `barred` lanes at
/// `LANE_FLOOR` no longer fit, rather than at the stylesheet's bound alone: 1 while they fit in it.
fn lanes_fit(barred: usize) -> f64 {
    let lanes = f64::from(u32::try_from(barred).unwrap_or(u32::MAX));
    let needed = lanes * LANE_FLOOR + (lanes - 1.0).max(0.0) * LANE_GAP;
    if needed <= BAR_FROM {
        1.0
    } else {
        BAR_FROM / needed
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
    // A slot that holds none of the budget, such as an idle one under measured admission, is a lane of no width.
    let style = share.map_or_else(
        || {
            Share {
                from: 1.0,
                size: 0.0,
            }
            .style()
        },
        |(share, _)| share.style(),
    );
    let fill = share.and_then(|(_, reserved)| {
        memory.map(|used| {
            (
                fill_id.as_str(),
                crate::chart::ratio(used, reserved).min(1.0),
            )
        })
    });
    // A slot that holds none of the budget leaves the bar for the row of such slots under it.
    let empty = if share.is_none_or(|(_, reserved)| reserved == 0) {
        " empty"
    } else {
        ""
    };
    html! {
        li #(key) class=(format!("lane slot {class} {state}{empty}")) style=(style) data-row
            aria-label=(format!("{} {}: {}", profile.name, slot.slot, slot_state.line()))
            data-on:pointerenter=(link.enter) data-on:focusin=(link.enter)
            data-on:pointerleave=(link.pointer_leave) data-on:focusout=(link.leave) data-class:linked=(link.linked) {
            (block(fill, parts))
            span.lane-name { (profile.name) " " (slot.slot) }
            @if let Some((_, reserved)) = share { span.lane-size { (super::block_size(reserved)) } }
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
    repositories_noting(status, None)
}

/// `repositories`, each row with what of `attention` concerns its repository, in core's words.
fn repositories_noting(status: &Status, attention: Option<&Attention>) -> Markup {
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
            // Pause and resume of every one sit by the title, as the section's own actions, not as a last row.
            div.section-head {
                h2 #repositories-title { "Repositories" }
                @if !every.is_empty() {
                    div.all {
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
            @if status.repositories.is_empty() {
                p.muted { "No repositories yet. Add a " code { "[[repositories]]" } " table to runnerd.toml." }
            }
            ul.repo-list {
                @for repository in &status.repositories {
                    (repository_row_noting(status, repository, attention))
                }
            }
        }
    }
}

/// One repository's row; a pause or resume replaces it. Hovering it lights up the slots its runners hold and its
/// latest jobs.
pub(crate) fn repository_row(status: &Status, repository: &RepositoryStatus) -> Markup {
    repository_row_noting(status, repository, None)
}

/// `repository_row`, with the items of `attention` about the repository under its name.
fn repository_row_noting(
    status: &Status,
    repository: &RepositoryStatus,
    attention: Option<&Attention>,
) -> Markup {
    let notes: Vec<&AttentionItem> = attention
        .map(|attention| {
            attention
                .items
                .iter()
                .filter(|item| matches!(&item.subject, Subject::Repository { name } if *name == repository.name))
                .collect()
        })
        .unwrap_or_default();
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
            @for item in &notes {
                p class=(format!("repo-note {}", tone_class(item.tone))) {
                    span.tone-mark aria-hidden="true" {} (item.title)
                }
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
            @if let Some(battery) = overview.status.battery_line() {
                p.notice.tone-warn { (battery) }
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

/// The oldest queued jobs, how long each has waited and why: `QUEUED_SHOWN` on a wide screen, `QUEUED_SHOWN_NARROW`
/// on a phone, and while more wait than a screen shows, a link to the page of every one (ADR 0174). A row opens the
/// job's page and lights up its repository.
pub(crate) fn queue(overview: &Overview, parts: &Parts) -> Markup {
    let queued = &overview.status.queued;
    let more = queued.len().saturating_sub(QUEUED_SHOWN);
    html! {
        section #queue .queue aria-labelledby="queue-title" {
            div.section-head {
                h2 #queue-title { "Waiting" }
                // A phone shows the first three; the link to the rest shows there whenever more wait.
                @if queued.len() > QUEUED_SHOWN_NARROW {
                    a class=[(more == 0).then_some("narrow-only")] href=(Page::Queue.path()) { "All " (queued.len()) " queued" }
                }
            }
            // A repository left out of the list is named on the queue's page, so the overview links there however
            // few jobs it shows.
            @if let Some(unlisted) = overview.status.unlisted_line() {
                p.queue-unlisted.tone-warn { a href=(Page::Queue.path()) { (unlisted) } }
            }
            @if queued.is_empty() {
                p.muted { (overview.status.empty_queue_line()) }
            } @else {
                ol.queue-list {
                    @for job in queued.iter().take(QUEUED_SHOWN) {
                        (queued_row(&overview.status, job, overview.now, &overview.local, parts))
                    }
                }
            }
        }
    }
}

/// A queued job's row: how long it has waited, its name, where it runs and why it waits, as core words it. It opens
/// the job's page when the history holds the job (`local`), with GitHub's page as a second link; else GitHub's page;
/// else nothing, and the row is only read. Its wait is a part, so the time passing sends only that.
pub(crate) fn queued_row(
    status: &Status,
    job: &QueuedJob,
    now: u64,
    local: &LocalPages,
    parts: &Parts,
) -> Markup {
    let repository = js(&job.repository);
    let enter = format!("$_repo = {repository}");
    let waited = format!("waited-{}", job.id);
    let title = job.title();
    let main = html! {
        (parts.html(&waited, html! {
            span.waited #(waited) {
                @if job.queued_at > 0 {
                    span.visually-hidden { "Waited " }
                    (format::duration(now.saturating_sub(job.queued_at)))
                } @else {
                    "–"
                }
            }
        }))
        (job_name(&title))
        span.repo {
            (job.place(status))
            @if let Some(branch) = &job.branch { " · " (branch) }
        }
        @if let Some(waiting) = &job.waiting {
            span class=(format!("reason {}", reason_class(waiting.kind()))) { (waiting.reason(job, status, now)) }
        }
    };
    html! {
        li #(format!("queued-{}", job.id)) data-row data-class:linked=(format!("$_repo == {repository}"))
            data-on:pointerenter=(enter) data-on:focusin=(enter)
            data-on:pointerleave="evt.pointerType == 'mouse' && ($_repo = '')"
            data-on:focusout="$_repo = ''" {
            @match (local.has(job.id), &job.url) {
                (true, url) => {
                    a.queue-main href=(format!("/jobs/{}", job.id)) { (main) }
                    @if let Some(url) = url {
                        a.queue-github href=(url) rel="noopener" aria-label=(format!("{} on GitHub", title.name)) { "GitHub" }
                    }
                }
                (false, Some(url)) => { a.queue-main href=(url) rel="noopener" { (main) } }
                (false, None) => { div.queue-main tabindex="-1" { (main) } }
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
