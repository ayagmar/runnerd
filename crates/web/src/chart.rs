//! Charts as inline SVG, drawn on the server: time series (lines with optional areas and a limit), sparklines and
//! stacked bars. The SVG holds only shapes, stretched to the box with `preserveAspectRatio="none"`; every label is
//! HTML around it, so text keeps its size at any width. No data fetching here: callers pass the points.
//!
//! A time series is drawn in seconds since a fixed anchor, and a transform maps the chart's window onto the box, so
//! the path data stays put as time passes. A live chart follows the page's clock, `$shown.at`, and draws each
//! series in stretches of time, each a part of its region (`views::Parts`): a new sample changes only the latest
//! stretch, which is all its stream sends.

use std::fmt::Write as _;

use maud::{Markup, html};

use crate::views::Parts;

/// The SVG's coordinate box. Shapes are drawn in it and stretched to the element.
const WIDTH: f64 = 1000.0;
const HEIGHT: f64 = 1000.0;
/// A series with more points than this is reduced to the highest per stretch of time, so peaks stay.
const MAX_POINTS: usize = 500;
/// The points a live chart's stretch spans at most.
const STRETCH: u64 = 32;

/// `part / whole`, for shares shown as CSS lengths and weights.
#[allow(
    clippy::cast_precision_loss,
    reason = "shares of times and sizes shown at pixel precision"
)]
pub(crate) fn ratio(part: u64, whole: u64) -> f64 {
    part as f64 / whole.max(1) as f64
}

/// One series of a time chart.
#[derive(Debug)]
pub(crate) struct Series {
    /// For the legend and the summary read to screen readers.
    pub(crate) label: String,
    /// The CSS class giving its colour, such as `cpu` or `memory`.
    pub(crate) class: &'static str,
    /// `(time, value)`, oldest first.
    pub(crate) points: Vec<(u64, u64)>,
    /// Fill under the line.
    pub(crate) area: bool,
}

impl Series {
    /// A line without an area.
    pub(crate) fn line(label: &str, class: &'static str, points: Vec<(u64, u64)>) -> Series {
        Series {
            label: label.to_owned(),
            class,
            points,
            area: false,
        }
    }
}

/// A horizontal line across a chart, such as a cap, with its label.
#[derive(Debug)]
pub(crate) struct Limit {
    pub(crate) value: u64,
    pub(crate) label: String,
    /// The line's class beside `limit`, such as `reserved`; empty for a plain one.
    pub(crate) class: &'static str,
}

/// A chart of values over time.
#[derive(Debug)]
pub(crate) struct TimeChart {
    /// What it shows, for the figure's accessible name.
    pub(crate) title: String,
    pub(crate) start: u64,
    pub(crate) end: u64,
    /// Shows only the last this many seconds up to `end`, from `start` at the earliest.
    pub(crate) window: Option<u64>,
    /// A live chart's id, which names its parts. Its end follows the page's clock.
    pub(crate) live: Option<String>,
    pub(crate) series: Vec<Series>,
    /// Horizontal lines, such as a memory cap, each with its label.
    pub(crate) limits: Vec<Limit>,
    /// Writes a value on the y axis.
    pub(crate) value: fn(u64) -> String,
    /// Writes a time on the x axis.
    pub(crate) time: fn(u64) -> String,
    /// Points further apart than this many seconds are not joined.
    pub(crate) gap: u64,
}

#[allow(
    clippy::cast_precision_loss,
    reason = "coordinates in a 1000-unit box; values far below 2^52"
)]
fn y_of(value: u64, top: u64) -> f64 {
    HEIGHT - (value as f64 / top.max(1) as f64 * HEIGHT).clamp(0.0, HEIGHT)
}

/// A label's height in percent of a job chart's plot, with a gap: what two labels keep apart.
const LABEL: f64 = 16.0;

/// Where a line's label goes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    /// The height it hangs from, in percent from the top.
    at: f64,
    /// The line at that height.
    line: usize,
    /// Under `at` rather than above it.
    below: bool,
    /// How many labels above it sit between it and `at`.
    stack: u32,
}

/// Places the labels of lines at `ys`, in percent from the top, so that none overlaps another or leaves the plot:
/// the highest line's label (the first of equals) above it, each lower one's under its line, or where there is no
/// room under it, above its line, or stacked under the label above when that is too close.
fn place(ys: &[f64]) -> Vec<Placement> {
    let mut order: Vec<usize> = (0..ys.len()).collect();
    order.sort_by(|a, b| ys[*a].total_cmp(&ys[*b]));
    let mut placements: Vec<Placement> = ys
        .iter()
        .enumerate()
        .map(|(line, y)| Placement {
            at: *y,
            line,
            below: false,
            stack: 0,
        })
        .collect();
    for pair in order.windows(2) {
        let (upper, lower) = (pair[0], pair[1]);
        let above = placements[upper];
        if ys[lower] + LABEL <= 100.0 {
            placements[lower].below = true;
        } else if !above.below && ys[lower] - above.at < LABEL {
            // The upper label climbs one place and this one takes its spot, so they read in the lines' order.
            for placement in &mut placements {
                if !placement.below && placement.line == above.line {
                    placement.stack += 1;
                }
            }
            placements[lower] = above;
        }
    }
    placements
}

/// The time a chart shows, and how its series are placed in it.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// The first second shown, `from`, and the last, `end`.
    from: u64,
    end: u64,
    /// The time drawn at x = 0: a day's start or the chart's start, so the coordinates stay small and change once a
    /// day at most.
    anchor: u64,
}

impl Frame {
    fn new(start: u64, end: u64, window: Option<u64>) -> Frame {
        let from = window.map_or(start, |window| end.saturating_sub(window).max(start));
        Frame {
            from,
            end: end.max(from),
            anchor: (from - from % 86_400).max(start),
        }
    }

    fn span(self) -> u64 {
        (self.end - self.from).max(1)
    }

    /// The transform from the series' coordinates to the box.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a scale of seconds to a 1000-unit box"
    )]
    fn transform(self) -> String {
        format!(
            "scale({:.6},1) translate(-{},0)",
            WIDTH / self.span() as f64,
            self.from - self.anchor
        )
    }
}

/// How a live chart's first second follows the page's clock, as a Datastar expression.
fn live_from(start: u64, window: Option<u64>) -> String {
    match window {
        Some(window) => format!("Math.max($shown.at - {window}, {start})"),
        None => start.to_string(),
    }
}

/// The transform of a live chart, following `$shown.at`.
fn live_transform(start: u64, window: Option<u64>, anchor: u64) -> String {
    let from = live_from(start, window);
    format!(
        "'scale(' + {WIDTH} / Math.max(1, $shown.at - {from}) + ',1) translate(' + ({anchor} - {from}) + ',0)'"
    )
}

/// A point as drawn: its time, its height in the box, and whether a line joins it to the point before.
#[derive(Debug, Clone, Copy)]
struct Drawn {
    at: u64,
    y: i64,
    joined: bool,
}

/// The points to draw: split where the samples are more than `gap` apart, and with `bucket`, each part reduced to
/// its highest point per `bucket` seconds of time. The buckets are counted from time 0, so a bucket that is full
/// never changes.
fn drawn(points: &[(u64, u64)], top: u64, gap: u64, bucket: Option<u64>) -> Vec<Drawn> {
    let mut drawn: Vec<Drawn> = Vec::with_capacity(points.len().min(MAX_POINTS * 2));
    let mut last: Option<u64> = None;
    for &(at, value) in points {
        let joined = last.is_some_and(|last| at.saturating_sub(last) <= gap);
        last = Some(at);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "y is clamped to the 1000-unit box"
        )]
        let y = y_of(value, top).round() as i64;
        match (bucket, drawn.last_mut()) {
            (Some(bucket), Some(previous)) if joined && previous.at / bucket == at / bucket => {
                if y < previous.y {
                    *previous = Drawn {
                        at,
                        y,
                        joined: previous.joined,
                    };
                }
            }
            _ => drawn.push(Drawn { at, y, joined }),
        }
    }
    drawn
}

/// The line and the area of `points`, in seconds since `anchor`; a point not joined to the one before starts a new
/// line.
fn paths(points: &[Drawn], anchor: u64) -> (String, String) {
    let mut line = String::new();
    let mut area = String::new();
    let runs = points.chunk_by(|_, b| b.joined);
    for run in runs {
        let Some(first) = run.first() else { continue };
        #[allow(
            clippy::cast_possible_wrap,
            reason = "seconds since the anchor, which is at most a day before the window"
        )]
        let x = |point: &Drawn| point.at.wrapping_sub(anchor) as i64;
        let _ = write!(line, "M{},{}", x(first), first.y);
        let _ = write!(area, "M{},{HEIGHT}V{}", x(first), first.y);
        if run.len() > 1 {
            line.push('l');
            area.push('l');
        }
        for (i, pair) in run.windows(2).enumerate() {
            let separator = if i == 0 { "" } else { " " };
            let step = format!(
                "{separator}{},{}",
                x(&pair[1]) - x(&pair[0]),
                pair[1].y - pair[0].y
            );
            line.push_str(&step);
            area.push_str(&step);
        }
        let _ = write!(area, "V{HEIGHT}Z");
    }
    (line, area)
}

/// Whether a series is drawn as one path or in stretches of time, as a live chart's is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawing {
    Whole,
    Stretches,
}

/// A stretch of a series: its number (time / its length) on a live chart, and its line and area.
struct Stretch {
    number: Option<u64>,
    line: String,
    area: String,
}

/// A series' stretches: one for the whole of a still chart, one per `STRETCH` points' time for a live one, each
/// starting at the last point of the one before when they are joined. A live chart draws only the stretches that
/// reach into its frame.
fn stretches(
    points: &[(u64, u64)],
    top: u64,
    gap: u64,
    frame: Frame,
    drawing: Drawing,
) -> Vec<Stretch> {
    let bucket = (points.len() > MAX_POINTS)
        .then(|| frame.span().div_ceil(MAX_POINTS as u64).next_power_of_two());
    if drawing == Drawing::Whole {
        let (line, area) = paths(&drawn(points, top, gap, bucket), frame.anchor);
        return vec![Stretch {
            number: None,
            line,
            area,
        }];
    }
    let length = bucket.unwrap_or(1) * STRETCH;
    let first = frame.from / length;
    // From the last point before the first stretch, which the first stretch starts at when they are joined.
    let before = points.partition_point(|point| point.0 < first * length);
    let points = drawn(&points[before.saturating_sub(1)..], top, gap, bucket);
    let mut stretches = Vec::new();
    let mut previous: Option<Drawn> = None;
    for group in points.chunk_by(|a, b| a.at / length == b.at / length) {
        let number = group[0].at / length;
        if number >= first {
            let mut run = Vec::with_capacity(group.len() + 1);
            if let Some(previous) = previous.filter(|_| group[0].joined) {
                run.push(Drawn {
                    joined: false,
                    ..previous
                });
            }
            run.extend_from_slice(group);
            let (line, area) = paths(&run, frame.anchor);
            stretches.push(Stretch {
                number: Some(number),
                line,
                area,
            });
        }
        previous = group.last().copied();
    }
    stretches
}

/// A chart's series in its frame: an area group and a line group per series, with each stretch a part when `live`
/// names the chart.
fn series_group(
    series: &[(&Series, Vec<Stretch>)],
    frame: Frame,
    live: Option<(&str, &str)>,
    parts: &Parts,
) -> Markup {
    let path = |id: Option<String>, group: Option<&String>, d: &str| {
        if let (Some(id), Some(group)) = (id, group) {
            parts.stretch(&id, group, html! { path id=(id) d=(d) {} })
        } else {
            html! { path d=(d) {} }
        }
    };
    let transform = match live {
        Some(_) => parts.clocked(frame.transform()),
        None => frame.transform(),
    };
    html! {
        g transform=(transform) data-attr:transform=[live.map(|(_, transform)| transform)] {
            @for (index, (series, stretches)) in series.iter().enumerate() {
                @let id = live.map(|(id, _)| format!("{id}-{index}"));
                @if series.area {
                    @let group = id.as_ref().map(|id| format!("{id}a"));
                    g class=(format!("area {}", series.class)) id=[group.as_ref()] {
                        @for stretch in stretches {
                            (path(group.as_ref().zip(stretch.number).map(|(group, n)| format!("{group}{n}")), group.as_ref(), &stretch.area))
                        }
                    }
                }
                @let group = id.as_ref().map(|id| format!("{id}l"));
                g class=(format!("line {}", series.class)) id=[group.as_ref()] {
                    @for stretch in stretches {
                        (path(group.as_ref().zip(stretch.number).map(|(group, n)| format!("{group}{n}")), group.as_ref(), &stretch.line))
                    }
                }
            }
        }
    }
}

/// The powers a chart's y axis rounds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Base {
    /// Powers of 1024, for sizes.
    Binary,
    /// Powers of ten.
    Decimal,
}

/// A rounded-up top for the y axis: 1, 2 or 5 times a power of `base`.
/// The top leaves a tenth of headroom, so a limit at the maximum is not drawn on the edge.
pub(crate) fn nice_top(max: u64, base: Base) -> u64 {
    let max = (max + max / 10).max(1);
    let base: u64 = match base {
        Base::Binary => 1024,
        Base::Decimal => 10,
    };
    let mut unit = 1u64;
    while unit.saturating_mul(base) <= max {
        unit = unit.saturating_mul(base);
    }
    [
        1, 2, 4, 5, 8, 10, 20, 40, 50, 80, 100, 200, 400, 500, 800, 1000,
    ]
    .iter()
    .map(|step| unit.saturating_mul(*step))
    .find(|top| *top >= max)
    .unwrap_or(max)
}

/// A stretch of a chart's time, such as a job's step, drawn while a Datastar expression holds.
#[derive(Debug)]
pub(crate) struct Band {
    pub(crate) from: u64,
    /// Its end; none while it lasts, as a running step's does, so it reaches the chart's end.
    pub(crate) to: Option<u64>,
    /// When to draw it, such as `$_step == 3`.
    pub(crate) shown: String,
    /// What the chart shows over the stretch, such as its peak.
    pub(crate) label: String,
}

impl Band {
    /// The band over a chart from `origin` to `end`, its place given in the chart's units as `--from` and `--to`
    /// against the chart's `--span`. With `live`, the id of a live chart, a band that lasts follows the page's clock
    /// to the chart's end, and its label is a part.
    fn render(
        &self,
        index: usize,
        origin: u64,
        end: u64,
        live: Option<&str>,
        parts: &Parts,
    ) -> Markup {
        let from = self.from.clamp(origin, end) - origin;
        let to = self.to.unwrap_or(end).clamp(origin, end).max(origin + from) - origin;
        let label = html! { @if !self.label.is_empty() { (self.label) } };
        let label = match live {
            Some(id) if !self.label.is_empty() => {
                let id = format!("{id}-band{index}");
                parts.html(&id, html! { span.band-label #(id) { (label) } })
            }
            _ => html! { @if !self.label.is_empty() { span.band-label { (label) } } },
        };
        let follow = live
            .filter(|_| self.to.is_none())
            .map(|_| format!("$shown.at - {origin}"));
        let style = format!("--from: {from}; --to: {to}");
        let style = if follow.is_some() {
            parts.clocked(style)
        } else {
            style
        };
        html! {
            span class=(if from + to > end - origin { "band end" } else { "band" }) aria-hidden="true"
                data-class:on=(self.shown) style=(style) data-style:--to=[follow] {
                (label)
            }
        }
    }
}

impl TimeChart {
    /// The chart, with its y axis topped at `top`.
    pub(crate) fn render(&self, top: u64, parts: &Parts) -> Markup {
        self.render_with(top, &[], parts)
    }

    /// Each line with its height, in the box's units, and where its label goes.
    fn placed_limits(&self, top: u64) -> Vec<(&Limit, f64, Placement)> {
        let ys: Vec<f64> = self
            .limits
            .iter()
            .map(|limit| y_of(limit.value, top))
            .collect();
        let placements = place(&ys.iter().map(|y| y / HEIGHT * 100.0).collect::<Vec<_>>());
        self.limits
            .iter()
            .zip(ys)
            .zip(placements)
            .map(|((limit, y), placement)| (limit, y, placement))
            .collect()
    }

    /// The chart with bands over it, each drawn while its expression holds. A band needs a chart without a window,
    /// whose start stays put.
    pub(crate) fn render_with(&self, top: u64, bands: &[Band], parts: &Parts) -> Markup {
        let frame = Frame::new(self.start, self.end, self.window);
        let summary = self
            .series
            .iter()
            .map(|series| {
                let peak = series
                    .points
                    .iter()
                    .filter(|point| point.0 >= frame.from)
                    .map(|point| point.1)
                    .max();
                match peak {
                    Some(peak) => format!("{} up to {}", series.label, (self.value)(peak)),
                    None => format!("{}: no samples", series.label),
                }
            })
            .chain(self.limits.iter().map(|limit| limit.label.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        let live = self.live.as_deref().map(|id| {
            (
                id,
                live_transform(self.start, self.window, frame.anchor),
                format!(
                    "Math.max(1, $shown.at - {})",
                    live_from(self.start, self.window)
                ),
            )
        });
        let series: Vec<(&Series, Vec<Stretch>)> = self
            .series
            .iter()
            .map(|series| {
                let drawing = if live.is_some() {
                    Drawing::Stretches
                } else {
                    Drawing::Whole
                };
                let stretches = stretches(&series.points, top, self.gap, frame, drawing);
                (series, stretches)
            })
            .collect();
        let limits = self.placed_limits(top);
        let span_style = format!("--span: {}", frame.span());
        let span_style = if live.is_some() {
            parts.clocked(span_style)
        } else {
            span_style
        };
        let axis = |id: Option<String>| {
            html! {
                div.chart-x id=[id] aria-hidden="true" {
                    span { ((self.time)(frame.from)) }
                    span { ((self.time)(frame.end)) }
                }
            }
        };
        html! {
            figure.chart {
                div.chart-plot {
                    div.chart-y aria-hidden="true" {
                        span { (( self.value)(top)) }
                        span { "0" }
                    }
                    div.chart-area style=(span_style)
                        data-style:--span=[live.as_ref().map(|(_, _, span)| span)] {
                        svg viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none"
                            role="img" aria-label=(format!("{}: {summary}", self.title)) {
                            line.grid x1="0" x2=(WIDTH) y1=(HEIGHT / 2.0) y2=(HEIGHT / 2.0) vector-effect="non-scaling-stroke" {}
                            (series_group(&series, frame, live.as_ref().map(|(id, transform, _)| (*id, transform.as_str())), parts))
                            @for (limit, y, _) in &limits {
                                line class={ "limit " (limit.class) } x1="0" x2=(WIDTH) y1=(format!("{y:.0}")) y2=(format!("{y:.0}"))
                                    vector-effect="non-scaling-stroke" {}
                            }
                        }
                        @for (limit, _, placement) in &limits {
                            span class={ "limit-label " (limit.class) @if placement.below { " below" } }
                                style=(format!("--y: {:.1}%; --stack: {}", placement.at, placement.stack)) { (limit.label) }
                        }
                        @for (index, band) in bands.iter().enumerate() {
                            (band.render(index, frame.from, frame.end, live.as_ref().map(|(id, _, _)| *id), parts))
                        }
                    }
                }
                @match &live {
                    Some((id, _, _)) => {
                        @let id = format!("{id}-x");
                        (parts.html(&id, axis(Some(id.clone()))))
                    }
                    None => (axis(None)),
                }
            }
        }
    }
}

/// A small line with no axes, such as a slot's memory over the last `window` seconds since `start`. With `live`, it
/// is a live chart with that id.
pub(crate) struct Sparkline<'points> {
    pub(crate) points: &'points [(u64, u64)],
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) window: u64,
    pub(crate) top: u64,
    pub(crate) class: &'static str,
    pub(crate) live: Option<String>,
}

impl Sparkline<'_> {
    pub(crate) fn render(&self, parts: &Parts) -> Markup {
        let frame = Frame::new(self.start, self.end, Some(self.window));
        let series = Series {
            label: String::new(),
            class: self.class,
            points: Vec::new(),
            area: true,
        };
        let drawing = if self.live.is_some() {
            Drawing::Stretches
        } else {
            Drawing::Whole
        };
        let stretches = stretches(self.points, self.top, u64::MAX, frame, drawing);
        let transform = live_transform(self.start, Some(self.window), frame.anchor);
        let live = self.live.as_deref().map(|id| (id, transform.as_str()));
        html! {
            svg.sparkline viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none" aria-hidden="true" {
                (series_group(&[(&series, stretches)], frame, live, parts))
            }
        }
    }
}

/// One bar of a stacked bar chart: its label and a value per part.
#[derive(Debug)]
pub(crate) struct Bar {
    pub(crate) label: String,
    pub(crate) parts: Vec<u64>,
}

/// Stacked bars with a CSS class per part, such as jobs per day by conclusion.
pub(crate) fn stacked_bars(
    title: &str,
    classes: &[&str],
    bars: &[Bar],
    value: fn(u64) -> String,
    bands: &[Band],
) -> Markup {
    let top = nice_top(
        bars.iter()
            .map(|bar| bar.parts.iter().sum::<u64>())
            .max()
            .unwrap_or(0),
        Base::Decimal,
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "a few hundred bars in a 1000-unit box"
    )]
    let slot = WIDTH / bars.len().max(1) as f64;
    let mut rects = Vec::new();
    for (i, bar) in bars.iter().enumerate() {
        #[allow(clippy::cast_precision_loss, reason = "a few hundred bars")]
        let x = i as f64 * slot + slot * 0.12;
        let mut base = HEIGHT;
        for (part, class) in bar.parts.iter().zip(classes) {
            if *part == 0 {
                continue;
            }
            let height = HEIGHT - y_of(*part, top);
            base -= height;
            rects.push(html! {
                rect class=(class) x=(format!("{x:.1}")) y=(format!("{base:.1}"))
                    width=(format!("{:.1}", slot * 0.76)) height=(format!("{height:.1}")) {
                    title { (bar.label) ": " (value(*part)) }
                }
            });
        }
    }
    let first = bars
        .first()
        .map(|bar| bar.label.as_str())
        .unwrap_or_default();
    let last = bars
        .last()
        .map(|bar| bar.label.as_str())
        .unwrap_or_default();
    html! {
        figure.chart.bars {
            div.chart-plot {
                div.chart-y aria-hidden="true" {
                    span { (value(top)) }
                    span { "0" }
                }
                div.chart-area style=(format!("--span: {}", bars.len().max(1))) {
                    svg viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none" role="img"
                        aria-label=(title) {
                        line.grid x1="0" x2=(WIDTH) y1=(HEIGHT / 2.0) y2=(HEIGHT / 2.0) vector-effect="non-scaling-stroke" {}
                        @for rect in rects { (rect) }
                    }
                    // A band's times are bar indexes here: bar `i` spans `i..i + 1`.
                    @for (index, band) in bands.iter().enumerate() {
                        (band.render(index, 0, bars.len() as u64, None, &Parts::default()))
                    }
                }
            }
            div.chart-x aria-hidden="true" {
                span { (first) }
                span { (last) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::Part;

    /// The lines of a path written by `paths`, each as its points in seconds since the anchor and height.
    fn lines(d: &str) -> Vec<Vec<(i64, i64)>> {
        let mut lines: Vec<Vec<(i64, i64)>> = Vec::new();
        for line in d.split('M').filter(|line| !line.is_empty()) {
            let (first, rest) = line.split_once('l').unwrap_or((line, ""));
            let pair = |text: &str| {
                let (x, y) = text.split_once(',').unwrap();
                (x.parse::<i64>().unwrap(), y.parse::<i64>().unwrap())
            };
            let mut point = pair(first);
            let mut points = vec![point];
            for step in rest.split(' ').filter(|step| !step.is_empty()) {
                let (dx, dy) = pair(step);
                point = (point.0 + dx, point.1 + dy);
                points.push(point);
            }
            lines.push(points);
        }
        lines
    }

    /// A job's memory chart over `points`, sampled every 2 s, from 0 to `end`.
    fn memory_chart(points: Vec<(u64, u64)>, end: u64, live: bool) -> TimeChart {
        TimeChart {
            title: "Memory".to_owned(),
            start: 0,
            end,
            window: None,
            live: live.then(|| "memory".to_owned()),
            series: vec![Series::line("memory", "memory", points)],
            limits: vec![],
            value: |_| String::new(),
            time: |_| String::new(),
            gap: 6,
        }
    }

    /// The `d` of each line path in `html`.
    fn line_paths(html: &str) -> Vec<String> {
        let group = html.find(r#"class="line memory""#).unwrap();
        let end = group + html[group..].find("</g>").unwrap();
        html[group..end]
            .split(r#" d=""#)
            .skip(1)
            .map(|rest| rest[..rest.find('"').unwrap()].to_owned())
            .collect()
    }

    fn rendered(chart: &TimeChart) -> (String, Vec<Part>) {
        let parts = Parts::default();
        let html = chart.render(2 << 30, &parts).into_string();
        (html, parts.into_parts().0)
    }

    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a height in the 1000-unit box"
    )]
    fn a_long_series_drawn_with_fewer_points_keeps_its_peak() {
        let mut points: Vec<(u64, u64)> = (0..5000).map(|at| (at, 100)).collect();
        points[3333].1 = 9000;
        let (html, _) = rendered(&memory_chart(points, 4999, false));
        let drawn: Vec<(i64, i64)> = lines(&line_paths(&html)[0]).concat();
        assert!(
            drawn.len() <= MAX_POINTS + 1,
            "{} points drawn",
            drawn.len()
        );
        let top = drawn.iter().map(|point| point.1).min().unwrap();
        assert_eq!(top, y_of(9000, 2 << 30).round() as i64, "the peak is drawn");
    }

    /// The span each label of lines at `ys` covers, in percent from the top, as the stylesheet places it.
    fn label_spans(ys: &[f64]) -> Vec<(f64, f64)> {
        place(ys)
            .iter()
            .map(|placement| {
                let top = if placement.below {
                    placement.at
                } else {
                    placement.at - LABEL * f64::from(placement.stack + 1)
                };
                (top, top + LABEL)
            })
            .collect()
    }

    #[test]
    fn labels_of_lines_together_near_the_bottom_stack_apart_inside_the_plot() {
        // First, a runner reserving 1 CPU with its profile now at 1, on a chart topped at 40 cores: both at 97.5%.
        for ys in [
            [97.5, 97.5],
            [80.0, 95.0],
            [20.0, 22.0],
            [50.0, 50.0],
            [20.0, 97.5],
        ] {
            let spans = label_spans(&ys);
            assert!(
                spans[0].1 <= spans[1].0 || spans[1].1 <= spans[0].0,
                "labels of lines at {ys:?} overlap: {spans:?}"
            );
            assert!(
                spans
                    .iter()
                    .all(|(top, bottom)| *top >= 0.0 && *bottom <= 100.0),
                "labels of lines at {ys:?} leave the plot: {spans:?}"
            );
        }
    }

    #[test]
    fn a_two_hour_job_sampled_every_two_seconds_draws_one_connected_line() {
        let points: Vec<(u64, u64)> = (0..=3600).map(|i| (i * 2, 1 << 30)).collect();
        let (still, _) = rendered(&memory_chart(points.clone(), 7200, false));
        let still = line_paths(&still);
        assert_eq!(still.len(), 1);
        assert_eq!(lines(&still[0]).len(), 1, "{}", still[0]);
        // Live, the line comes in stretches, each starting where the one before ends.
        let (live, _) = rendered(&memory_chart(points, 7200, true));
        let stretches: Vec<Vec<(i64, i64)>> = line_paths(&live)
            .iter()
            .map(|d| lines(d).concat())
            .collect();
        assert!(stretches.len() > 10, "{} stretches", stretches.len());
        for pair in stretches.windows(2) {
            assert_eq!(pair[0].last(), pair[1].first(), "the stretches join");
        }
    }

    #[test]
    fn a_long_job_with_a_pause_in_its_samples_draws_two_lines() {
        let points: Vec<(u64, u64)> = (0..=3600)
            .filter(|i| !(1000..1100).contains(i))
            .map(|i| (i * 2, 1 << 30))
            .collect();
        let (html, _) = rendered(&memory_chart(points, 7200, false));
        assert_eq!(lines(&line_paths(&html)[0]).len(), 2);
    }

    #[test]
    fn a_new_sample_changes_only_the_latest_stretch_of_a_live_chart() {
        let mut points: Vec<(u64, u64)> = (0..300).map(|i| (i * 2, (i % 7) << 28)).collect();
        let (_, before) = rendered(&memory_chart(points.clone(), 598, true));
        points.push((600, 3 << 28));
        let (_, after) = rendered(&memory_chart(points, 601, true));
        let changed: Vec<&str> = after
            .iter()
            .filter(|part| part.group.is_some() && !before.contains(part))
            .map(|part| part.id.as_str())
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "one stretch of the line changes: {changed:?}"
        );
    }

    #[test]
    fn a_live_charts_paths_stay_put_as_time_passes() {
        let points: Vec<(u64, u64)> = (0..300).map(|i| (i * 2, (i % 7) << 28)).collect();
        let (_, now) = rendered(&memory_chart(points.clone(), 600, true));
        let (_, later) = rendered(&memory_chart(points, 601, true));
        let stretches = |parts: &[Part]| -> Vec<Part> {
            parts
                .iter()
                .filter(|part| part.group.is_some())
                .cloned()
                .collect()
        };
        assert_eq!(stretches(&now), stretches(&later));
    }

    #[test]
    fn a_running_steps_band_on_a_live_chart_reaches_the_charts_end() {
        let points: Vec<(u64, u64)> = (0..50).map(|i| (1000 + i * 2, 1 << 30)).collect();
        let mut chart = memory_chart(points, 1100, true);
        chart.start = 1000;
        let band = Band {
            from: 1040,
            to: None,
            shown: "$_step == 1".to_owned(),
            label: String::new(),
        };
        let html = chart
            .render_with(2 << 30, &[band], &Parts::default())
            .into_string();
        let attr = r#"data-style:--to=""#;
        let rest = &html[html.find(attr).expect("the band follows the clock") + attr.len()..];
        let follow = rest[..rest.find('"').unwrap()].replace("$shown.at", "1100");
        let (at, origin) = follow.split_once(" - ").expect("a difference");
        let to = at.parse::<u64>().unwrap() - origin.parse::<u64>().unwrap();
        assert_eq!(
            to, 100,
            "at the page's clock the band ends at the chart's end: {follow}"
        );
    }
}
