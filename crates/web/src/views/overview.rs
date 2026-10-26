//! `/`: first whether CI is keeping up (the slots running, the jobs waiting, the waits and failures of the last
//! day) and every queued job with why it waits; then the memory budget as blocks, every slot and the job it runs,
//! the latest jobs, the repositories with pause and resume, and the slice's CPU and memory over the last half hour.

use std::collections::BTreeMap;

use maud::{Markup, html};
use runnerd_core::api::{
    Activity, ActivityQuery, ClosedPart, HeldKind, HeldPart, HostSample, HostSamples, Job, JobPage,
    JobQuery, JobState, JobSummary, JobTitle, ProfileStatus, QueuedJob, RepositoryStatus, Sample,
    SampleSeries, Savings, SlotState, SlotStatus, Status, Tone, WaitKind, cpus, in_cores,
};
use runnerd_core::client::{Client, ClientError};

use super::{
    Live, Page, Parts, badge, digest, js, page, profile_class, repository_id, slot_id, time_ago,
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
const RECENT: u32 = 8;
/// The most jobs `GET /v1/jobs` lists at once.
const MAX_JOBS: u32 = 500;
/// Queued jobs listed, the oldest; the history lists the rest.
const QUEUED_SHOWN: usize = 8;
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
    /// What the jobs' minutes would have cost on GitHub's runners; `None` from a daemon before it.
    pub(crate) savings: Option<Savings>,
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
        // A daemon before the savings route has none to show.
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
pub(crate) fn regions(overview: &Overview) -> [(&'static str, Markup, Parts); 7] {
    let region = |id, render: &dyn Fn(&Parts) -> Markup| {
        let parts = Parts::default();
        let markup = render(&parts);
        (id, markup, parts)
    };
    [
        region("health", &|_| health(overview)),
        region("queue", &|_| queue(overview)),
        region("budget", &|parts| budget(overview, parts)),
        region("slots", &|parts| slots(overview, parts)),
        region("host", &|parts| host(overview, parts)),
        region("repositories", &|_| repositories(&overview.status)),
        region("recent", &|_| recent(overview)),
    ]
}

/// The whole page.
pub(crate) fn render(overview: &Overview) -> Markup {
    let [health, queue, budget, slots, host, repositories, recent] = regions(overview);
    let live = Live {
        url: "/live/overview",
        at: overview.now,
        log: None,
        regions: [
            &health,
            &queue,
            &budget,
            &slots,
            &host,
            &repositories,
            &recent,
        ]
        .iter()
        .map(|(id, markup, _)| (*id, digest(&markup.0)))
        .collect(),
    };
    let main = html! {
        h1.visually-hidden { "Overview" }
        (health.1)
        (queue.1)
        (budget.1)
        (slots.1)
        div.split {
            (recent.1)
            (repositories.1)
        }
        (host.1)
    };
    page("Overview", Some(Page::Overview), Some(&live), &main)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "sizes as flex weights in GiB; precision far beyond what a pixel shows"
)]
fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}

/// What an element standing for a slot does on hover, focus and touch: it selects the slot, which outlines its
/// block, card and job rows and shows its line of the budget's readout. Only the key is kept, so each live update
/// renders that line from the new data.
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

/// What the budget's readout shows for a block: a figure in bold, and the words after it.
#[derive(Clone)]
struct Lens {
    figure: String,
    text: String,
}

/// A slot's lens, its card's id, and the job it runs, if any.
struct SlotView<'overview> {
    key: String,
    job: Option<&'overview Job>,
    memory: Option<u64>,
    lens: Lens,
}

fn slot_view<'overview>(
    overview: &'overview Overview,
    profile: &ProfileStatus,
    slot: &SlotStatus,
) -> SlotView<'overview> {
    let job = overview.job_in(&profile.name, slot.slot);
    let memory = job
        .and_then(|job| overview.samples.get(&job.summary.id))
        .and_then(|samples| samples.last())
        .map(|sample| sample.memory);
    let name = format!("{} {}", profile.name, slot.slot);
    let memory_limit = job
        .and_then(|job| job.limits(Some(profile)).memory)
        .map_or(profile.memory, |limit| limit.value);
    let cap = format::bytes(memory_limit);
    let lens = match (&slot.runner, job, memory) {
        (Some(_), Some(job), Some(memory)) => Lens {
            figure: format::bytes(memory),
            text: format!(
                "of {cap} in {name}, {} on {}",
                job.summary.title(),
                job.summary.repository
            ),
        },
        (Some(_), Some(job), None) => Lens {
            figure: cap,
            text: format!(
                "{name}, {} on {}",
                job.summary.title(),
                job.summary.repository
            ),
        },
        _ => Lens {
            figure: cap,
            text: format!("{name}, {}", Status::slot_state(profile, slot).line()),
        },
    };
    SlotView {
        key: slot_id(&profile.name, slot.slot),
        job,
        memory,
        lens,
    }
}

/// The budget: the slice as the ground, a block per slot sized by its memory cap (under measured admission, a block
/// per runner sized by what it reserves), filled as high as the job's last sampled memory; the mirror and each seed
/// container; what is free, and what no runner may start in. A block opens its job, or its slot's card;
/// hovering or focusing one says what it holds in the readout above the bar.
pub(crate) fn budget(overview: &Overview, parts: &Parts) -> Markup {
    let status = &overview.status;
    let budget = &status.budget;
    let bar = status.budget_parts();
    let slot_blocks: Vec<(&ProfileStatus, &SlotStatus, u64)> = bar
        .slots
        .iter()
        .filter_map(|part| {
            let profile = status.profiles.get(part.profile)?;
            let slot = profile.slots.iter().find(|slot| slot.slot == part.slot)?;
            Some((profile, slot, part.memory))
        })
        .collect();
    let held = held(status, &bar.held);
    let used_now = overview.host.samples.last().map(|sample| sample.memory);
    // What is in use now is the readout's, which changes with every sample.
    let label = format!(
        "Memory budget of {}: {}",
        budget.slice,
        budget.memory_reading().line(),
    );
    let free = bar.free.as_ref().map(|free| free.memory);
    let free_lens = bar.free.as_ref().map(|free| Lens {
        figure: free.reading.figure.clone(),
        text: free.reading.text.clone(),
    });
    let mut lenses: Vec<(String, Lens)> = status
        .profiles
        .iter()
        .flat_map(|profile| {
            profile.slots.iter().map(|slot| {
                let view = slot_view(overview, profile, slot);
                (view.key, view.lens)
            })
        })
        .collect();
    lenses.extend(
        held.iter()
            .map(|part| (part.key.clone(), part.lens.clone())),
    );
    if let Some(lens) = &free_lens {
        lenses.push(("free".to_owned(), lens.clone()));
    }
    let closed = bar.closed.as_ref().map(|closed| {
        (
            closed,
            Lens {
                figure: closed.reading.figure.clone(),
                text: closed.reading.text.clone(),
            },
        )
    });
    if let Some((_, lens)) = &closed {
        lenses.push(("closed".to_owned(), lens.clone()));
    }
    html! {
        section #budget .budget aria-labelledby="budget-title"
            "data-signals:_slot__ifmissing"="''" "data-signals:_repo__ifmissing"="''" {
            h2 #budget-title .visually-hidden { "Budget" }
            (readout(overview, used_now, &lenses, parts))
            div.bar role="group" aria-label=(label) {
                @for (profile, slot, memory) in &slot_blocks {
                    (block(overview, profile, slot, *memory, parts))
                }
                @for part in &held {
                    @let link = link(&part.key, None);
                    div class=(format!("block {}", part.class)) tabindex="0" role="img"
                        style=(format!("--size: {:.3}", gib(part.memory)))
                        aria-label=(format!("{} {}", part.lens.figure, part.lens.text))
                        data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
                        data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
                        (super::block_size(part.memory))
                        span.name { (part.name) }
                    }
                }
                @if let Some((free, lens)) = free.zip(free_lens.as_ref()) {
                    @let link = link("free", None);
                    div.free tabindex="0" role="img" style=(format!("--size: {:.3}", gib(free)))
                        aria-label=(format!("{} {}", lens.figure, lens.text))
                        data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
                        data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
                        (super::block_size(free))
                    }
                }
                @if let Some((part, lens)) = &closed {
                    @let link = link("closed", None);
                    div.free.closed tabindex="0" role="img" style=(format!("--size: {:.3}", gib(part.memory)))
                        aria-label=(format!("{} {}", lens.figure, lens.text))
                        data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
                        data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
                        (super::block_size(part.memory))
                        span.name { (part.kind.name()) }
                    }
                }
            }
            (bar_keys(&held, free_lens.as_ref(), closed.as_ref()))
        }
    }
}

/// The slice's total, and beside it the selected block's line. Each block has its own name, so the lines are for the
/// eye only.
fn readout(
    overview: &Overview,
    used_now: Option<u64>,
    lenses: &[(String, Lens)],
    parts: &Parts,
) -> Markup {
    let budget = &overview.status.budget;
    let reading = budget.memory_reading();
    html! {
        div.figures {
            (parts.html("budget-readout", html! {
                p #budget-readout .readout {
                    span.big { (reading.figure) }
                    span { " " (reading.text) }
                    @if let Some(used) = used_now {
                        span.muted { " · " (format::bytes(used)) " in use" }
                    }
                    @if let Some(cpu) = budget.cpu {
                        span.muted { " · " (cpu.line()) }
                    }
                    @if let Some(host) = budget.host_line() {
                        span.muted { " · " (host) }
                    }
                }
            }))
            // The blocks' descriptions, so a block's name stays what it shows.
            p.lens aria-hidden="true" {
                @for (key, lens) in lenses {
                    // Every line takes its place, hidden but for the selected one, so the readout is as tall as its
                    // longest line and selecting a block never moves the bar.
                    @let id = lens_id(key);
                    (parts.html(&id, html! {
                        span #(id) data-class:on=(format!("$_slot == {}", js(key))) {
                            strong { (lens.figure) } " " (lens.text)
                        }
                    }))
                }
            }
        }
    }
}

/// The id of a block's line in the readout, which also describes the block.
fn lens_id(key: &str) -> String {
    format!("lens-{key}")
}

/// On a touch screen the mirror, a seed container, the free remainder and what is closed may be too thin to tap: keys
/// under the bar select them instead, and say their sizes.
fn bar_keys(
    held: &[Held],
    free_lens: Option<&Lens>,
    closed: Option<&(&ClosedPart, Lens)>,
) -> Markup {
    html! {
        @if !held.is_empty() || free_lens.is_some() || closed.is_some() {
            div.bar-keys {
                @for part in held {
                    @let key = js(&part.key);
                    button class=(format!("bar-key {}", part.class)) type="button"
                        data-on:click=(format!("$_slot = $_slot == {key} ? '' : {key}"))
                        data-class:linked=(format!("$_slot == {key}")) {
                        span.swatch aria-hidden="true" {} (part.lens.figure) " " (part.name)
                    }
                }
                @if let Some(lens) = free_lens {
                    button.bar-key.key-free type="button" data-on:click="$_slot = $_slot == 'free' ? '' : 'free'"
                        data-class:linked="$_slot == 'free'" {
                        span.swatch aria-hidden="true" {} (lens.figure) " free"
                    }
                }
                @if let Some((part, lens)) = closed {
                    button.bar-key.key-closed type="button" data-on:click="$_slot = $_slot == 'closed' ? '' : 'closed'"
                        data-class:linked="$_slot == 'closed'" {
                        span.swatch aria-hidden="true" {} (lens.figure) " " (part.kind.name())
                    }
                }
            }
        }
    }
}

/// Memory the budget holds for something other than a slot: the mirror, a profile's seed container, or a shell.
struct Held {
    key: String,
    /// The block's classes.
    class: String,
    /// What its block says under its size.
    name: String,
    memory: u64,
    lens: Lens,
}

/// The mirror, each seed container, in the colour of the profile it reseeds, then each shell open in a record.
fn held(status: &Status, held: &[HeldPart]) -> Vec<Held> {
    held.iter()
        .map(|part| {
            let (key, class) = match &part.kind {
                HeldKind::Mirror => ("mirror".to_owned(), "mirror".to_owned()),
                HeldKind::Seed { profile } => (
                    format!("seed-{}", slot_id(profile, 0)),
                    format!("seed {}", profile_class(status, profile)),
                ),
                HeldKind::Shell { job } => (format!("shell-{job}"), "shell".to_owned()),
            };
            Held {
                key,
                class,
                name: part.name(),
                memory: part.memory,
                lens: Lens {
                    figure: format::bytes(part.memory),
                    text: part.holder(),
                },
            }
        })
        .collect()
}

fn block(
    overview: &Overview,
    profile: &ProfileStatus,
    slot: &SlotStatus,
    memory: u64,
    parts: &Parts,
) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let view = slot_view(overview, profile, slot);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a ratio of two sizes for a CSS fill"
    )]
    let fill = view
        .memory
        .map(|used| (used as f64 / memory.max(1) as f64).min(1.0));
    let state = match Status::slot_state(profile, slot) {
        SlotState::Idle | SlotState::Reseeding => "idle",
        SlotState::Running { .. } => "busy",
        SlotState::Warm { .. } => "starting warm",
        SlotState::Starting { .. } => "starting",
    };
    let href = view.job.map_or(format!("#{}", view.key), |job| {
        format!("/jobs/{}", job.summary.id)
    });
    let repository = slot
        .runner
        .as_ref()
        .map(|runner| runner.repository.as_str());
    let link = link(&view.key, repository);
    let fill_id = format!("fill-{}", view.key);
    html! {
        a class=(format!("block {class} {state}")) href=(href)
            style=(format!("--size: {:.3}", gib(memory)))
            aria-describedby=(lens_id(&view.key))
            data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
            data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
            @if let Some(fill) = fill {
                (parts.html(&fill_id, html! {
                    span.fill #(fill_id) aria-hidden="true" style=(format!("--fill: {fill:.3}")) {}
                }))
            }
            // Its name is what it shows, for voice control: the space joins the two as words.
            (super::block_size(memory))
            " "
            span.name { (profile.name) " " (slot.slot) }
        }
    }
}

/// A card per slot: what runs in it, its current step, and its memory and CPU over the last minutes.
pub(crate) fn slots(overview: &Overview, parts: &Parts) -> Markup {
    let status = &overview.status;
    html! {
        section #slots .slots aria-labelledby="slots-title" {
            h2 #slots-title .visually-hidden { "Slots" }
            @if status.profiles.is_empty() {
                p.muted { "No profiles in runnerd.toml, so no slots." }
            }
            ul.slot-grid {
                @for profile in &status.profiles {
                    @for slot in &profile.slots {
                        (slot_card(overview, profile, slot, parts))
                    }
                }
            }
        }
    }
}

fn slot_card(
    overview: &Overview,
    profile: &ProfileStatus,
    slot: &SlotStatus,
    parts: &Parts,
) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let view = slot_view(overview, profile, slot);
    let slot_state = Status::slot_state(profile, slot);
    let state = match slot_state {
        SlotState::Idle | SlotState::Reseeding => " idle",
        SlotState::Warm { .. } => " starting warm",
        SlotState::Starting { .. } => " starting",
        SlotState::Running { .. } => "",
    };
    let repository = slot
        .runner
        .as_ref()
        .map(|runner| runner.repository.as_str());
    let link = link(&view.key, repository);
    html! {
        li #(view.key) class=(format!("slot {class}{state}"))
            data-on:pointerenter=(link.enter) data-on:focusin=(link.enter)
            data-on:pointerleave=(link.pointer_leave) data-on:focusout=(link.leave) data-class:linked=(link.linked) {
            div.slot-head {
                h3.slot-name { (profile.name) " " (slot.slot) }
                span.slot-caps { (slot_caps(profile, view.job)) }
            }
            @match (&slot.runner, view.job) {
                (None, _) => {
                    @if slot_state == SlotState::Reseeding {
                        p.slot-idle { (format::sentence(&slot_state.line())) }
                    } @else {
                        p.visually-hidden { (format::sentence(&slot_state.line())) }
                    }
                }
                (Some(_), Some(job)) if slot_state.is_running() => { (running_job(overview, profile, job, &view.key, parts)) }
                (Some(runner), _) => {
                    span.repo { (runner.repository) }
                    @let id = format!("{}-waiting", view.key);
                    p.slot-idle { (format::sentence(&slot_state.line())) ", " (parts.html(&id, html! { span.muted #(id) { "started " (time_ago(runner.started_at, overview.now)) } })) }
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
        parts.html(&id, html! { dd #(id) { (text) } })
    };
    let elapsed = format!("{key}-elapsed");
    html! {
        div.slot-title {
            a.slot-job href=(format!("/jobs/{}", summary.id)) {
                span.repo { (summary.repository) }
                span.job-name { (summary.title().name) }
            }
            (parts.html(&elapsed, html! {
                p.elapsed #(elapsed) { span.visually-hidden { "Running for " } (format::duration(now.saturating_sub(started))) }
            }))
        }
        p.step {
            @if let Some((place, step)) = current {
                span.step-count { (place) "/" (job.steps.len()) }
                " " (step.name)
            } @else {
                "Setting up"
            }
        }
        dl.slot-figures {
            div.memory {
                dt.visually-hidden { "Memory" }
                (figure("memory", last.map_or("–".to_owned(), |sample| format::bytes(sample.memory))))
                (sparkline(&memory, limits.memory.map_or(profile.memory, |limit| limit.value), "memory"))
            }
            div.cpu {
                dt.visually-hidden { "CPU" }
                (figure("cpu", last.map_or("–".to_owned(), |sample| in_cores(sample.cpu))))
                (sparkline(&cpu, limits.cpu.map_or(u64::from(profile.cpus) * 1000, |limit| limit.value), "cpu"))
            }
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
        section #host .host aria-labelledby="host-title" {
            h2 #host-title .visually-hidden { "Slice" }
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
            h2 #repositories-title .visually-hidden { "Repositories" }
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
    let class = profile_class(status, &repository.profile);
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
        li #(repository_id(&repository.name)) class=(format!("repository {class}"))
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

/// The newest jobs of the history, enough that `RECENT` are left once `recent` drops the ones `status` lists queued:
/// a burst of queued jobs has the newest IDs.
pub(crate) async fn read_recent(
    daemon: &Client,
    status: &Status,
) -> Result<Vec<JobSummary>, ClientError> {
    let queued = u32::try_from(status.queued.len()).unwrap_or(u32::MAX);
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

/// The latest jobs, any state but queued, which the queue lists. A running job's row lights up its slot.
pub(crate) fn recent(overview: &Overview) -> Markup {
    let queued = &overview.status.queued;
    // The history has queued jobs too; the queue shows them.
    let latest: Vec<&JobSummary> = overview
        .recent
        .iter()
        .filter(|job| {
            job.state != JobState::Queued && !queued.iter().any(|known| known.id == job.id)
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
                        @let running = job.runner.as_ref().filter(|_| job.state == JobState::Running);
                        @let key = running.map(|runner| js(&slot_id(&runner.profile, runner.slot)));
                        @let linked = match &key {
                            Some(key) => format!("$_slot == {key} || $_repo == {}", js(&job.repository)),
                            None => format!("$_repo == {}", js(&job.repository)),
                        };
                        @let enter = key.as_ref().map(|key| format!("$_slot = {key}"));
                        li data-class:linked=(linked)
                            data-on:pointerenter=[enter.as_ref()] data-on:focusin=[enter.as_ref()]
                            data-on:pointerleave=[enter.as_ref().map(|_| "evt.pointerType == 'mouse' && ($_slot = '')")]
                            data-on:focusout=[enter.as_ref().map(|_| "$_slot = ''")] {
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

/// The figures that say whether CI keeps up: slots running, jobs waiting and the oldest's wait, the waits of the
/// last day with the longest of each hour, and the failures of the last day.
pub(crate) fn health(overview: &Overview) -> Markup {
    let health = overview.status.health(overview.now);
    let day = &overview.day;
    let waits = &day.started;
    let p95 = day.wait_p95();
    html! {
        section #health .health aria-labelledby="health-title" {
            h2 #health-title .visually-hidden { "Health" }
            dl.headline.health-figures {
                div {
                    dt { "Running" }
                    dd { span.figure-main { (health.running) } span.figure-of { " of " (health.slots) " slots" } }
                    @if health.starting > 0 {
                        dd.figure-note { (health.starting) " runner" @if health.starting > 1 { "s" } " starting" }
                    }
                    @if health.warm > 0 {
                        dd.figure-note { (health.warm) " warm, waiting for jobs" }
                    }
                }
                div class=(tone_class(health.waiting_tone())) {
                    dt { "Waiting" }
                    dd { a.figure-main href="#queue" { (health.waiting) } }
                    @if let Some(oldest) = health.oldest_wait {
                        dd.figure-note { "oldest " (format::duration(oldest)) }
                    } @else if health.waiting == 0 {
                        dd.figure-note { "none queued" }
                    }
                }
                div {
                    dt { "Wait p95, 24 h" }
                    dd { span.figure-main { (p95.map_or("–".to_owned(), format::duration)) } }
                    dd.figure-note { (waits.len()) " job" @if waits.len() != 1 { "s" } " started" }
                    (wait_hours(&overview.day))
                }
                // Red only for the failures runnerd may be to blame for; a job's own failing steps are its repository's.
                div class=(tone_class(day.failed_tone())) {
                    dt { "Failed, 24 h" }
                    dd { span.figure-main { (day.failed) } }
                    dd.figure-note {
                        "of " (day.finished) " finished; "
                        @if day.runner_failed > 0 {
                            (day.runner_failed) " by their runner; "
                        }
                        a href="/history?outcome=failure" { "failures" }
                    }
                }
            }
            @if let Some(savings) = &overview.savings {
                p.health-savings { (savings.month.headline()) }
            }
            @if overview.status.shutting_down {
                p.warning { "runnerd is shutting down: it starts no new runners and stops once running jobs finish." }
            }
        }
    }
}

/// A health figure's class for what it means: amber `waiting`, red `failing`.
fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Warn => "waiting",
        Tone::Bad => "failing",
        Tone::Good | Tone::Neutral | Tone::Quiet => "",
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
                @if more > 0 { a href="/history?outcome=queued" { "All " (queued.len()) " queued" } }
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
        li data-class:linked=(format!("$_repo == {repository}"))
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
