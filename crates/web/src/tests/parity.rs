//! The dashboard's pages of the shared parity fixtures state core's facts in core's words, and give each outcome and
//! wait the class of core's meaning, as the TUI's and `runnerctl`'s tests check theirs (ADR 0062). Facts, not layout:
//! each is found in its row or element, read as text. The dashboard reads the wall clock, so only facts that do not
//! change with it are checked: no ages, and no waits or run times of jobs still going.

use runnerd_fixtures::{Daemon, Facts, JobRow, TIMED_OUT, daemon, facts};

use super::{Dashboard, fake};
use crate::format::sentence;
use crate::views::outcome_kind_class;
use crate::views::overview::reason_class;

/// The dashboard of a fake daemon answering with the fixtures, signed in, and the facts.
async fn dashboard() -> (Dashboard, String, Daemon, Facts) {
    let daemon = daemon();
    let facts = facts(&daemon);
    let mut fixtures = fake::Fixtures::new(runnerd_fixtures::NOW);
    fixtures.status = daemon.status.clone();
    fixtures.jobs = daemon.jobs.clone();
    fixtures.samples.clear();
    fixtures.records = daemon.records.clone();
    fixtures.caches = daemon.caches.clone();
    fixtures.savings = Some(daemon.savings.clone());
    let dashboard = Dashboard::serving(&[], fixtures).await;
    let cookie = dashboard.sign_in().await;
    (dashboard, cookie, daemon, facts)
}

/// The text a page shows: each tag a space, entities decoded and spaces collapsed.
fn text(html: &str) -> String {
    let mut text = String::new();
    let mut tag = false;
    for c in html.chars() {
        match c {
            '<' => tag = true,
            '>' => {
                tag = false;
                text.push(' ');
            }
            c if !tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The markup of the row that links to `job`'s page, up to the end of its list item or table row.
fn row_of(html: &str, job: u64) -> &str {
    let from = html
        .find(&format!("href=\"/jobs/{job}\""))
        .unwrap_or_else(|| panic!("no row links to job {job}"));
    let rest = &html[from..];
    let to = ["</li>", "</tr>"]
        .iter()
        .filter_map(|end| rest.find(end))
        .min()
        .unwrap_or(rest.len());
    &rest[..to]
}

/// Each finished job's row has its outcome as a badge of its meaning, and its name; `figures` adds its wait and run
/// time.
fn assert_rows(html: &str, facts: &[JobRow], figures: bool) {
    for fact in facts {
        let row = row_of(html, fact.job);
        let badge = format!(
            "<span class=\"badge {}\">{}</span>",
            outcome_kind_class(fact.kind),
            fact.label
        );
        assert!(row.contains(&badge), "{badge}: {row}");
        assert!(text(row).contains(&fact.name), "{}: {row}", fact.name);
        if figures {
            for figure in [&fact.waited, &fact.ran].into_iter().flatten() {
                assert!(row.contains(&format!(">{figure}<")), "{figure}: {row}");
            }
        }
    }
}

#[tokio::test]
async fn the_overview_states_the_budget_health_slots_waits_and_latest_jobs_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let html = dashboard.page(&cookie, "/").await.body;
    let shown = text(&html);
    assert!(shown.contains(&facts.budget), "{shown}");
    assert!(shown.contains(&facts.cpu), "{shown}");
    for fact in [&facts.host, &facts.free, &facts.host_part] {
        assert!(shown.contains(fact.as_str()), "{fact}: {shown}");
    }
    assert!(shown.contains(&facts.savings), "{}: {shown}", facts.savings);
    assert!(
        shown.contains(facts.stopping),
        "{}: {shown}",
        facts.stopping
    );
    let health = &facts.health;
    for figure in [
        format!("Running {} of {} slots", health.running, health.slots),
        format!("{} runners starting", health.starting),
        format!("{} warm", health.warm),
        format!("Waiting {}", health.waiting),
    ] {
        assert!(shown.contains(&figure), "{figure}: {shown}");
    }
    // A running slot shows its job instead.
    for slot in facts.slots.iter().filter(|slot| slot.word != "running") {
        let line = sentence(&slot.line);
        assert!(shown.contains(&line), "{line}: {shown}");
    }
    for wait in &facts.waits {
        let reason = format!(
            "<span class=\"reason {}\">{}</span>",
            reason_class(wait.kind),
            wait.reason
        );
        assert!(html.contains(&reason), "{reason}");
    }
    // The latest eight jobs, the two running ones among them: all but the oldest two finished ones.
    let latest = &facts.rows[..facts.rows.len() - 2];
    assert_rows(&html, latest, false);
}

#[tokio::test]
async fn the_history_states_each_outcome_wait_and_run_time_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let html = dashboard.page(&cookie, "/history").await.body;
    // The job list, below the charts that link to days.
    let jobs = &html[html.find("<table").unwrap()..];
    assert_rows(jobs, &facts.rows, true);
}

#[tokio::test]
async fn a_job_states_its_outcome_steps_build_cache_and_failed_runs_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let html = dashboard
        .page(&cookie, &format!("/jobs/{TIMED_OUT}"))
        .await
        .body;
    let shown = text(&html);
    let job = &facts.job;
    let badge = format!(
        "<span class=\"badge {}\">{}</span>",
        outcome_kind_class(job.row.kind),
        job.row.label
    );
    assert!(html.contains(&badge), "{badge}");
    for label in &job.milestones {
        assert!(html.contains(&format!("<dt>{label}</dt>")), "{label}");
    }
    // Its caps show on its charts, which a job without samples has none of.
    for figure in [
        job.cpu_time.clone(),
        job.unseen.clone(),
        job.cpu_held.clone(),
        format!("Failed {} of {}", job.failed, job.finished),
    ]
    .iter()
    .chain(&job.build_cache)
    {
        assert!(shown.contains(figure.as_str()), "{figure}: {shown}");
    }
    for step in &job.steps {
        let name = format!("<span class=\"step-name\">{}</span>", step.name);
        let from = html
            .find(&name)
            .unwrap_or_else(|| panic!("no step {}", step.name));
        let item = &html[html[..from].rfind("<li").unwrap()..];
        let item = &item[..item.find("</li>").unwrap()];
        let class = format!("<li class=\"{}\">", outcome_kind_class(step.kind));
        assert!(
            item.starts_with(&class),
            "{} is {:?}: {item}",
            step.name,
            step.kind
        );
        // Without its end, a step's time is `–`.
        let took = step.took.as_deref().unwrap_or("–");
        assert!(text(item).ends_with(took), "{took}: {item}");
    }
}

#[tokio::test]
async fn records_and_the_build_cache_state_their_limits_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let html = dashboard.page(&cookie, "/caches").await.body;
    let shown = text(&html);
    assert!(shown.contains(&facts.build_cache_week), "{shown}");
    assert!(shown.contains(&facts.actions_cache), "{shown}");
    assert!(
        shown.contains(runnerd_core::api::BUILD_CACHE_TOOLS),
        "the build cache names the tools it serves: {shown}"
    );
    let summary = format!("{}; {}", facts.records_usage, facts.records_pressure);
    let at = html
        .find(&sentence(&summary))
        .unwrap_or_else(|| panic!("no summary {summary}: {shown}"));
    let element = &html[html[..at].rfind('<').unwrap()..at];
    assert_eq!(
        element.contains("over-limit"),
        facts.records_tone == runnerd_core::api::Tone::Warn,
        "the summary stands out exactly when core's tone warns: {element}"
    );
}

#[tokio::test]
async fn sizing_states_what_blocks_auto_sizing_as_core_does() {
    let (dashboard, cookie, _, _) = dashboard().await;
    let plan = runnerd_fixtures::sizing(runnerd_fixtures::NOW)
        .auto_plan
        .unwrap();
    dashboard.fake.fixtures.lock().unwrap().auto_plan = Some(plan.clone());
    let shown = text(&dashboard.page(&cookie, "/sizing").await.body);
    assert!(shown.contains(&plan.blocked_line().unwrap()), "{shown}");
}

#[tokio::test]
async fn sizing_states_each_profile_s_advice_and_need_as_core_does() {
    let (dashboard, cookie, _, _) = dashboard().await;
    let sizing = runnerd_fixtures::sizing(runnerd_fixtures::NOW);
    dashboard.fake.fixtures.lock().unwrap().sizing = Some(sizing.clone());
    let page = dashboard.page(&cookie, "/sizing").await.body;
    let shown = text(&page);
    for profile in &sizing.profiles {
        assert!(
            shown.contains(&sizing.advice_line(profile)),
            "{}: {shown}",
            profile.name
        );
        // A profile a bound holds shows what its jobs need, not the bound.
        let need = runnerd_core::config::ByteSize(sizing.need(profile).memory);
        assert!(
            shown.contains(&format!("needs {need}")),
            "{}: {shown}",
            profile.name
        );
        assert!(
            !page.contains(&format!("/config?resize={}", profile.name)),
            "{} has nothing to apply under auto sizing",
            profile.name
        );
    }
}

#[tokio::test]
async fn insights_state_each_insight_s_kind_and_sentence_as_core_does() {
    let (dashboard, cookie, _, _) = dashboard().await;
    let insights = runnerd_fixtures::insights(runnerd_fixtures::NOW);
    dashboard.fake.fixtures.lock().unwrap().insights = Some(insights.clone());
    let page = dashboard.page(&cookie, "/insights").await.body;
    for insight in &insights.insights {
        let at = page
            .find(&insight.sentence)
            .unwrap_or_else(|| panic!("no card says {:?}: {page}", insight.sentence));
        let card = &page[page[..at].rfind("<li").unwrap()..at];
        assert!(card.contains(insight.kind.label()), "{card}");
        assert!(
            card.contains(&format!(
                "{} {}",
                insight.severity.label(),
                insight.kind.word()
            )),
            "classed by its severity and kind: {card}"
        );
    }
}
