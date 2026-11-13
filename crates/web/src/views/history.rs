//! `/history`: the jobs page. One query string, one page, three lenses over it: the list (a search of job name,
//! workflow and branch, filters by repository, workflow, branch, outcome, time and record, and sortable columns),
//! the timeline of what each slot ran, and the trends of the last 30 days. The filters are a plain form, so a page
//! works without scripts and can be bookmarked; what a list shows and how it is cut is core's (`JobList`).

use maud::{Markup, html};
use runnerd_core::api::{
    BarEnd, Bucket, Column, Feature, GapKind, JOBS_HEADING, JobList, JobOrder, JobQuery, JobRange,
    JobSort, JobState, JobSummary, Lane, RecordFilter, Section, SlotTimeline, SortOrder, Stats,
    StatsQuery, StatsSummary, Status, TimelineBar,
};
use runnerd_core::client::{Client, ClientError};
use serde::de::{DeserializeOwned, IntoDeserializer};
use serde::{Deserialize, Serialize};

use super::{
    Page, Parts, badge, heading, js, outcome_kind_class, page, profile_class, time_ago, tone_class,
};
use crate::chart::{Band, Bar, Base, Series, TimeChart, nice_top, stacked_bars};
use crate::format;

/// How far back the trends reach.
const TREND_DAYS: u64 = 30;
const PAGE: u32 = 50;
/// The latest jobs read for the workflows and branches the filters offer.
const CHOICES_READ: u32 = 500;

/// The outcomes the filter offers: a conclusion or a state.
const OUTCOMES: [(&str, &str); 8] = [
    ("success", "Succeeded"),
    ("failure", "Failed"),
    ("cancelled", "Cancelled"),
    ("timed_out", "Timed out"),
    ("running", "Running"),
    ("queued", "Queued"),
    ("lost", "Lost"),
    ("elsewhere", "Not run here"),
];

/// Which lens of the page shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Lens {
    /// The jobs, a page at a time.
    #[default]
    List,
    /// What each slot ran over the time range.
    Timeline,
    /// The last 30 days, per day.
    Trends,
}

impl Lens {
    const ALL: [Lens; 3] = [Lens::List, Lens::Timeline, Lens::Trends];

    fn label(self) -> &'static str {
        match self {
            Lens::List => "List",
            Lens::Timeline => "Timeline",
            Lens::Trends => "Trends",
        }
    }
}

/// How far back the jobs or the timeline reach, as core's [`JobRange`], in the URL as `24h`, `7d` or `30d`; every job
/// kept has no parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range(JobRange);

impl Range {
    /// The URL's name for each range but every job kept.
    fn slug(range: JobRange) -> Option<&'static str> {
        match range {
            JobRange::Day => Some("24h"),
            JobRange::Week => Some("7d"),
            JobRange::Month => Some("30d"),
            JobRange::All => None,
        }
    }
}

impl Serialize for Range {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Range::slug(self.0).unwrap_or_default())
    }
}

impl<'de> Deserialize<'de> for Range {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Range, D::Error> {
        let slug = String::deserialize(deserializer)?;
        JobRange::ALL
            .into_iter()
            .find(|range| Range::slug(*range) == Some(slug.as_str()))
            .map(Range)
            .ok_or_else(|| serde::de::Error::custom("no such range"))
    }
}

/// The order the list is read in, as core's [`JobOrder`], in the URL as its sort and direction: `wait-desc`. Newest
/// first, the default, has no parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Order(JobOrder);

impl Order {
    fn slug(order: JobOrder) -> String {
        format!(
            "{}-{}",
            value_of(&order.sort).unwrap_or_default(),
            value_of(&order.order).unwrap_or_default()
        )
    }
}

impl Serialize for Order {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&Order::slug(self.0))
    }
}

impl<'de> Deserialize<'de> for Order {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Order, D::Error> {
        let slug = String::deserialize(deserializer)?;
        JobOrder::ALL
            .into_iter()
            .find(|order| Order::slug(*order) == slug)
            .map(Order)
            .ok_or_else(|| serde::de::Error::custom("no such order"))
    }
}

/// The page's query string: the filters, the sort, the lens and the page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Filter {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none"
    )]
    pub(crate) q: Option<String>,
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
    pub(crate) workflow: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none"
    )]
    pub(crate) branch: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none"
    )]
    pub(crate) outcome: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none_of"
    )]
    pub(crate) range: Option<Range>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none_of"
    )]
    pub(crate) record: Option<RecordFilter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none_of"
    )]
    pub(crate) order: Option<Order>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none_of"
    )]
    pub(crate) lens: Option<Lens>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "empty_none_of"
    )]
    pub(crate) before: Option<u64>,
}

/// An empty form field is no filter.
fn empty_none<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.filter(|value| !value.is_empty()))
}

/// An empty form field is no filter, and so is a value that is none of `T`'s: an old or mistyped link shows the page
/// without it rather than an error.
fn empty_none_of<'de, D: serde::Deserializer<'de>, T: DeserializeOwned>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Ok(empty_none(deserializer)?.and_then(|value| {
        T::deserialize(
            IntoDeserializer::<serde::de::value::Error>::into_deserializer(value.as_str()),
        )
        .ok()
        .or_else(|| serde_json::from_str(&value).ok())
    }))
}

impl Filter {
    fn lens(&self) -> Lens {
        self.lens.unwrap_or_default()
    }

    /// The order the list is read in.
    fn job_order(&self) -> JobOrder {
        self.order.map_or(JobOrder::NEWEST, |order| order.0)
    }

    /// Whether the list is in the default order, newest first, which pages.
    fn pages(&self) -> bool {
        self.job_order().pages()
    }

    /// What it asks `/v1/jobs` for at `now`.
    fn query(&self, now: u64) -> JobQuery {
        let mut query = JobQuery {
            repository: self.repository.clone(),
            workflow: self.workflow.clone(),
            branch: self.branch.clone(),
            q: self.q.clone(),
            record: self.record,
            since: self.range.and_then(|range| range.0.since(now)),
            before: self.before,
            limit: Some(PAGE),
            ..JobQuery::default()
        };
        self.job_order().apply(&mut query);
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

    /// Whether it asks for what a daemon older than search and sorting ignores.
    fn searches(&self) -> bool {
        self.q.is_some() || self.branch.is_some() || self.record.is_some() || !self.pages()
    }

    /// This page's link with `change` made, back on the first page.
    fn link(&self, change: impl FnOnce(&mut Filter)) -> String {
        let mut next = Filter {
            before: None,
            ..self.clone()
        };
        change(&mut next);
        let query = serde_urlencoded::to_string(&next).unwrap_or_default();
        if query.is_empty() {
            "/history".to_owned()
        } else {
            format!("/history?{query}")
        }
    }
}

/// What the list lens shows: the daemon's page, or why there is none.
#[derive(Debug, Clone)]
pub(crate) enum Listing {
    /// The page, as the daemon answered.
    Jobs(Box<JobList>),
    /// The query, refused in core's words before it was sent.
    Refused(String),
}

/// What the timeline lens shows.
#[derive(Debug, Clone)]
pub(crate) enum Timeline {
    /// What each slot ran.
    Drawn(SlotTimeline),
    /// The daemon is older than the timeline.
    Unsupported,
    /// The daemon refused the window, in its words.
    Refused(String),
}

/// Everything the jobs page shows: the list, the timeline or the trends, as its lens says.
#[derive(Debug, Clone)]
pub(crate) struct History {
    pub(crate) now: u64,
    pub(crate) filter: Filter,
    pub(crate) status: Status,
    pub(crate) listing: Option<Listing>,
    pub(crate) timeline: Option<Timeline>,
    pub(crate) stats: Option<Stats>,
    /// The workflows of the repository's latest jobs (of every repository's without one), by name.
    pub(crate) workflows: Vec<String>,
    /// Their branches, by name.
    pub(crate) branches: Vec<String>,
}

impl History {
    pub(crate) async fn load(
        daemon: &Client,
        filter: Filter,
        now: u64,
    ) -> Result<History, ClientError> {
        let status = daemon.get("/v1/status").await?;
        let lens = filter.lens();
        let listing = match lens {
            Lens::List => {
                let query = filter.query(now);
                Some(match query.check() {
                    Ok(()) => Listing::Jobs(Box::new(daemon.jobs(&query).await?)),
                    Err(reason) => Listing::Refused(reason),
                })
            }
            Lens::Timeline | Lens::Trends => None,
        };
        let timeline = match lens {
            Lens::Timeline => {
                let query = timeline_range(&filter).timeline(now);
                Some(match daemon.timeline(&query).await {
                    Ok(timeline) => Timeline::Drawn(timeline),
                    Err(error) if error.is_unsupported() => Timeline::Unsupported,
                    Err(ClientError::Api {
                        status, message, ..
                    }) if status == axum::http::StatusCode::BAD_REQUEST => {
                        Timeline::Refused(message)
                    }
                    Err(error) => return Err(error),
                })
            }
            Lens::List | Lens::Trends => None,
        };
        let trends = match lens {
            Lens::Trends => Some(
                daemon
                    .get_query(
                        "/v1/stats",
                        &StatsQuery {
                            repository: filter.repository.clone(),
                            since: Some(day_start(now).saturating_sub((TREND_DAYS - 1) * 86_400)),
                            until: None,
                            bucket: Bucket::Day,
                        },
                    )
                    .await?,
            ),
            Lens::List | Lens::Timeline => None,
        };
        let (workflows, branches) = match lens {
            Lens::List => {
                let latest = daemon
                    .jobs(&JobQuery {
                        repository: filter.repository.clone(),
                        limit: Some(CHOICES_READ),
                        ..JobQuery::default()
                    })
                    .await?;
                choices(&latest.page.jobs, &filter)
            }
            Lens::Timeline | Lens::Trends => (vec![], vec![]),
        };
        Ok(History {
            now,
            filter,
            status,
            listing,
            timeline,
            stats: trends,
            workflows,
            branches,
        })
    }
}

/// The workflows and branches the filters offer: those of `jobs`, and the ones filtered on, which stay offered though
/// none of the latest jobs ran them.
fn choices(jobs: &[JobSummary], filter: &Filter) -> (Vec<String>, Vec<String>) {
    let sorted = |mut names: Vec<String>, current: &Option<String>| {
        names.extend(current.clone());
        names.sort();
        names.dedup();
        names
    };
    let workflows = jobs.iter().filter_map(|job| job.workflow.clone()).collect();
    let branches = jobs.iter().filter_map(|job| job.branch.clone()).collect();
    (
        sorted(workflows, &filter.workflow),
        sorted(branches, &filter.branch),
    )
}

fn day_start(at: u64) -> u64 {
    at - at % 86_400
}

/// The trends' days, all repositories of the filter summed, oldest first.
fn days(stats: &Stats, now: u64) -> Vec<StatsSummary> {
    stats.daily(
        day_start(now).saturating_sub((TREND_DAYS - 1) * 86_400),
        TREND_DAYS,
    )
}

/// The whole page.
pub(crate) fn render(history: &History) -> Markup {
    let main = html! {
        (heading(runnerd_core::api::View::Jobs.title(), Some(Page::History), Some(lenses(history))))
        @match history.filter.lens() {
            Lens::List => {
                (filters(history))
                (jobs(history))
            }
            Lens::Timeline => { (timeline(history)) }
            Lens::Trends => { (trends(history)) }
        }
    };
    page(Section::History.title(), Some(Page::History), None, &main)
}

/// The lenses, as links that keep the filters.
/// The link to `lens` from the page `filter` shows. The list keeps every filter; the timeline draws every slot, so its
/// link keeps only the time range, and the trends chart one repository or all, so theirs keeps only that: a filter a
/// lens does not draw never sits in its URL as if it did.
fn lens_link(filter: &Filter, lens: Lens) -> String {
    match lens {
        Lens::List => filter.link(|next| next.lens = None),
        Lens::Timeline => Filter {
            lens: Some(Lens::Timeline),
            range: filter.range,
            ..Filter::default()
        }
        .link(|_| {}),
        Lens::Trends => Filter {
            lens: Some(Lens::Trends),
            repository: filter.repository.clone(),
            ..Filter::default()
        }
        .link(|_| {}),
    }
}

fn lenses(history: &History) -> Markup {
    let current = history.filter.lens();
    html! {
        nav.lenses aria-label="Lens" {
            @for lens in Lens::ALL {
                a href=(lens_link(&history.filter, lens))
                    aria-current=[(lens == current).then_some("page")] { (lens.label()) }
            }
        }
    }
}

/// A select of the filter form: `None` is its first option, `all`.
fn select(
    name: &str,
    label: &str,
    all: &str,
    current: Option<&str>,
    options: &[(String, String)],
) -> Markup {
    html! {
        label.field {
            span.field-name { (label) }
            select name=(name) data-on:change="el.form.requestSubmit()" {
                option value="" selected[current.is_none()] { (all) }
                @for (value, text) in options {
                    option value=(value) selected[current == Some(value.as_str())] { (text) }
                }
            }
        }
    }
}

/// The search and the filters, one GET form: a change submits it, and its button does without scripts.
fn filters(history: &History) -> Markup {
    let filter = &history.filter;
    let named = |names: &[String]| -> Vec<(String, String)> {
        names
            .iter()
            .map(|name| (name.clone(), name.clone()))
            .collect()
    };
    let repositories: Vec<String> = history
        .status
        .repositories
        .iter()
        .map(|repository| repository.name.clone())
        .chain(filter.repository.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let outcomes: Vec<(String, String)> = OUTCOMES
        .iter()
        .map(|(value, label)| ((*value).to_owned(), (*label).to_owned()))
        .collect();
    let ranges: Vec<(String, String)> = JobRange::ALL
        .iter()
        .filter_map(|range| {
            Some((
                Range::slug(*range)?.to_owned(),
                format::sentence(range.words()),
            ))
        })
        .collect();
    let records: Vec<(String, String)> = RecordFilter::ALL
        .iter()
        .filter(|record| **record != RecordFilter::Any || filter.record == Some(RecordFilter::Any))
        .filter_map(|record| Some((value_of(record)?, format::sentence(record.words()))))
        .collect();
    let record = filter.record.as_ref().and_then(value_of);
    // Every order, each way: a header's sort reads back in the select as it is.
    let orders: Vec<(String, String)> = JobOrder::ALL
        .iter()
        .filter(|order| !order.pages())
        .map(|order| (Order::slug(*order), format::sentence(order.words())))
        .collect();
    let order = filter
        .order
        .filter(|order| !order.0.pages())
        .map(|order| Order::slug(order.0));
    html! {
        form.filters method="get" action="/history" role="search" {
            label.field.search {
                span.field-name { "Search job name, workflow, branch" }
                input type="search" name="q" value=[filter.q.as_deref()] data-search
                    autocomplete="off" spellcheck="false" maxlength="200";
            }
            (select("repository", "Repository", "All", filter.repository.as_deref(), &named(&repositories)))
            @if history.workflows.len() > 1 || filter.workflow.is_some() {
                (select("workflow", "Workflow", "All", filter.workflow.as_deref(), &named(&history.workflows)))
            }
            @if history.branches.len() > 1 || filter.branch.is_some() {
                (select("branch", "Branch", "All", filter.branch.as_deref(), &named(&history.branches)))
            }
            (select("outcome", "Outcome", "Any", filter.outcome.as_deref(), &outcomes))
            (select("range", "Queued", &format::sentence(JobRange::All.words()), filter.range.and_then(|range| Range::slug(range.0)), &ranges))
            (select("record", "Record", "Any job", record.as_deref(), &records))
            (select("order", "Sort", &format::sentence(JobOrder::NEWEST.words()), order.as_deref(), &orders))
            button.button-quiet type="submit" { "Search" }
            @if *filter != Filter::default() {
                a.clear href="/history" { "Clear" }
            }
        }
    }
}

/// A value as the query string writes it: `kept`, `wait`, `asc`.
fn value_of<T: Serialize>(value: &T) -> Option<String> {
    serde_urlencoded::to_string([("v", value)])
        .ok()
        .and_then(|pair| pair.strip_prefix("v=").map(str::to_owned))
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
    let Some(stats) = &history.stats else {
        return html! {};
    };
    let days = days(stats, history.now);
    let peak = StatsSummary::of(&days).memory_peak.unwrap_or(0);
    let bars: Vec<Bar> = days
        .iter()
        .map(|day| Bar {
            label: format::short_date(day.start),
            parts: vec![day.succeeded, day.failed, day.cancelled, day.other()],
        })
        .collect();
    let durations = daily(&days, StatsSummary::average_run);
    let waits = daily(&days, StatsSummary::average_wait);
    let peaks = daily(&days, |day| day.memory_peak);
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
            h2 #trends-title data-text=(day_lens("Last 30 days", &dates)) { "Last 30 days" }
            p.visually-hidden { "Jobs that finished, by the day they were queued (UTC). Arrow keys step through the days." }
            @if let Some(repository) = &history.filter.repository { p.muted { "Jobs of " (repository) "." } }
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
fn day_lens(whole: &str, per_day: &[String]) -> String {
    let entries: Vec<String> = per_day.iter().map(|entry| js(entry)).collect();
    format!(
        "$_day < 0 ? {} : [{}][$_day]",
        js(whole),
        entries.join(", ")
    )
}

/// A day's value at its middle, so a line's points sit over the bars of the same days.
fn daily(days: &[StatsSummary], value: fn(&StatsSummary) -> Option<u64>) -> Vec<(u64, u64)> {
    days.iter()
        .filter_map(|day| value(day).map(|value| (day.start + 43_200, value)))
        .collect()
}

fn per_day(
    title: &str,
    series: Vec<Series>,
    value: fn(u64) -> String,
    days: &[StatsSummary],
) -> TimeChart {
    TimeChart {
        title: title.to_owned(),
        start: days.first().map_or(0, |day| day.start),
        end: days.last().map_or(0, |day| day.start + 86_399),
        window: None,
        live: None,
        series,
        limits: vec![],
        value,
        time: format::short_date,
        gap: 86_400,
    }
}

/// The 30 days' totals, or the picked day's.
fn headline(days: &[StatsSummary]) -> Markup {
    let total = StatsSummary::of(days);
    let peak = |peak: Option<u64>| peak.map_or("–".to_owned(), format::bytes);
    let share = |summary: &StatsSummary| {
        summary
            .success_percent()
            .map_or("–".to_owned(), |percent| format!("{percent}%"))
    };
    let average = |seconds: Option<u64>| seconds.map_or("–".to_owned(), format::duration);
    let each = |value: &dyn Fn(&StatsSummary) -> String| days.iter().map(value).collect::<Vec<_>>();
    let figures = [
        (
            JOBS_HEADING,
            total.jobs.to_string(),
            each(&|day| day.jobs.to_string()),
        ),
        ("Succeeded", share(&total), each(&|day| share(day))),
        (
            "Average run",
            average(total.average_run()),
            each(&|day| average(day.average_run())),
        ),
        (
            "Average wait",
            average(total.average_wait()),
            each(&|day| average(day.average_wait())),
        ),
        (
            "Sampled memory peak",
            peak(total.memory_peak),
            each(&|day| peak(day.memory_peak)),
        ),
    ];
    html! {
        dl.headline aria-live="polite" {
            @for (label, whole, per_day) in &figures {
                div { dt { (label) } dd data-text=(day_lens(whole, per_day)) { (whole) } }
            }
        }
    }
}

/// How a column's cells line up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    /// From the start, as text.
    Start,
    /// To the end, as figures.
    Number,
}

/// A column header that sorts the list: by `sort`, largest or latest first, then the other way on a second click.
fn sort_header(filter: &Filter, column: Column, sort: JobSort, align: Align) -> Markup {
    let current = filter.job_order();
    let order = current.order;
    let sorted = current.sort == sort;
    let next = if sorted && order == SortOrder::Desc {
        SortOrder::Asc
    } else {
        SortOrder::Desc
    };
    let href = filter.link(|next_filter| {
        // Newest first is the default, which pages: it leaves the query string.
        let chosen = JobOrder { sort, order: next };
        next_filter.order = (!chosen.pages()).then_some(Order(chosen));
    });
    let aria = sorted.then_some(if order == SortOrder::Asc {
        "ascending"
    } else {
        "descending"
    });
    html! {
        th scope="col" class=[(align == Align::Number).then_some("number")] aria-sort=[aria] {
            a.sort href=(href) {
                (column.title())
                span.sort-mark aria-hidden="true" {
                    @if sorted { @if order == SortOrder::Asc { "↑" } @else { "↓" } }
                }
            }
        }
    }
}

fn jobs(history: &History) -> Markup {
    let now = history.now;
    let filter = &history.filter;
    let list = match &history.listing {
        Some(Listing::Jobs(list)) => list,
        Some(Listing::Refused(reason)) => {
            return html! { p.notice.tone-warn role="alert" { (format::sentence(reason)) "." } };
        }
        None => return html! {},
    };
    let next = list.page.next.filter(|_| filter.pages()).map(|before| {
        let next = Filter {
            before: Some(before),
            ..filter.clone()
        };
        format!(
            "/history?{}",
            serde_urlencoded::to_string(&next).unwrap_or_default()
        )
    });
    html! {
        section.jobs aria-labelledby="jobs-title" {
            h2 #jobs-title .visually-hidden { (JOBS_HEADING) }
            @if let Some(unread) = list.unread_words().filter(|_| filter.searches()) {
                p.notice.tone-warn role="status" { (format::sentence(&unread)) "." }
            }
            @if let Some(shown) = list.shown_line() {
                p.list-count.muted { (format::sentence(&shown)) "." }
            }
            @if list.page.jobs.is_empty() {
                p.muted { (format::sentence(JobList::none_words())) "." }
            } @else {
                div.table-wrap {
                    table.job-table.history-table {
                        thead {
                            tr {
                                th scope="col" { (Column::Job.title()) }
                                th scope="col" { (Column::Outcome.title()) }
                                (sort_header(filter, Column::Queued, JobSort::Queued, Align::Start))
                                (sort_header(filter, Column::Waited, JobSort::Wait, Align::Number))
                                (sort_header(filter, Column::Ran, JobSort::Run, Align::Number))
                                (sort_header(filter, Column::MemoryPeak, JobSort::Memory, Align::Number))
                                th scope="col" .number { (Column::Cpu.title()) }
                            }
                        }
                        tbody {
                            @for job in &list.page.jobs {
                                @let class = job.runner.as_ref().map_or("p-none".to_owned(), |runner| profile_class(&history.status, &runner.profile));
                                tr class=(class) data-row {
                                    th scope="row" {
                                        a href=(format!("/jobs/{}", job.id)) {
                                            span.swatch aria-hidden="true" {}
                                            span.job-name { (job.title().name) }
                                            span.repo { (job.repository) @if let Some(branch) = &job.branch { " · " (branch) } }
                                        }
                                    }
                                    td {
                                        (badge(job))
                                        @if let Some(mark) = job.record_mark {
                                            @if let Some(words) = mark.words() {
                                                " " span class=(format!("record-mark {}", tone_class(mark.tone()))) { (words) }
                                            }
                                        }
                                    }
                                    // On a phone a row is a card: each figure says what it is.
                                    td { (time_ago(job.timeline.queued_at, now)) }
                                    td.number data-label="waited" { (job.waited(now).map_or("–".to_owned(), format::duration)) }
                                    td.number data-label="ran" { (job.ran(now).map_or("–".to_owned(), format::duration)) }
                                    td.number data-label="peak" { (job.metrics.memory_peak.map_or("–".to_owned(), format::bytes)) }
                                    td.number data-label="CPU" { (job.metrics.cpu_average.map_or("–".to_owned(), format::cores_used)) }
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

/// Where a moment sits across `timeline`'s window, from 0 to 1.
fn place(timeline: &SlotTimeline, at: u64) -> f64 {
    crate::chart::ratio(
        at.clamp(timeline.since, timeline.until) - timeline.since,
        timeline.until - timeline.since,
    )
}

/// A job's bar across a lane: its wait lighter, then its run, coloured by its outcome, a link to its page.
/// A bar's line: its job, repository, outcome and, when core words it, its end.
fn bar_label(bar: &TimelineBar) -> String {
    let outcome = bar.outcome();
    format!(
        "{}, {}: {}{}",
        bar.title(),
        bar.repository,
        outcome.label,
        bar.end_words()
            .map(|words| format!(", {words}"))
            .unwrap_or_default()
    )
}

/// The signal value that selects bar `index` of the timeline, for the readout under the lanes.
fn bar_key(index: usize) -> String {
    format!("b{index}")
}

/// A bar over when its job ran, and the wait before it. Pointed at, focused or tapped, it is named in the readout
/// under the lanes; a tap on a bar not named yet names it, the next opens it.
fn timeline_bar(timeline: &SlotTimeline, bar: &TimelineBar, index: usize, left: &str) -> Markup {
    let outcome = bar.outcome();
    let begins = begins(bar);
    // A bar that goes on reaches the window's end; one whose end is unknown is a mark where it began, and the lane's
    // gap after it says the rest is unknown.
    let ends = match bar.end {
        BarEnd::At { at } => at,
        BarEnd::Open => timeline.until,
        BarEnd::Unknown | BarEnd::Other => begins,
    };
    let from = place(timeline, begins);
    let to = place(timeline, ends.max(begins));
    let waits = bar
        .started_at
        .map(|started| place(timeline, bar.queued_at.min(started)));
    let label = bar_label(bar);
    let key = bar_key(index);
    html! {
        @if let Some(waits) = waits.filter(|waits| *waits < from) {
            span.tl-wait aria-hidden="true" style=(format!("--from: {waits:.4}; --to: {from:.4}")) {}
        }
        a class=(format!("tl-bar {}", outcome_kind_class(outcome.kind))) href=(format!("/jobs/{}", bar.job))
            style=(format!("--from: {from:.4}; --to: {to:.4}; --left: {left}")) title=(label) aria-label=(label) data-row
            data-class:on=(format!("$_bar == '{key}'"))
            data-on:pointerenter=(format!("evt.pointerType == 'mouse' && ($_bar = '{key}')"))
            // Keyboard focus names it; a tap's focus does not, so the tap's click names it rather than opening it.
            data-on:focus=(format!("el.matches(':focus-visible') && ($_bar = '{key}')"))
            data-on:click=(format!("$_bar != '{key}' && (evt.preventDefault(), $_bar = '{key}')")) {}
    }
}

/// When a bar begins on the timeline: when its job started, else when it was queued.
fn begins(bar: &TimelineBar) -> u64 {
    bar.started_at.unwrap_or(bar.queued_at)
}

/// The most a bar's share of a track can be and still be one of its least pixels: 3px of a 250px track, a phone's.
const BAR_MIN_SHARE: f64 = 0.012;
/// How many earlier bars may push a bar to the right.
const BAR_PUSHED_BY: usize = 12;

/// A lane's bars in the order they begin, each by its index in `bars` with the CSS `left` of its box. Every bar is at
/// least 3px wide and keeps 3px of its own: it starts no sooner than 3px past the bar before it, and no later than
/// leaves 3px for each bar after it before the track's end. The later bar is drawn over the earlier, so each keeps a
/// pixel a pointer or finger can reach, at any track width. CSS's `max()` and `min()` do the arithmetic in pixels.
fn laid_out(timeline: &SlotTimeline, bars: &[TimelineBar]) -> Vec<(usize, String)> {
    let mut order: Vec<usize> = (0..bars.len()).collect();
    order.sort_by_key(|index| bars.get(*index).map(begins));
    let starts: Vec<f64> = order
        .iter()
        .filter_map(|index| bars.get(*index))
        .map(|bar| place(timeline, begins(bar)))
        .collect();
    let count = starts.len();
    order
        .iter()
        .zip(&starts)
        .enumerate()
        .map(|(rank, (index, from))| {
            let mut terms = vec![format!("{:.3}%", from * 100.0)];
            for back in 1..=rank.min(BAR_PUSHED_BY) {
                let earlier = starts[rank - back];
                #[allow(clippy::cast_precision_loss, reason = "a count of a dozen bars")]
                let reach = earlier + back as f64 * BAR_MIN_SHARE;
                if reach > *from {
                    terms.push(format!("calc({:.3}% + {}px)", earlier * 100.0, back * 3));
                }
            }
            let left = if terms.len() > 1 {
                format!("max({})", terms.join(", "))
            } else {
                terms.swap_remove(0)
            };
            let room = count - rank;
            #[allow(clippy::cast_precision_loss, reason = "a count of bars in one lane")]
            let crowded = from + room as f64 * BAR_MIN_SHARE > 1.0;
            let left = if crowded {
                format!("min({left}, calc(100% - {}px))", room * 3)
            } else {
                left
            };
            (*index, left)
        })
        .collect()
}

/// The range the timeline draws: the last day unless a longer one is asked for, which core caps at what a timeline
/// shows.
fn timeline_range(filter: &Filter) -> JobRange {
    match filter.range.map(|range| range.0) {
        None | Some(JobRange::Day) => JobRange::Day,
        Some(JobRange::Week | JobRange::Month | JobRange::All) => JobRange::Week,
    }
}

/// The timeline lens: a lane per slot that ran in the time range, its jobs as bars over when each ran, what the
/// history cannot say hatched; then the jobs no runner here took.
fn timeline(history: &History) -> Markup {
    let range = timeline_range(&history.filter);
    let picker = html! {
        nav.lenses.ranges aria-label="Time range" {
            @for each in [JobRange::Day, JobRange::Week] {
                a href=(history.filter.link(|next| next.range = (each != JobRange::Day).then_some(Range(each))))
                    aria-current=[(each == range).then_some("page")] { (format::sentence(each.words())) }
            }
        }
    };
    let timeline = match &history.timeline {
        Some(Timeline::Drawn(timeline)) => timeline,
        Some(Timeline::Unsupported) => {
            return html! { (picker) p.notice.tone-warn role="status" { (format::sentence(&Feature::Timeline.unsupported())) "." } };
        }
        Some(Timeline::Refused(why)) => {
            return html! { (picker) p.notice.tone-warn role="status" { (format::sentence(why)) "." } };
        }
        None => return picker,
    };
    let lane_name = |lane: &Lane| format!("{} {}", lane.profile, lane.slot);
    // Each bar's index among all the timeline's, lane by lane, then the unassigned: the readout's key for it.
    let starts: Vec<usize> = timeline
        .lanes
        .iter()
        .scan(0, |next, lane| {
            let start = *next;
            *next += lane.bars.len();
            Some(start)
        })
        .collect();
    let assigned: usize = timeline.lanes.iter().map(|lane| lane.bars.len()).sum();
    let ticks = [0u64, 1, 2, 3, 4]
        .map(|quarter| timeline.since + (timeline.until - timeline.since) * quarter / 4);
    let tick = |at: u64| {
        if range == JobRange::Day {
            format::clock(at)
        } else {
            format::short_date(at)
        }
    };
    html! {
        (picker)
        section.timeline aria-label="Timeline" "data-signals:_bar__ifmissing"="''" {
            @if let Some(cut) = timeline.cut_line(history.now) {
                p.notice.tone-warn role="status" { (format::sentence(&cut)) "." }
            }
            ul.tl-key aria-label="Key" {
                li { span.tl-swatch.no-job {} (GapKind::NoJob.words()) }
                li { span.tl-swatch.unknown {} (GapKind::Unknown.words()) }
                li { span.tl-swatch.wait {} "waiting for a runner" }
            }
            @if timeline.lanes.is_empty() && timeline.unassigned.is_empty() {
                p.muted { (format::sentence(SlotTimeline::NONE)) "." }
            }
            ol.tl-lanes {
                @for (lane, start) in timeline.lanes.iter().zip(&starts) {
                    li class=(format!("tl-lane {}", profile_class(&history.status, &lane.profile))) {
                        span.tl-name { (lane_name(lane)) }
                        div.tl-track {
                            @for gap in lane.gaps(timeline).iter().filter(|gap| gap.kind == GapKind::Unknown) {
                                span.tl-gap aria-hidden="true" title=(gap.kind.words()) style=(format!("--from: {:.4}; --to: {:.4}", place(timeline, gap.span.from), place(timeline, gap.span.to))) {}
                            }
                            @for (index, left) in laid_out(timeline, &lane.bars) { (timeline_bar(timeline, &lane.bars[index], start + index, &left)) }
                        }
                    }
                }
                @if !timeline.unassigned.is_empty() {
                    li.tl-lane.p-none {
                        span.tl-name { (format::sentence(SlotTimeline::UNASSIGNED)) }
                        div.tl-track {
                            @for (index, left) in laid_out(timeline, &timeline.unassigned) { (timeline_bar(timeline, &timeline.unassigned[index], assigned + index, &left)) }
                        }
                    }
                }
            }
            div.tl-axis aria-hidden="true" {
                @for at in ticks { span { (tick(at)) } }
            }
            // The bar pointed at, focused or tapped, named with a link to its job: a bar is a few pixels wide.
            div.tl-readout aria-hidden="true" {
                @for (index, bar) in timeline.lanes.iter().flat_map(|lane| &lane.bars).chain(&timeline.unassigned).enumerate() {
                    p data-show=(format!("$_bar == '{}'", bar_key(index))) style="display: none" {
                        a href=(format!("/jobs/{}", bar.job)) tabindex="-1" { (bar_label(bar)) }
                        " " span.muted { (format::clock(bar.started_at.unwrap_or(bar.queued_at))) }
                    }
                }
            }
        }
    }
}
