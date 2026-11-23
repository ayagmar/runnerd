//! A host on battery on the fakes (ADR 0161): its power supplies are files under the engine's `sys`, which a test
//! writes as the kernel would.

use super::sleep::stop_fixture;
use super::*;

/// The laptop's battery in `dir`'s `sys` says `status`.
fn battery(dir: &TestDir, status: &str) {
    let supply = dir.0.join("sys/class/power_supply/BAT0");
    std::fs::create_dir_all(&supply).unwrap();
    std::fs::write(supply.join("type"), "Battery\n").unwrap();
    std::fs::write(supply.join("status"), format!("{status}\n")).unwrap();
}

// Catches a host on battery that starts runners or keeps idle ones, one that stops a busy runner, a hold the status
// does not explain, one a restart forgets, and one that outlasts the plug.
#[tokio::test]
async fn on_battery_idle_runners_stop_busy_ones_finish_and_nothing_starts_until_it_is_plugged_in() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-battery"),
    );
    battery(&dir, "Discharging");
    let (mut engine, handle) = stop_fixture(&github, &docker, &dir);
    github.0.lock().unwrap().queued = vec![job(2)];
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(
        removed(&docker.calls()),
        ["remove container-2"],
        "the idle runner goes; the busy one stays"
    );
    assert!(
        registered(&github.calls()).is_empty(),
        "nothing starts on battery, though slots are free"
    );
    assert_eq!(waiting(&handle), [Some(api::Waiting::OnBattery)]);
    let status = handle.status.borrow().clone();
    assert!(status.on_battery, "the status says the host is on battery");
    assert_eq!(
        status.queued[0]
            .waiting
            .as_ref()
            .map(|waiting| waiting.reason(&status.queued[0], &status, 0)),
        Some("The host is on battery; runners start once it is plugged in".to_owned())
    );

    // A restart on battery holds from its first tick.
    drop(engine);
    let (mut engine, handle) = engine_with(
        &github,
        &docker,
        &dir,
        &CONFIG.replace("slots = 2", "slots = 3"),
    );
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert!(
        registered(&github.calls()).is_empty(),
        "a restart on battery starts nothing"
    );

    // Plugged in and held at a charge limit: on mains power.
    battery(&dir, "Not charging");
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "a runner starts for job 2 once the host is plugged in"
    );
    assert_ne!(waiting(&handle), [Some(api::Waiting::OnBattery)]);
    assert!(
        !handle.status.borrow().on_battery,
        "plugged in, the status says no hold"
    );
}

// Catches M2 of review 1 of #344: a laptop unplugged while idle whose warm runners go with nothing to say why.
#[tokio::test]
async fn an_idle_host_on_battery_says_so_in_its_status() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-battery-idle"),
    );
    battery(&dir, "Discharging");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    let status = handle.status.borrow().clone();
    assert!(status.queued.is_empty(), "nothing queued");
    assert_eq!(
        status.battery_line(),
        Some("On battery: no runner starts until the host is plugged in")
    );
}

// Catches `run` ignored, and a reload to `pause` that waits for the next restart.
#[tokio::test]
async fn on_battery_run_starts_runners_and_a_reload_to_pause_holds_at_the_next_tick() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-battery-run"),
    );
    battery(&dir, "Discharging");
    let run = CONFIG.replace(
        "admission = \"caps\"",
        "admission = \"caps\"\non_battery = \"run\"",
    );
    github.0.lock().unwrap().queued = vec![job(1)];
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &run);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "on_battery = \"run\" starts a runner on battery"
    );
    let poll = engine.config.host.poll_interval;
    let idle = engine.governor.listings_every(poll);
    assert!(
        engine.listing_every("o/r") < idle,
        "a job queued lists o/r at the active pace"
    );

    // GitHub has not handed the runner its job yet when the config says pause.
    engine.reload(Config::parse(CONFIG).unwrap(), None, None);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(waiting(&handle), [Some(api::Waiting::OnBattery)]);
    assert_eq!(
        handle.status.borrow().profiles[0].slots[0].runner,
        None,
        "its idle runner stopped"
    );
    // Catches L1 of review 1 of #344: a job that cannot start listed three times as often, on battery.
    assert_eq!(
        engine.listing_every("o/r"),
        idle,
        "on battery, o/r is listed at the idle pace"
    );
}

// Catches L2 of review 1 of #344: a pass between ticks that starts a runner on the reading from before the unplug.
#[tokio::test]
async fn a_pass_after_the_host_is_unplugged_starts_nothing() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("engine-battery-pass"),
    );
    battery(&dir, "Charging");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert!(!handle.status.borrow().on_battery, "plugged in, no hold");

    battery(&dir, "Discharging");
    engine.set_listed("o/r", vec![job(1)]);
    assert_eq!(engine.refill().await, 0, "the pass starts nothing");
    assert!(
        registered(&github.calls()).is_empty(),
        "no runner registers"
    );
}
