//! `runnerctl`'s text of the shared parity fixtures states core's facts in core's words and colours them by core's
//! meaning, as the dashboard's and the TUI's tests check theirs (ADR 0062). Facts, not layout: each is found in its
//! row or line, not at a column.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests fail by panicking"
)]

use runnerd_core::api::JobPage;
use runnerd_fixtures::{Facts, NOW, TIMED_OUT, daemon, facts};

use super::{Paint, Tone, caches, job, jobs, outcome_tone, records, status};

const PLAIN: Paint = Paint { colour: false };
const COLOUR: Paint = Paint { colour: true };

/// The line of `text` whose first words are `first`.
fn line_of<'text>(text: &'text str, first: &[&str]) -> &'text str {
    text.lines()
        .find(|line| {
            line.split_whitespace()
                .take(first.len())
                .eq(first.iter().copied())
        })
        .unwrap_or_else(|| panic!("no line starts with {first:?}:\n{text}"))
}

/// Whether `text` writes `word` in `tone`, as the terminal colours it.
fn coloured(text: &str, word: &str, tone: Tone) -> bool {
    text.contains(&COLOUR.paint(tone, word))
}

fn fixtures() -> (runnerd_fixtures::Daemon, Facts) {
    let daemon = daemon();
    let facts = facts(&daemon);
    (daemon, facts)
}

#[test]
fn status_states_the_budget_each_slot_and_why_each_job_waits_as_core_does() {
    let (daemon, facts) = fixtures();
    let text = status(&daemon.status, NOW, PLAIN);
    let first = text.lines().next().unwrap();
    assert!(first.contains(&facts.budget), "{first}");
    assert!(first.contains(&facts.cpu), "{first}");
    let painted = status(&daemon.status, NOW, COLOUR);
    for slot in &facts.slots {
        let row = line_of(&text, &[&slot.profile, &slot.slot.to_string()]);
        assert!(
            row.split_whitespace().any(|word| word == slot.word),
            "{}: {row}",
            slot.word
        );
        assert!(
            coloured(&painted, slot.word, slot.tone.into()),
            "{} {} is {} in {:?}: {painted}",
            slot.profile,
            slot.slot,
            slot.word,
            slot.tone
        );
    }
    for wait in &facts.waits {
        let row = line_of(&text, &[&wait.job.to_string()]);
        assert!(row.ends_with(&wait.reason), "{}: {row}", wait.reason);
    }
}

#[test]
fn jobs_state_each_outcome_wait_and_run_time_as_core_does() {
    let (daemon, facts) = fixtures();
    let page = JobPage {
        jobs: daemon.finished(),
        next: None,
    };
    let text = jobs(&page, NOW, PLAIN);
    let painted = jobs(&page, NOW, COLOUR);
    for row in &facts.rows {
        let line = line_of(&text, &[&row.job.to_string()]);
        assert!(line.contains(&row.title), "{}: {line}", row.title);
        assert!(line.contains(&row.label), "{}: {line}", row.label);
        for figure in [&row.waited, &row.ran].into_iter().flatten() {
            assert!(line.contains(figure.as_str()), "{figure}: {line}");
        }
        assert!(
            coloured(&painted, &row.label, outcome_tone(row.kind)),
            "{} is {:?}: {painted}",
            row.label,
            row.kind
        );
    }
}

#[test]
fn a_job_states_its_outcome_steps_caps_and_build_cache_as_core_does() {
    let (daemon, facts) = fixtures();
    let shown = daemon.job(TIMED_OUT).unwrap();
    let text = job(shown, NOW, PLAIN);
    let head = text.lines().next().unwrap();
    assert!(head.contains(&facts.job.row.title), "{head}");
    assert!(
        coloured(
            &job(shown, NOW, COLOUR),
            &facts.job.row.label,
            outcome_tone(facts.job.row.kind)
        ),
        "{head}"
    );
    assert!(text.contains(&facts.job.limits), "{text}");
    assert!(text.contains(&facts.job.cpu_time), "{text}");
    for part in &facts.job.build_cache {
        assert!(text.contains(part.as_str()), "{part}: {text}");
    }
    for step in &facts.job.steps {
        let row = text
            .lines()
            .find(|line| line.contains(&step.name))
            .unwrap_or_else(|| panic!("no row for {}: {text}", step.name));
        let after = row.split(&step.name).nth(1).unwrap().trim();
        match &step.took {
            Some(took) => assert!(after.starts_with(took.as_str()), "{took}: {row}"),
            // Nothing claims how long a step took without its end.
            None => assert!(after.is_empty(), "{} shows no duration: {row}", step.name),
        }
    }
}

#[test]
fn records_and_caches_state_their_limits_as_core_does() {
    let (daemon, facts) = fixtures();
    let text = records(&daemon.records, NOW, PLAIN);
    let summary = text.lines().next().unwrap();
    assert!(summary.contains(&facts.records_usage), "{summary}");
    assert!(summary.contains(&facts.records_pressure), "{summary}");
    assert!(
        records(&daemon.records, NOW, COLOUR)
            .starts_with(&COLOUR.paint(facts.records_tone.into(), summary)),
        "the summary is in {:?}",
        facts.records_tone
    );
    let text = caches(&daemon.caches, NOW, PLAIN);
    assert!(text.contains(&facts.build_cache_week), "{text}");
}
