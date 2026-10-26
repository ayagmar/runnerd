//! What people read: API types turned into aligned text, with durations like `1m 12s`, binary sizes like `213M`
//! and colour when the output is a terminal. Pure functions of the API types and the current time; printing is
//! `output`'s, and nothing here decides anything the daemon has not. Figures, durations, outcomes and names are
//! `runnerd_core::api`'s, as the dashboard writes them; this module only lays them out and colours them.

use std::fmt::Write as _;

use runnerd_core::api::{
    self, BuildCacheCounts, Caches, Digest, Event, EventEnvelope, Insights, Job, JobPage,
    JobSizing, JobSummary, NotifyTest, ProfilePlan, QueuedJob, RecordNote, RecordState, Records,
    Sizing, SlotState, Status, Step, WaitSpread,
};
pub(crate) use runnerd_core::api::{ago, cores_used, cpu_time, duration};
use runnerd_core::api::{in_cores, job_title};
use runnerd_core::config::ByteSize;

/// How a piece of text should stand out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Bold,
    Dim,
    Good,
    Bad,
    Busy,
}

impl From<api::Tone> for Tone {
    fn from(tone: api::Tone) -> Tone {
        match tone {
            api::Tone::Good => Tone::Good,
            api::Tone::Warn => Tone::Busy,
            api::Tone::Bad => Tone::Bad,
            api::Tone::Neutral => Tone::Plain,
            api::Tone::Quiet => Tone::Dim,
        }
    }
}

/// Colours text, or leaves it plain.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Paint {
    pub(crate) colour: bool,
}

impl Paint {
    pub(crate) fn paint(self, tone: Tone, text: &str) -> String {
        let code = match tone {
            Tone::Plain => return text.to_owned(),
            Tone::Bold => "1",
            Tone::Dim => "2",
            Tone::Good => "32",
            Tone::Bad => "31",
            Tone::Busy => "33",
        };
        if self.colour && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

/// Binary sizes as `runnerd.toml` writes them, and as the dashboard shows them: `512B`, `20K`, `213M`, `4.37G`, `9G`.
pub(crate) fn bytes(n: u64) -> String {
    ByteSize(n).to_string()
}

/// Columns padded to their widest cell, two spaces apart. Colour codes do not count towards widths.
#[derive(Debug, Default)]
pub(crate) struct Table {
    rows: Vec<Vec<(String, Tone)>>,
    right: Vec<usize>,
}

impl Table {
    /// A table whose first row is `headers`, dimmed.
    pub(crate) fn new(headers: &[&str]) -> Table {
        Table {
            rows: vec![
                headers
                    .iter()
                    .map(|header| ((*header).to_owned(), Tone::Dim))
                    .collect(),
            ],
            right: Vec::new(),
        }
    }

    /// Aligns these columns to the right, as numbers read best.
    pub(crate) fn right(mut self, columns: &[usize]) -> Table {
        self.right = columns.to_vec();
        self
    }

    pub(crate) fn row(&mut self, cells: Vec<(String, Tone)>) {
        self.rows.push(cells);
    }

    pub(crate) fn render(&self, paint: Paint, indent: &str) -> String {
        let columns = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                self.rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|(text, _)| text.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let mut out = String::new();
        for row in &self.rows {
            let mut line = String::from(indent);
            for (column, (text, tone)) in row.iter().enumerate() {
                let pad = widths[column].saturating_sub(text.chars().count());
                let last = column + 1 == row.len();
                if column > 0 {
                    line.push_str("  ");
                }
                if self.right.contains(&column) {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(&paint.paint(*tone, text));
                } else {
                    line.push_str(&paint.paint(*tone, text));
                    if !last {
                        line.push_str(&" ".repeat(pad));
                    }
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

fn cell(text: impl Into<String>, tone: Tone) -> (String, Tone) {
    (text.into(), tone)
}

fn plain(text: impl Into<String>) -> (String, Tone) {
    cell(text, Tone::Plain)
}

fn dash() -> (String, Tone) {
    cell("-", Tone::Dim)
}

/// How the terminal colours an outcome's kind.
pub(crate) fn outcome_tone(kind: api::OutcomeKind) -> Tone {
    match kind {
        api::OutcomeKind::Succeeded => Tone::Good,
        api::OutcomeKind::Failed => Tone::Bad,
        api::OutcomeKind::Running | api::OutcomeKind::Waiting => Tone::Busy,
        api::OutcomeKind::Ended => Tone::Dim,
    }
}

/// A job's outcome as one word, as every client words it, and how to colour it.
pub(crate) fn outcome(job: &JobSummary) -> (String, Tone) {
    let outcome = job.outcome();
    (outcome.label.into_owned(), outcome_tone(outcome.kind))
}

/// Why a queued job waits, as the dashboard and the TUI say it; nothing when the daemon did not say.
pub(crate) fn waiting(job: &QueuedJob, status: &Status, now: u64) -> String {
    job.waiting
        .as_ref()
        .map(|waiting| waiting.reason(job, status, now))
        .unwrap_or_default()
}

/// How long ago a queued job was queued; nothing when GitHub did not say.
pub(crate) fn queued_ago(job: &QueuedJob, now: u64) -> String {
    if job.queued_at == 0 {
        String::new()
    } else {
        ago(job.queued_at, now)
    }
}

/// `runnerctl status`.
pub(crate) fn status(status: &Status, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{} {}  {}",
        paint.paint(Tone::Bold, "runnerd"),
        status.version,
        status.budget.line()
    );
    if status.shutting_down {
        let _ = write!(
            out,
            "  {}",
            paint.paint(Tone::Busy, "shutting down: waiting for running jobs")
        );
    }
    out.push_str("\n\n");
    let mut slots =
        Table::new(&["PROFILE", "SLOT", "STATE", "REPOSITORY", "RUNNER", "UP"]).right(&[1, 5]);
    for profile in &status.profiles {
        for slot in &profile.slots {
            let mut row = vec![plain(&profile.name), plain(slot.slot.to_string())];
            let state = Status::slot_state(profile, slot);
            row.push(cell(state.word(), state.tone().into()));
            if let Some(runner) = &slot.runner {
                row.push(plain(&runner.repository));
                row.push(cell(&runner.name, Tone::Dim));
                row.push(plain(duration(now.saturating_sub(runner.started_at))));
            }
            slots.row(row);
        }
    }
    out.push_str(&slots.render(paint, ""));
    for profile in status.profiles.iter().filter(|profile| profile.reseeding) {
        let _ = writeln!(
            out,
            "{}",
            paint.paint(
                Tone::Busy,
                &format!("{}: reseeding its snapshot from slot 1", profile.name)
            )
        );
    }
    out.push('\n');
    let mut repositories =
        Table::new(&["REPOSITORY", "PROFILE", "STATE", "QUEUED", "LABELS"]).right(&[3]);
    // A row per pool; the repository's name and state on its first.
    for repository in &status.repositories {
        for (at, pool) in repository.listed_pools().iter().enumerate() {
            let first = at == 0;
            repositories.row(vec![
                plain(if first { repository.name.as_str() } else { "" }),
                plain(&pool.profile),
                match (first, repository.paused) {
                    (false, _) => plain(""),
                    (true, true) => cell("paused", Tone::Busy),
                    (true, false) => cell("active", Tone::Good),
                },
                plain(pool.queued_jobs.to_string()),
                cell(pool.labels.join(","), Tone::Dim),
            ]);
        }
    }
    out.push_str(&repositories.render(paint, ""));
    if !status.queued.is_empty() {
        out.push('\n');
        let mut queued = Table::new(&[
            "JOB",
            "REPOSITORY",
            "WORKFLOW / NAME",
            "QUEUED",
            "WAITS FOR",
        ]);
        for job in &status.queued {
            queued.row(vec![
                plain(job.id.to_string()),
                plain(job.place(status)),
                plain(job.title().to_string()),
                cell(queued_ago(job, now), Tone::Dim),
                plain(waiting(job, status, now)),
            ]);
        }
        out.push_str(&queued.render(paint, ""));
    }
    out
}

/// `runnerctl jobs`.
pub(crate) fn jobs(page: &JobPage, now: u64, paint: Paint) -> String {
    if page.jobs.is_empty() {
        return "no jobs match\n".to_owned();
    }
    let mut table = Table::new(&[
        "JOB",
        "REPOSITORY",
        "WORKFLOW / NAME",
        "BRANCH",
        "RESULT",
        "WAITED",
        "RAN",
        "MEM PEAK",
        "QUEUED",
    ])
    .right(&[5, 6, 7]);
    for job in &page.jobs {
        table.row(vec![
            cell(job.id.to_string(), Tone::Dim),
            plain(&job.repository),
            plain(job.title().to_string()),
            job.branch.as_deref().map_or_else(dash, plain),
            outcome(job),
            job.waited(now).map_or_else(dash, |s| plain(duration(s))),
            job.ran(now).map_or_else(dash, |s| plain(duration(s))),
            job.metrics
                .memory_peak
                .map_or_else(dash, |m| plain(bytes(m))),
            cell(ago(job.timeline.queued_at, now), Tone::Dim),
        ]);
    }
    let mut out = table.render(paint, "");
    if page
        .jobs
        .iter()
        .any(|job| job.metrics.memory_peak.is_some())
    {
        let _ = writeln!(
            out,
            "{}",
            paint.paint(Tone::Dim, &api::sampled_note(api::SAMPLE_INTERVAL))
        );
    }
    if let Some(next) = page.next {
        let _ = writeln!(
            out,
            "{}",
            paint.paint(Tone::Dim, &format!("older jobs: --before {next}"))
        );
    }
    out
}

/// A step's mark for its outcome: `✓` succeeded, `✗` failed, `●` running, `○` waiting, `-` neither.
fn step_mark(step: &Step) -> (String, Tone) {
    match step.outcome().kind {
        api::OutcomeKind::Succeeded => cell("✓", Tone::Good),
        api::OutcomeKind::Failed => cell("✗", Tone::Bad),
        api::OutcomeKind::Running => cell("●", Tone::Busy),
        api::OutcomeKind::Waiting => cell("○", Tone::Dim),
        api::OutcomeKind::Ended => cell("-", Tone::Dim),
    }
}

/// `, capped at 6G and 4 CPUs`: what the job's container was held to, when the record has it.
fn caps(job: &Job) -> String {
    job.limits(None)
        .line()
        .map_or_else(String::new, |limits| format!(", {limits}"))
}

/// `runnerctl job <id>`.
pub(crate) fn job(job: &Job, now: u64, paint: Paint) -> String {
    let summary = &job.summary;
    let mut out = String::new();
    let (result, tone) = outcome(summary);
    let _ = writeln!(
        out,
        "{}  {}  {}",
        paint.paint(Tone::Bold, &summary.title().to_string()),
        paint.paint(tone, &result),
        paint.paint(Tone::Dim, &format!("job {}", summary.id))
    );
    let mut context = vec![summary.repository.clone()];
    if let Some(branch) = &summary.branch {
        context.push(branch.clone());
    }
    context.push(format!("run {}", summary.run_id));
    if summary.run_attempt > 1 {
        context.push(format!("attempt {}", summary.run_attempt));
    }
    let _ = writeln!(out, "{}", context.join(" · "));
    if let Some(url) = &summary.url {
        let _ = writeln!(out, "{}", paint.paint(Tone::Dim, url));
    }
    if let Some(runner) = &summary.runner {
        let _ = writeln!(
            out,
            "runner {} {}",
            runner.name,
            paint.paint(
                Tone::Dim,
                &format!(
                    "(profile {}, slot {}{})",
                    runner.profile,
                    runner.slot,
                    caps(job)
                )
            )
        );
    }
    if let Some(admission) = &job.admission {
        let _ = writeln!(out, "{}", paint.paint(Tone::Dim, &admission.line()));
    }
    out.push_str(&timeline(summary, now, paint));
    out.push_str(&steps(&job.steps, now, paint));
    out.push_str(&usage(summary, paint));
    if let Some(note) = &job.record {
        let _ = writeln!(out, "{}", record_note(summary.id, note, now, paint));
    }
    out
}

/// What became of a failed job's record.
fn record_note(job: u64, note: &RecordNote, now: u64, paint: Paint) -> String {
    let state = note.state();
    match state {
        RecordState::Skipped { .. } => format!("record {}", paint.paint(Tone::Dim, &state.line())),
        RecordState::Removed { at, .. } => format!(
            "record {}",
            paint.paint(Tone::Dim, &format!("{}, {}", state.line(), ago(at, now)))
        ),
        RecordState::Kept { .. } => format!(
            "{}: `{}` opens a shell in it",
            state.line(),
            RecordNote::shell_command(job)
        ),
    }
}

/// `runnerctl records`.
pub(crate) fn records(records: &Records, now: u64, paint: Paint) -> String {
    let mut out = format!(
        "{}\n",
        paint.paint(records.summary_tone().into(), &records.summary())
    );
    if records.records.is_empty() {
        out.push_str(&paint.paint(Tone::Dim, Records::empty_reason()));
        out.push('\n');
        return out;
    }
    let mut table = Table::new(&["JOB", "REPOSITORY", "NAME", "SIZE", "MADE"]).right(&[3]);
    for record in &records.records {
        let mut made = ago(record.created_at, now);
        if records.shell_open(record.job) {
            made.push_str(", shell open");
        }
        table.row(vec![
            plain(record.job.to_string()),
            plain(&record.repository),
            cell(record.title().to_string(), Tone::Dim),
            plain(bytes(record.bytes)),
            cell(made, Tone::Dim),
        ]);
    }
    out.push_str(&table.render(paint, ""));
    out
}

fn timeline(summary: &JobSummary, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    let timeline = &summary.timeline;
    let _ = writeln!(
        out,
        "\n{}  {}",
        paint.paint(Tone::Bold, "Timeline"),
        paint.paint(
            Tone::Dim,
            &format!("queued {}", ago(timeline.queued_at, now))
        )
    );
    let mut table = Table::default().right(&[1]);
    // A warm runner's container started before the job was queued: it comes first, before `+0s`.
    for milestone in timeline.milestones() {
        let at = milestone.at;
        let offset = if at < timeline.queued_at {
            format!("-{}", duration(timeline.queued_at - at))
        } else {
            format!("+{}", duration(at - timeline.queued_at))
        };
        table.row(vec![plain(milestone.label), plain(offset)]);
    }
    out.push_str(&table.render(paint, "  "));
    out
}

fn steps(steps: &[Step], now: u64, paint: Paint) -> String {
    let mut out = String::new();
    if !steps.is_empty() {
        let _ = writeln!(out, "\n{}", paint.paint(Tone::Bold, "Steps"));
        let mut table = Table::default().right(&[1, 3]);
        for step in steps {
            let took = step.took(now).map_or_else(String::new, duration);
            let outcome = step.outcome();
            // The mark says success, running and waiting; the word says how any other step ended.
            let note = match outcome.kind {
                api::OutcomeKind::Failed | api::OutcomeKind::Ended => outcome.label.into_owned(),
                api::OutcomeKind::Succeeded
                | api::OutcomeKind::Running
                | api::OutcomeKind::Waiting => String::new(),
            };
            table.row(vec![
                step_mark(step),
                cell(step.number.to_string(), Tone::Dim),
                plain(&step.name),
                plain(took),
                cell(note, Tone::Dim),
            ]);
        }
        out.push_str(&table.render(paint, "  "));
    }
    out
}

fn usage(summary: &JobSummary, paint: Paint) -> String {
    let mut out = String::new();
    let metrics = &summary.metrics;
    let _ = writeln!(out, "\n{}", paint.paint(Tone::Bold, "Usage"));
    let mut usage = Table::default();
    if let Some(time) = metrics.cpu_time {
        let mut parts = vec![format!("{} CPU time", cpu_time(time))];
        if let Some(average) = metrics.cpu_average {
            parts.push(format!("{} average", in_cores(average)));
        }
        if let Some(peak) = metrics.cpu_peak {
            parts.push(format!("{} sampled peak", in_cores(peak)));
        }
        usage.row(vec![plain("cpu"), plain(parts.join(", "))]);
    }
    if let Some(peak) = metrics.memory_peak {
        let mut parts = vec![format!("{} sampled peak", bytes(peak))];
        if let Some(average) = metrics.memory_average {
            parts.push(format!("{} average", bytes(average)));
        }
        usage.row(vec![plain("memory"), plain(parts.join(", "))]);
    }
    if let Some(received) = metrics.network_received {
        let mut parts = vec![format!("{} received", bytes(received))];
        if let Some(sent) = metrics.network_sent {
            parts.push(format!("{} sent", bytes(sent)));
        }
        parts.push(format!("{} from the mirror", bytes(metrics.mirror_bytes)));
        if let Some(throttled) = metrics.sock_throttled.filter(|count| *count > 0) {
            parts.push(format!("throttled {throttled} times for memory pressure"));
        }
        usage.row(vec![plain("network"), plain(parts.join(", "))]);
    }
    if let Some(line) = build_cache(&metrics.build_cache) {
        usage.row(vec![plain("build cache"), plain(line)]);
    }
    let log = &summary.log;
    usage.row(vec![
        plain("log"),
        // The last column: the hint's colour codes pad nothing after it.
        plain(format!(
            "{}  {}",
            log.description(),
            paint.paint(Tone::Dim, &format!("runnerctl logs {}", summary.id))
        )),
    ]);
    out.push_str(&usage.render(paint, "  "));
    if metrics.memory_peak.is_some() {
        let _ = writeln!(
            out,
            "  {}",
            paint.paint(Tone::Dim, &api::sampled_note(api::SAMPLE_INTERVAL))
        );
    }
    out
}

/// `runnerctl sizing`: per profile its limits, its jobs' peaks and waits, how busy its slots were and the advice,
/// then a row per job of each repository's workflows.
pub(crate) fn sizing(sizing: &Sizing, now: u64, paint: Paint) -> String {
    let mut out = format!(
        "{}\n",
        paint.paint(
            Tone::Dim,
            &format!(
                "from {} of history; {}{}",
                duration(sizing.covered()),
                sizing.method_line(),
                if sizing.auto {
                    "; auto sizing applies it"
                } else {
                    ""
                }
            )
        )
    );
    for profile in &sizing.profiles {
        let peaks = &profile.peaks;
        let _ = writeln!(
            out,
            "\n{}  {} × {}, {} CPUs; {} jobs, slots busy {}%, waited p50 {} p95 {}",
            paint.paint(Tone::Bold, &profile.name),
            bytes(profile.memory),
            profile.slots,
            profile.cpus,
            peaks.jobs,
            profile.busy,
            duration(profile.wait.p50),
            duration(profile.wait.p95),
        );
        let starts = &profile.starts;
        if starts.warm > 0 {
            let _ = writeln!(
                out,
                "  warm runners took {} of {} jobs: waited p50 {}, the others {}",
                starts.warm,
                starts.total(),
                duration(starts.warm_p50),
                duration(starts.cold_p50),
            );
        }
        if peaks.jobs > 0 {
            let at_cap = if peaks.at_cap > 0 {
                paint.paint(Tone::Bad, &format!(", {} at the cap", peaks.at_cap))
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "  memory p50 {} p95 {} p99 {} max {}{at_cap}",
                bytes(peaks.memory.p50),
                bytes(peaks.memory.p95),
                bytes(peaks.memory.p99),
                bytes(peaks.memory.max),
            );
            let _ = writeln!(
                out,
                "  cpu    p50 {} p95 {} p99 {} max {}",
                cores_used(peaks.cpu.p50),
                cores_used(peaks.cpu.p95),
                cores_used(peaks.cpu.p99),
                cores_used(peaks.cpu.max),
            );
        }
        let _ = writeln!(
            out,
            "  {}",
            paint.paint(profile.advice.tone().into(), &sizing.advice_line(profile))
        );
        // A raise of `cpus` its jobs ask for and auto sizing cannot make.
        if let Some(plan) = sizing.auto_plan.as_ref().filter(|_| sizing.auto) {
            let next = plan.profile_plan(&profile.name, now);
            if matches!(next, ProfilePlan::Refused { .. }) {
                let _ = writeln!(
                    out,
                    "  {}",
                    paint.paint(
                        api::Tone::Warn.into(),
                        &format!("{}: {}", next.headline(plan), next.reason(sizing, profile))
                    )
                );
            }
        }
        out.push_str(&sizing_changes(sizing, &profile.name, now, paint));
        // Under auto sizing the daemon applies it.
        if !sizing.auto && profile.advice.change().is_some() {
            let _ = writeln!(
                out,
                "  {}",
                paint.paint(
                    Tone::Dim,
                    &format!("runnerctl sizing apply {}", profile.name)
                )
            );
        }
        if !profile.jobs.is_empty() {
            out.push('\n');
            out.push_str(&sizing_jobs(&profile.jobs, paint));
        }
    }
    out
}

/// A line per change auto sizing made to a profile, and how to undo the last.
fn sizing_changes(sizing: &Sizing, profile: &str, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    for change in sizing
        .changes
        .iter()
        .filter(|change| change.profile == profile)
    {
        let _ = writeln!(
            out,
            "  auto sizing {}: {}",
            ago(change.at, now),
            change.line()
        );
    }
    if !out.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            paint.paint(Tone::Dim, &format!("runnerctl sizing undo {profile}"))
        );
    }
    out
}

/// A row per job of a profile: its runs, peaks, wait and what it needs.
fn sizing_jobs(jobs: &[JobSizing], paint: Paint) -> String {
    let mut table = Table::new(&[
        "REPOSITORY",
        "JOB",
        "RUNS",
        "P50",
        "P95",
        "P99",
        "MAX",
        "CPU P99",
        "WAIT P95",
        "NEEDS",
    ])
    .right(&[2, 3, 4, 5, 6, 7, 8]);
    for job in jobs {
        let needs = job.needs();
        let needs = cell(needs.text, needs.tone.into());
        table.row(vec![
            plain(&job.repository),
            cell(
                job_title(job.workflow.as_deref(), &job.name, None).to_string(),
                Tone::Dim,
            ),
            plain(job.peaks.jobs.to_string()),
            plain(bytes(job.peaks.memory.p50)),
            plain(bytes(job.peaks.memory.p95)),
            plain(bytes(job.peaks.memory.p99)),
            plain(bytes(job.peaks.memory.max)),
            plain(cores_used(job.peaks.cpu.p99)),
            plain(duration(job.wait.p95)),
            needs,
        ]);
    }
    table.render(paint, "  ")
}

/// `runnerctl caches`.
pub(crate) fn caches(caches: &Caches, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    match caches.measured_at {
        Some(at) => {
            let _ = writeln!(
                out,
                "{}",
                paint.paint(Tone::Dim, &format!("measured {}", ago(at, now)))
            );
        }
        None => {
            return "not measured yet: runnerd measures caches at start and when runners are idle\n"
                .to_owned();
        }
    }
    let mut table = Table::new(&["VOLUME", "HOLDS", "SIZE", "LIMIT"]).right(&[2, 3]);
    for volume in &caches.volumes {
        let limit = volume.limit_status();
        table.row(vec![
            plain(&volume.name),
            cell(volume.kind.description(), Tone::Dim),
            cell(
                bytes(volume.bytes),
                limit
                    .as_ref()
                    .map_or(Tone::Plain, |limit| limit.tone().into()),
            ),
            limit.map_or_else(dash, |limit| plain(limit.label)),
        ]);
    }
    out.push_str(&table.render(paint, ""));
    if let Some(build) = &caches.build {
        let _ = writeln!(out, "build cache: {}", build.week_line());
    }
    out
}

/// What the build cache did for a job, if it had it: `None` for a job without it, or one that never used it.
fn build_cache(counts: &BuildCacheCounts) -> Option<String> {
    counts.activity().map(|activity| activity.line())
}

/// `runnerctl insights`: one line per insight, warnings first, then each repository's waits this week against the
/// last.
pub(crate) fn insights(insights: &Insights, paint: Paint) -> String {
    let mut out = String::new();
    if insights.insights.is_empty() {
        let _ = writeln!(out, "{}", insights.empty_line());
    }
    for insight in &insights.insights {
        let _ = writeln!(
            out,
            "{} {:<9} {}",
            paint.paint(
                insight.tone().into(),
                &format!("{:<7}", insight.severity.label())
            ),
            insight.kind.word(),
            insight.sentence
        );
    }
    if insights.waits.is_empty() {
        return out;
    }
    let _ = writeln!(out);
    let mut table = Table::new(&[
        "REPOSITORY",
        "JOBS",
        "WAIT P50",
        "P95",
        "WARM P50",
        "COLD P50",
        "P95 WEEK BEFORE",
    ])
    .right(&[1, 2, 3, 4, 5, 6]);
    let wait = |spread: &WaitSpread, value: Option<u64>| {
        spread
            .reading(value)
            .map_or_else(dash, |seconds| plain(duration(seconds)))
    };
    for repository in &insights.waits {
        let now = &repository.now;
        table.row(vec![
            plain(&repository.repository),
            plain(now.all.jobs.to_string()),
            wait(&now.all, now.all.p50),
            wait(&now.all, now.all.p95),
            wait(&now.warm, now.warm.p50),
            wait(&now.cold, now.cold.p50),
            wait(&repository.before.all, repository.before.all.p95),
        ]);
    }
    out.push_str(&table.render(paint, ""));
    out
}

/// `runnerctl insights --digest`: the last 24 hours.
pub(crate) fn digest(digest: &Digest) -> String {
    let mut out = format!("last 24 hours: {}\n", digest.summary_line());
    for insight in &digest.new {
        let _ = writeln!(out, "new: {}", insight.sentence);
    }
    if !digest.waits.is_empty() {
        let _ = writeln!(out, "longest waits:");
    }
    for wait in &digest.waits {
        let _ = writeln!(out, "  {}", wait.line());
    }
    out
}

/// `runnerctl notify test`: what each target did.
pub(crate) fn notify_test(tested: &NotifyTest, paint: Paint) -> String {
    let mut out = String::new();
    for delivery in &tested.targets {
        let _ = match &delivery.error {
            None => writeln!(
                out,
                "{} {}",
                delivery.target,
                paint.paint(Tone::Good, "delivered")
            ),
            Some(error) => writeln!(
                out,
                "{} {}",
                delivery.target,
                paint.paint(Tone::Bad, &format!("not delivered: {error}"))
            ),
        };
    }
    out
}

/// One line for an event, as `watch` prints when its output is not a terminal. `None` for live samples.
pub(crate) fn event_line(envelope: &EventEnvelope, paint: Paint) -> Option<String> {
    let kind = envelope.event.kind();
    let text = match &envelope.event {
        Event::JobQueued { job } => format!("{} {} {}", job.id, job.repository, job.title()),
        Event::JobStarted { job } => {
            let on = job
                .runner
                .as_ref()
                .map(|runner| format!(" on {} slot {}", runner.profile, runner.slot))
                .unwrap_or_default();
            format!("{} {} {}{on}", job.id, job.repository, job.title())
        }
        Event::JobStep { job, step } => {
            let (mark, tone) = step_mark(step);
            let took = step
                .took(envelope.at)
                .filter(|_| step.completed_at.is_some())
                .map(|s| format!(" {}", duration(s)))
                .unwrap_or_default();
            format!("{job} {} {}{took}", paint.paint(tone, &mark), step.name)
        }
        Event::JobFinished { job } => {
            let (result, tone) = outcome(job);
            let ran = job
                .ran(envelope.at)
                .map(|s| format!(" after {}", duration(s)))
                .unwrap_or_default();
            format!(
                "{} {} {} {}{ran}",
                job.id,
                job.repository,
                job.title(),
                paint.paint(tone, &result)
            )
        }
        Event::SlotChanged { profile, slot } => match &slot.runner {
            Some(runner) => format!(
                "{profile} slot {} {} {}",
                slot.slot,
                SlotState::of_runner(runner).word(),
                runner.repository
            ),
            None => format!("{profile} slot {} {}", slot.slot, SlotState::Idle.word()),
        },
        Event::ProfileChanged { profile } => format!(
            "{} {} {} slots{}",
            profile.name,
            profile.image,
            profile.slots.len(),
            if profile.reseeding { " reseeding" } else { "" }
        ),
        Event::ProfileRemoved { profile } => format!("{profile} removed"),
        Event::BudgetChanged { budget } => budget.line(),
        Event::RepositoryChanged { repository } => format!(
            "{} {} {} queued",
            repository.name,
            if repository.paused {
                paint.paint(Tone::Busy, "paused")
            } else {
                paint.paint(Tone::Good, "active")
            },
            repository.queued_jobs
        ),
        Event::RepositoryRemoved { repository } => format!("{repository} removed"),
        Event::CacheReseeded { profile, error, .. } => match error {
            Some(error) => format!(
                "{profile} {}",
                paint.paint(Tone::Bad, &format!("failed: {error}"))
            ),
            None => format!("{profile} checked"),
        },
        Event::RecordCreated { record } => format!(
            "{} {} kept as {}, {}",
            record.job,
            record.repository,
            record.image,
            bytes(record.bytes)
        ),
        Event::RecordDeleted { job, removed_by } => format!("{job} record {removed_by}"),
        Event::SizingChanged { change } => format!("{} {}", change.profile, change.line()),
        Event::RunnerFailed { failure } => format!(
            "{} {}: {}; no runner for {}s",
            failure.repository, failure.profile, failure.error, failure.held_for
        ),
        Event::JobSample { .. } | Event::HostSample { .. } | Event::LogLine { .. } => return None,
    };
    Some(format!(
        "{} {text}",
        paint.paint(Tone::Dim, &format!("{kind:<18}"))
    ))
}

#[cfg(test)]
mod parity;

#[cfg(test)]
mod tests {
    use runnerd_core::api::{
        AdmissionPolicy, Budget, Event, EventEnvelope, RepositoryStatus, RunnerStatus, SlotStatus,
    };

    use super::{Paint, Table, Tone, bytes, event_line};

    #[test]
    fn watch_prints_a_repository_paused_or_removed_elsewhere() {
        let line = |event| {
            event_line(
                &EventEnvelope {
                    id: Some(9),
                    at: 1000,
                    event,
                },
                Paint { colour: false },
            )
        };
        let paused = Event::RepositoryChanged {
            repository: RepositoryStatus {
                name: "halyard/ledger".to_owned(),
                profile: "jvm".to_owned(),
                labels: vec!["self-hosted".to_owned()],
                pools: vec![runnerd_core::api::PoolStatus {
                    profile: "jvm".to_owned(),
                    labels: vec!["self-hosted".to_owned()],
                    queued_jobs: 2,
                }],
                paused: true,
                queued_jobs: 2,
            },
        };
        assert_eq!(
            line(paused).as_deref(),
            Some("repository.changed halyard/ledger paused 2 queued")
        );
        let removed = Event::RepositoryRemoved {
            repository: "halyard/ledger".to_owned(),
        };
        assert_eq!(
            line(removed).as_deref(),
            Some("repository.removed halyard/ledger removed")
        );
    }

    #[test]
    fn watch_on_a_pipe_names_slots_and_the_budget_as_status_does() {
        let line = |event| {
            event_line(
                &EventEnvelope {
                    id: Some(9),
                    at: 1000,
                    event,
                },
                Paint { colour: false },
            )
            .unwrap_or_default()
        };
        let warm = Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 2,
                runner: Some(RunnerStatus {
                    repository: "halyard/ledger".to_owned(),
                    name: "ci-jvm-2".to_owned(),
                    runner_id: 7,
                    container: "c".to_owned(),
                    busy: false,
                    started_at: 900,
                    reserved: 8 << 30,
                    warm: true,
                }),
            },
        };
        assert_eq!(
            line(warm),
            "slot.changed       jvm slot 2 warm halyard/ledger"
        );
        let empty = Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 2,
                runner: None,
            },
        };
        assert_eq!(line(empty), "slot.changed       jvm slot 2 idle");
        let held = Event::BudgetChanged {
            budget: Budget {
                slice: "runnerd.slice".to_owned(),
                memory_max: Some(33 << 30),
                memory_in_use: 30 << 30,
                admission: AdmissionPolicy::Measured,
                held: true,
                ..Budget::default()
            },
        };
        let held = line(held);
        assert!(
            held.contains("so no runner starts"),
            "a held budget says no runner starts: {held}"
        );
    }

    #[test]
    fn sizes_use_the_binary_units_of_the_config() {
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(20 * 1024), "20K");
        assert_eq!(bytes(213 << 20), "213M");
        assert_eq!(bytes((12 << 30) / 10), "1.2G");
    }

    #[test]
    fn coloured_columns_line_up_with_plain_ones() {
        let mut table = Table::new(&["A", "B"]);
        table.row(vec![
            ("long".to_owned(), Tone::Good),
            ("x".to_owned(), Tone::Plain),
        ]);
        let plain = table.render(Paint { colour: false }, "");
        let coloured = table.render(Paint { colour: true }, "");
        assert_eq!(plain, "A     B\nlong  x\n");
        let stripped = coloured
            .replace("\x1b[32m", "")
            .replace("\x1b[2m", "")
            .replace("\x1b[0m", "");
        assert_eq!(stripped, plain);
    }
}
