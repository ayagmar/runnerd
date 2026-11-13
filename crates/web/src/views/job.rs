//! `/jobs/{id}`: one job's phases, its steps on a shared time axis, its CPU and memory over time against what
//! admission reserved and the caps its container ran with (or, for records without them, the profile's limits as
//! configured now), and its log, live while it runs, and the same job's latest runs (wait, run time, outcome and
//! memory peak) beside it. A failed job whose runner was kept as a record gets the command that opens a shell in it.
//!
//! A failed job opens on what failed: core's headline, the exit code, the failed step's last lines and the shell
//! command. A finished job's log reads as core outlines it: a fold per step and per group, the failed step open,
//! colours from core's runs, a link per line, and a search of the lines' messages. A running job's log is its live
//! tail. A daemon older than the outline gets the plain tail and says so; never an empty log.

use std::fmt::Write as _;

use maud::{Markup, html};
use runnerd_core::api::{
    ActionPreview, Baseline, BuildCacheCounts, ChartLine, ChartLineKind, Color, CompareQuery,
    Feature, Job, JobAction, JobActions, JobComparison, JobPage, JobQuery, JobState, JobSummary,
    LINES_MAX, LineKind, LineRange, LogEvidence, LogLinesQuery, LogOutline, LogSearch,
    LogSearchQuery, OutcomeKind, PhaseKind, ProfileStatus, RecordNote, RecordState, Sample,
    SampleSeries, Status, StepLog, StyledLine, in_cores,
};
use runnerd_core::client::{Client, ClientError};
use serde::Deserialize;

use super::{
    Live, Parts, badge, digest, outcome_class, outcome_kind_class, page, profile_class, time_ago,
    tone_class,
};
use crate::chart::{Band, Base, Limit, Series, TimeChart, nice_top};
use crate::format;

/// Log lines the page shows; the full log is at `/jobs/{id}/log`.
pub(crate) const LOG_TAIL: u64 = 1000;
/// Runs of the same job the page compares.
const RUNS: u32 = 30;
/// The most lines of a finished job's log the page outlines, its last; the full log has the rest.
const OUTLINED_LINES: u64 = 5000;
/// How many reads of styled lines the page makes at most: each is cut at a few MiB.
const LINE_READS: usize = 4;
/// Matches a page of a log search shows, and the lines around each.
const SEARCH_MATCHES: u32 = 50;
const SEARCH_CONTEXT: u32 = 1;

/// The job page's query string: a search of the log, its next page, and a line to open at.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct JobPageQuery {
    #[serde(default)]
    pub(crate) q: Option<String>,
    #[serde(default)]
    pub(crate) from: Option<u64>,
    /// A line to open the log at, counted from 1 as the page numbers them.
    #[serde(default)]
    pub(crate) line: Option<u64>,
}

/// A finished job's log as the page shows it.
#[derive(Debug, Clone)]
pub(crate) enum Outlined {
    /// Core's outline and the lines read of it, from `first`.
    Read {
        outline: Box<LogOutline>,
        lines: Vec<StyledLine>,
        first: u64,
    },
    /// The daemon is older than the outline: the plain tail shows.
    Unsupported,
    /// The job runs: its live tail shows.
    Live,
}

/// A search of the log, when the page asks one.
#[derive(Debug, Clone)]
pub(crate) enum Searched {
    /// The matches.
    Found { query: String, search: LogSearch },
    /// Refused in the daemon's words.
    Refused(String),
    /// The daemon is older than the search.
    Unsupported,
}

/// The job beside its previous run.
#[derive(Debug, Clone)]
pub(crate) enum Compared {
    /// The daemon's comparison.
    Read(JobComparison),
    /// The daemon is older than comparisons.
    Unsupported,
}

/// Everything the job page shows.
#[derive(Debug, Clone)]
pub(crate) struct JobView {
    pub(crate) now: u64,
    pub(crate) job: Job,
    pub(crate) samples: SampleSeries,
    /// The job's profile as configured now, while it is: its memory and CPUs are what the charts show for records
    /// without their container's caps, which a reload since may have changed.
    pub(crate) profile: Option<ProfileStatus>,
    /// The number of the first line in `log`.
    pub(crate) log_from: u64,
    pub(crate) log: Vec<String>,
    /// The latest runs of the same job (repository, workflow and name), oldest first: this one among them unless 30
    /// newer ran since.
    pub(crate) runs: Vec<JobSummary>,
    /// The colour class of the job's profile, which its runs' memory peaks are drawn in.
    pub(crate) profile_class: String,
    /// Whether the daemon cancels and re-runs jobs.
    pub(crate) actions: JobActions,
    pub(crate) outlined: Outlined,
    pub(crate) searched: Option<Searched>,
    pub(crate) compared: Option<Compared>,
    /// The line the page opens at, 0-based.
    pub(crate) line: Option<u64>,
}

impl JobView {
    pub(crate) async fn load(
        daemon: &Client,
        id: u64,
        now: u64,
        query: &JobPageQuery,
    ) -> Result<JobView, ClientError> {
        let mut job: Job = daemon.get(&format!("/v1/jobs/{id}")).await?;
        let samples: SampleSeries = daemon.get(&format!("/v1/jobs/{id}/samples")).await?;
        // Samples stored since the job was read raise a running job's peaks, as those that arrive later do.
        for sample in &samples.samples {
            job.observe(sample);
        }
        let status: Status = daemon.get("/v1/status").await?;
        let log_from = job.summary.log.lines.saturating_sub(LOG_TAIL);
        let log = daemon
            .text(&format!("/v1/jobs/{id}/logs?from={log_from}"))
            .await?
            .lines()
            .map(str::to_owned)
            .collect();
        let page: JobPage = daemon
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    repository: Some(job.summary.repository.clone()),
                    workflow: job.summary.workflow.clone(),
                    name: Some(job.summary.name.clone()),
                    limit: Some(RUNS),
                    ..JobQuery::default()
                },
            )
            .await?;
        // Without a workflow the query matches every workflow's job of that name.
        let mut runs: Vec<JobSummary> = page
            .jobs
            .into_iter()
            .filter(|run| run.workflow == job.summary.workflow)
            .collect();
        runs.reverse();
        let profile_class = job.summary.runner.as_ref().map_or_else(
            || "p-none".to_owned(),
            |runner| profile_class(&status, &runner.profile),
        );
        let outlined = if job.summary.state.is_final() {
            read_outline(daemon, id).await?
        } else {
            Outlined::Live
        };
        let searched = match query.q.as_deref().filter(|q| !q.is_empty()) {
            Some(q) => Some(search(daemon, id, q, query.from).await?),
            None => None,
        };
        let compared = if job.summary.state.is_final() {
            match daemon.compare(id, &CompareQuery::default()).await {
                Ok(comparison) => Some(Compared::Read(comparison)),
                Err(error) if error.is_unsupported() => Some(Compared::Unsupported),
                // A job the history does not hold as a run of a job has nothing to compare.
                Err(ClientError::Api {
                    status: axum::http::StatusCode::NOT_FOUND,
                    ..
                }) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        Ok(JobView {
            actions: status.job_actions,
            outlined,
            searched,
            compared,
            line: query.line.and_then(|line| line.checked_sub(1)),
            runs,
            profile_class,
            profile: configured(&status, &job),
            now,
            job,
            samples,
            log_from,
            log,
        })
    }

    /// A sample as it arrives: the charts take it, and the job's peaks rise to it while it runs.
    pub(crate) fn add_sample(&mut self, sample: Sample) {
        self.job.observe(&sample);
        self.samples.samples.push(sample);
    }
}

/// A finished job's outline and its last lines, or that the daemon has none.
async fn read_outline(daemon: &Client, id: u64) -> Result<Outlined, ClientError> {
    let outline = match daemon.log_outline(id).await {
        Ok(outline) => outline,
        Err(error) if error.is_unsupported() => return Ok(Outlined::Unsupported),
        Err(error) => return Err(error),
    };
    let first = outline.lines.saturating_sub(OUTLINED_LINES);
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut from = first;
    for _ in 0..LINE_READS {
        if from >= outline.lines {
            break;
        }
        let mut read = daemon
            .styled_lines(
                id,
                &LogLinesQuery {
                    from: Some(from),
                    limit: Some(LINES_MAX),
                    follow: false,
                },
            )
            .await?;
        let before = lines.len();
        while let Some(line) = read.next().await? {
            from = line.line + 1;
            lines.push(line);
        }
        if lines.len() == before {
            break;
        }
    }
    Ok(Outlined::Read {
        outline: Box::new(outline),
        lines,
        first,
    })
}

/// A page of a search of the log for `q`, from line `from`.
async fn search(
    daemon: &Client,
    id: u64,
    q: &str,
    from: Option<u64>,
) -> Result<Searched, ClientError> {
    let query = LogSearchQuery {
        q: q.to_owned(),
        from,
        limit: Some(SEARCH_MATCHES),
        context: Some(SEARCH_CONTEXT),
        follow: false,
    };
    match daemon.search_log(id, &query).await {
        Ok(search) => Ok(Searched::Found {
            query: q.to_owned(),
            search,
        }),
        Err(error) if error.is_unsupported() => Ok(Searched::Unsupported),
        Err(ClientError::Api {
            status, message, ..
        }) if status.is_client_error() => Ok(Searched::Refused(message)),
        Err(error) => Err(error),
    }
}

/// The job's profile as configured now.
fn configured(status: &Status, job: &Job) -> Option<ProfileStatus> {
    let runner = job.summary.runner.as_ref()?;
    status
        .profiles
        .iter()
        .find(|profile| profile.name == runner.profile)
        .cloned()
}

impl JobView {
    /// Whether the job still runs, so its page is live.
    fn live(&self) -> bool {
        !self.job.summary.state.is_final()
    }
}

/// The live regions, by id, in the order the page has them, with their parts.
pub(crate) fn regions(view: &JobView) -> [(&'static str, Markup, Parts); 5] {
    let region = |id, render: &dyn Fn(&Parts) -> Markup| {
        let parts = Parts::default();
        let markup = render(&parts);
        (id, markup, parts)
    };
    [
        region("job-head", &|_| head(view)),
        region("timeline", &|parts| timeline(view, parts)),
        region("steps", &|parts| steps(view, parts)),
        region("metrics", &|parts| metrics(view, parts)),
        region("runs", &|_| runs(view)),
    ]
}

/// The whole page.
pub(crate) fn render(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    let url = format!("/live/jobs/{}", summary.id);
    let [head, timeline, steps, metrics, runs] = regions(view);
    let live = view.live().then(|| Live {
        url: &url,
        at: view.now,
        log: Some(view.log_from + view.log.len() as u64),
        regions: [&head, &timeline, &steps, &metrics, &runs]
            .iter()
            .map(|(id, markup, _)| (*id, digest(&markup.0)))
            .collect(),
    });
    let main = html! {
        (head.1)
        div #job-action {}
        (failure(view))
        (timeline.1)
        (steps.1)
        (log(view))
        (metrics.1)
        @if let Some(admission) = &view.job.admission {
            p.admission { (format::sentence(&admission.line())) }
        }
        (compare(view))
        (runs.1)
        (record(view))
    };
    page(&summary.title().to_string(), None, live.as_ref(), &main)
}

/// What a job's page shows only once the job is final, by id: the failure summary, the outlined log, the comparison
/// and the record. The live stream sends them once as the job ends, so the page does not keep a running job's tail.
pub(crate) fn finished(view: &JobView) -> Markup {
    html! {
        (failure(view))
        (log(view))
        (compare(view))
        (record(view))
    }
}

/// The title, outcome and where the job came from.
pub(crate) fn head(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        header #job-head .job-head {
            p.crumbs {
                a href=(format!("/history?repository={}", summary.repository)) { (summary.repository) }
                @if let Some(workflow) = &summary.workflow { " · " (workflow) }
            }
            h1 { (summary.title().name) " " (badge(summary)) }
            // Waited, ran and the queued time are the timeline's.
            p.job-facts {
                @if let Some(branch) = &summary.branch { code { (branch) } }
                @let run = html! { "run " (summary.run_id) @if summary.run_attempt > 1 { ", attempt " (summary.run_attempt) } };
                @if let Some(url) = &summary.url {
                    a.external href=(url) rel="noreferrer" aria-label=(format!("Run {} on GitHub", summary.run_id)) { (run) }
                } @else {
                    span { (run) }
                }
                @if let Some(runner) = &summary.runner { span { (runner.profile) " " (runner.slot) } }
            }
            (actions(view))
        }
    }
}

/// Cancel and re-run, as core offers them on this job, each asking the daemon for a preview first; or why the page
/// offers none.
fn actions(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        @if view.actions.offered() {
            div.job-actions {
                @for action in JobAction::for_job(summary) {
                    button.button-quiet type="button" data-indicator:_asking="" data-asks
                        data-attr:aria-busy="$_asking && 'true'" data-attr:aria-disabled="$_asking && 'true'"
                        data-on:click=(format!("!$_asking && @post('{}', {{payload: {{}}}})", action_path(summary.id, *action))) {
                        (action.confirm_label()) "…"
                    }
                }
            }
        } @else if let Some(why) = view.actions.why_not() {
            p.muted.job-actions-off { (format::sentence(why)) "." }
        }
    }
}

/// The dashboard's route for `action` on job `id`. The confirmation of the preview the user saw goes in the request's
/// body, as its own payload, never in a URL, which a proxy logs, nor in a signal, which a stream's URL would carry.
pub(crate) fn action_path(id: u64, action: JobAction) -> String {
    let route = match action {
        JobAction::Cancel => "cancel",
        JobAction::Rerun | JobAction::RerunFailed | JobAction::Unsupported => "rerun",
    };
    if action == JobAction::RerunFailed {
        format!("/jobs/{id}/{route}?failed=true")
    } else {
        format!("/jobs/{id}/{route}")
    }
}

/// The daemon's preview of `action` on job `id`, in `#job-action`: what it does, to which jobs, and a button that
/// sends it with the confirmation this preview issued; without one, nothing to send.
pub(crate) fn action_preview(id: u64, action: JobAction, preview: &ActionPreview) -> Markup {
    html! {
        div #job-action .action-preview role="dialog" aria-labelledby="action-title" tabindex="-1"
            data-init="el.focus()" data-dismiss-to="[data-asks]" {
            h2 #action-title { (preview.title) }
            p { (format::sentence(&preview.scope)) }
            @for note in &preview.notes { p.muted { (format::sentence(note)) } }
            @if !preview.siblings.is_empty() {
                div.table-wrap {
                    table.job-table.siblings {
                        tbody {
                            @for job in &preview.siblings {
                                tr {
                                    th scope="row" { (job.title().to_string()) }
                                    td { (badge(job)) }
                                }
                            }
                        }
                    }
                }
            }
            @if !preview.siblings_note.is_empty() { p.muted { (format::sentence(&preview.siblings_note)) } }
            div.toast-actions {
                @if let Some(confirm) = &preview.confirm {
                    button.button-danger type="button" data-indicator:_sending=""
                        data-attr:aria-busy="$_sending && 'true'" data-attr:disabled="$_sending"
                        data-on:click=(format!("@post({}, {{payload: {{confirm: {}}}}})", super::js(&action_path(id, action)), super::js(confirm))) { (preview.confirm_label) }
                }
                button.button-quiet type="button" data-dismiss { "Keep it" }
            }
        }
    }
}

/// `#job-action` emptied, once an action is sent.
pub(crate) fn action_cleared() -> Markup {
    html! { div #job-action {} }
}

/// On a failed job, what its log says of the failure, first: core's headline, the failed step's last lines, and
/// the command that opens a shell in its record while it is kept.
fn failure(view: &JobView) -> Markup {
    let Outlined::Read { outline, .. } = &view.outlined else {
        return html! { div #failure {} };
    };
    let Some(failure) = &outline.failure else {
        return html! { div #failure {} };
    };
    html! {
        section #failure .failure-summary aria-labelledby="failure-title" {
            h2 #failure-title { (failure.headline(&view.job.steps)) }
            @if let Some(excerpt) = &failure.excerpt {
                ol.log-lines.excerpt aria-label="Its last lines" {
                    @for line in excerpt { (styled_line(line, view.line, Place::Copy)) }
                }
            }
            @if let Some(step) = failure.step.and_then(|step| outline.step_lines(step)).filter(|range| !range.is_empty()) {
                p { a href=(format!("#L{}", step.to)) { "The step in the log" } }
            }
            @if let Some(shell) = &failure.shell { (copy_command(shell)) }
        }
    }
}

/// A command and a button that copies it.
fn copy_command(command: &str) -> Markup {
    html! {
        div.copy-command {
            code { (command) }
            button.button-quiet type="button"
                data-on:click=(format!("navigator.clipboard.writeText({})", super::js(command)))
                aria-label="Copy the command" { "Copy" }
        }
    }
}

/// The job beside its previous run: core's figures and what changed, then its steps', or why there is none.
fn compare(view: &JobView) -> Markup {
    let Some(compared) = &view.compared else {
        return html! { div #compare {} };
    };
    html! {
        section #compare .compare aria-labelledby="compare-title" {
            h2 #compare-title { "Compared with the previous run" }
            @match compared {
                Compared::Unsupported => { p.muted { (format::sentence(&Feature::Comparison.unsupported())) "." } }
                Compared::Read(comparison) => {
                    @match &comparison.baseline {
                        Baseline::Chosen { job, .. } => {
                            @if let Some(against) = comparison.baseline.against(view.now) {
                                p.muted { a href=(format!("/jobs/{job}")) { (format::sentence(&against)) } }
                            }
                            (comparison_tables(comparison))
                        }
                        other => { p.muted { (format::sentence(&other.words().unwrap_or_default())) "." } }
                    }
                }
            }
        }
    }
}

fn comparison_tables(comparison: &JobComparison) -> Markup {
    html! {
        div.table-wrap {
            table.compare-table {
                thead { tr { th scope="col" { span.visually-hidden { "Figure" } } th scope="col" .number { "This run" } th scope="col" .number { "Previous" } th scope="col" .number { "Change" } } }
                tbody {
                    @for figure in &comparison.figures {
                        tr {
                            th scope="row" { (figure.figure.label()) }
                            td.number { (figure.this_words()) }
                            td.number { (figure.baseline_words()) }
                            td.number class=(tone_class(figure.tone())) { (figure.change_words()) }
                        }
                    }
                }
            }
        }
        @if !comparison.steps.is_empty() {
            details.compare-steps {
                summary { "Steps" }
                div.table-wrap {
                    table.compare-table {
                        thead { tr { th scope="col" { "Step" } th scope="col" .number { "This run" } th scope="col" .number { "Previous" } th scope="col" .number { "Change" } } }
                        tbody {
                            @for step in &comparison.steps {
                                tr {
                                    th scope="row" {
                                        (step.name)
                                        @if let Some(words) = step.matched.words() { " " span.muted { (words) } }
                                    }
                                    td.number { (step.time.this_words()) }
                                    td.number { (step.time.baseline_words()) }
                                    td.number class=(tone_class(step.time.tone())) { (step.time.change_words()) }
                                }
                            }
                        }
                    }
                }
                @if let Some(cut) = comparison.steps_cut_words() { p.muted { (format::sentence(cut)) "." } }
            }
        }
    }
}

/// The phases from queued to cleaned up, as one bar.
pub(crate) fn timeline(view: &JobView, parts: &Parts) -> Markup {
    let summary = &view.job.summary;
    let now = view.now;
    let phases = summary.phases();
    let live = view.live();
    html! {
        section #timeline .timeline aria-labelledby="timeline-title" {
            h2 #timeline-title .visually-hidden { "Timeline" }
            // Named by the legend, whose figures are parts that change on their own.
            div.phases role="img" aria-labelledby="phase-legend" {
                @for phase in &phases {
                    // Each phase grows by its seconds; an ongoing one follows the page's clock.
                    @let (class, from, to) = (phase_class(phase.kind), phase.from, phase.to);
                    @let seconds = phase.seconds(now);
                    @let follows = live && to.is_none();
                    @let share = format!("--share: {seconds}");
                    div class=(format!("phase {class}{}", if to.is_none() { " ongoing" } else { "" }))
                        style=(if follows { parts.clocked(share) } else { share })
                        data-style:--share=[follows.then(|| format!("Math.max(0, $shown.at - {from})"))] {}
                }
            }
            ol #phase-legend .phase-legend {
                @for phase in &phases {
                    @let class = phase_class(phase.kind);
                    li class=(class) {
                        span.swatch aria-hidden="true" {}
                        span { (phase.kind.label()) }
                        @let id = format!("phase-{class}-time");
                        @let ongoing = phase.to.is_none();
                        (parts.html(&id, html! {
                            span #(id) class=(if ongoing { "figure ongoing" } else { "figure" }) {
                                (format::duration(phase.seconds(now))) @if ongoing { "…" }
                            }
                        }))
                        @if phase.kind == PhaseKind::Waiting {
                            @if let Some(note) = summary.unseen_note(now) { span.phase-note { (note) } }
                        }
                    }
                }
            }
            details.marks {
                summary { "Exact times" }
                dl {
                    @for milestone in summary.timeline.milestones() {
                        div { dt { (milestone.label) } dd { (format::timestamp(milestone.at)) } }
                    }
                }
            }
        }
    }
}

/// The CSS class of a phase of the timeline.
fn phase_class(kind: PhaseKind) -> &'static str {
    match kind {
        PhaseKind::Waiting => "wait",
        PhaseKind::Starting => "start",
        PhaseKind::Running => "run",
        PhaseKind::CleaningUp => "clean",
    }
}

/// The steps on one axis from the job's start to its end, with their durations.
pub(crate) fn steps(view: &JobView, parts: &Parts) -> Markup {
    let job = &view.job;
    let now = view.now;
    let start = job
        .summary
        .timeline
        .started_at
        .or_else(|| job.steps.iter().filter_map(|step| step.started_at).min());
    let end = job.summary.timeline.finished_at.unwrap_or(now);
    let live = view.live();
    // The bars' places are seconds since the start against the track's `--span`, which follows the page's clock
    // while the job runs, as a running step's end does.
    let span = start.map(|start| end.saturating_sub(start).max(1));
    let follow = |start: u64| format!("Math.max(1, $shown.at - {start})");
    html! {
        section #steps .steps aria-labelledby="steps-title" {
            h2 #steps-title .visually-hidden { "Steps" }
            @if job.steps.is_empty() {
                p.muted { "GitHub has not reported any steps yet." }
            } @else {
                // A step that ran lights up its stretch of the memory and CPU charts on hover, focus or tap.
                ol.step-list "data-signals:_step__ifmissing"="0"
                    style=[span.map(|span| format!("--span: {span}")).map(|style| if live { parts.clocked(style) } else { style })]
                    data-style:--span=[start.filter(|_| live).map(follow)] {
                    @for (index, step) in job.steps.iter().enumerate() {
                        @let outcome = step.outcome();
                        @let class = outcome_kind_class(outcome.kind);
                        @let seconds = step.took(now);
                        @let usage = step.started_at.and_then(|from| view.samples.usage_between(from, from + seconds.unwrap_or(0)));
                        @let number = index + 1;
                        @let enter = format!("$_step = {number}");
                        li class=(class) tabindex=[usage.is_some().then_some("0")]
                            data-on:pointerenter=[usage.is_some().then_some(&enter)]
                            data-on:focus=[usage.is_some().then_some(&enter)]
                            data-on:pointerleave=[usage.is_some().then_some("evt.pointerType == 'mouse' && ($_step = 0)")]
                            data-on:blur=[usage.is_some().then_some("$_step = 0")]
                            data-class:linked=[usage.is_some().then(|| format!("$_step == {number}"))] {
                            span.step-name {
                                (step.name)
                                @if let Some(usage) = &usage {
                                    @let id = format!("step-{number}-usage");
                                    (parts.html(&id, html! { span.visually-hidden #(id) { ", " (usage.description()) } }))
                                }
                            }
                            @let id = format!("step-{number}-time");
                            (parts.html(&id, html! {
                                span.step-time #(id) {
                                    @match (seconds, outcome.kind) {
                                        // A step that ended without running says how it ended: `skipped`.
                                        (Some(0) | None, OutcomeKind::Ended) => (outcome.label),
                                        (Some(seconds), OutcomeKind::Running) => { (format::duration(seconds)) "…" },
                                        (Some(seconds), _) => { (format::duration(seconds)) },
                                        (None, _) => "–",
                                    }
                                }
                            }))
                            span.step-track aria-hidden="true" {
                                @if let (Some(start), Some(span), Some(from)) = (start, span, step.started_at) {
                                    @let to = from + seconds.unwrap_or(0);
                                    @let left = from.saturating_sub(start);
                                    @let right = to.saturating_sub(start);
                                    @let follows = live && step.is_running();
                                    @let style = format!("--from: {left}; --to: {right}");
                                    span.step-bar style=(if follows { parts.clocked(style) } else { style })
                                        data-style:--to=[follows.then(|| format!("$shown.at - {start}"))] {}
                                    // Shown on the side of the track away from the bar while the row is lit up:
                                    // the charts may be off screen.
                                    @if let Some(usage) = &usage {
                                        @let id = format!("step-{number}-note");
                                        (parts.html(&id, html! {
                                            span #(id) class=(if left + right > span { "step-usage start" } else { "step-usage end" }) { (usage.description()) }
                                        }))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// CPU and memory over the job's life, against what admission reserved and its container's caps or else its profile's
/// limits as configured now, with the totals.
pub(crate) fn metrics(view: &JobView, parts: &Parts) -> Markup {
    let summary = &view.job.summary;
    let samples = &view.samples.samples;
    let start = samples
        .first()
        .map(|sample| sample.at)
        .or(summary.timeline.container_started_at)
        .unwrap_or(summary.timeline.queued_at);
    // A running job's charts reach the page's clock.
    let live = view.live();
    let last = samples.last().map_or(view.now, |sample| sample.at);
    let end = if live { last.max(view.now) } else { last }.max(start + 1);
    let gap = view.samples.interval.max(1) * 3;
    let memory_peak = samples.iter().map(|s| s.memory).max().unwrap_or(0);
    let cpu_peak = samples.iter().map(|s| s.cpu).max().unwrap_or(0);
    let LimitLines {
        memory: memory_line,
        cpu: cpu_line,
        configured_now,
    } = limit_lines(view);
    let highest = |lines: &[Limit]| lines.iter().map(|line| line.value).max().unwrap_or(0);
    let memory_limit = highest(&memory_line);
    let cpu_limit = highest(&cpu_line);
    let profile = summary
        .runner
        .as_ref()
        .map_or("profile", |runner| runner.profile.as_str());
    let time = |at: u64| format!("{} UTC", format::clock(at));
    let memory = TimeChart {
        title: "Memory over the job".to_owned(),
        start,
        end,
        window: None,
        live: live.then(|| "metrics-memory".to_owned()),
        series: vec![Series {
            label: "memory".to_owned(),
            class: "memory",
            points: samples.iter().map(|s| (s.at, s.memory)).collect(),
            area: true,
        }],
        limits: memory_line,
        value: format::bytes,
        time,
        gap,
    };
    let cpu = TimeChart {
        title: "CPU over the job".to_owned(),
        start,
        end,
        window: None,
        live: live.then(|| "metrics-cpu".to_owned()),
        series: vec![Series {
            label: "CPU".to_owned(),
            class: "cpu",
            points: samples.iter().map(|s| (s.at, s.cpu)).collect(),
            area: true,
        }],
        limits: cpu_line,
        value: in_cores,
        time,
        gap,
    };
    let bands = step_bands(view);
    html! {
        section #metrics .metrics aria-labelledby="metrics-title" {
            h2 #metrics-title .visually-hidden { "Resources" }
            @if configured_now {
                p.muted { "This record lacks its container's caps: the lines marked “" (profile) " now” are its limits in the config now, which may not be the ones the job ran with." }
            }
            @if samples.is_empty() {
                p.muted { "No samples: the runner's container never ran, or its samples are past `history.samples`." }
            } @else {
                div.charts {
                    div.chart-card {
                        h3 { "Memory" (parts.html("metrics-memory-peak", html! { span.reading #metrics-memory-peak { "sampled peak " (format::bytes(memory_peak)) } })) }
                        (memory.render_with(nice_top(memory_peak.max(memory_limit), Base::Binary), &bands.memory, parts))
                    }
                    div.chart-card {
                        h3 { "CPU" (parts.html("metrics-cpu-peak", html! { span.reading #metrics-cpu-peak { "sampled peak " (in_cores(cpu_peak)) } })) }
                        (cpu.render_with(nice_top(cpu_peak.max(cpu_limit), Base::Decimal), &bands.cpu, parts))
                    }
                }
            }
            (totals(view))
        }
    }
}

/// The job's totals over its whole run, which history keeps after its samples are deleted.
fn totals(view: &JobView) -> Markup {
    let metrics = &view.job.summary.metrics;
    html! {
        dl.totals {
            @for total in metrics.totals() {
                div { dt { (total.label) } dd { (total.value) } }
            }
            @if let Some(held) = view.job.summary.cpu_held(view.now) {
                div { dt { "CPU held back" } dd { (held) } }
            }
            (build_cache(&metrics.build_cache))
        }
    }
}

/// What the shared build cache served the job and took from it, for a job that used it at all.
fn build_cache(cache: &BuildCacheCounts) -> Markup {
    html! {
        @if let Some(activity) = cache.activity() {
            div {
                dt { "From the build cache" }
                dd { (activity.lookups_line()) ", " (activity.served_line()) }
            }
            div {
                dt { "To the build cache" }
                dd {
                    (activity.stored_line())
                    @if let Some(refused) = activity.refused_line() { ", " (refused) }
                }
            }
        }
    }
}

/// The same job's latest runs, oldest first: a column each, its wait under its run time on one scale, and its
/// memory peak under it against the largest; this run is outlined, as it is now. Each column opens its run. A live
/// region: as this run ends its column, the medians and the failures follow.
fn runs(view: &JobView) -> Markup {
    let compared = view.job.compare(&view.runs, view.now);
    let runs = &compared.runs;
    let longest = runs
        .iter()
        .map(|run| run.waited(view.now).unwrap_or(0) + run.ran(view.now).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let peak = compared.largest_peak.unwrap_or(0);
    let ratio = |part: u64, whole: u64| format!("{:.3}", crate::chart::ratio(part, whole));
    html! {
        @if runs.len() > 1 {
            section #runs class=(format!("runs {}", view.profile_class)) aria-labelledby="runs-title" {
                div.section-head {
                    h2 #runs-title { "Latest " (runs.len()) " runs of this job" }
                    a href=(format!("/history?repository={}", view.job.summary.repository)) { "History of " (view.job.summary.repository) }
                }
                dl.headline {
                    div { dt { "Median run" } dd { (compared.median_ran.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Median wait" } dd { (compared.median_waited.map_or("–".to_owned(), format::duration)) } }
                    div { dt { "Failed" } dd { (compared.failed) " of " (compared.finished) } }
                    div { dt { "Largest memory peak" } dd { (if peak > 0 { format::bytes(peak) } else { "–".to_owned() }) } }
                }
                p.run-scale aria-hidden="true" { "Wait and run, up to " (format::duration(longest)) }
                // On a narrow screen the runs scroll sideways, and start scrolled to the latest.
                div.run-scroll {
                    ol.run-bars style=(format!("--runs: {}", runs.len())) {
                        @for run in runs {
                            @let waited = run.waited(view.now).unwrap_or(0);
                            @let ran = run.ran(view.now).unwrap_or(0);
                            @let this = run.id == view.job.summary.id;
                            @let label = run_label(run, view.now);
                            li class=(if this { "this" } else { "" }) {
                                a href=(format!("/jobs/{}", run.id)) aria-label=(label) title=(label)
                                    aria-current=[this.then_some("page")] {
                                    span.run-time aria-hidden="true" {
                                        span class=(format!("ran {}", outcome_class(run))) style=(format!("--h: {}", ratio(ran, longest))) {}
                                        span.waited style=(format!("--h: {}", ratio(waited, longest))) {}
                                    }
                                    span.run-peak aria-hidden="true" {
                                        span style=(format!("--h: {}", ratio(run.metrics.memory_peak.unwrap_or(0), peak))) {}
                                    }
                                }
                            }
                        }
                    }
                }
                ul.legend {
                    li.success { span.swatch {} "ran, succeeded" }
                    li.failure { span.swatch {} "ran, failed" }
                    li.wait { span.swatch {} "waited for a runner" }
                    li.peak { span.swatch {} "memory peak" }
                }
            }
        } @else {
            // The region's place, for when a second run comes.
            section #runs hidden {}
        }
    }
}

/// A run's column, in words: `Oct 22 14:02 UTC, success: waited 12s, ran 7m 34s, memory peak 4.1G`.
fn run_label(run: &JobSummary, now: u64) -> String {
    let mut label = format!(
        "{} {} UTC, {}",
        format::short_date(run.timeline.queued_at),
        format::clock(run.timeline.queued_at),
        run.outcome().label
    );
    match (run.waited(now), run.ran(now)) {
        (Some(waited), Some(ran)) => {
            let verb = if run.state == JobState::Running {
                "running for"
            } else {
                "ran"
            };
            let _ = write!(
                label,
                ": waited {}, {verb} {}",
                format::duration(waited),
                format::duration(ran)
            );
        }
        (Some(waited), None) if run.timeline.started_at.is_some() => {
            let _ = write!(label, ": waited {}", format::duration(waited));
        }
        (Some(waited), None) => {
            let _ = write!(label, ": waiting for {}", format::duration(waited));
        }
        (None, _) => label.push_str(": did not start"),
    }
    if let Some(peak) = run.metrics.memory_peak {
        let _ = write!(label, ", memory peak {}", format::bytes(peak));
    }
    label
}

/// Each step's stretch over the memory and CPU charts, labelled with what it used.
struct StepBands {
    memory: Vec<Band>,
    cpu: Vec<Band>,
}

fn step_bands(view: &JobView) -> StepBands {
    let mut bands = StepBands {
        memory: Vec::new(),
        cpu: Vec::new(),
    };
    for (index, step) in view.job.steps.iter().enumerate() {
        let Some(from) = step.started_at else {
            continue;
        };
        let end = from + step.took(view.now).unwrap_or(0);
        let Some(usage) = view.samples.usage_between(from, end) else {
            continue;
        };
        let to = step.completed_at;
        let shown = format!("$_step == {}", index + 1);
        bands.memory.push(Band {
            from,
            to,
            shown: shown.clone(),
            label: usage.memory(),
        });
        bands.cpu.push(Band {
            from,
            to,
            shown,
            label: usage.cpu(),
        });
    }
    bands
}

struct LimitLines {
    memory: Vec<Limit>,
    cpu: Vec<Limit>,
    /// Some line is the profile's limit in the config now, the record lacking its container's cap.
    configured_now: bool,
}

/// The job's chart lines as `Job::chart_lines` labels them, a reservation drawn apart from a cap.
fn limit_lines(view: &JobView) -> LimitLines {
    let lines = view.job.chart_lines(view.profile.as_ref());
    let configured_now = lines
        .memory
        .iter()
        .chain(&lines.cpu)
        .any(|line| line.kind == ChartLineKind::ProfileNow);
    let limits = |lines: Vec<ChartLine>| {
        lines
            .into_iter()
            .map(|line| Limit {
                value: line.value,
                class: if line.kind == ChartLineKind::Reserved {
                    "reserved"
                } else {
                    ""
                },
                label: line.label,
            })
            .collect()
    };
    LimitLines {
        memory: limits(lines.memory),
        cpu: limits(lines.cpu),
        configured_now,
    }
}

/// What became of the failed job's record: the command that opens a shell in it while it is kept.
fn record(view: &JobView) -> Markup {
    let Some(note) = &view.job.record else {
        return html! { div #record {} };
    };
    // The failure summary has the shell command when there is one.
    let summarized = matches!(&view.outlined, Outlined::Read { outline, .. } if outline.failure.as_ref().is_some_and(|failure| failure.shell.is_some()));
    if summarized && matches!(note.state(), RecordState::Kept { .. }) {
        return html! { div #record {} };
    }
    let id = view.job.summary.id;
    let command = RecordNote::shell_command(id);
    html! {
        section #record .record aria-labelledby="record-title" {
            @match note.state() {
                state @ RecordState::Skipped { .. } => {
                    h2 #record-title { "Record" }
                    p.muted { (format::sentence(&state.line())) "." }
                }
                state @ RecordState::Removed { at, .. } => {
                    h2 #record-title { "Record" }
                    p.muted { "Kept, then " (state.line()) " " (time_ago(at, view.now)) "." }
                }
                state @ RecordState::Gone { .. } => {
                    h2 #record-title { "Record" }
                    p.muted { "Kept, then " (state.line()) "." }
                }
                state @ RecordState::Kept { .. } => {
                div.section-head {
                    h2 #record-title { "Open a shell" }
                    p.muted { (format::sentence(&state.line())) }
                }
                p {
                    "runnerd kept this job's runner as it failed. In a terminal on the host, this opens a shell in its "
                    "workspace and shows the step that failed:"
                }
                (copy_command(&command))
                }
            }
        }
    }
}

/// The log: a search of its messages, then core's outline of a finished job's lines, or a running job's live tail.
pub(crate) fn log(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        section #log .log aria-labelledby="log-title" {
            div.section-head {
                h2 #log-title { "Log" }
                p.muted {
                    a href=(format!("/jobs/{}/log", summary.id)) { "Full log" }
                    " · " (summary.log.description())
                }
            }
            (search_form(view))
            (search_results(view))
            @match &view.outlined {
                Outlined::Read { outline, lines, first } => {
                    @if let Some(words) = outline.evidence.words() {
                        p class=(format!("evidence {}", tone_class(outline.evidence.tone()))) { (format::sentence(words)) "." }
                    }
                    @if let Some(tail) = LogOutline::tail_words(*first, lines.len()) {
                        p.muted { (format::sentence(&tail)) "." }
                    }
                    (outlined(view, outline, lines))
                }
                Outlined::Unsupported => {
                    p.evidence.tone-quiet { (format::sentence(&Feature::LogOutline.unsupported())) "." }
                    (tail(view))
                }
                Outlined::Live => {
                    @if let Some(words) = LogEvidence::Running.words() {
                        p class=(format!("evidence {}", tone_class(LogEvidence::Running.tone()))) { (format::sentence(words)) "." }
                    }
                    label.follow {
                        input type="checkbox" data-follow="log-lines" checked;
                        " Follow"
                    }
                    (tail(view))
                }
            }
        }
    }
}

/// The plain tail: a running job's, which the stream appends to, or an older daemon's.
fn tail(view: &JobView) -> Markup {
    let summary = &view.job.summary;
    html! {
        @if view.log_from > 0 {
            p.muted { "Showing the last " (view.log.len()) " lines." }
        }
        @if view.log.is_empty() && summary.log.lines > 0 {
            p.muted { "The log's lines are past " code { "history.logs" } " and were deleted." }
        }
        (log_lines(view))
    }
}

/// The search of the log's messages, a plain form: its results are a page of their own.
fn search_form(view: &JobView) -> Markup {
    let query = match &view.searched {
        Some(Searched::Found { query, .. }) => Some(query.as_str()),
        _ => None,
    };
    html! {
        form.log-search-form method="get" role="search" {
            label.field.search {
                span.field-name { "Search each line's message, not its time or command" }
                input type="search" name="q" value=[query] data-search autocomplete="off" spellcheck="false" maxlength="200";
            }
            button.button-quiet type="submit" { "Search" }
            @if query.is_some() { a.clear href="?" { "Clear" } }
        }
    }
}

fn search_results(view: &JobView) -> Markup {
    let Some(searched) = &view.searched else {
        return html! {};
    };
    html! {
        @match searched {
            Searched::Unsupported => { p.notice.tone-warn { (format::sentence(&Feature::LogSearch.unsupported())) "." } }
            Searched::Refused(reason) => { p.notice.tone-warn role="alert" { (format::sentence(reason)) "." } }
            Searched::Found { query, search } => {
                section.log-results aria-label="Matches" {
                    p.muted { (format::sentence(&search.found_words(query))) "." }
                    @if !search.lines.is_empty() {
                        ol.log-lines.matches {
                            @for line in &search.lines {
                                @let hit = search.matches.contains(&line.line);
                                li class=(if hit { "hit" } else { "context" }) {
                                    a.ln href=(format!("?line={}#L{}", line.line + 1, line.line + 1)) { (line.line + 1) }
                                    span.text { (styled_runs(line)) }
                                }
                            }
                        }
                    }
                    @if let Some(next) = search.next {
                        p.pager { a.button-quiet href=(format!("?{}", serde_urlencoded::to_string([("q", query.as_str()), ("from", &next.to_string())]).unwrap_or_default())) { "Search on" } }
                    }
                }
            }
        }
    }
}

/// Which part of the log a line is in: a step's, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Step(u32),
    Unattributed,
}

/// The lines as core outlines them: a fold per step, holding a fold per group; the failed step open, and the
/// line the page was opened at.
fn outlined(view: &JobView, outline: &LogOutline, lines: &[StyledLine]) -> Markup {
    let ranges: Vec<(u32, LineRange)> = outline
        .steps
        .iter()
        .filter_map(|step| match step.lines {
            StepLog::Lines(range) => Some((step.number, range)),
            StepLog::Empty | StepLog::Unattributed | StepLog::Unknown => None,
        })
        .collect();
    let owner = |line: u64| {
        ranges
            .iter()
            .find(|(_, range)| (range.from..range.to).contains(&line))
            .map_or(Owner::Unattributed, |(number, _)| Owner::Step(*number))
    };
    let failed = outline.failure.as_ref().and_then(|failure| failure.step);
    let mut segments: Vec<(Owner, &[StyledLine])> = Vec::new();
    let mut start = 0;
    for index in 1..=lines.len() {
        if index == lines.len() || owner(lines[index].line) != owner(lines[start].line) {
            segments.push((owner(lines[start].line), &lines[start..index]));
            start = index;
        }
    }
    if lines.is_empty() {
        return html! {};
    }
    html! {
        div.log-view #log-view tabindex="0" aria-label="Log lines" {
            @for (who, part) in &segments {
                @let target = view.line.is_some_and(|line| part.iter().any(|each| each.line == line));
                @let open = target || matches!(who, Owner::Step(number) if Some(*number) == failed);
                details.log-step.failed[open && !target && matches!(who, Owner::Step(number) if Some(*number) == failed)] open[open]
                    data-scroll-here[open && (target || view.line.is_none())] {
                    summary {
                        @match who {
                            Owner::Step(number) => {
                                span.step-number { (number) }
                                (view.job.steps.iter().find(|step| step.number == *number).map_or_else(|| format!("Step {number}"), |step| step.name.clone()))
                            }
                            Owner::Unattributed => { span.step-number { "–" } "Lines no step claims" }
                        }
                        span.muted.step-lines { (part.len()) " lines" }
                    }
                    ol.log-lines { (lines_of(part, view.line)) }
                }
            }
        }
    }
}

/// `lines` with each `##[group]` folded up to its `##[endgroup]`, open when it holds `target`.
fn lines_of(lines: &[StyledLine], target: Option<u64>) -> Markup {
    let mut out = String::new();
    let mut index = 0;
    while let Some(line) = lines.get(index) {
        match line.kind {
            LineKind::Group => {
                let mut depth = 0u32;
                let mut end = None;
                for (offset, inner) in lines[index + 1..].iter().enumerate() {
                    match inner.kind {
                        LineKind::Group => depth += 1,
                        LineKind::EndGroup if depth == 0 => {
                            end = Some(index + 1 + offset);
                            break;
                        }
                        LineKind::EndGroup => depth -= 1,
                        _ => {}
                    }
                }
                let inner = &lines[index + 1..end.unwrap_or(lines.len())];
                let open = target.is_some_and(|target| {
                    target == line.line || inner.iter().any(|each| each.line == target)
                });
                out.push_str(
                    &html! {
                        li.group id=(format!("L{}", line.line + 1)) {
                            details open[open] {
                                summary { (line_number(line)) span.text { (styled_runs(line)) } }
                                ol.log-lines { (lines_of(inner, target)) }
                            }
                        }
                    }
                    .into_string(),
                );
                index = end.map_or(lines.len(), |end| end + 1);
            }
            LineKind::EndGroup => index += 1,
            _ => {
                out.push_str(&styled_line(line, target, Place::Log).into_string());
                index += 1;
            }
        }
    }
    maud::PreEscaped(out)
}

/// A line's number, a link to it.
fn line_number(line: &StyledLine) -> Markup {
    let number = line.line + 1;
    html! { a.ln href=(format!("?line={number}#L{number}")) aria-label=(format!("Line {number}")) { (number) } }
}

/// One line: its number, its text in core's colours, and how much of it the full log has past what is shown.
/// Where a styled line is drawn: in the log, which a `#L{n}` link reaches, or as a copy elsewhere, such as the failure
/// summary's last lines, which links to the log's line instead of taking its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Log,
    Copy,
}

fn styled_line(line: &StyledLine, target: Option<u64>, place: Place) -> Markup {
    let class = match line.kind {
        LineKind::Error => "error",
        LineKind::Warning => "warning",
        LineKind::Notice => "notice",
        LineKind::Debug => "debug",
        LineKind::Command => "command",
        _ => "",
    };
    html! {
        li id=[(place == Place::Log).then(|| format!("L{}", line.line + 1))] class=[(!class.is_empty()).then_some(class)]
            aria-current=[(place == Place::Log && target == Some(line.line)).then_some("true")] {
            (line_number(line))
            span.text { (styled_runs(line)) }
            @if let Some(omitted) = line.omitted_words() { " " span.omitted { "(" (omitted) ")" } }
        }
    }
}

/// A line's runs, each in the colour, weight and mark core gives it.
fn styled_runs(line: &StyledLine) -> Markup {
    html! {
        @for run in &line.runs {
            @let color = run.color.and_then(color_class);
            @if color.is_none() && !run.bold && !run.mark {
                (run.text)
            } @else if run.mark {
                mark class=[color] { (run.text) }
            } @else {
                span class=(format!("{}{}", color.unwrap_or(""), if run.bold { " b" } else { "" })) { (run.text) }
            }
        }
    }
}

/// The class of a colour core gives a run.
fn color_class(color: Color) -> Option<&'static str> {
    match color {
        Color::Red => Some("c-red"),
        Color::Green => Some("c-green"),
        Color::Yellow => Some("c-yellow"),
        Color::Blue => Some("c-blue"),
        Color::Magenta => Some("c-magenta"),
        Color::Cyan => Some("c-cyan"),
        Color::Grey => Some("c-grey"),
        Color::Unknown => None,
    }
}

/// The log's last lines, which the live stream replaces whenever it connects and appends to after.
pub(crate) fn log_lines(view: &JobView) -> Markup {
    html! {
        ol #log-lines .log-lines.tail tabindex="0" aria-label="Log lines" {
            @for (number, line) in (view.log_from + 1..).zip(&view.log) { (log_line(number, line)) }
        }
    }
}

/// Log line `number`, counted from 1, in core's colours. Each line carries its number, so the stream can drop the
/// oldest from the page without renumbering the rest.
pub(crate) fn log_line(number: u64, text: &str) -> Markup {
    let line = StyledLine::parse(number.saturating_sub(1), text);
    html! { li value=(number) { span.ln aria-hidden="true" { (number) } span.text { (styled_runs(&line)) } } }
}

/// The last of the log lines a stream appends: it also sets `shown.log` to its number, in the same patch as the
/// lines, so what the page says it has stays what it has even if the stream drops before its next signal.
pub(crate) fn last_log_line(number: u64, text: &str) -> Markup {
    let line = StyledLine::parse(number.saturating_sub(1), text);
    html! { li value=(number) "data-signals:shown.log"=(number) { span.ln aria-hidden="true" { (number) } span.text { (styled_runs(&line)) } } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coloured_log_line_shows_its_text_in_cores_colours_without_escapes() {
        let line = "\u{1b}[36;1mdocker info --format x\u{1b}[0m done";
        let html = log_line(7, line).into_string();
        assert!(!html.contains('\u{1b}'), "{html}");
        assert!(
            html.contains(r#"<span class="c-cyan b">docker info --format x</span> done"#),
            "{html}"
        );
    }
}
