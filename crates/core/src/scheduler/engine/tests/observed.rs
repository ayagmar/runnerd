//! A tick's job reads on the fakes (ADR 0180): what its listing's pages showed of a followed job stands for a read of
//! it when it is the whole answer, a job is read once a tick, and anything else is read as before.

use super::*;

/// Whether the listing's run pages show the jobs GitHub describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pages {
    /// They show each job as its read describes it.
    Shown,
    /// They show none: every job is read.
    Hidden,
}

/// The name of the runner `running_runner` puts in slot 1.
fn runner_one() -> String {
    identity("ci", ID_A).runner_name("app", 1, 1)
}

/// GitHub describes job `details.id` so, in a read and, if `pages` shows them, in its run's page.
fn describe(github: &FakeGithub, pages: Pages, details: &JobDetails) {
    let mut state = github.0.lock().unwrap();
    state.jobs.insert(details.id, details.clone());
    match pages {
        Pages::Shown => state.pages.insert(details.id, details.clone()),
        Pages::Hidden => state.pages.remove(&details.id),
    };
}

/// Job 1 of attempt 2 on `runner_one`, `status`, its first step done.
fn on_runner(status: &str, conclusion: Option<&str>) -> JobDetails {
    let mut details = JobDetails {
        run_attempt: 2,
        runner_name: Some(runner_one()),
        ..details(1, status, conclusion)
    };
    if status != "completed" {
        details.steps.truncate(1);
    }
    details
}

fn job_reads(github: &FakeGithub) -> usize {
    github.0.lock().unwrap().job_reads
}

/// What a lifecycle came to: the job's record, the key admission knew its runner's job by, and GitHub's requests.
#[derive(Debug)]
struct Lifecycle {
    job: api::Job,
    key: Option<crate::admission::Key>,
    job_reads: usize,
    listings: usize,
    requests: usize,
}

/// Job 1 on this machine's runner in slot 1: listed queued while GitHub lags, then running, then completed once its
/// container stopped, with GitHub's pages as `pages` says.
async fn lifecycle(pages: Pages) -> Lifecycle {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "@@job 1 1\n");
    github.0.lock().unwrap().queued = vec![QueuedJob {
        run_attempt: 2,
        ..job(1)
    }];
    describe(&github, pages, &on_runner("in_progress", None));
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    // The capture's report is in before the tick lists, as it is but right after a start.
    engine.take_found();
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().queued.clear();
    engine.age_running_reads(Duration::from_hours(1));
    engine.tick(Shutdown::No).await;
    let key = engine
        .occupants_mut()
        .next()
        .and_then(|occupant| occupant.key().cloned());

    describe(&github, pages, &on_runner("completed", Some("success")));
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 1, JobState::Completed).await;
    assert!(!engine.is_closing(1), "GitHub said job 1 completed");
    let state = github.0.lock().unwrap();
    Lifecycle {
        job,
        key,
        job_reads: state.job_reads,
        listings: state
            .calls
            .iter()
            .filter(|call| call.starts_with("queued "))
            .count(),
        requests: state.calls.len() + state.job_reads,
    }
}

/// The bugs it catches: an observation that records a job otherwise than its read (steps, attempt, conclusion,
/// times, workflow), names its runner's job otherwise for admission, or only moves the cost to more listings.
#[tokio::test]
async fn a_job_its_listing_shows_on_its_runner_is_recorded_alike_with_fewer_requests() {
    let read = lifecycle(Pages::Hidden).await;
    let shown = lifecycle(Pages::Shown).await;
    let facts = |lifecycle: &Lifecycle| {
        let summary = &lifecycle.job.summary;
        (
            summary.state,
            summary.conclusion.clone(),
            summary.run_attempt,
            summary.workflow.clone(),
            summary.timeline.started_at,
            summary.timeline.finished_at,
            lifecycle.job.steps.clone(),
        )
    };
    assert_eq!(facts(&shown), facts(&read), "the same record");
    assert_eq!(shown.key, read.key, "admission knows the job alike");
    assert!(read.key.is_some(), "admission named the job");
    assert_eq!(shown.listings, read.listings, "no listing more");
    assert!(read.job_reads >= 3, "read before: {}", read.job_reads);
    assert_eq!(shown.job_reads, 0, "the listing answered every read");
    assert_eq!(
        read.requests - shown.requests,
        read.job_reads,
        "every request saved is a job read"
    );
}

/// The bug it catches: a job another machine's runner took read again though the tick's listing just showed it.
#[tokio::test]
async fn a_job_another_machine_took_closes_from_the_listing_without_a_read() {
    let github = FakeGithub::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) =
        a_job_that_left_the_queue(&github, &FakeDocker::default(), &dir).await;
    let mut elsewhere = details(5, "completed", Some("success"));
    elsewhere.runner_name = Some("runnerd-7c3e5a19-skiff-jvm-2-1763000000".to_owned());
    describe(&github, Pages::Shown, &elsewhere);
    let before = job_reads(&github);
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 5, JobState::Elsewhere).await.summary;
    assert_eq!(job.outcome().label, "ran on skiff");
    assert_eq!(job_reads(&github), before, "no read");
    assert!(
        github
            .0
            .lock()
            .unwrap()
            .following
            .last()
            .is_some_and(|following| following.contains(&5)),
        "the listing followed job 5"
    );
}

/// The bugs it catches: a job the listing leaves out (its run ended or was left unread, or it was of an earlier
/// attempt) taken as completed, or not read at all; and the failed reads' clock of ADR 0109 changed by the listing.
#[tokio::test]
async fn a_job_the_listing_leaves_out_is_read_and_its_absence_closes_nothing() {
    let github = FakeGithub::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) =
        a_job_that_left_the_queue(&github, &FakeDocker::default(), &dir).await;
    // GitHub's pages show job 5 nowhere, and its read fails.
    github.0.lock().unwrap().failed_jobs.insert(5);
    tokio::time::pause();
    let before = job_reads(&github);
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github) - before, 1, "read, as before");
    assert_eq!(state_of(&handle, 5).await, JobState::Queued);
    tokio::time::advance(super::super::history::LOST_AFTER).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(
        state_of(&handle, 5).await,
        JobState::Elsewhere,
        "a quarter hour of failed reads gives up, as before"
    );
}

/// The bugs it catches: details from a listing begun before this machine's runner took its job, or that name no
/// runner yet or another one, taken for the runner's job, which would record it unstarted or run elsewhere.
#[tokio::test]
async fn a_listing_that_does_not_name_the_runner_does_not_stand_for_its_read() {
    for (seen, shown) in [
        (None, "names no runner yet"),
        (Some("runnerd-other"), "names another runner"),
    ] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        running_runner(&github, &docker, "@@job 1 1\n");
        describe(&github, Pages::Hidden, &on_runner("in_progress", None));
        github.0.lock().unwrap().pages.insert(
            1,
            JobDetails {
                runner_name: seen.map(str::to_owned),
                ..on_runner("in_progress", None)
            },
        );
        let (mut engine, handle, _dir) = engine(&github, &docker);
        engine.recover().await.unwrap();
        until_state(&handle, 1, JobState::Running).await;
        engine.take_found();
        engine.tick(Shutdown::No).await;
        assert_eq!(reads_of(&github, 1), 1, "read where the listing {shown}");
        assert!(
            engine
                .occupants_mut()
                .next()
                .is_some_and(|occupant| occupant.has_key()),
            "the read named the job where the listing {shown}"
        );
    }
}

/// The bugs it catches: what a tick's listing showed kept for a later tick, when its listing failed or a restart came,
/// which would record a job as it was minutes before.
#[tokio::test]
async fn what_a_listing_showed_stands_for_its_own_tick_alone() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "@@job 1 1\n");
    describe(&github, Pages::Shown, &on_runner("in_progress", None));
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    // The capture's report is in before the tick lists, as it is but right after a start.
    engine.take_found();
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github), 0, "the listing answered");

    // The next listing fails: the last one's pages stand for nothing now.
    github
        .0
        .lock()
        .unwrap()
        .failed_queued
        .insert("o/r".to_owned());
    engine.age_running_reads(Duration::from_hours(1));
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github), 1, "read while the listing fails");

    // It answers again.
    github.0.lock().unwrap().failed_queued.clear();
    engine.age_running_reads(Duration::from_hours(1));
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github), 1, "the listing answers again");

    // A restarted daemon whose first listing shows nothing reads the job.
    handle.store.flush().await;
    drop((engine, handle));
    github.0.lock().unwrap().pages.clear();
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    // The capture's report is in before the tick lists, as it is but right after a start.
    engine.take_found();
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github), 2, "read after the restart");
}

/// The bugs it catches: a reload that removes a repository leaving its jobs answered by nothing, or a paused one
/// whose listing stops answering for its jobs.
#[tokio::test]
async fn a_paused_repository_is_answered_by_its_listing_and_a_removed_one_by_reads() {
    let github = FakeGithub::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) =
        a_job_that_left_the_queue(&github, &FakeDocker::default(), &dir).await;
    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    let mut running = details(5, "in_progress", None);
    running.runner_name = Some("runnerd-7c3e5a19-skiff-jvm-2-1763000000".to_owned());
    describe(&github, Pages::Shown, &running);
    let before = job_reads(&github);
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 5, JobState::Elsewhere).await.summary;
    assert_eq!(job.outcome().label, "ran on skiff");
    assert_eq!(
        job_reads(&github),
        before,
        "the paused repository's listing answered"
    );

    // Job 6 left the queue of o/r, which a reload then removes: nothing lists it, and its job is read.
    let (mut engine, handle) = {
        drop((engine, handle));
        a_job_that_left_the_queue(&github, &FakeDocker::default(), &TestDir::new("engine")).await
    };
    let only_s = CONFIG.replace("name = \"o/r\"", "name = \"o/s\"");
    engine.reload(Config::parse(&only_s).unwrap(), Some(1 << 30), None);
    let before = job_reads(&github);
    engine.tick(Shutdown::No).await;
    assert_eq!(job_reads(&github) - before, 1, "read, o/r no longer listed");
    until_state(&handle, 5, JobState::Elsewhere).await;
}

/// The bugs it catches: a used-up rate limit taken for an answer, or the listing not standing for reads again once it
/// resets.
#[tokio::test]
async fn after_the_rate_limit_resets_the_listing_answers_again() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "@@job 1 1\n");
    describe(&github, Pages::Shown, &on_runner("in_progress", None));
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    // The capture's report is in before the tick lists, as it is but right after a start.
    engine.take_found();
    let until = std::time::SystemTime::now() + Duration::from_millis(200);
    {
        let mut state = github.0.lock().unwrap();
        state.rate_limited = Some(until);
        // A read fails while the limit is used up, and after it too: one would leave the job unnamed.
        state.limited_jobs.insert(1, until);
    }
    engine.tick(Shutdown::No).await;
    assert!(
        engine
            .occupants_mut()
            .next()
            .is_some_and(|occupant| !occupant.has_key()),
        "nothing answered while the limit was used up"
    );
    github.0.lock().unwrap().rate_limited = None;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let before = job_reads(&github);
    engine.tick(Shutdown::No).await;
    assert!(
        engine
            .occupants_mut()
            .next()
            .is_some_and(|occupant| occupant.has_key()),
        "the listing named the job once the limit reset"
    );
    assert_eq!(job_reads(&github), before, "without a read");
}

/// The bug it catches: a runner here whose capture has not reported its job, read twice in one tick (as a job that
/// left the queue, then for admission), or not claimed when the listing names it.
#[tokio::test]
async fn a_runner_here_the_listing_names_takes_its_job_without_a_read() {
    for pages in [Pages::Hidden, Pages::Shown] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        // An adopted runner whose capture has reported no job yet.
        running_runner(&github, &docker, "");
        let (mut engine, handle, _dir) = engine(&github, &docker);
        engine.memory_max = Some(1 << 30);
        engine.recover().await.unwrap();
        github.0.lock().unwrap().queued = vec![job(5)];
        engine.tick(Shutdown::No).await;
        until_state(&handle, 5, JobState::Queued).await;
        github.0.lock().unwrap().queued.clear();
        describe(
            &github,
            pages,
            &JobDetails {
                runner_name: Some(runner_one()),
                ..details(5, "in_progress", None)
            },
        );
        let before = reads_of(&github, 5);
        engine.tick(Shutdown::No).await;
        until_state(&handle, 5, JobState::Running).await;
        let reads = reads_of(&github, 5) - before;
        match pages {
            Pages::Hidden => assert_eq!(reads, 1, "read once, not again for admission"),
            Pages::Shown => assert_eq!(reads, 0, "the listing answered"),
        }
        assert!(
            engine
                .occupants_mut()
                .next()
                .is_some_and(|occupant| occupant.has_key()),
            "the runner's job is named for admission ({pages:?})"
        );
    }
}
