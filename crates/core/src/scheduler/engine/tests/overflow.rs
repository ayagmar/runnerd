//! An overflow host on the fakes (ADR 0162): one engine, and a fake GitHub whose listings show another machine's
//! runnerd runners and take queued jobs away as that machine would: a cold queue, a warm peer, a peer asleep, a
//! primary that is full, a takeover, a restart in the middle and a wake.

use super::*;
use crate::scheduler::engine::clock::Clock;
use crate::time;

/// `CONFIG` on an overflow host.
fn overflow_config() -> String {
    CONFIG.replace(
        "admission = \"caps\"\n",
        "admission = \"caps\"\nrole = \"overflow\"\n",
    )
}

/// An engine on `config` in `dir`, on a stopped clock, recovered.
async fn on_clock(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
    clock: &Clock,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let (mut engine, handle) = engine_with(github, docker, dir, config);
    engine.clock = clock.clone();
    engine.recover().await.unwrap();
    (engine, handle)
}

/// An overflow host's engine, on a stopped clock.
async fn overflow(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle, Clock) {
    let clock = Clock::stopped();
    let (engine, handle) = on_clock(github, docker, dir, &overflow_config(), &clock).await;
    (engine, handle, clock)
}

/// Job `id` of `o/r`, which GitHub queued at `at` (ms).
fn queued_at(id: u64, at: u64) -> QueuedJob {
    QueuedJob {
        created_at: at,
        ..job(id)
    }
}

/// A runner the other machine, `desk`, registered on `o/r`.
fn peer(id: u64, busy: bool) -> Runner {
    runner(id, &format!("runnerd-0123abcd-desk-app-1-{id}"), busy)
}

/// The other machine takes job `id`: GitHub no longer lists it queued, and lists that machine's runner busy.
fn peer_takes(github: &FakeGithub, id: u64) {
    let mut state = github.0.lock().unwrap();
    state.queued.retain(|job| job.id != id);
    state.runners.push(peer(900 + id, true));
}

/// A tick `by` after the last, on `clock`.
async fn tick_after(engine: &mut Engine<FakeGithub, FakeDocker>, clock: &Clock, by: u64) {
    clock.advance(Duration::from_secs(by));
    engine.tick(Shutdown::No).await;
    engine.publish();
}

fn registrations(github: &FakeGithub) -> usize {
    registered(&github.calls()).len()
}

// Catches an overflow host that starts a runner for a job the moment it is listed, beside the primary's.
#[tokio::test]
async fn a_cold_queue_gets_a_runner_overflow_after_from_when_github_queued_the_job() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-cold"),
    );
    let (mut engine, handle, clock) = overflow(&github, &docker, &dir).await;
    let queued = clock.now_ms();
    github.0.lock().unwrap().queued = vec![queued_at(1, queued)];
    tick_after(&mut engine, &clock, 0).await;
    assert_eq!(registrations(&github), 0, "left to the primary at first");
    assert_eq!(
        waiting(&handle),
        [Some(api::Waiting::Overflow {
            until: (queued + 45_000) / 1000
        })]
    );
    tick_after(&mut engine, &clock, 15).await;
    tick_after(&mut engine, &clock, 29).await;
    assert_eq!(registrations(&github), 0, "44 s on, still the primary's");
    tick_after(&mut engine, &clock, 1).await;
    assert_eq!(
        registrations(&github),
        1,
        "45 s on, no other machine took it"
    );
}

// Catches an overflow host that starts a runner for a job the primary's warm runner took.
#[tokio::test]
async fn a_job_the_primarys_warm_runner_takes_gets_no_runner_here() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-warm-peer"),
    );
    let (mut engine, _handle, clock) = overflow(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![queued_at(1, clock.now_ms())];
    github.0.lock().unwrap().runners = vec![peer(800, false)];
    tick_after(&mut engine, &clock, 0).await;
    clock.advance(Duration::from_secs(3));
    peer_takes(&github, 1);
    for _ in 0..6 {
        tick_after(&mut engine, &clock, 15).await;
    }
    assert_eq!(registrations(&github), 0, "the primary ran it");
}

// A primary that is asleep or off registers nothing: its jobs are this host's once they waited `overflow_after`, one
// runner each.
#[tokio::test]
async fn while_the_primary_sleeps_its_jobs_get_one_runner_each_once_they_waited() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-peer-asleep"),
    );
    let (mut engine, _handle, clock) = overflow(&github, &docker, &dir).await;
    let at = clock.now_ms();
    {
        let mut state = github.0.lock().unwrap();
        // The primary's warm runner, offline as its host sleeps.
        let mut asleep = peer(800, false);
        asleep.status = "offline".to_owned();
        state.runners = vec![asleep];
        state.queued = vec![queued_at(1, at), queued_at(2, at + 20_000)];
    }
    tick_after(&mut engine, &clock, 0).await;
    tick_after(&mut engine, &clock, 15).await;
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 0);
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 1, "job 1 waited 45 s, job 2 25 s");
    tick_after(&mut engine, &clock, 15).await;
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 2, "job 2 waited 45 s too");
}

// Catches finding M3: a start from a runners listing that began before the job was first listed, which cannot show a
// runner another machine registered for it since.
#[tokio::test]
async fn a_job_the_full_primary_left_waits_for_a_listing_after_a_fresh_runners_listing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-primary-full"),
    );
    let (mut engine, _handle, clock) = overflow(&github, &docker, &dir).await;
    {
        let mut state = github.0.lock().unwrap();
        state.runners = vec![peer(800, true), peer(801, true)];
        state.queued = vec![queued_at(1, clock.now_ms() - 600_000)];
    }
    tick_after(&mut engine, &clock, 0).await;
    assert_eq!(
        registrations(&github),
        0,
        "ten minutes old, but first listed here in this tick"
    );
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 1);
}

// Catches a start while the runners listing fails: what the other machines registered since the last answer is not
// known.
#[tokio::test]
async fn no_runner_starts_while_the_runners_listing_fails() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-runners-fail"),
    );
    let (mut engine, _handle, clock) = overflow(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![queued_at(1, clock.now_ms() - 25_000)];
    tick_after(&mut engine, &clock, 0).await;
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 0, "listed twice, 40 s old");
    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::BAD_GATEWAY);
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(
        registrations(&github),
        0,
        "55 s old, but the runners listing failed"
    );
    github.0.lock().unwrap().failed_runners.clear();
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 1);
}

/// An idle runner of this machine in slot 1 of `app`, which started `ago` before now, as a restart adopts it.
fn idle_runner_since(github: &FakeGithub, docker: &FakeDocker, ago: Duration) {
    let name = identity("ci", ID_A).runner_name("app", 1, 1);
    github.0.lock().unwrap().runners = vec![runner(7, &name, false)];
    let mut state = docker.0.lock().unwrap();
    state.containers = vec![runner_container(1, 7, true)];
    state
        .container_captures
        .insert("container-1".to_owned(), String::new());
    state.started.insert(
        "container-1".to_owned(),
        time::now_ms() - u64::try_from(ago.as_millis()).unwrap(),
    );
}

// Catches an overflow host whose runner the primary took the job from waiting a primary's 5 minutes: GitHub gives the
// next job to that idle runner before the primary starts one.
#[tokio::test]
async fn an_idle_runner_whose_job_the_primary_took_stops_after_two_minutes() {
    for (config, stops) in [(overflow_config(), true), (CONFIG.to_owned(), false)] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("overflow-takeover"),
        );
        idle_runner_since(&github, &docker, Duration::from_secs(130));
        let clock = Clock::stopped();
        let (mut engine, _handle) = on_clock(&github, &docker, &dir, &config, &clock).await;
        tick_after(&mut engine, &clock, 0).await;
        let removed = removed(&docker.calls()).len();
        assert_eq!(removed == 1, stops, "{config}");
    }
}

// Catches finding H1: an overflow host that keeps the default warm runner, which GitHub hands the next job to before
// the primary boots one. A primary with the same config keeps one.
#[tokio::test]
async fn an_overflow_host_keeps_no_warm_runner_for_an_active_repository() {
    for (config, warm) in [(overflow_config(), 0), (CONFIG.to_owned(), 1)] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("overflow-warm"),
        );
        let config = config.replace("warm = 0\n", "");
        let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
        // `o/r` had a job queued five minutes ago, so it is active.
        let at = time::now_ms() - 300_000;
        let job = QueuedJob {
            workflow: Some("CI".to_owned()),
            created_at: at,
            ..job(1)
        };
        handle.store.queued("o/r", &job, at);
        handle.store.flush().await;
        engine.recover().await.unwrap();
        engine.tick(Shutdown::No).await;
        assert_eq!(registrations(&github), warm, "{config}");
    }
}

// A restart forgets which listings showed a job: the new daemon waits for two of its own, however old the job.
#[tokio::test]
async fn a_restart_in_the_middle_lists_the_job_twice_again() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-restart"),
    );
    let clock = Clock::stopped();
    let config = overflow_config();
    github.0.lock().unwrap().queued = vec![queued_at(1, clock.now_ms())];
    {
        let (mut engine, _handle) = on_clock(&github, &docker, &dir, &config, &clock).await;
        tick_after(&mut engine, &clock, 0).await;
        tick_after(&mut engine, &clock, 15).await;
        tick_after(&mut engine, &clock, 15).await;
    }
    let (mut engine, handle) = on_clock(&github, &docker, &dir, &config, &clock).await;
    tick_after(&mut engine, &clock, 20).await;
    assert_eq!(
        registrations(&github),
        0,
        "50 s old, but listed once since the restart"
    );
    assert!(
        matches!(
            waiting(&handle)[..],
            [Some(api::Waiting::Overflow { until })] if until * 1000 < clock.now_ms()
        ),
        "its time is up, and it waits for a second listing: {:?}",
        waiting(&handle)
    );
    tick_after(&mut engine, &clock, 5).await;
    assert_eq!(registrations(&github), 1);
}

// Catches a job GitHub gave no queue time that a sleep made look old: its time counts from its first listing after
// the wake, not before the sleep.
#[tokio::test]
async fn after_a_wake_a_job_without_a_queue_time_waits_overflow_after_again() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-wake"),
    );
    let (mut engine, handle, clock) = overflow(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![queued_at(1, 0)];
    tick_after(&mut engine, &clock, 0).await;
    clock.sleep(Duration::from_mins(10));
    tick_after(&mut engine, &clock, 0).await;
    let woke = clock.now_ms();
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 0, "15 s since the wake");
    assert_eq!(
        waiting(&handle),
        [Some(api::Waiting::Overflow {
            until: (woke + 45_000) / 1000
        })]
    );
    tick_after(&mut engine, &clock, 30).await;
    assert_eq!(registrations(&github), 1, "45 s since the wake");
}

// Catches finding M3 as the review found it open: the full primary frees a slot and registers a runner for the job,
// which the runners listing before the start shows online and idle, and this host starts a second one beside it.
#[tokio::test]
async fn a_job_waits_while_the_runners_listing_shows_the_primarys_fresh_idle_runner_for_it() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-fresh-peer"),
    );
    let (mut engine, handle, clock) = overflow(&github, &docker, &dir).await;
    {
        let mut state = github.0.lock().unwrap();
        state.runners = vec![peer(800, true), peer(801, true)];
        state.queued = vec![queued_at(1, clock.now_ms() - 600_000)];
    }
    tick_after(&mut engine, &clock, 0).await;
    // The primary frees a slot and registers a runner, which GitHub has not yet given the job.
    github.0.lock().unwrap().runners.push(peer(802, false));
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(
        registrations(&github),
        0,
        "the primary's idle runner may take it"
    );
    assert!(
        matches!(waiting(&handle)[..], [Some(api::Waiting::Overflow { .. })]),
        "left to the primary: {:?}",
        waiting(&handle)
    );
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(
        registrations(&github),
        1,
        "the same runner idle a listing later is not getting it"
    );
}

// Catches finding 2 of the review: on an idle repository the runners are listed every third probe, so a job first
// listed here waited for that turn, three poll intervals, rather than the next listing.
#[tokio::test]
async fn after_a_restart_an_old_job_starts_at_the_next_scheduled_listing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-pace"),
    );
    let (mut engine, _handle, clock) = overflow(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![queued_at(1, clock.now_ms() - 3_600_000)];
    let interval = Duration::from_secs(15);
    for _ in 0..2 {
        clock.advance(interval);
        engine.age_listings(interval);
        engine
            .tick_listing(Shutdown::No, crate::scheduler::engine::tick::Scope::Due)
            .await;
    }
    assert_eq!(
        registrations(&github),
        1,
        "listed twice with its runners between"
    );
}

// Catches finding 3 of the review: a job this host already took on held again when the next runners listing failed,
// so its status says it is left to the primary while this host's runner boots for it.
#[tokio::test]
async fn a_job_once_released_stays_released_when_a_runners_listing_fails() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("overflow-released"),
    );
    let (mut engine, handle, clock) = overflow(&github, &docker, &dir).await;
    github.0.lock().unwrap().queued = vec![queued_at(1, clock.now_ms() - 600_000)];
    tick_after(&mut engine, &clock, 0).await;
    tick_after(&mut engine, &clock, 15).await;
    assert_eq!(registrations(&github), 1);
    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::BAD_GATEWAY);
    tick_after(&mut engine, &clock, 15).await;
    assert!(
        !matches!(waiting(&handle)[..], [Some(api::Waiting::Overflow { .. })]),
        "this host's runner boots for it: {:?}",
        waiting(&handle)
    );
}
