//! The predicates over a slot's runner that differ from one another, one state per shape, each where it decides: a
//! runner listed idle whose job a capture named, an adopted runner with no job, and a job named after a release took
//! its copy of the runner. The state of the listing lags what a capture knows, so these are the readings a wrong
//! accessor changes silently.

use std::time::SystemTime;

use super::*;
use crate::scheduler::engine::Purpose;
use crate::scheduler::engine::control::RunningJobs;
use crate::scheduler::engine::history::Found;
use crate::scheduler::engine::runners::Removal;
use crate::scheduler::warm::Learned;
use crate::scheduler::{Pool, SlotRef};
use crate::time;

fn deletes(github: &FakeGithub) -> Vec<String> {
    github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("delete "))
        .collect()
}

fn runner_name_in(handle: &Handle, slot: usize) -> Option<String> {
    let status = handle.status.borrow();
    let profile = status
        .profiles
        .iter()
        .find(|profile| profile.name == "app")?;
    let slot = profile
        .slots
        .iter()
        .find(|candidate| candidate.slot == slot)?;
    Some(slot.runner.as_ref()?.name.clone())
}

fn first_slot() -> SlotRef {
    SlotRef {
        profile: "app".to_owned(),
        index: 0,
    }
}

/// A runner in slot 1 whose capture named job 1, and that the last listing showed idle, as a listing can lag.
async fn listed_idle_with_a_job_named(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    running_job(github, docker, "o/r", 1, 1);
    let (mut engine, handle) = engine_with(github, docker, dir, config);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    github.0.lock().unwrap().runners[0].busy = false;
    engine.tick(Shutdown::No).await;
    let occupant = engine.occupants_mut().next().unwrap();
    assert!(!occupant.listed_busy(), "the listing shows it idle");
    assert_eq!(occupant.named_job(), Some(1), "its capture named job 1");
    (engine, handle)
}

/// A graceful stop goes by the listing alone: GitHub, which knows, refuses to drop a runner that took a job.
#[tokio::test]
async fn a_graceful_stop_asks_github_about_a_runner_listed_idle_with_a_job_named() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("graceful-asks");
    let (mut engine, _handle) = listed_idle_with_a_job_named(&github, &docker, &dir, CONFIG).await;

    engine.tick(Shutdown::Graceful).await;

    assert_eq!(deletes(&github), ["delete 7"], "GitHub is asked");
    assert_eq!(
        removed(&docker.calls()),
        Vec::<&String>::new(),
        "it keeps its job"
    );
}

/// Everything but a graceful stop takes a named job over the listing.
#[tokio::test]
async fn a_runner_listed_idle_with_a_job_named_waits_for_no_job_anywhere() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("named-not-idle");
    let config = CONFIG
        .replace("slots = 2", "slots = 1")
        .replace("warm = 0", "warm = 1");
    let (mut engine, _handle) = listed_idle_with_a_job_named(&github, &docker, &dir, &config).await;
    assert_eq!(
        deletes(&github),
        Vec::<String>::new(),
        "not stopped as a waiting runner an older release started"
    );

    assert!(
        matches!(
            engine.pause(&one("o/r"), RunningJobs::Keep).await,
            Err(ControlError::Busy { .. })
        ),
        "a pause that keeps running jobs refuses"
    );
    assert!(
        matches!(
            engine.clean_cache("runnerd-pnpm").await,
            Err(ControlError::Busy { .. })
        ),
        "a clean waits for it"
    );
    assert_eq!(
        deletes(&github),
        Vec::<String>::new(),
        "a clean does not stop it as a waiting runner"
    );

    let mut learned = Learned::default();
    learned.note("o/r", time::now_ms());
    engine.warm.testing_learned(learned);
    let pool = Pool {
        repository: "o/r".to_owned(),
        profile: "app".to_owned(),
    };
    assert_eq!(
        engine.warm_purpose(&pool),
        Purpose::Warm,
        "it is not the warm runner the pool wants"
    );

    engine.ask_reseed("app");
    assert!(
        !engine.upkeep_waits_for_warm("app"),
        "a reseed waits for its job, not only for warm runners"
    );
}

/// A runner a reload left outdated gives way only while it waits: a named job is not waiting, whatever the listing says.
#[tokio::test]
async fn a_reload_does_not_stop_a_runner_listed_idle_with_a_job_named() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("reload-named");
    let (mut engine, _handle) = listed_idle_with_a_job_named(&github, &docker, &dir, CONFIG).await;

    let bigger = CONFIG.replace("memory = \"4G\"", "memory = \"6G\"");
    engine.reload(Config::parse(&bigger).unwrap(), None, None);
    engine.tick(Shutdown::No).await;

    assert_eq!(deletes(&github), Vec::<String>::new(), "it keeps its job");
    assert_eq!(
        removed(&docker.calls()),
        Vec::<&String>::new(),
        "its container stays"
    );
}

/// The two ways a runner with no job named counts as waiting at a detaching stop, and the one that does not.
#[tokio::test]
async fn a_detaching_stop_takes_an_adopted_runner_with_no_job_and_leaves_one_it_started_that_a_listing_shows_busy()
 {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    // Adopted, listed busy, no job named.
    running_runner(&github, &docker, "");
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("detach-shape");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    // The first listing shows the adopted runner newly busy, which may have taken job 1: the next starts a runner.
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    let started = {
        let mut state = github.0.lock().unwrap();
        state.queued.clear();
        for runner in &mut state.runners {
            runner.busy = true;
        }
        state.runners[1].name.clone()
    };
    engine.tick(Shutdown::No).await;

    engine.remove_every_idle().await;

    assert_eq!(
        deletes(&github),
        ["delete 7"],
        "only the adopted runner goes"
    );
    let names: Vec<String> = engine
        .occupied()
        .map(|(_, occupant)| occupant.runner_name.clone())
        .collect();
    assert_eq!(names, [started], "the one it started is left to its job");
}

/// A capture started again reports the job it reported before: the job has not started again.
#[tokio::test]
async fn a_capture_that_reports_its_job_again_keeps_when_it_was_found() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("found-again");
    let (mut engine, _handle) = listed_idle_with_a_job_named(&github, &docker, &dir, CONFIG).await;
    let hour = Duration::from_secs(3600);
    let occupant = engine.occupants_mut().next().unwrap();
    let name = occupant.runner_name.clone();
    occupant.found_job_at(SystemTime::now() - hour);
    let first = occupant.job_found_at();

    engine.note_found(&Found::named(name, 1));

    assert_eq!(engine.occupants_mut().next().unwrap().job_found_at(), first);
}

/// The copy a release works from was taken before its capture's last report, and its logs show no job.
#[tokio::test]
async fn a_job_named_during_a_release_is_the_job_its_runner_ran() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(5)];
    let dir = TestDir::new("named-in-release");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.warm.testing_learned(Learned::default());
    engine.tick(Shutdown::No).await;
    let copy = engine.occupant(&first_slot()).unwrap().clone();
    assert!(copy.idle(), "the copy knows of no job");

    engine.note_found(&Found::named(copy.runner_name.clone(), 5));
    // Naming the job noted the repository active too: only the release's own note is left to see.
    engine.warm.testing_learned(Learned::default());
    engine.released(
        &first_slot(),
        &copy,
        Some("no job here\n"),
        Ok(Removal::Done),
    );

    assert!(
        engine.measure_is_due("app"),
        "slot 1 ran a job, so it is measured"
    );
    assert!(
        engine.warm.active("o/r", time::now_ms()),
        "its repository stays active past the job"
    );
}

/// Whether `slot` of the profile shows a runner busy.
fn shows_busy(handle: &Handle, slot: usize) -> Option<bool> {
    let status = handle.status.borrow();
    let slot = status.profiles[0]
        .slots
        .iter()
        .find(|candidate| candidate.slot == slot)?;
    Some(slot.runner.as_ref()?.busy)
}

/// GitHub refuses to drop a waiting runner as it goes, as it just took a job: until the next listing it counts busy,
/// and as one that may have taken a listed job of its pool, which a pass meanwhile leaves to it.
#[tokio::test]
async fn a_waiting_runner_github_keeps_as_it_goes_may_have_taken_a_listed_job() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    idle_runner(&github, &docker);
    let dir = TestDir::new("kept-busy");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().busy_deletes = 1;

    engine.remove_every_idle().await;

    engine.publish();
    assert_eq!(
        shows_busy(&handle, 1),
        Some(true),
        "GitHub says it took a job"
    );
    engine.set_queued("o/r", vec![job(1)]);
    assert_eq!(engine.refill().await, 0, "job 1 may be the one it took");
}

/// A runner the listings showed busy that GitHub keeps as it goes took its job before them: a pass starts a runner
/// for a listed job beside it.
#[tokio::test]
async fn a_runner_listed_busy_github_keeps_as_it_goes_took_no_listed_job() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "");
    let dir = TestDir::new("kept-busy-already");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().busy_deletes = 1;

    engine.remove_every_idle().await;

    assert!(
        engine.occupied().next().is_some(),
        "GitHub keeps the adopted runner"
    );
    engine.set_queued("o/r", vec![job(1)]);
    assert_eq!(engine.refill().await, 1, "job 1 gets a runner");
}

/// A runner a restart adopts that takes its job before the first listing gets the claim of a runner first listed
/// busy: GitHub may list that job queued a while longer, and it gets no twin.
#[tokio::test]
async fn an_adopted_runner_that_takes_a_job_before_the_first_listing_gets_no_twin_for_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("adopted-newly-busy");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    handle.store.flush().await;
    until_captures_end(&engine).await;
    drop((engine, handle));

    // It takes job 1 while no daemon runs, and GitHub still lists the job queued.
    for runner in &mut github.0.lock().unwrap().runners {
        runner.busy = true;
    }
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 1 is the one the adopted runner may have taken"
    );
}

/// The cost of that claim: a runner a restart adopts that was busy before it holds back one listed job of its pool,
/// for one listing.
#[tokio::test]
async fn an_adopted_runner_already_busy_holds_back_a_listed_job_for_one_listing_at_most() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("adopted-busy");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    handle.store.flush().await;
    until_captures_end(&engine).await;
    drop((engine, handle));

    // It took job 1 before the restart, which no capture named; job 2 queues meanwhile.
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
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 2 may be the one it took, as far as the first listing tells"
    );
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "the next listing still shows job 2, which gets a runner"
    );
}

/// A runners listing that fails still ends the claim of a runner the listing before first showed busy: the job it
/// may have taken would have left the queue by then, so one still listed is another start's.
#[tokio::test]
async fn a_runners_listing_that_fails_ends_the_claim_of_a_runner_first_listed_busy_before_it() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("newly-busy-failed");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
    for runner in &mut github.0.lock().unwrap().runners {
        runner.busy = true;
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 1 is the one the runner first listed busy may have taken"
    );

    github
        .0
        .lock()
        .unwrap()
        .failed_runners
        .insert("o/r".to_owned(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "job 1, still listed after the failed listing, gets a runner"
    );
}

/// The claim comes with the first listing that shows the adopted runner busy: while its runners listing fails, it holds
/// back no listed job of its pool, which an outage would otherwise starve.
#[tokio::test]
async fn an_adopted_runner_whose_runners_listing_fails_holds_back_no_listed_job() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("adopted-unlisted");
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
        state
            .failed_runners
            .insert("o/r".to_owned(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    }
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "job 2 gets a runner though no listing answered for the adopted one"
    );
}

/// What a runner first listed busy may have taken stays counted until the next listing is applied, not from the top
/// of the tick that fetches it: a pass while that listing waits must not start a twin for the job it took.
#[tokio::test]
async fn a_pass_while_the_next_listing_waits_still_counts_the_job_a_newly_busy_runner_may_have_taken()
 {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1), job(2)];
    let dir = TestDir::new("newly-busy");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 2);
    // The runner in slot 2 ran job 2, and the listing still shows job 1 beside slot 1's busy runner.
    let (ended, second) = {
        let occupant = engine.occupants_mut().nth(1).unwrap();
        occupant.has_taken(2);
        (occupant.container.clone(), occupant.runner_name.clone())
    };
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        for runner in &mut state.runners {
            runner.busy = true;
        }
    }
    engine.tick(Shutdown::No).await;
    assert_eq!(
        registered(&github.calls()).len(),
        2,
        "job 1 is the one it may have taken"
    );
    engine.publish();
    assert_eq!(
        runner_name_in(&handle, 2).as_deref(),
        Some(second.as_str()),
        "slot 2 shows the runner before it goes"
    );
    let exits = engine.exits.clone();
    // A held listing of the queued jobs is not logged until it answers; the runners listing beside it is.
    let listings = |calls: &[String]| calls.iter().filter(|call| *call == "runners o/r").count();
    let before = listings(&github.calls());
    github.hold_queued("o/r");
    let pass = async {
        github
            .until_calls(|calls| listings(calls) > before, "the next listing")
            .await;
        for container in &mut docker.0.lock().unwrap().containers {
            if container.id == ended {
                container.running = false;
            }
        }
        exits.stopped(ended.clone());
        tokio::time::timeout(Duration::from_secs(60), async {
            while runner_name_in(&handle, 2).as_deref() == Some(second.as_str()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the pass released slot 2");
        assert_eq!(
            registered(&github.calls()).len(),
            2,
            "the pass starts no twin for the job slot 1 may have taken"
        );
        github.release_queued();
    };
    tokio::join!(engine.tick(Shutdown::No), pass);
}
