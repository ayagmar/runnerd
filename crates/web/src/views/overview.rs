//! `/`: the memory budget as blocks, every slot and the job it runs, the slice's CPU and memory over the last half
//! hour, the repositories with pause and resume, and the latest jobs.

use std::collections::BTreeMap;

use maud::{Markup, html};
use runnerd_core::api::{
    HostSample, HostSamples, Job, JobPage, JobQuery, JobState, JobSummary, ProfileStatus,
    RepositoryStatus, Sample, SampleSeries, SlotStatus, Status,
};

use super::{Page, badge, page, profile_class, slug, time_ago};
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

/// The budget: the slice as the ground, a block per slot sized by its memory cap, filled as high as the job's last
/// sampled memory; whatever else holds memory (the mirror, a seed container); and what is free.
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
    let label = format!(
        "Memory budget of {}: {} of {} reserved by running containers{}",
        budget.slice,
        format::bytes(budget.memory_in_use),
        budget
            .memory_max
            .map_or("no limit".to_owned(), format::bytes),
        used_now.map_or(String::new(), |used| format!(
            ", {} in use",
            format::bytes(used)
        )),
    );
    html! {
        section #budget .budget aria-labelledby="budget-title" {
            div.section-head {
                h2 #budget-title { "Budget" }
                p.figures {
                    span.big { (format::bytes(budget.memory_in_use)) }
                    span { " of " (budget.memory_max.map_or("unlimited".to_owned(), format::bytes)) " reserved in " code { (budget.slice) } }
                    @if let Some(used) = used_now {
                        span.muted { " · " (format::bytes(used)) " in use, sampled" }
                    }
                }
            }
            div.bar role="img" aria-label=(label) {
                @for profile in &status.profiles {
                    @for slot in &profile.slots {
                        (block(overview, profile, slot))
                    }
                }
                @if other > 0 {
                    div.block.other style=(format!("--size: {:.3}", gib(other))) title=(format!("Mirror and seed containers: {}", format::bytes(other))) {
                        span.size { (format::bytes(other)) }
                        span.name { "mirror" }
                    }
                }
                @if let Some(free) = free.filter(|free| *free > 0) {
                    div.free style=(format!("--size: {:.3}", gib(free))) {
                        span.size { (format::bytes(free)) }
                        span.name { "free" }
                    }
                }
            }
            @if status.shutting_down {
                p.warning { "runnerd is shutting down: it starts no new runners and stops once running jobs finish." }
            }
        }
    }
}

fn block(overview: &Overview, profile: &ProfileStatus, slot: &SlotStatus) -> Markup {
    let class = profile_class(&overview.status, &profile.name);
    let job = overview.job_in(&profile.name, slot.slot);
    let memory = job
        .and_then(|job| overview.samples.get(&job.summary.id))
        .and_then(|samples| samples.last())
        .map(|sample| sample.memory);
    #[allow(
        clippy::cast_precision_loss,
        reason = "a ratio of two sizes for a CSS fill"
    )]
    let fill = memory.map(|memory| (memory as f64 / profile.memory.max(1) as f64).min(1.0));
    let state = match (&slot.runner, job) {
        (None, _) => "idle",
        (Some(_), Some(_)) => "busy",
        (Some(_), None) => "starting",
    };
    html! {
        div class=(format!("block {class} {state}")) style=(format!("--size: {:.3}; --fill: {:.3}", gib(profile.memory), fill.unwrap_or(0.0))) {
            @if fill.is_some() { span.fill aria-hidden="true" {} }
            span.size { (format::bytes(profile.memory)) }
            span.name { (profile.name) " " (slot.slot) }
            span.repo {
                @match (&slot.runner, memory) {
                    (Some(_), Some(memory)) => { (format::bytes(memory)) " used" },
                    (Some(runner), None) => { (runner.repository) },
                    (None, _) => { "idle" },
                }
            }
        }
    }
}

/// A card per slot: what runs in it, its current step, and its memory and CPU over the last minutes.
pub(crate) fn slots(overview: &Overview) -> Markup {
    let status = &overview.status;
    html! {
        section #slots .slots aria-labelledby="slots-title" {
            div.section-head {
                h2 #slots-title { "Slots" }
                p.muted {
                    (overview.running.len()) " running · "
                    (status.repositories.iter().map(|repository| repository.queued_jobs).sum::<usize>()) " queued"
                }
            }
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
    let job = overview.job_in(&profile.name, slot.slot);
    html! {
        li class=(format!("slot {class}")) {
            div.slot-head {
                span.swatch aria-hidden="true" {}
                span.slot-name { (profile.name) " · slot " (slot.slot) }
                span.slot-caps { (format::bytes(profile.memory)) " · " (profile.cpus) " CPUs" }
            }
            @match (&slot.runner, job) {
                (None, _) => {
                    p.slot-idle { @if profile.reseeding && slot.slot == 1 { "Reseeding the profile's snapshot" } @else { "Idle" } }
                }
                (Some(runner), None) => {
                    p.slot-idle { "Runner for " strong { (runner.repository) } " waiting for its job" }
                    p.muted { "started " (time_ago(runner.started_at, overview.now)) }
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
        a.slot-job href=(format!("/jobs/{}", summary.id)) {
            span.repo { (summary.repository) }
            span.job-name { (summary.name) }
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
            div { dt { "Running" } dd { (format::duration(now.saturating_sub(started))) } }
            div.memory {
                dt { "Memory" }
                dd { (last.map_or("–".to_owned(), |sample| format::bytes(sample.memory))) }
                (sparkline(&memory, from, now, profile.memory, "memory"))
            }
            div.cpu {
                dt { "CPU" }
                dd { (last.map_or("–".to_owned(), |sample| format::cores(sample.cpu))) }
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
        title: format!("Memory of {} over the last 30 minutes", status.budget.slice),
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
        title: format!("CPU of {} over the last 30 minutes", status.budget.slice),
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
            div.section-head {
                h2 #host-title { "Slice" }
                p.muted { "Every runner container together, sampled every " (overview.host.interval) " s" }
            }
            div.charts {
                div.chart-card {
                    h3 { "Memory" span.reading { (overview.host.samples.last().map_or("–".to_owned(), |s| format::bytes(s.memory))) } }
                    (memory_chart.render(memory_top))
                    p.muted { "Sampled peak " (format::bytes(memory_peak)) }
                }
                div.chart-card {
                    h3 { "CPU" span.reading { (overview.host.samples.last().map_or("–".to_owned(), |s| format!("{} cores", format::cores(s.cpu)))) } }
                    (cpu_chart.render(cpu_top))
                    p.muted { "Sampled peak " (format::cores(cpu_peak)) " cores" }
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
            h2 #repositories-title { "Repositories" }
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

/// One repository's row; a pause or resume replaces it.
pub(crate) fn repository_row(status: &Status, repository: &RepositoryStatus) -> Markup {
    let class = profile_class(status, &repository.profile);
    let (action, label) = if repository.paused {
        ("resume", "Resume")
    } else {
        ("pause", "Pause")
    };
    html! {
        li #(format!("repo-{}", slug(&repository.name))) class=(format!("repository {class}")) {
            div.repo-name {
                span.swatch aria-hidden="true" {}
                a href=(format!("/history?repository={}", repository.name)) { (repository.name) }
            }
            div.repo-meta {
                span { (repository.profile) }
                span { (repository.queued_jobs) " queued" }
                @if repository.paused { span.badge.neutral { "paused" } }
            }
            // One busy signal per row, so a pending pause greys out only its own button.
            @let busy = format!("_busy_{}", slug(&repository.name).replace('-', "_"));
            button.button-quiet type="button"
                data-indicator=(busy)
                data-attr:aria-busy=(format!("${busy} && 'true'"))
                data-attr:disabled=(format!("${busy}"))
                data-on:click=(format!("@post('/v1/repositories/{}/{action}')", repository.name))
                aria-label=(format!("{label} {}", repository.name)) { (label) }
        }
    }
}

/// The latest jobs, any state.
pub(crate) fn recent(overview: &Overview) -> Markup {
    html! {
        section #recent .recent aria-labelledby="recent-title" {
            div.section-head {
                h2 #recent-title { "Latest jobs" }
                a href="/history" { "All history" }
            }
            @if overview.recent.is_empty() {
                p.muted { "No jobs yet. Jobs show here once a runner of this machine takes one." }
            } @else {
                ul.job-list {
                    @for job in &overview.recent {
                        li {
                            a href=(format!("/jobs/{}", job.id)) {
                                (badge(job))
                                span.job-name { (job.name) }
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
