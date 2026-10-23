//! `/`: the memory budget as blocks, every slot and the job it runs, the slice's CPU and memory over the last half
//! hour, the repositories with pause and resume, and the latest jobs.

use std::collections::BTreeMap;

use maud::{Markup, html};
use runnerd_core::api::{
    HostSample, HostSamples, Job, JobPage, JobQuery, JobState, JobSummary, ProfileStatus,
    RepositoryStatus, Sample, SampleSeries, SlotStatus, Status,
};

use super::{Page, badge, js, page, profile_class, repository_id, slot_id, time_ago};
use crate::chart::{Base, Series, TimeChart, nice_top, sparkline};
use crate::daemon::{Daemon, Error};
use crate::format;

/// How far back the host charts reach.
pub(crate) const HOST_WINDOW: u64 = 30 * 60;
/// How far back a slot's sparkline reaches.
const SPARK_WINDOW: u64 = 5 * 60;
/// Latest jobs listed.
const RECENT: u32 = 8;

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
}

impl Overview {
    pub(crate) async fn load(daemon: &Daemon, now: u64) -> Result<Overview, Error> {
        let status: Status = daemon.get("/v1/status").await?;
        let host = daemon
            .get(&format!(
                "/v1/host/samples?since={}",
                now.saturating_sub(HOST_WINDOW)
            ))
            .await?;
        let recent: JobPage = daemon
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    limit: Some(RECENT),
                    ..JobQuery::default()
                },
            )
            .await?;
        let mut overview = Overview {
            now,
            status,
            running: Vec::new(),
            samples: BTreeMap::new(),
            host,
            recent: recent.jobs,
        };
        overview.reload_running(daemon).await?;
        Ok(overview)
    }

    /// Reads the running jobs again, and the samples of those not seen before.
    pub(crate) async fn reload_running(&mut self, daemon: &Daemon) -> Result<(), Error> {
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

    /// Adds a live sample of the slice, dropping those older than the window.
    pub(crate) fn add_host_sample(&mut self, sample: HostSample) {
        let since = sample.at.saturating_sub(HOST_WINDOW);
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

/// The whole page.
pub(crate) fn render(overview: &Overview) -> Markup {
    let main = html! {
        h1.visually-hidden { "Overview" }
        (budget(overview))
        (slots(overview))
        (host(overview))
        div.split {
            (repositories(&overview.status))
            (recent(overview))
        }
    };
    page(
        "Overview",
        Some(Page::Overview),
        Some("/live/overview"),
        &main,
    )
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
    let cap = format::bytes(profile.memory);
    let lens = match (&slot.runner, job, memory) {
        (Some(_), Some(job), Some(memory)) => Lens {
            figure: format::bytes(memory),
            text: format!(
                "of {cap} in {name}, {} on {}",
                job.summary.name, job.summary.repository
            ),
        },
        (Some(_), Some(job), None) => Lens {
            figure: cap,
            text: format!("{name}, {} on {}", job.summary.name, job.summary.repository),
        },
        (Some(runner), None, _) => Lens {
            figure: cap,
            text: format!(
                "{name}, a runner for {} waiting for its job",
                runner.repository
            ),
        },
        (None, _, _) if profile.reseeding && slot.slot == 1 => Lens {
            figure: cap,
            text: format!("{name}, reseeding the snapshot"),
        },
        (None, _, _) => Lens {
            figure: cap,
            text: format!("{name}, idle"),
        },
    };
    SlotView {
        key: slot_id(&profile.name, slot.slot),
        job,
        memory,
        lens,
    }
}

/// The budget: the slice as the ground, a block per slot sized by its memory cap, filled as high as the job's last
/// sampled memory; whatever else holds memory (the mirror, a seed container); and what is free. A block opens its
/// job, or its slot's card; hovering or focusing one says what it holds in the readout above the bar.
pub(crate) fn budget(overview: &Overview) -> Markup {
    let status = &overview.status;
    let budget = &status.budget;
    let reserved: u64 = status
        .profiles
        .iter()
        .map(|profile| profile.memory * profile.slots.len() as u64)
        .sum();
    let running_caps: u64 = status
        .profiles
        .iter()
        .map(|profile| {
            profile.memory
                * profile
                    .slots
                    .iter()
                    .filter(|slot| slot.runner.is_some())
                    .count() as u64
        })
        .sum();
    let other = budget.memory_in_use.saturating_sub(running_caps);
    let free = budget
        .memory_max
        .map(|max| max.saturating_sub(reserved + other));
    let used_now = overview.host.samples.last().map(|sample| sample.memory);
    let max = budget
        .memory_max
        .map_or("no limit".to_owned(), format::bytes);
    let label = format!(
        "Memory budget of {}: {} of {max} reserved by running containers{}",
        budget.slice,
        format::bytes(budget.memory_in_use),
        used_now.map_or(String::new(), |used| format!(
            ", {} in use",
            format::bytes(used)
        )),
    );
    let other_lens = Lens {
        figure: format::bytes(other),
        text: "held by the mirror and seed containers".to_owned(),
    };
    let free = free.filter(|free| *free > 0);
    let free_lens = free.map(|free| Lens {
        figure: format::bytes(free),
        text: format!("of {max} free"),
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
    if other > 0 {
        lenses.push(("other".to_owned(), other_lens.clone()));
    }
    if let Some(lens) = &free_lens {
        lenses.push(("free".to_owned(), lens.clone()));
    }
    html! {
        section #budget .budget aria-labelledby="budget-title"
            "data-signals:_slot__ifmissing"="''" "data-signals:_repo__ifmissing"="''" {
            h2 #budget-title .visually-hidden { "Budget" }
            (readout(overview, used_now, &lenses))
            div.bar role="group" aria-label=(label) {
                @for profile in &status.profiles {
                    @for slot in &profile.slots {
                        (block(overview, profile, slot))
                    }
                }
                @if other > 0 {
                    @let link = link("other", None);
                    div.block.other tabindex="0" role="img" style=(format!("--size: {:.3}", gib(other)))
                        aria-label=(format!("{} mirror and seed containers", other_lens.figure))
                        data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
                        data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
                        span.size { (other_lens.figure) }
                        span.name { "mirror" }
                    }
                }
                @if let Some((free, lens)) = free.zip(free_lens.as_ref()) {
                    @let link = link("free", None);
                    div.free tabindex="0" role="img" style=(format!("--size: {:.3}", gib(free)))
                        aria-label=(format!("{} {}", lens.figure, lens.text))
                        data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
                        data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
                        span.size { (lens.figure) }
                    }
                }
            }
            (bar_keys(other, &other_lens, free_lens.as_ref()))
            @if status.shutting_down {
                p.warning { "runnerd is shutting down: it starts no new runners and stops once running jobs finish." }
            }
        }
    }
}

/// The slice's total, and beside it the selected block's line. Each block has its own name, so the lines are for the
/// eye only.
fn readout(overview: &Overview, used_now: Option<u64>, lenses: &[(String, Lens)]) -> Markup {
    let budget = &overview.status.budget;
    html! {
        div.figures {
            p.readout {
                span.big { (format::bytes(budget.memory_in_use)) }
                span { " of " (budget.memory_max.map_or("unlimited".to_owned(), format::bytes)) " in " code { (budget.slice) } }
                @if let Some(used) = used_now {
                    span.muted { " · " (format::bytes(used)) " in use" }
                }
            }
            p.lens aria-hidden="true" {
                @for (key, lens) in lenses {
                    span style="display: none" data-show=(format!("$_slot == {}", js(key))) {
                        strong { (lens.figure) } " " (lens.text)
                    }
                }
            }
        }
    }
}

/// On a touch screen the mirror and the free remainder may be too thin to tap: keys under the bar select them
/// instead, and say their sizes.
fn bar_keys(other: u64, other_lens: &Lens, free_lens: Option<&Lens>) -> Markup {
    html! {
        @if other > 0 || free_lens.is_some() {
            div.bar-keys {
                @if other > 0 {
                    button.bar-key.key-mirror type="button" data-on:click="$_slot = $_slot == 'other' ? '' : 'other'"
                        data-class:linked="$_slot == 'other'" {
                        span.swatch aria-hidden="true" {} (other_lens.figure) " mirror"
                    }
                }
                @if let Some(lens) = free_lens {
                    button.bar-key.key-free type="button" data-on:click="$_slot = $_slot == 'free' ? '' : 'free'"
                        data-class:linked="$_slot == 'free'" {
                        span.swatch aria-hidden="true" {} (lens.figure) " free"
                    }
                }
            }
        }
    }
}

fn block(overview: &Overview, profile: &ProfileStatus, slot: &SlotStatus) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let view = slot_view(overview, profile, slot);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a ratio of two sizes for a CSS fill"
    )]
    let fill = view
        .memory
        .map(|memory| (memory as f64 / profile.memory.max(1) as f64).min(1.0));
    let state = match (&slot.runner, view.job) {
        (None, _) => "idle",
        (Some(_), Some(_)) => "busy",
        (Some(_), None) => "starting",
    };
    let href = view.job.map_or(format!("#{}", view.key), |job| {
        format!("/jobs/{}", job.summary.id)
    });
    let repository = slot
        .runner
        .as_ref()
        .map(|runner| runner.repository.as_str());
    let link = link(&view.key, repository);
    html! {
        a class=(format!("block {class} {state}")) href=(href)
            style=(format!("--size: {:.3}; --fill: {:.3}", gib(profile.memory), fill.unwrap_or(0.0)))
            // The name starts with what the block shows, for voice control.
            aria-label=(format!("{} {} {}: {} {}", format::bytes(profile.memory), profile.name, slot.slot, view.lens.figure, view.lens.text))
            data-on:pointerenter=(link.enter) data-on:focus=(link.enter)
            data-on:pointerleave=(link.pointer_leave) data-on:blur=(link.leave) data-class:linked=(link.linked) {
            @if fill.is_some() { span.fill aria-hidden="true" {} }
            span.size { (format::bytes(profile.memory)) }
            span.name { (profile.name) " " (slot.slot) }
        }
    }
}

/// A card per slot: what runs in it, its current step, and its memory and CPU over the last minutes.
pub(crate) fn slots(overview: &Overview) -> Markup {
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
                        (slot_card(overview, profile, slot))
                    }
                }
            }
        }
    }
}

fn slot_card(overview: &Overview, profile: &ProfileStatus, slot: &SlotStatus) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let view = slot_view(overview, profile, slot);
    let state = match (&slot.runner, view.job) {
        (None, _) => " idle",
        (Some(_), None) => " starting",
        (Some(_), Some(_)) => "",
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
                span.slot-caps { (format::bytes(profile.memory)) " · " (profile.cpus) " CPUs" }
            }
            @match (&slot.runner, view.job) {
                (None, _) => {
                    @if profile.reseeding && slot.slot == 1 {
                        p.slot-idle { "Reseeding the profile's snapshot" }
                    } @else {
                        p.visually-hidden { "Idle" }
                    }
                }
                (Some(runner), None) => {
                    span.repo { (runner.repository) }
                    p.slot-idle { "Waiting for its job " span.muted { (time_ago(runner.started_at, overview.now)) } }
                }
                (Some(_), Some(job)) => { (running_job(overview, profile, job)) }
            }
        }
    }
}

fn running_job(overview: &Overview, profile: &ProfileStatus, job: &Job) -> Markup {
    let summary = &job.summary;
    let now = overview.now;
    let started = summary
        .timeline
        .started_at
        .unwrap_or(summary.timeline.queued_at);
    let current = job
        .steps
        .iter()
        .find(|step| step.status == "in_progress")
        .or_else(|| {
            job.steps
                .iter()
                .rev()
                .find(|step| step.status == "completed")
        });
    let done = job
        .steps
        .iter()
        .filter(|step| step.status == "completed")
        .count();
    let samples = overview
        .samples
        .get(&summary.id)
        .map_or(&[][..], Vec::as_slice);
    let from = now.saturating_sub(SPARK_WINDOW).max(started.min(now));
    let memory: Vec<(u64, u64)> = samples
        .iter()
        .filter(|sample| sample.at >= from)
        .map(|sample| (sample.at, sample.memory))
        .collect();
    let cpu: Vec<(u64, u64)> = samples
        .iter()
        .filter(|sample| sample.at >= from)
        .map(|sample| (sample.at, sample.cpu))
        .collect();
    let last = samples.last();
    html! {
        div.slot-title {
            a.slot-job href=(format!("/jobs/{}", summary.id)) {
                span.repo { (summary.repository) }
                span.job-name { (summary.name) }
            }
            p.elapsed { span.visually-hidden { "Running for " } (format::duration(now.saturating_sub(started))) }
        }
        p.step {
            @if let Some(step) = current {
                span.step-count { (done) "/" (job.steps.len()) }
                " " (step.name)
            } @else {
                "Setting up"
            }
        }
        dl.slot-figures {
            div.memory {
                dt.visually-hidden { "Memory" }
                dd { (last.map_or("–".to_owned(), |sample| format::bytes(sample.memory))) }
                (sparkline(&memory, from, now, profile.memory, "memory"))
            }
            div.cpu {
                dt.visually-hidden { "CPU" }
                dd { (last.map_or("–".to_owned(), |sample| format!("{} cores", format::cores(sample.cpu)))) }
                (sparkline(&cpu, from, now, u64::from(profile.cpus) * 1000, "cpu"))
            }
        }
    }
}

/// The slice's memory and CPU over the last half hour, against `MemoryMax` and the CPUs the slots may use.
pub(crate) fn host(overview: &Overview) -> Markup {
    let status = &overview.status;
    let now = overview.now;
    let start = now.saturating_sub(HOST_WINDOW);
    let samples = &overview.host.samples;
    let memory: Vec<(u64, u64)> = samples.iter().map(|s| (s.at, s.memory)).collect();
    let cpu: Vec<(u64, u64)> = samples.iter().map(|s| (s.at, s.cpu)).collect();
    let cpus: u64 = status
        .profiles
        .iter()
        .map(|profile| u64::from(profile.cpus) * profile.slots.len() as u64)
        .sum();
    let gap = overview.host.interval.max(1) * 3;
    let memory_peak = memory.iter().map(|point| point.1).max().unwrap_or(0);
    let cpu_peak = cpu.iter().map(|point| point.1).max().unwrap_or(0);
    let memory_chart = TimeChart {
        title: format!(
            "Memory of {} over the last 30 minutes, sampled every {} s",
            status.budget.slice, overview.host.interval
        ),
        start,
        end: now,
        series: vec![Series {
            label: "memory".to_owned(),
            class: "memory",
            points: memory,
            area: true,
        }],
        limit: status
            .budget
            .memory_max
            .map(|max| (max, format!("MemoryMax {}", format::bytes(max)))),
        value: format::bytes,
        time: relative_clock,
        gap,
    };
    let memory_top = nice_top(
        memory_peak.max(status.budget.memory_max.unwrap_or(0)),
        Base::Binary,
    );
    let cpu_chart = TimeChart {
        title: format!(
            "CPU of {} over the last 30 minutes, sampled every {} s",
            status.budget.slice, overview.host.interval
        ),
        start,
        end: now,
        series: vec![Series {
            label: "CPU".to_owned(),
            class: "cpu",
            points: cpu,
            area: true,
        }],
        limit: (cpus > 0).then(|| (cpus * 1000, format!("{cpus} CPUs of all slots"))),
        value: |millicores| format!("{} cores", format::cores(millicores)),
        time: relative_clock,
        gap,
    };
    let cpu_top = nice_top(cpu_peak.max(cpus * 1000), Base::Decimal);
    html! {
        section #host .host aria-labelledby="host-title" {
            h2 #host-title .visually-hidden { "Slice" }
            div.charts {
                div.chart-card {
                    h3 {
                        "Memory"
                        span.reading {
                            (overview.host.samples.last().map_or("–".to_owned(), |s| format::bytes(s.memory)))
                            span.muted { " · peak " (format::bytes(memory_peak)) }
                        }
                    }
                    (memory_chart.render(memory_top))
                }
                div.chart-card {
                    h3 {
                        "CPU"
                        span.reading {
                            (overview.host.samples.last().map_or("–".to_owned(), |s| format!("{} cores", format::cores(s.cpu))))
                            span.muted { " · peak " (format::cores(cpu_peak)) }
                        }
                    }
                    (cpu_chart.render(cpu_top))
                }
            }
        }
    }
}

fn relative_clock(at: u64) -> String {
    format!("{} UTC", format::clock(at))
}

/// The configured repositories, with their queue and a pause or resume button.
pub(crate) fn repositories(status: &Status) -> Markup {
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
            }
        }
    }
}

/// One repository's row; a pause or resume replaces it. Hovering it lights up the slots its runners hold and its
/// latest jobs.
pub(crate) fn repository_row(status: &Status, repository: &RepositoryStatus) -> Markup {
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
                span { (repository.profile) }
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

/// The latest jobs, any state. A running job's row lights up its slot.
pub(crate) fn recent(overview: &Overview) -> Markup {
    html! {
        section #recent .recent aria-labelledby="recent-title" {
            h2 #recent-title .visually-hidden { "Latest jobs" }
            @if overview.recent.is_empty() {
                p.muted { "No jobs yet. Jobs show here once a runner of this machine takes one." }
            } @else {
                ul.job-list {
                    @for job in &overview.recent {
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
                                span.job-name { (job.name) }
                                span.repo { (job.repository) }
                                span.when { (time_ago(job.timeline.queued_at, overview.now)) }
                            }
                        }
                    }
                    li.all { a href="/history" { "All history" } }
                }
            }
        }
    }
}
