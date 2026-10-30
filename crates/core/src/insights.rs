//! Insights from the job history (ADR 0051): jobs that got slower and stayed so, jobs that fail and pass on a re-run,
//! jobs whose memory nears their profile's cap, jobs that find almost nothing in the build cache, repositories whose
//! downloads memory pressure throttled, and how long each repository's jobs wait, this week against the last when the
//! history holds all of last week; and the daily digest. Pure: the daemon reads the history, the
//! config and the clock and passes them in. It sends nothing and changes nothing.

use std::collections::{BTreeMap, BTreeSet};

use crate::api::{
    self, AutoNote, Digest, Insight, InsightKind, Insights, JobState, LongWait, RepositoryWaits,
    RunLink, Severity, WaitSpread, WeekWaits,
};
use crate::config::{ByteSize, Config};
use crate::sizing::spread;

const SECOND: u64 = 1000;
const DAY: u64 = 24 * 3600 * SECOND;
const WEEK: u64 = 7 * DAY;

/// The last runs a slower job's median is taken over; the baseline needs as many before them.
const RECENT: usize = 5;
/// A job is slower when its recent median is this share above its baseline's, in percent…
const SLOWER_PERCENT: u64 = 20;
/// …and at least this many seconds above it.
const SLOWER_SECONDS: u64 = 30;

/// The last runs whose memory peaks are read, and the fewest that say anything.
const CAP_RUNS: usize = 10;
const CAP_MIN_RUNS: usize = 5;
/// A job is near its cap when the 95th percentile of its peaks is this share of it, in percent.
const NEAR_CAP: u64 = 85;
/// Runs at `NEAR_CAP` or more that make it a warning.
const NEAR_CAP_WARNING: usize = 2;
/// Flaky runs that make it a warning.
const FLAKY_WARNING: u32 = 2;

/// A repository's waits are an insight when this week's p95 is this share above last week's, in percent, and a minute
/// above it, each week with this many jobs.
const WAITS_PERCENT: u64 = 50;
const WAITS_SECONDS: u64 = 60;
const WAITS_MIN_JOBS: u32 = 5;
/// The jobs a week needs for its p95 to be other than its longest wait, the nearest rank `spread` takes.
const WAITS_ONE_IN_20: u32 = 20;

/// A repository's runs the kernel throttled for memory pressure that make an insight.
const THROTTLED_MIN_RUNS: u32 = 2;

/// One job the history closed, as insights read it. Times in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// GitHub's job ID.
    pub id: u64,
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name, when known.
    pub workflow: Option<String>,
    /// The job's name.
    pub name: String,
    /// The branch the run is for.
    pub branch: Option<String>,
    /// The job's page on GitHub.
    pub url: Option<String>,
    /// The workflow run.
    pub run_id: u64,
    /// The run's attempt, 1 for the first.
    pub run_attempt: u32,
    /// `Completed` or `Lost`.
    pub state: JobState,
    /// GitHub's conclusion, for a completed job.
    pub conclusion: Option<String>,
    /// When GitHub queued it.
    pub queued_at: u64,
    /// When it started.
    pub started_at: Option<u64>,
    /// When it finished, or was found lost.
    pub finished_at: Option<u64>,
    /// When its runner's container started: before `queued_at`, it found a warm runner.
    pub runner_started_at: Option<u64>,
    /// The profile its runner ran in.
    pub profile: Option<String>,
    /// Its sampled memory peak.
    pub memory_peak: Option<u64>,
    /// What the build cache served it, if its runner had the build cache.
    pub build_cache: Option<api::BuildCacheCounts>,
    /// Times the kernel throttled its container's sockets for memory pressure, when counted.
    pub sock_throttled: Option<u64>,
}

impl Run {
    fn finished(&self) -> u64 {
        self.finished_at.unwrap_or(0)
    }

    fn succeeded(&self) -> bool {
        self.state == JobState::Completed && self.conclusion.as_deref() == Some("success")
    }

    /// Failed as `GET /v1/stats` counts failures: failed, timed out or lost.
    fn failed(&self) -> bool {
        self.state == JobState::Lost || self.failed_itself()
    }

    /// Failed or timed out: what the job did, not what became of its runner.
    fn failed_itself(&self) -> bool {
        self.state == JobState::Completed
            && matches!(self.conclusion.as_deref(), Some("failure" | "timed_out"))
    }

    /// Seconds from start to finish.
    fn took(&self) -> Option<u64> {
        Some(self.finished_at?.saturating_sub(self.started_at?) / SECOND)
    }

    fn waited(&self) -> Option<u64> {
        Some(self.started_at?.saturating_sub(self.queued_at) / SECOND)
    }

    fn link(&self) -> RunLink {
        RunLink {
            job: self.id,
            run_id: self.run_id,
            run_attempt: self.run_attempt,
            branch: self.branch.clone(),
            url: self.url.clone(),
            at: self.finished() / SECOND,
        }
    }
}

/// What insights read from the job history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    /// The jobs it closed in the days read.
    pub runs: Vec<Run>,
    /// From when it holds every job that ended, in ms (`history_kept`): before that, pruning or the daemon's first
    /// start left only some, so no week before it is compared.
    pub kept_since: u64,
}

/// A job as insights tell jobs apart: repository, workflow and name.
type JobKey<'run> = (&'run str, Option<&'run str>, &'run str);

fn key_of(run: &Run) -> JobKey<'_> {
    (&run.repository, run.workflow.as_deref(), &run.name)
}

/// The insights of the `days` before `now` (ms), and the waits of the 14 days before it. `history` may hold more:
/// only the runs finished by `now` count.
pub fn insights(history: &History, config: &Config, days: u32, now: u64) -> Insights {
    let runs = &history.runs;
    let since = now.saturating_sub(u64::from(days) * DAY);
    let window: Vec<&Run> = runs
        .iter()
        .filter(|run| {
            matches!(run.state, JobState::Completed | JobState::Lost)
                && run
                    .finished_at
                    .is_some_and(|at| (since..=now).contains(&at))
        })
        .collect();
    let mut jobs: BTreeMap<JobKey<'_>, Vec<&Run>> = BTreeMap::new();
    for run in &window {
        jobs.entry(key_of(run)).or_default().push(run);
    }
    let mut found = Vec::new();
    for (key, runs) in &mut jobs {
        runs.sort_by_key(|run| (run.finished(), run.id));
        found.extend(slower(*key, runs));
        found.extend(flaky(*key, runs));
        found.extend(near_cap(*key, runs, config));
        found.extend(cold_cache(*key, runs));
    }
    found.extend(throttled(&window));
    let waits = waits(history, now);
    found.extend(waits.iter().filter_map(waits_insight));
    found.sort_by(|a, b| {
        (a.severity, &a.repository, &a.workflow, &a.job, &a.key).cmp(&(
            b.severity,
            &b.repository,
            &b.workflow,
            &b.job,
            &b.key,
        ))
    });
    Insights {
        since: since / SECOND,
        until: now / SECOND,
        insights: found,
        waits,
    }
}

/// The 24 hours before `now` (ms): what finished and failed, the insights that hold at `now` and did not 24 hours
/// before, and the longest waits.
pub fn digest(history: &History, config: &Config, now: u64) -> Digest {
    let runs = &history.runs;
    let since = now.saturating_sub(DAY);
    let finished: Vec<&Run> = runs
        .iter()
        .filter(|run| {
            matches!(run.state, JobState::Completed | JobState::Lost)
                && run.finished_at.is_some_and(|at| at > since && at <= now)
        })
        .collect();
    let before: BTreeSet<String> = insights(history, config, api::INSIGHT_DAYS, since)
        .insights
        .into_iter()
        .map(|insight| insight.key)
        .collect();
    let new = insights(history, config, api::INSIGHT_DAYS, now)
        .insights
        .into_iter()
        .filter(|insight| !before.contains(&insight.key))
        .collect();
    let mut started: Vec<&Run> = runs
        .iter()
        .filter(|run| run.started_at.is_some_and(|at| at > since && at <= now))
        .collect();
    started.sort_by_key(|run| (std::cmp::Reverse(run.waited()), run.id));
    Digest {
        since: since / SECOND,
        until: now / SECOND,
        finished: finished.len() as u64,
        failed: finished.iter().filter(|run| run.failed()).count() as u64,
        new,
        waits: started
            .into_iter()
            .take(3)
            .map(|run| LongWait {
                job: run.id,
                repository: run.repository.clone(),
                workflow: run.workflow.clone(),
                name: run.name.clone(),
                waited: run.waited().unwrap_or(0),
                url: run.url.clone(),
            })
            .collect(),
        savings: None,
    }
}

/// `repository: workflow / job`, or `repository: job` when the workflow is not known.
fn subject((repository, workflow, name): JobKey<'_>) -> String {
    match workflow {
        Some(workflow) => format!("{repository}: {workflow} / {name}"),
        None => format!("{repository}: {name}"),
    }
}

fn key_text(kind: &str, (repository, workflow, name): JobKey<'_>) -> String {
    format!("{kind}:{repository}/{}/{name}", workflow.unwrap_or(""))
}

fn insight(
    key: JobKey<'_>,
    kind: &str,
    severity: Severity,
    sentence: String,
    found: InsightKind,
    runs: Vec<RunLink>,
) -> Insight {
    Insight {
        key: key_text(kind, key),
        severity,
        repository: key.0.to_owned(),
        workflow: key.1.map(str::to_owned),
        job: Some(key.2.to_owned()),
        sentence,
        kind: found,
        runs,
    }
}

/// The median: the mean of the middle two for an even count.
fn median(values: &[u64]) -> u64 {
    let mut values = values.to_vec();
    values.sort_unstable();
    let middle = values.len() / 2;
    match values.len() {
        0 => 0,
        len if len % 2 == 1 => values[middle],
        _ => u64::midpoint(values[middle - 1], values[middle]),
    }
}

/// A job whose successful runs on `main` or `master`, or else its busiest branch, got slower and stayed so.
fn slower(key: JobKey<'_>, runs: &[&Run]) -> Option<Insight> {
    let timed_on = |branch: &str| -> Vec<(&Run, u64)> {
        runs.iter()
            .filter(|run| run.succeeded() && run.branch.as_deref() == Some(branch))
            .filter_map(|run| Some((*run, run.took()?)))
            .collect()
    };
    let mut branches: BTreeMap<&str, usize> = BTreeMap::new();
    for run in runs.iter().filter(|run| run.succeeded()) {
        if let Some(branch) = &run.branch {
            *branches.entry(branch).or_default() += 1;
        }
    }
    // The default branch when it has runs enough, so a pull request that adds tests is not a regression; else the
    // most runs, and of equal counts the first by name.
    let busiest = branches
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(branch, _)| *branch)?;
    let (branch, timed) = ["main", "master"]
        .into_iter()
        .map(|branch| (branch, timed_on(branch)))
        .find(|(_, timed)| timed.len() >= 2 * RECENT)
        .unwrap_or_else(|| (busiest, timed_on(busiest)));
    if timed.len() < 2 * RECENT {
        return None;
    }
    let (baseline, recent) = timed.split_at(timed.len() - RECENT);
    let took = |runs: &[(&Run, u64)]| runs.iter().map(|(_, took)| *took).collect::<Vec<_>>();
    let before = median(&took(baseline));
    let now = median(&took(recent));
    let threshold = (before + before * SLOWER_PERCENT / 100).max(before + SLOWER_SECONDS);
    // Every recent run slower than the baseline's median: a job that flips between two speeds, such as build cache
    // hits and misses, moves its median with a few more slow runs in a row without getting slower.
    if now < threshold || recent.iter().any(|(_, took)| *took <= before) {
        return None;
    }
    // The first slow run after which no two runs in a row were fast.
    let mut since = None;
    let mut fast_in_a_row = 0;
    for (run, took) in &timed {
        if *took >= threshold {
            fast_in_a_row = 0;
            since = since.or(Some(*run));
        } else {
            fast_in_a_row += 1;
            if fast_in_a_row >= 2 {
                since = None;
            }
        }
    }
    let since = since?;
    let sentence = format!(
        "{} on {branch} takes {} now, up from {}.",
        subject(key),
        span(now),
        span(before)
    );
    let links = recent.iter().rev().map(|(run, _)| run.link()).collect();
    Some(insight(
        key,
        "slower",
        Severity::Warning,
        sentence,
        InsightKind::Slower {
            branch: Some(branch.to_owned()),
            before,
            now,
            since: Some(since.link()),
        },
        links,
    ))
}

/// A job that failed and then passed on a re-run of the same run.
fn flaky(key: JobKey<'_>, runs: &[&Run]) -> Option<Insight> {
    let mut by_run: BTreeMap<u64, Vec<&Run>> = BTreeMap::new();
    for run in runs {
        by_run.entry(run.run_id).or_default().push(run);
    }
    let mut failed_first: Vec<&Run> = by_run
        .values()
        .filter_map(|attempts| {
            attempts.iter().copied().find(|failed| {
                failed.failed_itself()
                    && attempts
                        .iter()
                        .any(|later| later.run_attempt > failed.run_attempt && later.succeeded())
            })
        })
        .collect();
    if failed_first.is_empty() {
        return None;
    }
    failed_first.sort_by_key(|run| std::cmp::Reverse((run.finished(), run.id)));
    let flaky = u32::try_from(failed_first.len()).unwrap_or(u32::MAX);
    let total = u32::try_from(by_run.len()).unwrap_or(u32::MAX);
    let sentence = format!(
        "{} failed and then passed on a re-run in {flaky} of its {total} runs.",
        subject(key)
    );
    // Once in the window may be chance: a notice; twice, a warning.
    let severity = if flaky >= FLAKY_WARNING {
        Severity::Warning
    } else {
        Severity::Notice
    };
    Some(insight(
        key,
        "flaky",
        severity,
        sentence,
        InsightKind::Flaky { flaky, total },
        failed_first.iter().take(5).map(|run| run.link()).collect(),
    ))
}

/// A job whose last runs' memory peaks near its profile's `memory`: the profile of its last measured run, whose
/// runs alone count, as a job a repository serves in several profiles peaks apart in each.
fn near_cap(key: JobKey<'_>, runs: &[&Run], config: &Config) -> Option<Insight> {
    let measured = runs
        .iter()
        .rev()
        .filter(|run| run.memory_peak.is_some() && run.profile.is_some());
    let name = measured.clone().next()?.profile.as_deref()?;
    let measured: Vec<&Run> = measured
        .filter(|run| run.profile.as_deref() == Some(name))
        .take(CAP_RUNS)
        .copied()
        .collect();
    if measured.len() < CAP_MIN_RUNS {
        return None;
    }
    let profile = config.profiles.get(name)?;
    let cap = profile.memory.0;
    let peaks: Vec<u64> = measured.iter().filter_map(|run| run.memory_peak).collect();
    let near = |peak: &u64| peak.saturating_mul(100) >= cap.saturating_mul(NEAR_CAP);
    let peak = spread(peaks.clone()).p95;
    if cap == 0 || !near(&peak) {
        return None;
    }
    let near_runs = peaks.iter().filter(|peak| near(peak)).count();
    let auto = if !config.sizing.auto {
        AutoNote::Off
    } else if profile
        .max_memory
        .is_some_and(|max| profile.memory.0 >= max.0)
    {
        AutoNote::AtMax
    } else {
        AutoNote::Raises
    };
    let what = match auto {
        AutoNote::Raises => format!("Auto sizing raises {name} when a run meets the cap."),
        AutoNote::AtMax => {
            format!("{name} is at its max_memory, so auto sizing cannot raise it.")
        }
        AutoNote::Off => format!("Auto sizing is off: raise {name}'s memory, or turn it on."),
    };
    let sentence = format!(
        "{} peaks at {}, {}% of {name}'s {}. {what}",
        subject(key),
        ByteSize(peak),
        peak.saturating_mul(100) / cap,
        ByteSize(cap)
    );
    // Auto sizing deals with it, and one spike may be chance: a notice. The owner acts on the rest.
    let severity = if auto == AutoNote::Raises || near_runs < NEAR_CAP_WARNING {
        Severity::Notice
    } else {
        Severity::Warning
    };
    Some(insight(
        key,
        "near-cap",
        severity,
        sentence,
        InsightKind::NearCap {
            profile: name.to_owned(),
            peak,
            cap,
            auto,
        },
        measured.iter().take(5).map(|run| run.link()).collect(),
    ))
}

/// A job's last runs that could write to the build cache, each with this many lookups at least…
const COLD_RUNS: usize = 5;
const COLD_MIN_RUNS: usize = 3;
const COLD_MIN_LOOKUPS: u64 = 10;
/// …found at most this share of their lookups, in percent: the runs before them stored what they built, so the
/// next should find most of it.
const COLD_PERCENT: u64 = 10;

fn cold_cache(key: JobKey<'_>, runs: &[&Run]) -> Option<Insight> {
    let writers: Vec<&Run> = runs
        .iter()
        .rev()
        .filter(|run| {
            run.build_cache
                .is_some_and(|cache| cache.writer && cache.lookups() >= COLD_MIN_LOOKUPS)
        })
        .take(COLD_RUNS)
        .copied()
        .collect();
    if writers.len() < COLD_MIN_RUNS {
        return None;
    }
    let mut total = api::BuildCacheCounts::default();
    for run in &writers {
        total.add(&run.build_cache.unwrap_or_default());
    }
    let percent = total.hit_percent()?;
    if percent > COLD_PERCENT {
        return None;
    }
    let branch = writers.first()?.branch.clone();
    let sentence = format!(
        "{} got {percent}% of its build cache lookups from the cache{} over its last {} runs: its tasks' inputs \
         differ from run to run.",
        subject(key),
        branch
            .as_deref()
            .map(|branch| format!(" on {branch}"))
            .unwrap_or_default(),
        writers.len()
    );
    Some(insight(
        key,
        "cold-cache",
        Severity::Notice,
        sentence,
        InsightKind::ColdCache {
            branch,
            hits: total.hits,
            lookups: total.lookups(),
        },
        writers.iter().map(|run| run.link()).collect(),
    ))
}

/// One insight per repository the kernel throttled `THROTTLED_MIN_RUNS` or more of the runs of in `window`.
fn throttled(window: &[&Run]) -> Vec<Insight> {
    let mut repositories: BTreeMap<&str, Vec<&Run>> = BTreeMap::new();
    for run in window {
        repositories.entry(&run.repository).or_default().push(run);
    }
    repositories
        .into_iter()
        .filter_map(|(repository, mut runs)| {
            runs.sort_by_key(|run| std::cmp::Reverse((run.finished(), run.id)));
            let hit: Vec<&Run> = runs
                .iter()
                .filter(|run| run.sock_throttled.is_some_and(|count| count > 0))
                .copied()
                .collect();
            let throttled = u32::try_from(hit.len()).unwrap_or(u32::MAX);
            if throttled < THROTTLED_MIN_RUNS {
                return None;
            }
            let total = u32::try_from(runs.len()).unwrap_or(u32::MAX);
            Some(Insight {
                key: format!("throttled:{repository}"),
                severity: Severity::Warning,
                repository: repository.to_owned(),
                workflow: None,
                job: None,
                sentence: format!(
                    "{repository}: memory pressure slowed the downloads of {throttled} of its {total} jobs: the \
                     host, or a job at its cap, was short of memory."
                ),
                kind: InsightKind::Throttled { throttled, total },
                runs: hit.iter().take(5).map(|run| run.link()).collect(),
            })
        })
        .collect()
}

/// Each repository's waits in the 7 days before `now` (ms) and the 7 before those. The week before stays empty when
/// it starts before the history holds every job: it would hold a sliver of it, such as the daemon's first minutes
/// and the jobs queued before it ran. This week is what the history has of it either way.
fn waits(history: &History, now: u64) -> Vec<RepositoryWaits> {
    let before_kept = now.saturating_sub(2 * WEEK) < history.kept_since;
    let mut weeks: BTreeMap<&str, [Vec<&Run>; 2]> = BTreeMap::new();
    for run in &history.runs {
        let Some(started) = run.started_at.filter(|at| *at <= now) else {
            continue;
        };
        let week = match now - started {
            back if back < WEEK => 0,
            back if back < 2 * WEEK && !before_kept => 1,
            _ => continue,
        };
        weeks.entry(&run.repository).or_default()[week].push(run);
    }
    weeks
        .into_iter()
        .map(|(repository, [now, before])| RepositoryWaits {
            repository: repository.to_owned(),
            now: week_waits(&now),
            before: week_waits(&before),
        })
        .collect()
}

fn week_waits(runs: &[&Run]) -> WeekWaits {
    let warm = |run: &&&Run| run.runner_started_at.is_some_and(|at| at <= run.queued_at);
    let of = |runs: Vec<&&Run>| {
        let waited: Vec<u64> = runs.iter().filter_map(|run| run.waited()).collect();
        let jobs = u32::try_from(waited.len()).unwrap_or(u32::MAX);
        let spread = (!waited.is_empty()).then(|| spread(waited));
        WaitSpread {
            jobs,
            p50: spread.as_ref().map(|spread| spread.p50),
            p95: spread.as_ref().map(|spread| spread.p95),
        }
    };
    WeekWaits {
        all: of(runs.iter().collect()),
        warm: of(runs.iter().filter(warm).collect()),
        cold: of(runs.iter().filter(|run| !warm(run)).collect()),
    }
}

fn waits_insight(waits: &RepositoryWaits) -> Option<Insight> {
    if waits.now.all.jobs < WAITS_MIN_JOBS || waits.before.all.jobs < WAITS_MIN_JOBS {
        return None;
    }
    let (before, now) = (waits.before.all.p95?, waits.now.all.p95?);
    let threshold = (before + before * WAITS_PERCENT / 100).max(before + WAITS_SECONDS);
    if now < threshold {
        return None;
    }
    Some(Insight {
        key: format!("waits:{}", waits.repository),
        severity: Severity::Notice,
        repository: waits.repository.clone(),
        workflow: None,
        job: None,
        sentence: if waits.now.all.jobs < WAITS_ONE_IN_20 {
            format!(
                "{}: the longest wait for a runner this week is {}, up from {} the week before.",
                waits.repository,
                span(now),
                span(before)
            )
        } else {
            format!(
                "{}: 1 job in 20 waits {} or more for a runner this week, up from {} the week before.",
                waits.repository,
                span(now),
                span(before)
            )
        },
        kind: InsightKind::Waits { before, now },
        runs: Vec::new(),
    })
}

/// A duration as a sentence reads it: `45s`, `4m 30s`, `1h 5m`.
fn span(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 if seconds.is_multiple_of(60) => format!("{}m", seconds / 60),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests;
