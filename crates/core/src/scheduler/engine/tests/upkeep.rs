//! What a profile's upkeep may do at once, why a reseed stays due, and what a reload or an ended task leaves of it.

use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::*;
use crate::cache;
use crate::scheduler::engine::caches::MEASURE_EVERY;
use crate::scheduler::engine::upkeep::{Due, MeasureTask, ReseedTask, Upkeep};

const APP: &str = "app";

fn reseed_running() -> ReseedTask {
    tokio::spawn(std::future::pending())
}

fn measure_running() -> MeasureTask {
    tokio::spawn(std::future::pending())
}

async fn ended<T>(task: JoinHandle<T>) -> JoinHandle<T> {
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    task
}

async fn measure_ended() -> MeasureTask {
    ended(tokio::spawn(async { Ok(cache::Measured::Unfinished) })).await
}

fn names<'upkeep>(found: impl Iterator<Item = &'upkeep String>) -> Vec<&'upkeep str> {
    found.map(String::as_str).collect()
}

#[test]
fn a_reason_to_reseed_gives_way_only_to_a_stronger_one() {
    use Due::{Changed, Oversized, Requested};
    let cases = [
        (Changed, Oversized, Oversized),
        (Oversized, Changed, Oversized),
        (Changed, Requested, Requested),
        (Requested, Changed, Requested),
        (Oversized, Requested, Requested),
        (Requested, Oversized, Requested),
    ];
    for (first, then, stays) in cases {
        let mut upkeep = Upkeep::default();
        upkeep.mark(APP, first);
        upkeep.mark(APP, then);
        assert_eq!(upkeep.due(APP), Some(stays), "{first:?} then {then:?}");
    }
}

#[test]
fn only_a_reseed_due_to_what_slot_1_changed_may_find_too_little_to_do() {
    assert!(!Due::Changed.forces(), "it weighs slot 1's changes");
    assert!(Due::Oversized.forces(), "the snapshot must start over");
    assert!(Due::Requested.forces(), "the API asked for it");
}

#[test]
fn a_profile_nothing_happened_to_is_idle_with_nothing_due() {
    let upkeep = Upkeep::default();
    assert_eq!(upkeep.due("unknown"), None);
    assert!(!upkeep.reseed_runs_or_is_requested("unknown"), "no reseed");
    assert!(
        upkeep.snapshot_unchanged_since("unknown", SystemTime::now()),
        "no reseed replaced its snapshot"
    );
    assert!(upkeep.seed_memories().is_empty(), "no seed runs");
}

#[tokio::test]
async fn a_measure_and_a_reseed_of_one_profile_never_run_together() {
    let now = Instant::now();

    let mut measuring = Upkeep::default();
    measuring.measure_started(APP, measure_running(), now);
    measuring.slot_one_ran(APP);
    measuring.mark(APP, Due::Changed);
    assert!(
        names(measuring.due_to_reseed()).is_empty(),
        "the reseed waits for the measure, which reads the layers it drops"
    );
    assert_eq!(
        measuring.due(APP),
        Some(Due::Changed),
        "still due meanwhile"
    );

    let mut reseeding = Upkeep::default();
    reseeding.reseed_started(APP, reseed_running(), 7);
    reseeding.slot_one_ran(APP);
    assert!(
        names(reseeding.due_to_measure(now + MEASURE_EVERY)).is_empty(),
        "no measure during a reseed"
    );
    reseeding.mark(APP, Due::Requested);
    assert!(
        names(reseeding.due_to_reseed()).is_empty(),
        "a second reseed would lose the first one's task"
    );
    assert!(
        !reseeding.requested_reseed_waits(APP),
        "the one running holds the room already"
    );
}

#[tokio::test]
async fn slot_1_is_measured_once_per_interval_and_never_when_a_reseed_is_due() {
    let start = Instant::now();
    let mut upkeep = Upkeep::default();
    upkeep.slot_one_ran(APP);
    assert_eq!(names(upkeep.due_to_measure(start)), [APP]);

    upkeep.measure_started(APP, measure_ended().await, start);
    assert_eq!(upkeep.finished_measures().len(), 1, "it ended");
    assert!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY)).is_empty(),
        "starting it took what was due"
    );

    upkeep.slot_one_ran(APP);
    assert!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY - Duration::from_secs(1))).is_empty(),
        "the interval since the last has not passed"
    );
    assert_eq!(names(upkeep.due_to_measure(start + MEASURE_EVERY)), [APP]);

    upkeep.mark(APP, Due::Changed);
    assert!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY)).is_empty(),
        "the reseed weighs slot 1's changes itself"
    );
    assert_eq!(upkeep.take_due(APP), Some(Due::Changed));
    assert_eq!(upkeep.due(APP), None, "starting it took what was due");
    assert!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY)).is_empty(),
        "and the measure it replaces"
    );
}

#[tokio::test]
async fn a_task_still_running_stays_and_one_that_ended_frees_the_profile() {
    let mut upkeep = Upkeep::default();
    upkeep.measure_started("measuring", measure_running(), Instant::now());
    upkeep.reseed_started("reseeding", reseed_running(), 7);
    assert!(upkeep.finished_measures().is_empty(), "the measure runs");
    assert!(upkeep.finished_reseeds().is_empty(), "the reseed runs");
    assert!(upkeep.reseed_runs("reseeding"), "still reseeding");
    assert_eq!(upkeep.seeds_memory(), 7);
    assert!(
        !upkeep.snapshot_unchanged_since("reseeding", SystemTime::now() + Duration::from_secs(60)),
        "the reseed may replace the snapshot after any check of it"
    );

    // A reseed that failed ended too: its memory is free and it may have replaced the snapshot.
    let before = SystemTime::now() - Duration::from_secs(60);
    let failed = ended(tokio::spawn(async {
        Err(docker::Error::RequestTimeoutError)
    }))
    .await;
    upkeep.reseed_started("reseeding", failed, 7);
    assert_eq!(upkeep.finished_reseeds().len(), 1, "it ended");
    assert!(!upkeep.reseed_runs("reseeding"), "the profile is free");
    assert_eq!(upkeep.seeds_memory(), 0, "its seed holds no memory");
    assert!(
        !upkeep.snapshot_unchanged_since("reseeding", before),
        "a check that began before it ended measured the old snapshot"
    );
    assert!(
        upkeep.snapshot_unchanged_since("reseeding", SystemTime::now() + Duration::from_secs(60)),
        "a check that began after it measured the new one"
    );
}

#[tokio::test]
async fn a_reload_that_removes_a_profile_forgets_what_was_due_and_keeps_what_runs() {
    let start = Instant::now();
    let mut upkeep = Upkeep::default();
    upkeep.reseed_started("gone", reseed_running(), 7);
    upkeep.request("gone");
    upkeep.measure_started("back", measure_ended().await, start);
    assert_eq!(upkeep.finished_measures().len(), 1, "it ended");
    upkeep.slot_one_ran("back");
    upkeep.mark("kept", Due::Oversized);
    upkeep.slot_one_ran("kept");

    upkeep.forget_removed(|name| name == "kept");

    assert_eq!(upkeep.due("gone"), None, "nothing stays due");
    assert!(
        upkeep.reseed_runs("gone"),
        "its reseed goes on, and retiring the profile waits for it"
    );
    assert_eq!(upkeep.seeds_memory(), 7, "it keeps the cap it started with");
    assert!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY)).is_empty(),
        "no measure stays due for a removed profile"
    );

    // Back in the config, its last measure still paces the next.
    upkeep.slot_one_ran("back");
    assert!(
        names(upkeep.due_to_measure(start)).is_empty(),
        "the interval since its last measure holds"
    );
    assert_eq!(
        names(upkeep.due_to_measure(start + MEASURE_EVERY)),
        ["back"]
    );

    assert_eq!(
        upkeep.due("kept"),
        Some(Due::Oversized),
        "a kept profile stays due"
    );
    assert!(upkeep.measure_is_due("kept"), "and its measure too");
}

/// `sized`'s config without the `small` profile and its repository.
fn without_small() -> String {
    let config = sized("0G", "8G", "1G");
    let (before, rest) = config.split_once("[profiles.small]").unwrap();
    let (_, repositories) = rest.split_once("[[repositories]]").unwrap();
    let (only_app, _) = repositories
        .split_once("[[repositories]]")
        .unwrap_or((repositories, ""));
    format!("{before}[[repositories]]{only_app}")
}

#[tokio::test]
async fn a_reseed_asked_for_before_a_reload_removed_its_profile_does_not_run_when_the_profile_comes_back()
 {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("upkeep-reload");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &sized("0G", "8G", "1G"));
    engine.recover().await.unwrap();
    engine.request_reseed("small").unwrap();

    engine.reload(Config::parse(&without_small()).unwrap(), None, None);
    engine.reload(Config::parse(&sized("0G", "8G", "1G")).unwrap(), None, None);
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }

    assert!(
        !engine.reseeding("small"),
        "the request went with the profile"
    );
    assert!(!engine.reseed_is_requested("small"), "and stays gone");
}

#[tokio::test]
async fn a_profile_a_reload_removed_stays_while_its_reseed_runs() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("upkeep-retired");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &sized("0G", "8G", "1G"));
    engine.recover().await.unwrap();
    engine.seed_runs("small", 1 << 30);

    engine.reload(Config::parse(&without_small()).unwrap(), None, None);
    engine.tick(Shutdown::No).await;

    assert!(
        engine.reseeding("small"),
        "the reload left its reseed running"
    );
    assert!(
        engine.is_retired("small"),
        "its limits stay known meanwhile"
    );
}

#[tokio::test]
async fn a_slot_volume_is_not_cleaned_while_its_profile_reseeds() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.seed_runs("app", 1 << 30);

    let refused = engine.clean_cache("runnerd-app-slot-2").await.unwrap_err();

    assert!(
        matches!(refused, ControlError::Conflict(_)),
        "the seed writes it: {refused}"
    );
    assert!(
        !docker
            .calls()
            .iter()
            .any(|call| call.starts_with("remove volume ")),
        "{:?}",
        docker.calls()
    );
}

#[tokio::test]
async fn a_check_of_the_caches_leaves_the_slot_volumes_of_a_profile_that_reseeds() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    // app has 2 slots; a third was open before.
    docker
        .0
        .lock()
        .unwrap()
        .volume_sizes
        .insert("runnerd-app-slot-3".to_owned(), 5 << 30);
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.seed_runs("app", 1 << 30);

    engine.tick(Shutdown::No).await;
    while engine.trim.as_ref().is_some_and(|trim| !trim.is_finished()) {
        tokio::task::yield_now().await;
    }
    engine.tick(Shutdown::No).await;

    assert!(engine.trim.is_none(), "the check ended");
    let calls = docker.calls();
    assert!(
        !calls
            .iter()
            .any(|call| call == "remove volume runnerd-app-slot-3"),
        "a seed mounts its profile's slot volumes: {calls:?}"
    );
}
