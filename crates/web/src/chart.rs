//! Charts as inline SVG, drawn on the server: time series (lines with optional areas and a limit), sparklines and
//! stacked bars. The SVG holds only shapes, stretched to the box with `preserveAspectRatio="none"`; every label is
//! HTML around it, so text keeps its size at any width. No data fetching here: callers pass the points.

use std::fmt::Write as _;

use maud::{Markup, html};

/// The SVG's coordinate box. Shapes are drawn in it and stretched to the element.
const WIDTH: f64 = 1000.0;
const HEIGHT: f64 = 100.0;
/// More points than this are reduced to the highest per pixel column, so peaks stay.
const MAX_POINTS: usize = 500;

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

/// A chart of values over time.
#[derive(Debug)]
pub(crate) struct TimeChart {
    /// What it shows, for the figure's accessible name.
    pub(crate) title: String,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) series: Vec<Series>,
    /// A horizontal line, such as a memory cap, with its label.
    pub(crate) limit: Option<(u64, String)>,
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
fn x_of(at: u64, start: u64, end: u64) -> f64 {
    let span = end.saturating_sub(start).max(1) as f64;
    (at.saturating_sub(start) as f64 / span * WIDTH).clamp(0.0, WIDTH)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "coordinates in a 100-unit box; values far below 2^52"
)]
fn y_of(value: u64, top: u64) -> f64 {
    HEIGHT - (value as f64 / top.max(1) as f64 * HEIGHT).clamp(0.0, HEIGHT)
}

/// The highest value per pixel column, so a reduced series keeps its peaks.
fn reduce(points: &[(u64, u64)], start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut reduced: Vec<(u64, u64)> = Vec::with_capacity(MAX_POINTS);
    let mut column = None;
    for &(at, value) in points {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "x is clamped to 0..=1000, and MAX_POINTS is small"
        )]
        let this = (x_of(at, start, end) / WIDTH * MAX_POINTS as f64) as usize;
        match reduced.last_mut() {
            Some(last) if column == Some(this) => {
                if value > last.1 {
                    *last = (at, value);
                }
            }
            _ => {
                reduced.push((at, value));
                column = Some(this);
            }
        }
    }
    reduced
}

/// The line through the points, split where the samples are more than `gap` apart, and the area under each part.
/// A long series is reduced after the split, so the points kept, further apart than the samples, stay joined.
fn paths(points: &[(u64, u64)], chart: &TimeChart, top: u64) -> (String, String) {
    let mut line = String::new();
    let mut area = String::new();
    let long = points.len() > MAX_POINTS;
    let runs = points.chunk_by(|a, b| b.0.saturating_sub(a.0) <= chart.gap);
    for run in runs {
        let run = if long {
            reduce(run, chart.start, chart.end)
        } else {
            run.to_vec()
        };
        let run: Vec<(f64, f64)> = run
            .iter()
            .map(|&(at, value)| (x_of(at, chart.start, chart.end), y_of(value, top)))
            .collect();
        if let (Some(first), Some(last)) = (run.first(), run.last()) {
            for (i, (x, y)) in run.iter().enumerate() {
                let _ = write!(line, "{}{x:.1},{y:.2}", if i == 0 { "M" } else { "L" });
            }
            let _ = write!(area, "M{:.1},{HEIGHT}", first.0);
            for (x, y) in &run {
                let _ = write!(area, "L{x:.1},{y:.2}");
            }
            let _ = write!(area, "L{:.1},{HEIGHT}Z", last.0);
        }
    }
    (line, area)
}

/// A rounded-up top for the y axis: 1, 2 or 5 times a power of ten, or of 1024 for sizes.
/// The top leaves a tenth of headroom, so a limit at the maximum is not drawn on the edge.
pub(crate) fn nice_top(max: u64, binary: bool) -> u64 {
    let max = (max + max / 10).max(1);
    let base: u64 = if binary { 1024 } else { 10 };
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

impl TimeChart {
    /// The chart, with its y axis topped at `top`.
    pub(crate) fn render(&self, top: u64) -> Markup {
        let summary = self
            .series
            .iter()
            .map(|series| {
                let peak = series.points.iter().map(|point| point.1).max();
                match peak {
                    Some(peak) => format!("{} up to {}", series.label, (self.value)(peak)),
                    None => format!("{}: no samples", series.label),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let limit_y = self.limit.as_ref().map(|(limit, _)| y_of(*limit, top));
        html! {
            figure.chart {
                div.chart-plot {
                    div.chart-y aria-hidden="true" {
                        span { (( self.value)(top)) }
                        span { "0" }
                    }
                    div.chart-area {
                        svg viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none"
                            role="img" aria-label=(format!("{}: {summary}", self.title)) {
                            line.grid x1="0" x2=(WIDTH) y1="50" y2="50" vector-effect="non-scaling-stroke" {}
                            @for series in &self.series {
                                @let (line, area) = paths(&series.points, self, top);
                                @if series.area {
                                    path class=(format!("area {}", series.class)) d=(area) {}
                                }
                                path class=(format!("line {}", series.class)) d=(line)
                                    vector-effect="non-scaling-stroke" {}
                            }
                            @if let Some(y) = limit_y {
                                line.limit x1="0" x2=(WIDTH) y1=(format!("{y:.2}")) y2=(format!("{y:.2}"))
                                    vector-effect="non-scaling-stroke" {}
                            }
                        }
                        @if let (Some((_, label)), Some(y)) = (&self.limit, limit_y) {
                            span.limit-label style=(format!("--y: {:.1}%", y / HEIGHT * 100.0)) { (label) }
                        }
                    }
                }
                div.chart-x aria-hidden="true" {
                    span { ((self.time)(self.start)) }
                    span { ((self.time)(self.end)) }
                }
            }
        }
    }
}

/// A small line with no axes, such as a slot's memory over the last minutes.
pub(crate) fn sparkline(
    points: &[(u64, u64)],
    start: u64,
    end: u64,
    top: u64,
    class: &str,
) -> Markup {
    let chart = TimeChart {
        title: String::new(),
        start,
        end,
        series: Vec::new(),
        limit: None,
        value: |_| String::new(),
        time: |_| String::new(),
        gap: u64::MAX,
    };
    let (line, area) = paths(points, &chart, top);
    html! {
        svg.sparkline viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none" aria-hidden="true" {
            path class=(format!("area {class}")) d=(area) {}
            path class=(format!("line {class}")) d=(line) vector-effect="non-scaling-stroke" {}
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
) -> Markup {
    let top = nice_top(
        bars.iter()
            .map(|bar| bar.parts.iter().sum::<u64>())
            .max()
            .unwrap_or(0),
        false,
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
                rect class=(class) x=(format!("{x:.1}")) y=(format!("{base:.2}"))
                    width=(format!("{:.1}", slot * 0.76)) height=(format!("{height:.2}")) {
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
                div.chart-area {
                    svg viewBox=(format!("0 0 {WIDTH} {HEIGHT}")) preserveAspectRatio="none" role="img"
                        aria-label=(title) {
                        line.grid x1="0" x2=(WIDTH) y1="50" y2="50" vector-effect="non-scaling-stroke" {}
                        @for rect in rects { (rect) }
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

    #[test]
    fn a_long_series_drawn_with_fewer_points_keeps_its_peak() {
        let mut points: Vec<(u64, u64)> = (0..5000).map(|at| (at, 100)).collect();
        points[3333].1 = 9000;
        let reduced = reduce(&points, 0, 4999);
        assert!(reduced.len() <= MAX_POINTS + 1);
        assert_eq!(reduced.iter().map(|point| point.1).max(), Some(9000));
    }

    /// The `d` of the drawn line of a job's memory chart over `points`, sampled every 2 s.
    fn drawn_line(points: Vec<(u64, u64)>) -> String {
        let chart = TimeChart {
            title: "Memory".to_owned(),
            start: 0,
            end: 7200,
            series: vec![Series::line("memory", "memory", points)],
            limit: None,
            value: |_| String::new(),
            time: |_| String::new(),
            gap: 6,
        };
        let html = chart.render(2 << 30).into_string();
        let from =
            html.find(r#"class="line memory" d=""#).unwrap() + r#"class="line memory" d=""#.len();
        html[from..from + html[from..].find('"').unwrap()].to_owned()
    }

    #[test]
    fn a_two_hour_job_sampled_every_two_seconds_draws_one_connected_line() {
        let line = drawn_line((0..=3600).map(|i| (i * 2, 1 << 30)).collect());
        assert_eq!(line.matches('M').count(), 1, "{line}");
        assert!(line.matches('L').count() > 100, "{line}");
    }

    #[test]
    fn a_long_job_with_a_pause_in_its_samples_draws_two_lines() {
        let line = drawn_line(
            (0..=3600)
                .filter(|i| !(1000..1100).contains(i))
                .map(|i| (i * 2, 1 << 30))
                .collect(),
        );
        assert_eq!(line.matches('M').count(), 2, "{line}");
    }
}
