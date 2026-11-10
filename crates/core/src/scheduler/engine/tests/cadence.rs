//! Each repository listed on its own schedule (ADR 0128), through the ticks the engine's loop calls when one is due,
//! on the model's clock: a scheduled tick `by` after the last ages every listing and claim by that much first.

use super::*;
use crate::scheduler::engine::governor::Step;
use crate::scheduler::engine::tick::Scope;
use crate::scheduler::warm::Learned;

/// An active repository's interval at the default `poll_interval`.
const ACTIVE: Duration = Duration::from_secs(5);

/// A tick the engine's loop calls when the first repository is due, `by` after the last: the listings it made.
async fn scheduled(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    by: Duration,
) -> Vec<String> {
    let before = github.calls().len();
    engine.age_listings(by);
    engine.tick_listing(Shutdown::No, Scope::Due).await;
    github.calls()[before..]
        .iter()
        .filter(|call| call.starts_with("queued ") || call.starts_with("runners "))
        .cloned()
        .collect()
}

/// `count` scheduled ticks `ACTIVE` apart: how many times each listing came.
async fn listings(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    count: usize,
) -> BTreeMap<String, usize> {
    let mut made = BTreeMap::new();
    for _ in 0..count {
        for call in scheduled(engine, github, ACTIVE).await {
            *made.entry(call).or_default() += 1;
        }
    }
    made
}

/// The queued listings of `listed`.
fn queued(listed: &[String]) -> Vec<&str> {
    listed
        .iter()
        .map(String::as_str)
        .filter(|call| call.starts_with("queued "))
        .collect()
}

fn count(listings: &BTreeMap<String, usize>, call: &str) -> usize {
    listings.get(call).copied().unwrap_or(0)
}

/// `o/r`'s runner runs a job past ADR 0107's ten minutes, so only that keeps `o/r` active, and `o/s` has nothing. The
/// bugs it catches: one cadence for every repository, an active one listed at another share of `poll_interval`, a
/// repository whose runner holds a job listed as an idle one, and a paused one listed as an active one.
#[tokio::test]
async fn a_repository_whose_runner_holds_a_job_is_listed_three_times_as_often_as_an_idle_one() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("cadence");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.warm.testing_learned(Learned::default());

    let made = listings(&mut engine, &github, 6).await;
    assert_eq!(count(&made, "queued o/r"), 6, "o/r every 5 s: {made:?}");
    assert_eq!(count(&made, "queued o/s"), 2, "o/s every 15 s: {made:?}");

    handle.control.set_paused(&one("o/r"), true).await.unwrap();
    let made = listings(&mut engine, &github, 6).await;
    assert_eq!(
        count(&made, "queued o/r"),
        2,
        "paused o/r is idle: {made:?}"
    );
}

/// The bugs it catches: runners listed at every probe, or never again once a start or a release changed them or their
/// listing failed until their turn, which leaves a new runner idle-looking and a gone one's registration for longer.
#[tokio::test]
async fn runners_are_listed_every_third_probe_and_at_the_next_after_a_start_a_release_or_a_failure()
{
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("cadence");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;

    let made = listings(&mut engine, &github, 6).await;
    assert_eq!(count(&made, "queued o/r"), 6, "{made:?}");
    assert_eq!(
        count(&made, "runners o/r"),
        2,
        "every third probe: {made:?}"
    );

    // Probe 7 starts a runner for job 2: probe 8, not its turn, lists the runners.
    github.0.lock().unwrap().queued = vec![job(2)];
    let started = scheduled(&mut engine, &github, ACTIVE).await;
    assert_eq!(registered(&github.calls()).len(), 1, "job 2 got a runner");
    assert_eq!(started, ["queued o/r"], "probe 7 is not runners' turn");
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/r", "queued o/r"],
        "the probe after the start"
    );
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["queued o/r"]
    );

    // Job 1 ends and its runner's container stops: the tick that releases it lists the runners at its probe.
    {
        let mut state = github.0.lock().unwrap();
        let name = state.runners[0].name.clone();
        let mut done = details(1, "completed", Some("success"));
        done.runner_name = Some(name.clone());
        state.jobs.insert(1, done);
        state.runners.retain(|runner| runner.name != name);
    }
    for container in &mut docker.0.lock().unwrap().containers {
        if container.id == "container-1" {
            container.running = false;
        }
    }
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/r", "queued o/r"],
        "the probe after the release"
    );

    // Its runners listing fails at its turn: the probe after lists them again.
    for _ in 0..2 {
        scheduled(&mut engine, &github, ACTIVE).await;
    }
    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::BAD_GATEWAY);
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/r", "queued o/r"],
        "its turn, which fails"
    );
    github.0.lock().unwrap().failed_runners.clear();
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/r", "queued o/r"],
        "the probe after the failed one"
    );
}

/// A runner a restart adopts, which took job 1 before it and is listed busy, while job 2 queued meanwhile. `o/r` is
/// active, listed every 5 s: the claim that job 2 may be the one it took holds through the listings 5 and 10 s after the
/// one that showed it busy, which may still show its job, and ends at the one 15 s after. The bugs it catches: a claim
/// ended by the next listing however soon (a twin for GitHub's lag), and a mark a later runners listing moves on,
/// which keeps a job from ever getting a runner while the capture names nothing.
#[tokio::test]
async fn a_newly_busy_runner_holds_back_a_listed_job_until_a_listing_a_poll_interval_after_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("cadence-claim");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    handle.store.flush().await;
    until_captures_end(&engine).await;
    drop((engine, handle));

    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(2)];
        for runner in &mut state.runners {
            runner.busy = true;
        }
    }
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    for (probe, at) in [(1, 5), (2, 10)] {
        let listed = scheduled(&mut engine, &github, ACTIVE).await;
        assert!(
            listed.contains(&"queued o/r".to_owned()),
            "probe {probe} lists o/r"
        );
        assert_eq!(
            registered(&github.calls()).len(),
            1,
            "{at} s after the listing that showed it busy, job 2 may be the one it took"
        );
    }
    let listed = scheduled(&mut engine, &github, ACTIVE).await;
    assert_eq!(listed, ["runners o/r", "queued o/r"], "its third probe");
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "15 s on, the listing still shows job 2, which gets a runner"
    );
}

/// With the rate limit low, the governor lists every repository every 30 s (ADR 0127). `o/s`'s runner takes job 2 and
/// the listing that shows it busy shows job 2 queued still; `o/r`, on another phase, is listed 26 s later. The bug it
/// catches: a claim that another repository's listing ends, so the plan, working from `o/s`'s listing, which showed
/// job 2, starts a second runner for it.
#[tokio::test]
async fn only_a_listing_of_its_own_repository_ends_a_newly_busy_runners_claim() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("cadence-own");
    let config = two_repositories().replace("slots = 2", "slots = 8");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // `o/r`, active, at 6 and 11 s; `o/s`, idle, at 15 s, where job 2 gets a runner.
    for by in [6, 5] {
        let listed = scheduled(&mut engine, &github, Duration::from_secs(by)).await;
        assert_eq!(queued(&listed), ["queued o/r"]);
    }
    {
        let mut state = github.0.lock().unwrap();
        state.queued.push(job(2));
        state.queued_on.insert(2, "o/s".to_owned());
    }
    scheduled(&mut engine, &github, Duration::from_secs(4)).await;
    assert_eq!(registered(&github.calls()).len(), 1, "job 2 got a runner");
    for runner in &mut github.0.lock().unwrap().runners {
        runner.busy = true;
    }
    engine.governor.testing_step(Step::Listings2);

    let listed = scheduled(&mut engine, &github, Duration::from_secs(26)).await;
    assert_eq!(queued(&listed), ["queued o/r"], "41 s");
    assert_eq!(
        scheduled(&mut engine, &github, Duration::from_secs(4)).await,
        ["runners o/s", "queued o/s"],
        "45 s: its runner shows busy, and job 2 queued"
    );
    let listed = scheduled(&mut engine, &github, Duration::from_secs(26)).await;
    assert_eq!(queued(&listed), ["queued o/r"], "71 s");
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "until o/s is listed again, job 2 may be the one its runner took"
    );
}

/// A reload removes `o/s`, which drains. Its runners are listed at each scheduled tick, whether or not `o/r` is due,
/// until two clean listings end the drain. The bug it catches: a drain listed only when a listed repository is due,
/// or never by a scheduled tick, so it never ends.
#[tokio::test]
async fn a_draining_repository_is_listed_at_every_scheduled_tick_until_its_drain_ends() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("cadence-drain");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.reload(Config::parse(CONFIG).unwrap(), None, None);

    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/s"],
        "o/r, idle, is not due"
    );
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["runners o/s"],
        "the second clean listing ends the drain"
    );
    assert_eq!(
        scheduled(&mut engine, &github, ACTIVE).await,
        ["queued o/r"],
        "o/s no longer drains, and o/r is due 15 s after its last listing"
    );
}

/// A wake-up between two scheduled ticks, as a command or a pause sends, brings a tick that lists every repository and
/// its runners, idle ones too. The bug it catches: a tick brought sooner that lists only what is due, so a command's
/// effect, as a resumed repository's jobs, waits for its pace.
#[tokio::test(start_paused = true)]
async fn a_tick_a_wake_up_brings_lists_every_repository_and_its_runners() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("cadence-wake");
    let (engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    let run = tokio::spawn(engine.run());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = github.calls().len();
    handle.control.wake.notify_one();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let calls = github.calls()[before..].to_vec();
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
    for listing in ["runners o/r", "runners o/s", "queued o/r", "queued o/s"] {
        assert!(
            calls.iter().any(|call| call == listing),
            "{listing} in {calls:?}"
        );
    }
}
