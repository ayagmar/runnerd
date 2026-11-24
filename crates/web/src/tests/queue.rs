//! The queue's page (ADR 0174): every job the status lists queued, with why it waits, reached from the overview
//! whenever the overview shows fewer than wait, and kept current as jobs leave.

use runnerd_core::api::{
    Event, EventEnvelope, JobState, ListingFailure, ListingProblem, QueueShown, Waiting,
};

use super::{Dashboard, NOW, read_until};

/// The first of the made-up queued jobs, all of halyard/storefront.
const FIRST: u64 = 130_000_000_000;

impl Dashboard {
    /// Replaces the queue with `count` jobs of halyard/storefront, oldest first, each waiting for a node slot; the
    /// history holds every one but the third, as a listing it has not recorded yet.
    fn queue_of(&self, count: u64) {
        let mut fixtures = self.fake.fixtures.lock().unwrap();
        let template = fixtures.status.queued[0].clone();
        let recorded = fixtures.jobs[0].clone();
        fixtures.status.queued = (0..count)
            .map(|n| {
                let mut job = template.clone();
                job.id = FIRST + n;
                job.name = format!("shard {n}");
                job.queued_at = NOW - 600 + n;
                job.url = Some(format!(
                    "https://github.com/halyard/storefront/actions/runs/1/job/{}",
                    job.id
                ));
                job.waiting = Some(Waiting::NoSlot {
                    profile: "node".to_owned(),
                });
                job
            })
            .collect();
        for n in (0..count).filter(|n| *n != 2) {
            let mut job = recorded.clone();
            job.summary.id = FIRST + n;
            job.summary.state = JobState::Queued;
            job.summary.runner = None;
            fixtures.jobs.push(job);
        }
        fixtures
            .jobs
            .sort_by_key(|job| std::cmp::Reverse(job.summary.id));
        let queued = fixtures.status.queued.len();
        for repository in &mut fixtures.status.repositories {
            repository.queued_jobs = if repository.name == "halyard/storefront" {
                queued
            } else {
                0
            };
        }
    }
}

/// The markup of `region`'s element, from its `id` to the start of `next`'s.
fn between<'html>(html: &'html str, region: &str, next: &str) -> &'html str {
    let from = html.find(&format!(r#"id="{region}""#)).unwrap();
    let to = html[from..].find(next).map_or(html.len(), |to| from + to);
    &html[from..to]
}

/// The bug it catches: past six waiting jobs, or three on a phone, the overview's link led to the history, which
/// says nothing of why a job waits and lists jobs the status leaves out; and every job past the sixth was out of
/// reach with its reason.
#[tokio::test]
async fn every_queued_job_is_on_the_queue_page_and_the_overview_links_there_once_it_shows_fewer() {
    for count in [0, 3, 4, 6, 7, 60] {
        let dashboard = Dashboard::start().await;
        dashboard.queue_of(count);
        let cookie = dashboard.sign_in().await;
        let overview = dashboard.page(&cookie, "/").await.body;
        let queue = between(&overview, "queue", r#"id="recent""#);
        let rows = queue.matches(r#"<li id="queued-"#).count();
        assert_eq!(
            rows,
            usize::try_from(count.min(6)).unwrap(),
            "{count}: the overview's rows"
        );
        let link = match count {
            0..=3 => None,
            4..=6 => Some(format!(
                r#"<a class="narrow-only" href="/queue">All {count} queued</a>"#
            )),
            _ => Some(format!(r#"<a href="/queue">All {count} queued</a>"#)),
        };
        match &link {
            Some(link) => assert!(queue.contains(link), "{count}: {link} in {queue}"),
            None => assert!(
                !queue.contains("href=\"/queue\""),
                "{count}: every job shows, so no link: {queue}"
            ),
        }
        assert!(
            !queue.contains("/history"),
            "{count}: the queue never links to the history: {queue}"
        );

        let page = dashboard.page(&cookie, "/queue").await;
        assert_eq!(page.status, axum::http::StatusCode::OK);
        let body = &page.body;
        assert!(
            body.contains(r#"<a href="/" data-key="1" aria-keyshortcuts="1" aria-current="page">"#),
            "{count}: the page is the overview's: {body}"
        );
        let waiting = between(body, "waiting", "</main>");
        assert_eq!(
            waiting.matches(r#"<li id="queued-"#).count(),
            usize::try_from(count).unwrap(),
            "{count}: every queued job is a row"
        );
        assert_eq!(
            waiting.matches("All 2 node slots are busy").count(),
            usize::try_from(count).unwrap(),
            "{count}: every row says why it waits"
        );
        if count == 0 {
            assert!(
                waiting.contains("Nothing waiting: every queued job has a runner."),
                "{waiting}"
            );
        } else {
            let oldest = waiting.find(&format!("queued-{FIRST}\"")).unwrap();
            let newest = waiting
                .find(&format!("queued-{}\"", FIRST + count - 1))
                .unwrap();
            assert!(oldest <= newest, "{count}: oldest first");
            assert!(
                body.contains(&format!(
                    r#"<span class="queue-count-value">{count}</span> waiting"#
                )),
                "{count}: the heading counts them: {body}"
            );
        }
        if count >= 3 {
            assert!(
                waiting.contains(&format!(r#"<a class="queue-main" href="https://github.com/halyard/storefront/actions/runs/1/job/{}" rel="noopener">"#, FIRST + 2)),
                "a job the history does not hold yet opens on GitHub, never a page that is not here: {waiting}"
            );
            assert!(
                waiting.contains(&format!(r#"<a class="queue-main" href="/jobs/{FIRST}">"#))
                    && waiting.contains(r#"aria-label="shard 0 on GitHub">GitHub</a>"#),
                "a job the history holds opens here, GitHub's page its second link: {waiting}"
            );
        }
    }
}

/// The bug it catches: a repository whose listing fails is left out of `Status::queued` while the history keeps its
/// jobs queued; a queue that does not say so reads as nothing of it waiting.
#[tokio::test]
async fn a_repository_whose_listing_fails_is_named_beside_the_queue_and_an_empty_queue_never_says_nothing_waits()
 {
    let dashboard = Dashboard::start().await;
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.status.queued.clear();
        for repository in &mut fixtures.status.repositories {
            repository.queued_jobs = 0;
            if repository.name == "halyard/storefront" {
                repository.queue = QueueShown::Unlisted;
                repository.listing = Some(ListingFailure {
                    since: NOW - 120,
                    problem: ListingProblem::Failed { status: 502 },
                });
            }
        }
    }
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/queue").await.body;
    let waiting = between(&page, "waiting", "</main>");
    assert!(
        waiting.contains(
            "Cannot list halyard/storefront's queued jobs: GitHub answers 502; none shows here"
        ) && waiting
            .contains(r#"href="/history?repository=halyard%2Fstorefront&amp;outcome=queued""#),
        "the repository left out is named, with its jobs the history keeps queued: {waiting}"
    );
    assert!(
        waiting.contains("No job of a listed repository waits.")
            && !waiting.contains("Nothing waiting"),
        "{waiting}"
    );
    let overview = dashboard.page(&cookie, "/").await.body;
    assert!(
        between(&overview, "queue", r#"id="recent""#)
            .contains("No job of a listed repository waits."),
        "the overview's empty queue says the same: {overview}"
    );
}

/// The bug it catches: rows keyed by a job's name merge or mislabel the jobs of a matrix, whose names may repeat.
#[tokio::test]
async fn jobs_of_one_name_are_each_their_own_row() {
    let dashboard = Dashboard::start().await;
    dashboard.queue_of(4);
    {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        for job in &mut fixtures.status.queued {
            job.name = "test".to_owned();
        }
    }
    let cookie = dashboard.sign_in().await;
    let page = dashboard.page(&cookie, "/queue").await.body;
    let waiting = between(&page, "waiting", "</main>");
    for n in 0..4 {
        assert_eq!(
            waiting
                .matches(&format!(r#"id="queued-{}""#, FIRST + n))
                .count(),
            1,
            "job {n} has one row of its own: {waiting}"
        );
    }
    assert_eq!(
        waiting
            .matches("<span class=\"job-name\"><span class=\"workflow\">E2E / </span>test</span>")
            .count(),
        4
    );
}

/// The bug it catches: a job that starts stays listed as waiting on an open page, or takes the other rows with it.
#[tokio::test]
async fn a_job_that_leaves_the_queue_leaves_the_open_page_and_the_rest_stay() {
    let dashboard = Dashboard::start().await;
    dashboard.queue_of(4);
    let cookie = dashboard.sign_in().await;
    let mut stream = dashboard.stream(&cookie, "/live/queue").await;
    let first = read_until(&mut stream, |text| text.contains("datastar-patch-signals")).await;
    assert!(
        first.contains(&format!(r#"id="queued-{FIRST}""#)),
        "{first}"
    );
    let started = {
        let mut fixtures = dashboard.fake.fixtures.lock().unwrap();
        fixtures.status.queued.remove(0);
        let job = fixtures
            .jobs
            .iter_mut()
            .find(|job| job.summary.id == FIRST)
            .unwrap();
        job.summary.state = JobState::Running;
        job.summary.clone()
    };
    dashboard
        .fake
        .events
        .send(EventEnvelope {
            id: None,
            at: NOW,
            event: Event::JobStarted { job: started },
        })
        .unwrap();
    let after = read_until(&mut stream, |text| text.contains(r#"id="waiting""#)).await;
    assert!(
        !after.contains(&format!(r#"id="queued-{FIRST}""#)),
        "the job that started is gone: {after}"
    );
    for n in 1..4 {
        assert!(
            after.contains(&format!(r#"id="queued-{}""#, FIRST + n)),
            "job {n} still waits: {after}"
        );
    }
    assert!(
        after.contains(r#"<span class="queue-count-value">3</span>"#),
        "the count follows: {after}"
    );
}
