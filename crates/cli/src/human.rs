//! What people read: API types turned into aligned text, with durations like `1m 12s`, binary sizes like `213M`
//! and colour when the output is a terminal. Pure functions of the API types and the current time; printing is
//! `output`'s, and nothing here decides anything the daemon has not. Sizes and CPU read as on the dashboard; durations
//! (`2h 5m`) and ages (`1h 52m ago`) stay exact and compact on purpose, where the dashboard writes a stopwatch
//! (`2h 05m`) and rounds ages (`1 h ago`).

use std::fmt::Write as _;

use runnerd_core::api::{
    Advice, CacheKind, Caches, Event, EventEnvelope, Job, JobPage, JobRunner, JobSizing, JobState,
    JobSummary, QueuedJob, Record, RecordNote, Records, RemovedRecords, Sizing, Status, Step,
};
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

/// `45s`, `1m 12s`, `2h 5m`, `3d 4h`.
pub(crate) fn duration(seconds: u64) -> String {
    let (d, h, m, s) = (
        seconds / 86_400,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// `2m ago`, or `now`.
pub(crate) fn ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0 => "now".to_owned(),
        seconds => format!("{} ago", duration(seconds)),
    }
}

/// Binary sizes as `runnerd.toml` writes them, and as the dashboard shows them: `512B`, `20K`, `213M`, `4.37G`, `9G`.
pub(crate) fn bytes(n: u64) -> String {
    ByteSize(n).to_string()
}

/// Millicores as cores, as the dashboard shows them: `0.25`, `1.5`, `12`. Usage is in cores, a cap in CPUs.
#[allow(
    clippy::cast_precision_loss,
    reason = "millicores of one host are far below 2^52"
)]
pub(crate) fn cores(millicores: u64) -> String {
    let text = format!("{:.2}", millicores as f64 / 1000.0);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// CPU time given in milliseconds: `4.6s` under a minute, then as [`duration`].
#[allow(clippy::cast_precision_loss, reason = "under a minute of milliseconds")]
pub(crate) fn cpu_time(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        duration(ms / 1000)
    }
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

/// A job's outcome as one word: its conclusion once it has one, otherwise its state; and how to colour it.
pub(crate) fn outcome(job: &JobSummary) -> (String, Tone) {
    let word = match (&job.conclusion, job.state) {
        (Some(conclusion), _) => conclusion.clone(),
        (None, JobState::Queued) => "queued".to_owned(),
        (None, JobState::Running) => "running".to_owned(),
        (None, JobState::Completed) => "completed".to_owned(),
        (None, JobState::Lost) => "lost".to_owned(),
        (None, JobState::Elsewhere) => "elsewhere".to_owned(),
    };
    let tone = match word.as_str() {
        "success" => Tone::Good,
        "failure" | "timed_out" | "lost" | "startup_failure" => Tone::Bad,
        "queued" | "running" => Tone::Busy,
        _ => Tone::Dim,
    };
    (word, tone)
}

/// `workflow / job`, or the job's name alone; `job <id>` until the daemon knows its name, as for a job its runner
/// took before the daemon saw it queued.
pub(crate) fn title(job: &JobSummary) -> String {
    named(job.workflow.as_deref(), &job.name, job.id)
}

/// A record's job as [`title`] names a job.
pub(crate) fn record_title(record: &Record) -> String {
    named(
        record.workflow.as_deref(),
        record.name.as_deref().unwrap_or(""),
        record.job,
    )
}

/// A queued job as [`title`] names a job.
pub(crate) fn queued_title(job: &QueuedJob) -> String {
    named(job.workflow.as_deref(), &job.name, job.id)
}

fn named(workflow: Option<&str>, name: &str, id: u64) -> String {
    match workflow {
        Some(workflow) => format!("{workflow} / {name}"),
        None if name.is_empty() => format!("job {id}"),
        None => name.to_owned(),
    }
}

/// How long ago a queued job was queued; nothing when GitHub did not say.
pub(crate) fn queued_ago(job: &QueuedJob, now: u64) -> String {
    if job.queued_at == 0 {
        String::new()
    } else {
        ago(job.queued_at, now)
    }
}

/// Seconds the job waited for a runner, until now while it waits.
pub(crate) fn waited(job: &JobSummary, now: u64) -> Option<u64> {
    let end = match job.timeline.started_at {
        Some(started) => started,
        None if job.state == JobState::Queued => now,
        None => return None,
    };
    Some(end.saturating_sub(job.timeline.queued_at))
}

/// Seconds the job ran, until now while it runs.
pub(crate) fn ran(job: &JobSummary, now: u64) -> Option<u64> {
    let started = job.timeline.started_at?;
    let end = match job.timeline.finished_at {
        Some(finished) => finished,
        None if job.state == JobState::Running => now,
        None => return None,
    };
    Some(end.saturating_sub(started))
}

const SAMPLED_NOTE: &str =
    "memory peaks are sampled every 2 s; spikes shorter than that do not show";

/// The memory line of a status: what is in use of what.
fn budget_line(status: &Status) -> String {
    let budget = &status.budget;
    let by = budget
        .admission_note()
        .map_or_else(String::new, |note| format!(" {note}"));
    let mut line = match budget.memory_max {
        Some(max) => format!(
            "{} of {} reserved in {}{by}",
            bytes(budget.memory_in_use),
            bytes(max),
            budget.slice
        ),
        None => format!(
            "{} reserved in {}, which has no MemoryMax",
            bytes(budget.memory_in_use),
            budget.slice
        ),
    };
    if let Some(cpu) = budget.cpu {
        let _ = write!(line, "; {}", cpu.line());
    }
    line
}

/// `runnerctl status`.
pub(crate) fn status(status: &Status, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{} {}  {}",
        paint.paint(Tone::Bold, "runnerd"),
        status.version,
        budget_line(status)
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
            match &slot.runner {
                Some(runner) => {
                    row.push(if runner.busy {
                        cell("busy", Tone::Busy)
                    } else {
                        cell("idle", Tone::Plain)
                    });
                    row.push(plain(&runner.repository));
                    row.push(cell(&runner.name, Tone::Dim));
                    row.push(plain(duration(now.saturating_sub(runner.started_at))));
                }
                None => {
                    row.push(cell("free", Tone::Dim));
                }
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
    for repository in &status.repositories {
        repositories.row(vec![
            plain(&repository.name),
            plain(&repository.profile),
            if repository.paused {
                cell("paused", Tone::Busy)
            } else {
                cell("active", Tone::Good)
            },
            plain(repository.queued_jobs.to_string()),
            cell(repository.labels.join(","), Tone::Dim),
        ]);
    }
    out.push_str(&repositories.render(paint, ""));
    if !status.queued.is_empty() {
        out.push('\n');
        let mut queued = Table::new(&["JOB", "REPOSITORY", "WORKFLOW / NAME", "QUEUED"]);
        for job in &status.queued {
            queued.row(vec![
                plain(job.id.to_string()),
                plain(&job.repository),
                plain(queued_title(job)),
                cell(queued_ago(job, now), Tone::Dim),
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
            plain(title(job)),
            job.branch.as_deref().map_or_else(dash, plain),
            outcome(job),
            waited(job, now).map_or_else(dash, |s| plain(duration(s))),
            ran(job, now).map_or_else(dash, |s| plain(duration(s))),
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
        let _ = writeln!(out, "{}", paint.paint(Tone::Dim, SAMPLED_NOTE));
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

/// A step's mark: `✓` succeeded, `✗` failed, `●` running, `○` waiting, `-` skipped or cancelled.
fn step_mark(step: &Step) -> (String, Tone) {
    match (step.status.as_str(), step.conclusion.as_deref()) {
        (_, Some("success")) => cell("✓", Tone::Good),
        (_, Some("failure" | "timed_out")) => cell("✗", Tone::Bad),
        (_, Some(_)) => cell("-", Tone::Dim),
        ("in_progress", None) => cell("●", Tone::Busy),
        _ => cell("○", Tone::Dim),
    }
}

/// How long a step took, until now while it runs.
fn step_took(step: &Step, now: u64) -> Option<u64> {
    let started = step.started_at?;
    let end = match step.completed_at {
        Some(completed) => completed,
        None if step.status == "in_progress" => now,
        None => return None,
    };
    Some(end.saturating_sub(started))
}

/// `, capped at 6G and 4 CPUs`: what the job's container was held to, when the record has it.
fn caps(runner: &JobRunner) -> String {
    let caps: Vec<String> = [
        runner.memory_cap.map(bytes),
        runner.cpu_cap.map(|cpu| format!("{} CPUs", cores(cpu))),
    ]
    .into_iter()
    .flatten()
    .collect();
    if caps.is_empty() {
        String::new()
    } else {
        format!(", capped at {}", caps.join(" and "))
    }
}

/// `runnerctl job <id>`.
pub(crate) fn job(job: &Job, now: u64, paint: Paint) -> String {
    let summary = &job.summary;
    let mut out = String::new();
    let (result, tone) = outcome(summary);
    let _ = writeln!(
        out,
        "{}  {}  {}",
        paint.paint(Tone::Bold, &title(summary)),
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
                    caps(runner)
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
    match (note, note.removed_at) {
        (
            RecordNote {
                skipped: Some(why), ..
            },
            _,
        ) => {
            format!(
                "record {}",
                paint.paint(Tone::Dim, &format!("not kept: {why}"))
            )
        }
        (RecordNote { removed_by, .. }, Some(at)) => {
            let why = removed_by.map_or_else(|| "removed".to_owned(), |by| by.to_string());
            format!(
                "record {}",
                paint.paint(Tone::Dim, &format!("{why}, {}", ago(at, now)))
            )
        }
        (RecordNote { bytes, .. }, None) => format!(
            "record kept{}: `runnerctl shell {job}` opens a shell in it",
            bytes
                .map(|bytes| format!(", {}", self::bytes(bytes)))
                .unwrap_or_default()
        ),
    }
}

/// What a removal of records freed.
pub(crate) fn removed(removed: &RemovedRecords) -> String {
    match removed.jobs.len() {
        0 => "no record removed".to_owned(),
        1 => format!("removed 1 record, {} freed", bytes(removed.bytes)),
        count => format!("removed {count} records, {} freed", bytes(removed.bytes)),
    }
}

/// `runnerctl records`.
pub(crate) fn records(records: &Records, now: u64, paint: Paint) -> String {
    let limits = &records.limits;
    let mut out = format!(
        "{} records, {} of {}; at most {}, each for {}\n",
        records.records.len(),
        bytes(records.bytes()),
        bytes(limits.max_total),
        limits.max_count,
        duration(limits.max_age)
    );
    if records.records.is_empty() {
        out.push_str(&paint.paint(
            Tone::Dim,
            "none kept: a profile with record_failures = true keeps each failed job's runner as a record, to open a \
             shell where it failed\n",
        ));
        return out;
    }
    let mut table = Table::new(&["JOB", "REPOSITORY", "NAME", "SIZE", "MADE"]).right(&[3]);
    for record in &records.records {
        let mut made = ago(record.created_at, now);
        if records.shells.contains(&record.job) {
            made.push_str(", shell open");
        }
        table.row(vec![
            plain(record.job.to_string()),
            plain(&record.repository),
            cell(record_title(record), Tone::Dim),
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
    let phases = [
        ("queued", Some(timeline.queued_at)),
        ("container started", timeline.container_started_at),
        ("job started", timeline.started_at),
        ("finished", timeline.finished_at),
        ("cleaned up", timeline.cleaned_at),
    ];
    for (name, at) in phases {
        if let Some(at) = at {
            table.row(vec![
                plain(name),
                plain(format!(
                    "+{}",
                    duration(at.saturating_sub(timeline.queued_at))
                )),
            ]);
        }
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
            let took = step_took(step, now).map_or_else(String::new, duration);
            let note = match step.conclusion.as_deref() {
                Some("success") | None => String::new(),
                Some(other) => other.to_owned(),
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
            parts.push(format!("{} cores average", cores(average)));
        }
        if let Some(peak) = metrics.cpu_peak {
            parts.push(format!("{} cores sampled peak", cores(peak)));
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
        usage.row(vec![plain("network"), plain(parts.join(", "))]);
    }
    let log = &summary.log;
    usage.row(vec![
        plain("log"),
        // The last column: the hint's colour codes pad nothing after it.
        plain(format!(
            "{} lines, {}{}  {}",
            log.lines,
            bytes(log.bytes),
            if log.truncated { ", truncated" } else { "" },
            paint.paint(Tone::Dim, &format!("runnerctl logs {}", summary.id))
        )),
    ]);
    out.push_str(&usage.render(paint, "  "));
    if metrics.memory_peak.is_some() {
        let _ = writeln!(out, "  {}", paint.paint(Tone::Dim, SAMPLED_NOTE));
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
                "from the last {} of history; peaks are sampled every {} s, so advice adds {}% to the largest \
                 and waits for {} jobs",
                duration(now.saturating_sub(sizing.since)),
                sizing.sample_interval,
                sizing.margin,
                sizing.min_jobs
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
                cores(peaks.cpu.p50),
                cores(peaks.cpu.p95),
                cores(peaks.cpu.p99),
                cores(peaks.cpu.max),
            );
        }
        let tone = match profile.advice {
            Advice::Raise { .. } | Advice::SliceTooSmall { .. } => Tone::Bad,
            Advice::Lower { .. } => Tone::Busy,
            Advice::Keep { .. } => Tone::Good,
            Advice::TooFew { .. } => Tone::Dim,
        };
        let _ = writeln!(out, "  {}", paint.paint(tone, &sizing.advice_line(profile)));
        if profile.advice.change().is_some() {
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
        let needs = match job.advice {
            Advice::TooFew { .. } => {
                cell(format!("max {}", bytes(job.peaks.memory.max)), Tone::Dim)
            }
            Advice::Raise { memory, .. } | Advice::SliceTooSmall { memory } => {
                cell(format!("{} > cap", bytes(memory)), Tone::Bad)
            }
            Advice::Keep { memory } | Advice::Lower { memory, .. } => plain(bytes(memory)),
        };
        table.row(vec![
            plain(&job.repository),
            cell(named(job.workflow.as_deref(), &job.name, 0), Tone::Dim),
            plain(job.peaks.jobs.to_string()),
            plain(bytes(job.peaks.memory.p50)),
            plain(bytes(job.peaks.memory.p95)),
            plain(bytes(job.peaks.memory.p99)),
            plain(bytes(job.peaks.memory.max)),
            plain(cores(job.peaks.cpu.p99)),
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
        let holds = match &volume.kind {
            CacheKind::Snapshot { profile } => format!("snapshot of {profile}"),
            CacheKind::Slot { profile, slot } => format!("{profile} slot {slot}"),
            CacheKind::Pnpm => "pnpm store".to_owned(),
            CacheKind::Maven => "Maven repository".to_owned(),
            CacheKind::Mirror => "Docker Hub mirror".to_owned(),
        };
        let over = volume.limit.is_some_and(|limit| volume.bytes > limit);
        table.row(vec![
            plain(&volume.name),
            cell(holds, Tone::Dim),
            cell(
                bytes(volume.bytes),
                if over { Tone::Busy } else { Tone::Plain },
            ),
            volume.limit.map_or_else(dash, |limit| plain(bytes(limit))),
        ]);
    }
    out.push_str(&table.render(paint, ""));
    out
}

/// One line for an event, as `watch` prints when its output is not a terminal. `None` for live samples.
pub(crate) fn event_line(envelope: &EventEnvelope, paint: Paint) -> Option<String> {
    let kind = envelope.event.kind();
    let text = match &envelope.event {
        Event::JobQueued { job } => format!("{} {} {}", job.id, job.repository, title(job)),
        Event::JobStarted { job } => {
            let on = job
                .runner
                .as_ref()
                .map(|runner| format!(" on {} slot {}", runner.profile, runner.slot))
                .unwrap_or_default();
            format!("{} {} {}{on}", job.id, job.repository, title(job))
        }
        Event::JobStep { job, step } => {
            let (mark, tone) = step_mark(step);
            let took = step_took(step, envelope.at)
                .filter(|_| step.completed_at.is_some())
                .map(|s| format!(" {}", duration(s)))
                .unwrap_or_default();
            format!("{job} {} {}{took}", paint.paint(tone, &mark), step.name)
        }
        Event::JobFinished { job } => {
            let (result, tone) = outcome(job);
            let ran = ran(job, envelope.at)
                .map(|s| format!(" after {}", duration(s)))
                .unwrap_or_default();
            format!(
                "{} {} {} {}{ran}",
                job.id,
                job.repository,
                title(job),
                paint.paint(tone, &result)
            )
        }
        Event::SlotChanged { profile, slot } => match &slot.runner {
            Some(runner) => format!(
                "{profile} slot {} {} {}",
                slot.slot,
                if runner.busy { "busy" } else { "idle" },
                runner.repository
            ),
            None => format!("{profile} slot {} free", slot.slot),
        },
        Event::ProfileChanged { profile } => format!(
            "{} {} {} slots{}",
            profile.name,
            profile.image,
            profile.slots.len(),
            if profile.reseeding { " reseeding" } else { "" }
        ),
        Event::ProfileRemoved { profile } => format!("{profile} removed"),
        Event::BudgetChanged { budget } => format!(
            "{} reserved{}{}",
            bytes(budget.memory_in_use),
            budget
                .memory_max
                .map(|max| format!(" of {}", bytes(max)))
                .unwrap_or_default(),
            budget
                .cpu
                .map(|cpu| format!("; {}", cpu.line()))
                .unwrap_or_default()
        ),
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
        Event::JobSample { .. } | Event::HostSample { .. } | Event::LogLine { .. } => return None,
    };
    Some(format!(
        "{} {text}",
        paint.paint(Tone::Dim, &format!("{kind:<18}"))
    ))
}

#[cfg(test)]
mod tests {
    use runnerd_core::api::{Event, EventEnvelope, RepositoryStatus};

    use super::{Paint, Table, Tone, bytes, duration, event_line};

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
    fn durations_read_like_people_write_them() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(72), "1m 12s");
        assert_eq!(duration(3600 + 5 * 60 + 9), "1h 5m");
        assert_eq!(duration(3 * 86_400 + 4 * 3600), "3d 4h");
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
