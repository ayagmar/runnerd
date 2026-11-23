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
}
