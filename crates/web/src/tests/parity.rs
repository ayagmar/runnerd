//! The dashboard's pages of the shared parity fixtures state core's facts in core's words, and give each outcome and
//! wait the class of core's meaning, as the TUI's and `runnerctl`'s tests check theirs (ADR 0062). Facts, not layout:
//! each is found in its row or element, read as text. The dashboard reads the wall clock, so only facts that do not
//! change with it are checked: no ages, and no waits or run times of jobs still going.

use runnerd_core::api::{Column, Section, View};
use runnerd_fixtures::{Daemon, Facts, JobRow, TIMED_OUT, daemon, facts};

use super::{Dashboard, fake};
use crate::format::sentence;
use crate::views::outcome_kind_class;
use crate::views::overview::reason_class;

/// The dashboard of a fake daemon answering with the fixtures, signed in, and the facts.
async fn dashboard() -> (Dashboard, String, Daemon, Facts) {
    dashboard_of(daemon()).await
}

/// The dashboard of a fake daemon answering with `daemon`, signed in, and its facts.
async fn dashboard_of(daemon: Daemon) -> (Dashboard, String, Daemon, Facts) {
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
async fn a_host_limit_past_memory_max_reads_as_core_says_it() {
    let (dashboard, cookie, daemon, facts) =
        dashboard_of(runnerd_fixtures::past_memory_max()).await;
    let html = dashboard.page(&cookie, "/").await.body;
    host_reads_as_core_says_it(&html, &daemon, &facts);
}

/// The host's figures, each its label and value, and the bar's free and held parts, each named by its line.
fn host_reads_as_core_says_it(html: &str, daemon: &Daemon, facts: &Facts) {
    let shown = text(html);
    let figures = daemon.status.budget.host_figures();
    assert!(!figures.is_empty(), "the fixture reads the host");
    for figure in figures {
        let line = format!("{} {}", figure.label, figure.value);
        assert!(shown.contains(&line), "{line}: {shown}");
    }
    for part in [&facts.free, &facts.host_part] {
        let label = format!(r#"aria-label="{}""#, maud::html! { (part) }.into_string());
        assert!(html.contains(&label), "{label}: {html}");
    }
}

#[tokio::test]
async fn a_host_credit_and_the_growth_a_held_job_waits_for_read_as_core_says_them() {
    let (dashboard, cookie, daemon, facts) = dashboard_of(runnerd_fixtures::host_credit()).await;
    let html = dashboard.page(&cookie, "/").await.body;
    host_reads_as_core_says_it(&html, &daemon, &facts);
    for wait in &facts.waits {
        assert!(html.contains(&wait.reason), "{}: {html}", wait.reason);
    }
}

#[tokio::test]
async fn the_overview_states_the_budget_health_slots_waits_and_latest_jobs_as_core_does() {
    let (dashboard, cookie, daemon, facts) = dashboard().await;
    let html = dashboard.page(&cookie, "/").await.body;
    let shown = text(&html);
    assert!(shown.contains(&facts.budget), "{shown}");
    assert!(shown.contains(&facts.cpu), "{shown}");
    host_reads_as_core_says_it(&html, &daemon, &facts);
    assert!(shown.contains(&facts.savings), "{}: {shown}", facts.savings);
    assert!(
        shown.contains(facts.stopping),
        "{}: {shown}",
        facts.stopping
    );
    assert!(shown.contains(facts.github), "{}: {shown}", facts.github);
    for figure in &facts.figures {
        let line = [
            Some(figure.label),
            Some(&figure.value),
            figure.of.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        assert!(shown.contains(&line), "{line}: {shown}");
        // The oldest wait follows the clock the dashboard reads, not the fixtures'.
        for note in figure
            .notes
            .iter()
            .filter(|_| figure.kind != runnerd_core::api::HealthKind::Waiting)
        {
            assert!(shown.contains(note), "{note}: {shown}");
        }
    }
    // Each lane: its line names it, its state word shows; a running one shows its job beside.
    for slot in &facts.slots {
        let label = format!(
            r#"aria-label="{}""#,
            maud::html! { (format!("{} {}: {}", slot.profile, slot.slot, slot.line)) }
                .into_string()
        );
        assert!(html.contains(&label), "{label}: {html}");
        let word = format!(r#"<span class="lane-state">{}</span>"#, slot.word);
        assert!(
            slot.word == "running" || html.contains(&word),
            "{word}: {html}"
        );
    }
    for wait in &facts.waits {
        let reason = format!(
            "<span class=\"reason {}\">{}</span>",
            reason_class(wait.kind),
            wait.reason
        );
        assert!(html.contains(&reason), "{reason}");
    }
    // The latest eight jobs, the two running ones among them: all but the oldest three finished ones.
    let latest = &facts.rows[..facts.rows.len() - 3];
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
    for total in &job.totals {
        let figure = format!("{} {}", total.label, total.value);
        assert!(shown.contains(&figure), "{figure}: {shown}");
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
async fn the_build_cache_states_its_week_and_tools_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let shown = text(&dashboard.page(&cookie, "/caches").await.body);
    assert!(shown.contains(&facts.build_cache_week), "{shown}");
    assert!(shown.contains(&facts.actions_cache), "{shown}");
    assert!(
        shown.contains(runnerd_core::api::BUILD_CACHE_TOOLS),
        "the build cache names the tools it serves: {shown}"
    );
}

#[tokio::test]
async fn records_state_their_limits_as_core_does() {
    let (dashboard, cookie, _, facts) = dashboard().await;
    let html = dashboard.page(&cookie, "/records").await.body;
    let shown = text(&html);
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
    let shown = text(&page);
    for day in &insights.host {
        assert!(
            shown.contains(&day.dated_line()),
            "{}: {shown}",
            day.dated_line()
        );
    }
}

/// `names` appear in `html`'s text in this order.
fn in_order(html: &str, names: &[&str]) {
    let shown = text(html);
    let mut at = 0;
    for name in names {
        let found = shown[at..]
            .find(name)
            .unwrap_or_else(|| panic!("{name} after {at}: {shown}"));
        at += found + name.len();
    }
}

/// Each view of several sections opens on its first, and every page of it links to each, named as core names them
/// (ADR 0132): no page left out of the regrouped navigation.
#[tokio::test]
async fn every_section_of_a_view_is_one_link_away_on_each_of_its_pages() {
    let (dashboard, cookie, _, _) = dashboard().await;
    let path = |section: Section| match section {
        Section::History => "/history",
        Section::Records => "/records",
        Section::Sizing => "/sizing",
        Section::Caches => "/caches",
        Section::Config => "/config",
    };
    let home = dashboard.page(&cookie, "/").await.body;
    for view in View::ALL {
        let sections = view.sections();
        if let Some(first) = sections.first() {
            assert!(
                home.contains(&format!("href=\"{}\"", path(*first))),
                "{} opens on {}",
                view.title(),
                first.title()
            );
        }
        if sections.len() < 2 {
            continue;
        }
        for shown in sections {
            let body = dashboard.page(&cookie, path(*shown)).await.body;
            let nav = &body[body
                .find("nav class=\"sections\"")
                .unwrap_or_else(|| panic!("{} has its sections", shown.title()))..];
            for section in sections {
                assert!(
                    nav.contains(&format!("href=\"{}\"", path(*section))),
                    "{} links {}",
                    shown.title(),
                    section.title()
                );
            }
            in_order(
                nav,
                &sections
                    .iter()
                    .map(|section| section.title())
                    .collect::<Vec<_>>(),
            );
        }
    }
}

#[tokio::test]
async fn the_pages_and_their_tables_are_named_as_core_names_them() {
    let (dashboard, cookie, _, _) = dashboard().await;
    let views: Vec<_> = View::ALL.iter().map(|view| view.title()).collect();
    in_order(&dashboard.page(&cookie, "/").await.body, &views);
    in_order(
        &dashboard.page(&cookie, "/history").await.body,
        &[
            Column::Job,
            Column::Outcome,
            Column::Queued,
            Column::Waited,
            Column::Ran,
            Column::MemoryPeak,
            Column::Cpu,
        ]
        .map(Column::title),
    );
    in_order(
        &dashboard.page(&cookie, "/records").await.body,
        &[Column::Job, Column::Made, Column::Size].map(Column::title),
    );
}

#[tokio::test]
async fn doctor_checks_that_are_not_current_follow_core_s_line_and_none_reads_as_passing() {
    for doctor in runnerd_fixtures::doctor_not_current() {
        let state = doctor.state.clone().unwrap();
        let mut fixtures = fake::Fixtures::new(runnerd_fixtures::NOW);
        fixtures.doctor = Some(doctor.clone());
        let dashboard = Dashboard::serving(&[], fixtures).await;
        let cookie = dashboard.sign_in().await;
        let before = crate::now();
        let page = dashboard.page(&cookie, "/config/doctor").await;
        let after = crate::now();
        let shown = text(&page.body);
        // The dashboard reads the wall clock, as the stale state's age does.
        let line = [before, after]
            .into_iter()
            .filter_map(|now| state.line(now))
            .find(|line| shown.contains(line.as_str()));
        assert!(line.is_some(), "core's line for {state:?}: {shown}");
        assert!(
            !page.body.contains("badge success") && !page.body.contains(r#"class="needs""#),
            "no check of {state:?} reads as current: {}",
            page.body
        );
        for check in &doctor.checks {
            assert_eq!(
                shown.contains(&check.detail),
                matches!(
                    state,
                    runnerd_core::api::DoctorState::Stale { .. }
                        | runnerd_core::api::DoctorState::Unknown
                ),
                "a stale run's checks show beside its line; a run not done or failed has none: {state:?}"
            );
        }
    }
}

#[tokio::test]
async fn attention_states_each_item_its_fix_and_the_rest_as_core_does() {
    let (dashboard, cookie, daemon, _) = dashboard().await;
    let doctor = runnerd_fixtures::doctor();
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.doctor = Some(doctor.clone());
        fixtures.insights = Some(runnerd_fixtures::insights(runnerd_fixtures::NOW));
    }
    let attention = runnerd_fixtures::attention(&daemon, &doctor);
    let html = dashboard.page(&cookie, "/").await.body;
    let region =
        &html[html.find(r#"id="attention""#).unwrap()..html.find(r#"id="budget-title""#).unwrap()];
    let shown = text(region);
    let facts = runnerd_fixtures::attention_facts(&attention, 4);
    assert!(facts.items.len() > 2, "the fixtures fold some on a phone");
    for (fact, item) in facts.items.iter().zip(&attention.items) {
        assert!(shown.contains(&fact.title), "{}: {shown}", fact.title);
        assert!(
            region.contains(&format!(
                "attention-item {}",
                crate::views::tone_class(fact.tone)
            )),
            "{} is {:?}: {region}",
            fact.title,
            fact.tone
        );
        let fix = match &item.fix {
            Some(runnerd_core::api::Fix::Command { command }) => Some(command.clone()),
            _ => fact.fix.clone(),
        };
        if let Some(fix) = fix {
            assert!(shown.contains(&fix), "{fix}: {shown}");
        }
    }
    for more in [attention.more(2), facts.more].into_iter().flatten() {
        assert!(shown.contains(&more), "{more}: {shown}");
    }
}

#[tokio::test]
async fn a_jobs_log_states_its_failure_evidence_and_colours_as_core_does() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    for facts in runnerd_fixtures::log_facts() {
        let html = dashboard
            .page(&cookie, &format!("/jobs/{}", facts.job))
            .await
            .body;
        let shown = text(&html);
        if let Some(headline) = &facts.headline {
            let at = html.find("failure-summary").unwrap_or(usize::MAX);
            assert!(
                at < html.find(r#"id="log-title""#).unwrap_or(0),
                "job {}: the failure comes before the log",
                facts.job
            );
            assert!(shown.contains(headline.as_str()), "{headline}: {shown}");
        }
        if let Some(evidence) = facts.evidence {
            let said = sentence(evidence);
            assert!(shown.contains(&said), "job {}: {said}: {shown}", facts.job);
        }
        // An `##[endgroup]` closes its fold and is not a line of its own.
        let drawn = facts
            .lines
            .iter()
            .filter(|line| line.kind != runnerd_core::api::LineKind::EndGroup);
        for line in drawn.filter(|_| facts.headline.is_some()) {
            let id = format!(r#"id="L{}""#, line.line + 1);
            assert!(html.contains(&id), "job {}: {id}", facts.job);
            for run in line.runs.iter().filter(|run| !run.text.trim().is_empty()) {
                let words = run.text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(shown.contains(&words), "{words}: {shown}");
            }
        }
    }
}

#[tokio::test]
async fn a_failed_job_s_page_opens_its_failed_step_and_shows_its_last_lines_as_core_does() {
    let dashboard = Dashboard::start().await;
    let cookie = dashboard.sign_in().await;
    // The second shares its failing step's second with the steps around it.
    for id in [
        runnerd_fixtures::LOG_FAILED,
        runnerd_fixtures::LOG_SAME_SECOND,
    ] {
        let logged = runnerd_fixtures::logged_jobs()
            .into_iter()
            .find(|logged| logged.job.summary.id == id)
            .unwrap();
        let failure = logged.outline.failure.clone().unwrap();
        let excerpt = failure.excerpt.clone().unwrap_or_default();
        assert!(
            failure.exit_code.is_some() && !excerpt.is_empty(),
            "job {id}: core knows the failed step's exit code and last lines"
        );
        let html = dashboard.page(&cookie, &format!("/jobs/{id}")).await.body;
        let summary =
            &html[html.find(r#"id="failure""#).unwrap()..html.find(r#"id="log-title""#).unwrap()];
        let shown = text(summary);
        for run in excerpt
            .iter()
            .flat_map(|line| &line.runs)
            .filter(|run| !run.text.trim().is_empty())
        {
            let words = run.text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(shown.contains(&words), "job {id}: {words}: {shown}");
        }
        assert!(
            html.contains(r#"<details class="log-step failed" open"#),
            "job {id}: the failed step's fold opens: {html}"
        );
    }
}

#[tokio::test]
async fn the_jobs_lenses_state_marks_gaps_ends_and_comparisons_as_core_does() {
    use runnerd_fixtures::{LENS_COMPARED, LENS_FIRST_RUN, LENS_ORPHAN};
    // The lenses' jobs at the dashboard's clock, so a week back from it reaches them.
    let dashboard = Dashboard::serving(&[], fake::Fixtures::new(crate::now())).await;
    let cookie = dashboard.sign_in().await;
    let facts = runnerd_fixtures::lens_facts();
    let list = dashboard.page(&cookie, "/history?range=30d").await.body;
    for mark in &facts.marks {
        let row = row_of(&list, mark.job);
        let shown = format!(
            r#"<span class="record-mark {}">{}</span>"#,
            crate::views::tone_class(mark.tone),
            mark.words
        );
        assert!(row.contains(&shown), "job {}: {shown}: {row}", mark.job);
    }
    let timeline = dashboard
        .page(&cookie, "/history?lens=timeline&range=7d")
        .await
        .body;
    for gap in facts
        .gaps
        .iter()
        .filter(|gap| gap.words != runnerd_core::api::GapKind::NoJob.words())
    {
        assert!(
            timeline.contains(&format!(r#"title="{}""#, gap.words)),
            "{}: {timeline}",
            gap.words
        );
    }
    for end in &facts.ends {
        let bar = &timeline[timeline
            .find(&format!(r#"href="/jobs/{}""#, end.job))
            .unwrap()..];
        let bar = &bar[..bar.find("</a>").unwrap()];
        assert!(
            bar.contains(end.words),
            "job {}: {}: {bar}",
            end.job,
            end.words
        );
    }
    let compared = text(
        &dashboard
            .page(&cookie, &format!("/jobs/{LENS_COMPARED}"))
            .await
            .body,
    );
    for delta in &facts.compared {
        for figure in [&delta.label, &delta.this, &delta.baseline, &delta.change] {
            assert!(compared.contains(figure.as_str()), "{figure}: {compared}");
        }
    }
    let first = text(
        &dashboard
            .page(&cookie, &format!("/jobs/{LENS_FIRST_RUN}"))
            .await
            .body,
    );
    assert!(
        first.contains(&sentence(&facts.first_run)),
        "{}: {first}",
        facts.first_run
    );
    let records = dashboard.page(&cookie, "/records").await.body;
    let at = records.find(&LENS_ORPHAN.to_string()).unwrap();
    let orphan = &records[records[..at].rfind("<tr").unwrap()..];
    let orphan = &orphan[..orphan.find("</tr>").unwrap()];
    assert!(
        orphan.contains(&sentence(runnerd_core::api::Records::history_gone()))
            && !orphan.contains(&format!(r#"href="/jobs/{LENS_ORPHAN}""#)),
        "a record whose history is gone says so and links nowhere: {orphan}"
    );
}

#[test]
fn a_job_actions_preview_and_answer_state_core_s_words() {
    use runnerd_core::api::JobAction;
    let fixtures = runnerd_fixtures::job_actions(&daemon());
    for preview in &fixtures.previews {
        let html = crate::views::job::action_preview(1, JobAction::Cancel, preview).into_string();
        let shown = text(&html);
        assert!(shown.contains(&preview.title), "{}: {shown}", preview.title);
        assert!(
            shown.contains(&sentence(&preview.scope)),
            "{}: {shown}",
            preview.scope
        );
        for note in &preview.notes {
            assert!(shown.contains(&sentence(note)), "{note}: {shown}");
        }
        assert_eq!(
            html.contains(&preview.confirm_label) && html.contains("payload: {confirm: "),
            preview.confirm.is_some(),
            "a preview without a confirmation offers nothing to send: {html}"
        );
    }
    for result in &fixtures.results {
        let fact = runnerd_fixtures::action_fact(result);
        let html = crate::views::notice(
            &sentence(&result.message),
            if result.succeeded() {
                crate::views::Tone::Done
            } else {
                crate::views::Tone::Failed
            },
        )
        .into_string();
        assert!(
            text(&html).contains(&sentence(&fact.text)),
            "{}: {html}",
            fact.text
        );
        assert_eq!(result.succeeded(), fact.succeeded);
    }
}

#[tokio::test]
async fn with_job_actions_off_a_job_says_why_and_offers_neither() {
    let (dashboard, cookie, daemon, _) = dashboard_of(runnerd_fixtures::job_actions_off()).await;
    let job = daemon.jobs[0].summary.id;
    let html = dashboard.page(&cookie, &format!("/jobs/{job}")).await.body;
    let why = daemon.status.job_actions.why_not().unwrap();
    assert!(text(&html).contains(&sentence(why)), "{why}: {html}");
    assert!(
        !html.contains("/cancel')") && !html.contains("/rerun"),
        "{html}"
    );
}
