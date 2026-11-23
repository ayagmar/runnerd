//! A sleep of the host on the fakes (ADR 0159): the clock pair moves as Linux moves it, the wall clock and the time
//! suspended together, and a step of the wall clock moves only the first.

use super::*;
use crate::scheduler::engine::clock::Clock;

/// Job 1 runs in slot 1, an idle runner started just now waits in slot 2, and the host was sampled: an engine on a
/// stopped clock, after its first tick.
async fn busy_and_idle_ticked(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle, Clock) {
    let (mut engine, handle) = stop_fixture(github, docker, dir);
    let clock = Clock::stopped();
    engine.clock = clock.clone();
    sample_host(&handle, &clock);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        removed(&docker.calls()),
        Vec::<&String>::new(),
        "nothing goes before the sleep"
    );
    (engine, handle, clock)
}

/// The busy-and-idle fixture of `stop`, on `CONFIG` with three slots.
fn stop_fixture(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    running_runner(github, docker, "@@job 1 1\n");
    let idle = identity("ci", ID_A).runner_name("app", 2, 1);
    {
        let mut state = github.0.lock().unwrap();
        state.jobs.insert(1, details(1, "in_progress", None));
        state.runners.push(runner(8, &idle, false));
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.containers.push(runner_container(2, 8, true));
        state
            .container_captures
            .insert("container-2".to_owned(), String::new());
        state
            .started
            .insert("container-2".to_owned(), crate::time::now_ms());
    }
    // A third slot is free: what keeps a runner from starting there is the wake's hold.
    engine_with(
        github,
        docker,
        dir,
        &CONFIG.replace("slots = 2", "slots = 3"),
    )
}

/// A host sample taken now, by `clock`.
fn sample_host(handle: &Handle, clock: &Clock) {
    handle.store.host_sample(api::HostSample {
        at: clock.now_ms() / 1000 + 1,
        cpu: 0,
        memory: 1 << 30,
    });
}

/// The daemon's sessions in `dir`'s history.
async fn sessions(handle: &Handle, dir: &TestDir) -> usize {
    handle.store.flush().await;
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap()
}

// Catches H4 and M6 of the audit: an idle runner whose session GitHub ended while the host slept counted as covering a
// queued job, a runner started on readings from before the sleep, and the job running across it read as the code's
// failure.
#[tokio::test]
async fn after_a_sleep_the_idle_runners_go_and_nothing_starts_until_the_host_is_sampled_again() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-slept"),
    );
    let (mut engine, handle, clock) = busy_and_idle_ticked(&github, &docker, &dir).await;
    clock.sleep(Duration::from_hours(1));
    // GitHub queued a job while the host slept.
    github.0.lock().unwrap().queued = vec![job(2)];
    engine.tick(Shutdown::No).await;
    assert_eq!(
        removed(&docker.calls()),
        ["remove container-2"],
        "the idle runner from before the sleep goes; the busy one stays"
    );
    assert!(
        registered(&github.calls()).is_empty(),
        "nothing starts on the readings from before the sleep"
    );
    assert_eq!(sessions(&handle, &dir).await, 2, "the sleep ends a session");
    sample_host(&handle, &clock);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "a runner starts for job 2 once the host is sampled after the wake"
    );
    // Job 1's runner lost its session while the host slept: GitHub fails it.
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("failure")));
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 1, JobState::Completed).await.summary;
    assert!(job.slept, "job 1 ran while the host slept");
    assert_eq!(job.outcome().label, "host slept");
}

// Catches H3 of the audit: a wall clock stepped forward, as a time daemon does on a laptop that booted with a wrong
// one, read as hours of sleep that split the session and stop the runners.
#[tokio::test]
async fn a_step_of_the_wall_clock_alone_is_no_sleep() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-stepped"),
    );
    let (mut engine, handle, clock) = busy_and_idle_ticked(&github, &docker, &dir).await;
    clock.advance(Duration::from_hours(3));
    sample_host(&handle, &clock);
    github.0.lock().unwrap().queued = vec![job(2)];
    engine.tick(Shutdown::No).await;
    assert_eq!(
        removed(&docker.calls()),
        Vec::<&String>::new(),
        "the idle runner takes job 2"
    );
    assert_eq!(sessions(&handle, &dir).await, 1, "one session goes on");
}

// Catches a wake that holds starts for good on a host whose slice the sampler never read, and a sleep below the
// threshold handled as one.
#[tokio::test]
async fn a_short_pause_changes_nothing_and_a_host_never_sampled_settles_on_a_listing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-unsampled"),
    );
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    let clock = Clock::stopped();
    engine.clock = clock.clone();
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    clock.sleep(Duration::from_secs(20));
    engine.tick(Shutdown::No).await;
    assert_eq!(
        sessions(&handle, &dir).await,
        1,
        "a pause of 20s is no sleep"
    );
    clock.sleep(Duration::from_hours(2));
    github.0.lock().unwrap().queued = vec![job(2)];
    engine.tick(Shutdown::No).await;
    assert_eq!(sessions(&handle, &dir).await, 2, "two hours are");
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "with no sample before the sleep, the listing is all it waits for"
    );
}

// Catches a wake that settles on a host sample alone while every listing fails, so a pass starts runners for what
// GitHub listed before the sleep.
#[tokio::test]
async fn after_a_sleep_nothing_starts_until_a_listing_answers() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-unlisted"),
    );
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    let clock = Clock::stopped();
    engine.clock = clock.clone();
    sample_host(&handle, &clock);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    clock.sleep(Duration::from_hours(1));
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(2)];
        state.failed_queued.insert("o/r".to_owned());
    }
    engine.tick(Shutdown::No).await;
    sample_host(&handle, &clock);
    assert_eq!(
        engine.refill().await,
        0,
        "a pass starts nothing before a listing answered"
    );
    engine.tick(Shutdown::No).await;
    assert!(
        registered(&github.calls()).is_empty(),
        "nothing starts while the listings fail"
    );
    github.0.lock().unwrap().failed_queued.clear();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "the first listing that answers settles it"
    );
}

/// Whether the history marks `job` as having run across a sleep.
async fn marked_slept(handle: &Handle, dir: &TestDir, job: u64) -> bool {
    handle.store.flush().await;
    rusqlite::Connection::open(dir.0.join("history.db"))
        .unwrap()
        .query_row(
            "SELECT slept_from IS NOT NULL FROM jobs WHERE id = ?1",
            [job],
            |row| row.get(0),
        )
        .unwrap()
}

// Catches review 1's scenario A: a runner that takes its job seconds after the wake, before any tick. Its take is the
// first the engine hears of the sleep; recorded first, the job reads as running across a sleep it only queued across,
// and the store's downtime finds it running and leaves its wait whole.
#[tokio::test]
async fn a_take_reported_on_waking_is_recorded_after_the_sleep() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-take-on-wake"),
    );
    let (mut engine, handle, clock) = busy_and_idle_ticked(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![job(2)];
    sample_host(&handle, &clock);
    engine.tick(Shutdown::No).await;
    clock.sleep(Duration::from_hours(1));
    let idle = identity("ci", ID_A).runner_name("app", 2, 1);
    engine.note_found(&crate::scheduler::engine::history::Found::named(idle, 2));
    assert!(
        !marked_slept(&handle, &dir, 2).await,
        "job 2 started after the sleep"
    );
    assert!(marked_slept(&handle, &dir, 1).await, "job 1 ran across it");
}

// Catches review 1's scenario B: the host sleeps while a tick's starts run, and a registration on the connection the
// sleep broke fails. Counted, it holds the pool back for a failure that says nothing of it.
#[tokio::test]
async fn a_registration_that_fails_after_a_sleep_in_the_starts_holds_no_pool() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-sleep-in-start"),
    );
    let config = CONFIG.replace("slots = 2", "slots = 3");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    let clock = Clock::stopped();
    engine.clock = clock.clone();
    sample_host(&handle, &clock);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        for id in 1..=3 {
            state.queued.push(job(id));
            state.queued_on.insert(id, "o/r".to_owned());
        }
    }
    // The host sleeps as the first registration answers; the next ones fail on the broken connection.
    let asleep = clock.clone();
    github.0.lock().unwrap().on_register = Some(Box::new(move |state| {
        asleep.sleep(Duration::from_hours(1));
        state.fail_registrations = 2;
    }));
    engine.tick(Shutdown::No).await;
    assert_eq!(
        engine
            .pool_hold("o/r", "app")
            .and_then(|hold| hold.held)
            .map(|held| held.cause),
        None,
        "no failure after the sleep holds the pool: {:?}",
        github.calls()
    );
}

// Catches review 1's finding 4: a restart between a wake and the stop of a runner that lived through the sleep adopts
// that runner as one ready for a job, though GitHub may have ended its session.
#[tokio::test]
async fn after_a_restart_an_idle_runner_that_lived_through_the_last_sleep_still_goes() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-slept-restart"),
    );
    let (engine, handle) = stop_fixture(&github, &docker, &dir);
    // The daemon saw the host sleep after the idle runner started, and stopped before it stopped that runner.
    let from = crate::time::now_ms() + 1000;
    handle.store.downtime(from, from + 3_600_000);
    handle.store.flush().await;
    drop((engine, handle));
    let (mut engine, _handle) = engine_with(
        &github,
        &docker,
        &dir,
        &CONFIG.replace("slots = 2", "slots = 3"),
    );
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        removed(&docker.calls()),
        ["remove container-2"],
        "the idle runner from before the sleep goes; the busy one stays"
    );
}
