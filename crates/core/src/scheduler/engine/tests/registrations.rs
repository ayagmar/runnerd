//! What the engine remembers of a registration whose slot is gone, through the ticks that write and end it.

use super::*;
use crate::scheduler::SlotRef;

/// The runner `idle_runner` adopts.
const RUNNER: u64 = 7;

/// An engine that adopted `idle_runner` and has seen one listing.
async fn adopted(
    github: &FakeGithub,
    docker: &FakeDocker,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    idle_runner(github, docker);
    let (mut engine, handle, dir) = engine(github, docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    (engine, handle, dir)
}

/// The runner's container exits and GitHub refuses both deletes of its registration this tick makes.
async fn released_while_github_holds_it(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    docker: &FakeDocker,
) {
    github.0.lock().unwrap().busy_deletes = 2;
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
}

fn app_slot() -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index: 0,
    }
}

#[tokio::test]
async fn a_released_runner_github_holds_is_known_as_ended_until_a_listing_no_longer_has_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    assert!(!engine.ended_here(RUNNER), "it still has its slot");

    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "released here, and still listed");
    assert_eq!(
        engine.state_counts().busy_orphans,
        1,
        "the listing's refused delete is recorded"
    );

    engine.tick(Shutdown::No).await;
    assert_eq!(github.0.lock().unwrap().runners.len(), 0, "GitHub let go");
    assert!(
        engine.ended_here(RUNNER),
        "the listing that deleted it still had it"
    );

    engine.tick(Shutdown::No).await;
    assert!(
        !engine.ended_here(RUNNER),
        "the next listing has no such runner"
    );
}

#[tokio::test]
async fn a_killed_runner_counts_as_ended_only_if_github_keeps_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    github.0.lock().unwrap().busy_deletes = 1;
    engine.kill(&app_slot()).await;
    assert!(engine.ended_here(RUNNER), "GitHub refused the delete");

    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    engine.kill(&app_slot()).await;
    assert!(!engine.ended_here(RUNNER), "GitHub let it go at once");
}

#[tokio::test]
async fn a_registration_no_slot_explains_is_recorded_as_refused_and_not_as_ended_here() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    {
        let mut state = github.0.lock().unwrap();
        state.runners.push(runner(
            9,
            &identity("ci", ID_A).runner_name("app", 2, 1),
            true,
        ));
        state.registered_on.insert(9, "o/r".to_owned());
        state.busy_deletes = 1;
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(engine.state_counts().busy_orphans, 1, "refused once");
    assert!(!engine.ended_here(9), "this engine never ran it");
}

#[tokio::test]
async fn a_failed_listing_says_nothing_of_a_runner_ended_here() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    github.0.lock().unwrap().runners.clear();

    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    engine.tick(Shutdown::No).await;
    assert!(engine.ended_here(RUNNER), "GitHub did not say it is gone");

    github.0.lock().unwrap().failed_runners.clear();
    engine.tick(Shutdown::No).await;
    assert!(!engine.ended_here(RUNNER), "this listing does");
}

#[tokio::test]
async fn a_runner_of_a_removed_repository_is_known_as_ended_while_its_repository_drains() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, _handle, _dir) = adopted(&github, &docker).await;
    engine.reload(
        Config::parse(&CONFIG.replace("name = \"o/r\"", "name = \"o/x\"")).unwrap(),
        None,
        None,
    );
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "the drain still lists it");

    github.0.lock().unwrap().runners.clear();
    engine.tick(Shutdown::No).await;
    assert!(!engine.ended_here(RUNNER), "the drain's listing ends it");
}

#[tokio::test]
async fn a_restarted_engine_knows_of_no_runner_it_ended() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("registrations");
    idle_runner(&github, &docker);
    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    released_while_github_holds_it(&mut engine, &github, &docker).await;
    assert!(engine.ended_here(RUNNER), "known before the restart");
    drop(engine);

    let (mut restarted, _handle) = engine_in(&github, &docker, &dir);
    restarted.recover().await.unwrap();
    assert!(!restarted.ended_here(RUNNER), "nothing survives a restart");
    assert_eq!(restarted.state_counts().busy_orphans, 0);
}
