//! `/history`: finished and running jobs, filtered by repository and outcome, a page at a time; and the trends of
//! the last 30 days from `/v1/stats`: jobs per day by conclusion, how long they waited and ran, their memory peaks.

use maud::{Markup, html};
use runnerd_core::api::{Bucket, JobPage, JobQuery, JobState, Stats, StatsQuery, StatsRow, Status};
use serde::{Deserialize, Serialize};

use super::{Durations, Page, Parts, badge, durations, js, page, profile_class, time_ago};
use crate::chart::{Band, Bar, Base, Series, TimeChart, nice_top, stacked_bars};
use crate::daemon::{Daemon, Error};
use crate::format;

/// How far back the trends reach.
const TREND_DAYS: u64 = 30;
const PAGE: u32 = 50;

/// The outcomes the filter offers: a conclusion or a state.
const OUTCOMES: [(&str, &str); 8] = [
    ("success", "Succeeded"),
    ("failure", "Failed"),
    ("cancelled", "Cancelled"),
    ("timed_out", "Timed out"),
    ("running", "Running"),
    ("queued", "Queued"),
    ("lost", "Lost"),
    ("elsewhere", "Ran elsewhere"),
];

/// The page's query string.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Filter {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none"
    )]
    pub(crate) repository: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none"
    )]
    pub(crate) outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) before: Option<u64>,
}

/// An empty form field is no filter.
fn empty_none<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.filter(|value| !value.is_empty()))
}

impl Filter {
    fn query(&self) -> JobQuery {
        let mut query = JobQuery {
            repository: self.repository.clone(),
            before: self.before,
            limit: Some(PAGE),
            ..JobQuery::default()
        };
        match self.outcome.as_deref() {
            Some("running") => query.state = Some(JobState::Running),
            Some("queued") => query.state = Some(JobState::Queued),
            Some("lost") => query.state = Some(JobState::Lost),
            Some("elsewhere") => query.state = Some(JobState::Elsewhere),
            Some(conclusion) => query.conclusion = Some(conclusion.to_owned()),
            None => {}
        }
        query
    }
}

/// Everything the history page shows.
#[derive(Debug, Clone)]
pub(crate) struct History {
    pub(crate) now: u64,
    pub(crate) filter: Filter,
    pub(crate) status: Status,
    pub(crate) page: JobPage,
    pub(crate) stats: Stats,
}

impl History {
    pub(crate) async fn load(daemon: &Daemon, filter: Filter, now: u64) -> Result<History, Error> {
        let status = daemon.get("/v1/status").await?;
        let page = daemon.get_query("/v1/jobs", &filter.query()).await?;
        let trends = daemon
            .get_query(
                "/v1/stats",
                &StatsQuery {
                    repository: filter.repository.clone(),
                    since: Some(day_start(now).saturating_sub((TREND_DAYS - 1) * 86_400)),
                    until: None,
                    bucket: Bucket::Day,
                },
            )
            .await?;
        Ok(History {
            now,
            filter,
            status,
            page,
            stats: trends,
        })
    }
}

fn day_start(at: u64) -> u64 {
    at - at % 86_400
}

/// One day of the trends, all repositories of the filter summed.
#[derive(Debug, Default, Clone)]
struct Day {
    start: u64,
    jobs: u64,
    succeeded: u64,
    failed: u64,
    cancelled: u64,
    queue_seconds: u64,
    run_seconds: u64,
    memory_peak: u64,
}

fn days(stats: &Stats, now: u64) -> Vec<Day> {
    let first = day_start(now).saturating_sub((TREND_DAYS - 1) * 86_400);
    let mut days: Vec<Day> = (0..TREND_DAYS)
        .map(|i| Day {
            start: first + i * 86_400,
            ..Day::default()
        })
        .collect();
    for row in &stats.rows {
        let StatsRow {
            start,
            jobs,
            succeeded,
            failed,
            cancelled,
            queue_seconds,
            run_seconds,
            memory_peak,
            ..
        } = row;
        if let Some(day) = days.iter_mut().find(|day| day.start == *start) {
            day.jobs += jobs;
            day.succeeded += succeeded;
            day.failed += failed;
            day.cancelled += cancelled;
            day.queue_seconds += queue_seconds;
            day.run_seconds += run_seconds;
            day.memory_peak = day.memory_peak.max(*memory_peak);
        }
    }
    days
}

/// The whole page.
pub(crate) fn render(history: &History) -> Markup {
    let main = html! {
        h1.visually-hidden { "History" }
        (filters(history))
        (trends(history))
        (jobs(history))
    };
    page("History", Some(Page::History), None, &main)
}

fn filters(history: &History) -> Markup {
    let filter = &history.filter;
    html! {
        // A change applies the filter; the button is for a browser without scripts.
        form.filters method="get" action="/history" data-on:change="el.requestSubmit()" {
            label {
                span.visually-hidden { "Repository" }
                select name="repository" {
                    option value="" { "All repositories" }
                    @for repository in &history.status.repositories {
                        option value=(repository.name) selected[filter.repository.as_deref() == Some(repository.name.as_str())] { (repository.name) }
                    }
                }
            }
            label {
                span.visually-hidden { "Outcome" }
                select name="outcome" {
                    option value="" { "Any outcome" }
                    @for (value, label) in OUTCOMES {
                        option value=(value) selected[filter.outcome.as_deref() == Some(value)] { (label) }
                    }
                }
            }
            noscript { button.button type="submit" { "Filter" } }
            @if filter.repository.is_some() || filter.outcome.is_some() {
                a href="/history" { "Clear" }
            }
        }
    }
}

/// Picks the day under the pointer, from the chart it is over: day `i` is the `i`th thirtieth of the chart's width.
const DAY_UNDER_POINTER: &str = "const area = evt.target.closest('.chart-card')?.querySelector('.chart-area'); \
    if (area) { const box = area.getBoundingClientRect(); \
    $_day = Math.min(29, Math.max(0, Math.floor((evt.clientX - box.left) / box.width * 30))) }";
/// Steps through the days with the arrow keys.
const DAY_KEYS: &str = "if (evt.key == 'ArrowLeft') { $_day = Math.max(0, ($_day < 0 ? 30 : $_day) - 1); evt.preventDefault() } \
    else if (evt.key == 'ArrowRight') { $_day = Math.min(29, $_day + 1); evt.preventDefault() } \
    else if (evt.key == 'Escape') { $_day = -1 }";

/// The last 30 days as three charts on one day axis. Pointing at a day, or stepping to it with the arrow keys, marks
/// it on every chart and puts its numbers in the headline.
fn trends(history: &History) -> Markup {
    let days = days(&history.stats, history.now);
    let peak = days.iter().map(|day| day.memory_peak).max().unwrap_or(0);
    let bars: Vec<Bar> = days
        .iter()
        .map(|day| Bar {
            label: format::short_date(day.start),
            parts: vec![
                day.succeeded,
                day.failed,
                day.cancelled,
                day.jobs
                    .saturating_sub(day.succeeded + day.failed + day.cancelled),
            ],
        })
        .collect();
    let durations = daily(&days, |day| day.run_seconds.checked_div(day.jobs));
    let waits = daily(&days, |day| day.queue_seconds.checked_div(day.jobs));
    let peaks = daily(&days, |day| (day.jobs > 0).then_some(day.memory_peak));
    let time_top = nice_top(
        durations
            .iter()
            .chain(&waits)
            .map(|point| point.1)
            .max()
            .unwrap_or(0),
        Base::Decimal,
    );
    let times = per_day(
        "Average run time and wait per day",
        vec![
            Series::line("run time", "run", durations),
            Series::line("wait", "wait", waits),
        ],
        format::duration,
        &days,
    );
    let memory = per_day(
        "Highest sampled memory peak per day",
        vec![Series::line("memory peak", "memory", peaks)],
        format::bytes,
        &days,
    );
    // One band per chart, a day wide, which the stylesheet moves to the picked day by `--day`.
    let mark = |from, to| {
        [Band {
            from,
            to: Some(to),
            shown: "$_day >= 0".to_owned(),
            label: String::new(),
        }]
    };
    let bar_marks = mark(0, 1);
    let day_marks = days
        .first()
        .map_or(mark(0, 0), |day| mark(day.start, day.start + 86_399));
    let dates: Vec<String> = days
        .iter()
        .map(|day| format::short_date(day.start))
        .collect();
    html! {
        section.trends aria-labelledby="trends-title" "data-signals:_day__ifmissing"="-1" "data-style:--day"="$_day" {
            h2 #trends-title data-text=(lens("Last 30 days", &dates)) { "Last 30 days" }
            p.visually-hidden { "Jobs that finished, by the day they were queued (UTC). Arrow keys step through the days." }
            (headline(&days))
            div.charts.three tabindex="0" role="group" aria-label="Charts per day"
                data-on:pointermove=(DAY_UNDER_POINTER) data-on:pointerdown=(DAY_UNDER_POINTER)
                data-on:pointerleave="evt.pointerType == 'mouse' && ($_day = -1)" data-on:keydown=(DAY_KEYS) data-on:blur="$_day = -1" {
                div.chart-card {
                    h3 { "Jobs per day" }
                    (stacked_bars("Jobs per day by conclusion", &["success", "failure", "neutral", "other"], &bars, |n| n.to_string(), &bar_marks))
                    ul.legend {
                        li.success { span.swatch {} "succeeded" }
                        li.failure { span.swatch {} "failed or lost" }
                        li.neutral { span.swatch {} "cancelled" }
                    }
                }
                div.chart-card {
                    h3 { "Run time and wait" }
                    (times.render_with(time_top, &day_marks, &Parts::default()))
                    ul.legend {
                        li.run { span.swatch {} "average run" }
                        li.wait { span.swatch {} "average wait for a runner" }
                    }
                }
                div.chart-card {
                    h3 { "Memory peak" }
                    (memory.render_with(nice_top(peak, Base::Binary), &day_marks, &Parts::default()))
                }
            }
        }
    }
}

/// A Datastar expression for text that is `whole` with no day picked and the picked day's entry of `per_day` else.
fn lens(whole: &str, per_day: &[String]) -> String {
    let entries: Vec<String> = per_day.iter().map(|entry| js(entry)).collect();
    format!(
        "$_day < 0 ? {} : [{}][$_day]",
        js(whole),
        entries.join(", ")
    )
}

/// A day's value at its middle, so a line's points sit over the bars of the same days.
fn daily(days: &[Day], value: fn(&Day) -> Option<u64>) -> Vec<(u64, u64)> {
    days.iter()
        .filter_map(|day| value(day).map(|value| (day.start + 43_200, value)))
        .collect()
}

fn per_day(title: &str, series: Vec<Series>, value: fn(u64) -> String, days: &[Day]) -> TimeChart {
    TimeChart {
        title: title.to_owned(),
        start: days.first().map_or(0, |day| day.start),
        end: days.last().map_or(0, |day| day.start + 86_399),
        window: None,
        live: None,
        series,
        limit: None,
        value,
        time: format::short_date,
        gap: 86_400,
    }
}

/// The 30 days' totals, or the picked day's.
fn headline(days: &[Day]) -> Markup {
    let total: u64 = days.iter().map(|day| day.jobs).sum();
    let succeeded: u64 = days.iter().map(|day| day.succeeded).sum();
    let run: u64 = days.iter().map(|day| day.run_seconds).sum();
    let queue: u64 = days.iter().map(|day| day.queue_seconds).sum();
    let peak = |peak: u64| {
        if peak > 0 {
            format::bytes(peak)
        } else {
            "–".to_owned()
        }
    };
    let share = |succeeded: u64, jobs: u64| {
        (succeeded * 100)
            .checked_div(jobs)
            .map_or("–".to_owned(), |percent| format!("{percent}%"))
    };
    let average = |sum: u64, jobs: u64| {
        sum.checked_div(jobs)
            .map_or("–".to_owned(), format::duration)
    };
    let each = |value: &dyn Fn(&Day) -> String| days.iter().map(value).collect::<Vec<_>>();
    let figures = [
        ("Jobs", total.to_string(), each(&|day| day.jobs.to_string())),
        (
            "Succeeded",
            share(succeeded, total),
            each(&|day| share(day.succeeded, day.jobs)),
        ),
        (
            "Average run",
            average(run, total),
            each(&|day| average(day.run_seconds, day.jobs)),
        ),
        (
            "Average wait",
            average(queue, total),
            each(&|day| average(day.queue_seconds, day.jobs)),
        ),
        (
            "Sampled memory peak",
            peak(days.iter().map(|day| day.memory_peak).max().unwrap_or(0)),
            each(&|day| peak(day.memory_peak)),
        ),
    ];
    html! {
        dl.headline aria-live="polite" {
            @for (label, whole, per_day) in &figures {
                div { dt { (label) } dd data-text=(lens(whole, per_day)) { (whole) } }
            }
        }
    }
}

fn jobs(history: &History) -> Markup {
    let now = history.now;
    let next = history.page.next.map(|before| {
        let filter = Filter {
            before: Some(before),
            ..history.filter.clone()
        };
        format!(
            "/history?{}",
            serde_urlencoded::to_string(&filter).unwrap_or_default()
        )
    });
    html! {
        section.jobs aria-labelledby="jobs-title" {
            h2 #jobs-title .visually-hidden { "Jobs" }
            @if history.page.jobs.is_empty() {
                p.muted { "No jobs match." }
            } @else {
                div.table-wrap {
                    table.job-table {
                        thead {
                            tr {
                                th scope="col" { "Job" }
                                th scope="col" { "Outcome" }
                                th scope="col" { "Queued" }
                                th scope="col" .number { "Waited" }
                                th scope="col" .number { "Ran" }
                                th scope="col" .number { abbr title="The highest of the samples taken every 2 seconds" { "Sampled peak" } }
                                th scope="col" .number { "Avg CPU" }
                            }
                        }
                        tbody {
                            @for job in &history.page.jobs {
                                @let Durations { waited, ran } = durations(job, now);
                                @let class = job.runner.as_ref().map_or("p-none".to_owned(), |runner| profile_class(&history.status, &runner.profile));
                                tr class=(class) {
                                    th scope="row" {
                                        a href=(format!("/jobs/{}", job.id)) {
                                            span.swatch aria-hidden="true" {}
                                            span.job-name { (job.name) }
                                            span.repo { (job.repository) @if let Some(branch) = &job.branch { " · " (branch) } }
                                        }
                                    }
                                    td { (badge(job)) }
                                    td { (time_ago(job.timeline.queued_at, now)) }
                                    td.number { (waited.map_or("–".to_owned(), format::duration)) }
                                    td.number { (ran.map_or("–".to_owned(), format::duration)) }
                                    td.number { (job.metrics.memory_peak.map_or("–".to_owned(), format::bytes)) }
                                    td.number { (job.metrics.cpu_average.map_or("–".to_owned(), format::cores)) }
                                }
                            }
                        }
                    }
                }
            }
            @if let Some(next) = next {
                p.pager { a.button-quiet href=(next) { "Older jobs" } }
            }
        }
    }
}
