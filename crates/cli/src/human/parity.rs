//! `runnerctl`'s text of the shared parity fixtures states core's facts in core's words and colours them by core's
//! meaning, as the dashboard's and the TUI's tests check theirs (ADR 0062). Facts, not layout: each is found in its
//! row or line, not at a column.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests fail by panicking"
)]

use runnerd_core::api::{self, JobPage};
use runnerd_fixtures::{Facts, NOW, TIMED_OUT, daemon, facts};

use super::{
    Paint, Tone, caches, insights, job, jobs, outcome_tone, plan_line, records, savings, sizing,
    status,
};

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
    assert!(first.contains(&facts.host), "{first}");
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
    assert!(text.contains(&facts.job.unseen), "{text}");
    assert!(text.contains(&facts.job.cpu_held), "{text}");
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
    assert!(text.contains(&facts.actions_cache), "{text}");
    assert!(text.contains(api::BUILD_CACHE_TOOLS), "{text}");
}

#[test]
fn insights_open_with_what_the_job_minutes_would_have_cost_as_core_words_it() {
    let (daemon, facts) = fixtures();
    let text = savings(&daemon.savings);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], facts.savings);
    assert!(lines.contains(&facts.savings_30_days.as_str()), "{text}");
}

#[test]
fn insights_state_each_insight_s_severity_kind_and_sentence_as_core_does() {
    let shown = runnerd_fixtures::insights(NOW);
    let text = insights(&shown, PLAIN);
    for insight in &shown.insights {
        let line = text
            .lines()
            .find(|line| line.ends_with(&insight.sentence))
            .unwrap_or_else(|| panic!("no line says {:?}:\n{text}", insight.sentence));
        let words: Vec<&str> = line.split_whitespace().take(2).collect();
        assert_eq!(
            words,
            [insight.severity.label(), insight.kind.word()],
            "{line}"
        );
    }
}

#[test]
fn sizing_states_what_blocks_auto_sizing_and_what_it_does_next_with_each_profile_as_core_does() {
    let advice = runnerd_fixtures::sizing(NOW);
    let plan = advice.auto_plan.as_ref().unwrap();
    let text = sizing(&advice, NOW, PLAIN);
    assert!(text.contains(&plan.rules()), "{text}");
    assert!(text.contains(&plan.blocked_line().unwrap()), "{text}");
    for profile in &advice.profiles {
        let (what, _) = plan_line(&advice, plan, profile, NOW);
        assert!(text.contains(&what), "{}: {text}", profile.name);
    }
    // Held, with when the change that holds it was made and what it holds.
    assert!(
        text.contains("changed 2h 00m ago; the interval holds it until then"),
        "{text}"
    );
    assert!(text.contains(&plan.refusal("node").unwrap()), "{text}");
}
