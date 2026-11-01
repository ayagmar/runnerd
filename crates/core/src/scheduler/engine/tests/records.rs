//! Records of failed jobs on the fakes: which runners are kept, the limits that remove the oldest, removal by hand,
//! and shells, which count against the budget like a slot.

use std::collections::HashMap;
use std::time::Duration;

use super::*;
use crate::docker::TtySize;
use crate::record;
use crate::scheduler::engine::RecordSelection;

pub(super) const FAILED: &str = "2026-10-23 01:08:55Z: Running job: build\n\
                      2026-10-23 01:09:27Z: Job build completed with result: Failed\n";

pub(super) fn recording() -> String {
    CONFIG.replace("slots = 2", "slots = 2\nrecord_failures = true")
}

const SIZE: TtySize = TtySize { cols: 80, rows: 24 };

/// Job 1 runs in slot 1 and then fails: its runner prints `log` and exits.
async fn job_ends(
    github: &FakeGithub,
    docker: &FakeDocker,
    engine: &mut Engine<FakeGithub, FakeDocker>,
    handle: &Handle,
    log: &str,
) {
    running_runner(github, docker, "@@job 1 1\n");
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("failure")));
    engine.recover().await.unwrap();
    until_state(handle, 1, JobState::Running).await;
    let mut state = docker.0.lock().unwrap();
    state.runner_log = Some(log.to_owned());
    state.written = 1 << 30;
    state.containers[0].running = false;
}

/// Ticks until the engine took the finished commits.
pub(super) async fn until_recorded(engine: &mut Engine<FakeGithub, FakeDocker>) {
    for _ in 0..200 {
        engine.tick(Shutdown::No).await;
        if engine.records.recordings.is_empty() {
            engine.publish();
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("a commit never finished");
}

/// A record image an earlier job left, made `age` seconds ago.
pub(super) fn record_image(job: u64, age: u64) -> docker::Image {
    let created = crate::time::now_ms() / 1000 - age;
    docker::Image {
        reference: record::image(job),
        labels: HashMap::from([
            (record::LABEL.to_owned(), job.to_string()),
            ("runnerd.repository".to_owned(), "o/r".to_owned()),
            ("runnerd.profile".to_owned(), "app".to_owned()),
            ("runnerd.created-at".to_owned(), created.to_string()),
        ]),
        bytes: 1 << 30,
    }
}

pub(super) async fn note(handle: &Handle, job: u64) -> Option<api::RecordNote> {
    handle.store.flush().await;
    handle.store.job(job).await.unwrap().unwrap().record
}

#[tokio::test]
async fn a_failed_job_leaves_a_record_and_its_container_goes_once_committed() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("record");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    let mut events = handle.store.subscribe();
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    until_recorded(&mut engine).await;

    let calls = docker.calls();
    let commit = calls
        .iter()
        .position(|call| call == "commit container-1 runnerd-record:1");
    let remove = calls.iter().position(|call| call == "remove container-1");
    assert!(
        commit.is_some() && commit < remove,
        "the container is committed, then removed: {calls:?}"
    );
    let records = handle.records.borrow().clone();
    assert_eq!(records.records.len(), 1);
    let kept = &records.records[0];
    assert_eq!(
        (kept.job, kept.repository.as_str(), kept.bytes),
        (1, "o/r", 1 << 30)
    );
    assert_eq!(kept.workflow.as_deref(), Some("ci"));
    let note = note(&handle, 1).await.unwrap();
    assert_eq!(note.bytes, Some(1 << 30));
    assert!(
        note.created_at.is_some(),
        "the history keeps that it was made"
    );
    let mut created = false;
    while let Ok(event) = events.try_recv() {
        created |= matches!(&event.event, api::Event::RecordCreated { record } if record.job == 1);
    }
    assert!(created, "record.created is announced");
}

#[tokio::test]
async fn an_idle_runner_whose_job_fails_while_it_is_deregistered_is_recorded_not_removed() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("record");
    idle_runner(&github, &docker);
    github.0.lock().unwrap().strict = true;
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
    }
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= crate::scheduler::IDLE_TIMEOUT;
    }
    // The tick saw it running and idle, so it removes it as expired; meanwhile it takes job 1, which fails, and its
    // runner exits, which ends its registration. Its capture reads the job and its last line a moment later.
    github.hold_deletes();
    let ends = async {
        github.until_waiting(1).await;
        {
            let mut state = github.0.lock().unwrap();
            state.runners.clear();
            state
                .jobs
                .insert(1, details(1, "completed", Some("failure")));
        }
        {
            let mut state = docker.0.lock().unwrap();
            state.runner_log = Some(FAILED.to_owned());
            state.written = 1 << 30;
            state.containers[0].running = false;
            state.container_captures.insert(
                "container-1".to_owned(),
                "@@job 1 1\n@@page 1 0 7\nfailed\n".to_owned(),
            );
        }
        github.release_deletes();
    };
    tokio::join!(engine.tick(Shutdown::No), ends);
    until_recorded(&mut engine).await;

    let calls = docker.calls();
    let commit = calls
        .iter()
        .position(|call| call == "commit container-1 runnerd-record:1");
    let remove = calls.iter().position(|call| call == "remove container-1");
    assert!(
        commit.is_some() && commit < remove,
        "the failed job's container is committed, then removed: {calls:?}"
    );
    let note = note(&handle, 1).await.unwrap();
    assert!(note.created_at.is_some(), "job 1's record was made");
    until_log_lines(&handle, 1, 1).await;
}

#[tokio::test]
async fn an_idle_runner_whose_capture_names_a_failed_job_while_github_still_deregisters_it_is_recorded()
 {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("record");
    idle_runner(&github, &docker);
    github.0.lock().unwrap().strict = true;
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
    }
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= crate::scheduler::IDLE_TIMEOUT;
    }
    // While its delete waits, it takes job 1, which its capture reports, and the job fails; GitHub has not dropped
    // its registration yet, so the delete answers `Deleted`, not "not found".
    github.hold_deletes();
    let ends = async {
        github.until_waiting(1).await;
        docker.0.lock().unwrap().container_captures.insert(
            "container-1".to_owned(),
            "@@job 1 1\n@@page 1 0 7\nfailed\n".to_owned(),
        );
        until_state(&handle, 1, JobState::Running).await;
        github
            .0
            .lock()
            .unwrap()
            .jobs
            .insert(1, details(1, "completed", Some("failure")));
        {
            let mut state = docker.0.lock().unwrap();
            state.runner_log = Some(FAILED.to_owned());
            state.written = 1 << 30;
            state.containers[0].running = false;
        }
        github.release_deletes();
    };
    tokio::join!(engine.tick(Shutdown::No), ends);
    until_recorded(&mut engine).await;

    let calls = docker.calls();
    let commit = calls
        .iter()
        .position(|call| call == "commit container-1 runnerd-record:1");
    let remove = calls.iter().position(|call| call == "remove container-1");
    assert!(
        commit.is_some() && commit < remove,
        "the failed job's container is committed, then removed: {calls:?}"
    );
    until_log_lines(&handle, 1, 1).await;
}

#[tokio::test]
async fn a_signal_waits_for_the_record_being_made() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("record");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    engine.tick(Shutdown::No).await;
    assert!(
        !engine.records.recordings.is_empty(),
        "job 1's record is being made"
    );
    handle.control.shutdown(Shutdown::Detach);
    tokio::time::timeout(Duration::from_secs(10), engine.schedule())
        .await
        .unwrap();
    let note = note(&handle, 1).await.unwrap();
    assert!(note.created_at.is_some(), "the record was made: {note:?}");
}

#[tokio::test]
async fn only_failed_jobs_of_a_profile_that_records_them_are_kept() {
    let succeeded = FAILED.replace("Failed", "Succeeded");
    for (config, log) in [
        (recording(), succeeded.as_str()),
        (CONFIG.to_owned(), FAILED),
    ] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new("no-record");
        let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
        job_ends(&github, &docker, &mut engine, &handle, log).await;
        until_recorded(&mut engine).await;
        let calls = docker.calls();
        assert!(
            !calls.iter().any(|call| call.starts_with("commit")),
            "nothing is committed: {calls:?}"
        );
        assert!(
            calls.contains(&"remove container-1".to_owned()),
            "{calls:?}"
        );
        assert_eq!(note(&handle, 1).await, None);
    }
}

#[tokio::test]
async fn a_runner_that_left_its_credentials_is_not_kept_and_the_history_says_why() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("secrets");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    docker.0.lock().unwrap().changed = vec![
        "/home/runner/_work/r/r/src/main.rs".to_owned(),
        "/home/runner/.credentials".to_owned(),
    ];
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    until_recorded(&mut engine).await;

    let calls = docker.calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("commit")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"remove container-1".to_owned()),
        "{calls:?}"
    );
    let skipped = note(&handle, 1).await.unwrap().skipped.unwrap();
    assert!(skipped.contains("/home/runner/.credentials"), "{skipped}");
    assert!(
        skipped.contains(&format!(
            "the image of runnerd {}",
            env!("CARGO_PKG_VERSION")
        )),
        "says which image deletes them: {skipped}"
    );
}

#[tokio::test]
async fn a_failure_past_max_count_removes_the_oldest_record_and_the_history_says_why() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 300), record_image(11, 200)];
    let dir = TestDir::new("count");
    let config = format!("{}\n[records]\nmax_count = 2\n", recording());
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    until_recorded(&mut engine).await;

    let jobs: Vec<u64> = handle
        .records
        .borrow()
        .records
        .iter()
        .map(|record| record.job)
        .collect();
    assert_eq!(jobs, [11, 1]);
    assert!(
        docker
            .calls()
            .contains(&"remove image runnerd-record:10".to_owned())
    );
    handle.store.flush().await;
    let removals: Vec<api::Event> = handle
        .store
        .events_after(0, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.event)
        .filter(|event| event.kind() == "record.deleted")
        .collect();
    assert_eq!(
        removals,
        [api::Event::RecordDeleted {
            job: 10,
            removed_by: api::RecordRemoval::Count
        }]
    );
}

#[tokio::test]
async fn a_failure_that_would_leave_docker_too_little_disk_is_not_kept() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().free = Some(11 << 30);
    let dir = TestDir::new("floor");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    until_recorded(&mut engine).await;

    assert!(!docker.calls().iter().any(|call| call.starts_with("commit")));
    let skipped = note(&handle, 1).await.unwrap().skipped.unwrap();
    assert!(skipped.contains("records.min_free"), "{skipped}");
}

#[tokio::test]
async fn records_past_max_age_and_those_removed_by_hand_go_unless_a_shell_is_open() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![
        record_image(10, 8 * 86_400),
        record_image(11, 3 * 86_400),
        record_image(12, 60),
    ];
    let dir = TestDir::new("trash");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.publish();
    let jobs = |handle: &Handle| -> Vec<u64> {
        handle
            .records
            .borrow()
            .records
            .iter()
            .map(|record| record.job)
            .collect()
    };
    assert_eq!(jobs(&handle), [11, 12], "the record past 7 days went");
    // The idle tick trimmed the shared caches, which a shell waits for.
    while engine.trim.as_ref().is_some_and(|trim| !trim.is_finished()) {
        tokio::task::yield_now().await;
    }

    engine.open_shell(12, SIZE).await.unwrap();
    let refused = engine.remove_records(&RecordSelection::Job(12)).await;
    assert!(
        matches!(refused, Err(ControlError::Conflict(_))),
        "{refused:?}"
    );
    let removed = engine
        .remove_records(&RecordSelection::OlderThan(Duration::from_hours(48)))
        .await
        .unwrap();
    assert_eq!(removed.jobs, [11]);
    let removed = engine.remove_records(&RecordSelection::All).await.unwrap();
    assert_eq!(
        removed.jobs,
        Vec::<u64>::new(),
        "the record with a shell open stays"
    );
    engine.publish();
    assert_eq!(jobs(&handle), [12]);

    handle.store.flush().await;
    let events: Vec<(u64, api::RecordRemoval)> = handle
        .store
        .events_after(0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            api::Event::RecordDeleted { job, removed_by } => Some((job, removed_by)),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        [
            (10, api::RecordRemoval::Age),
            (11, api::RecordRemoval::User)
        ]
    );
}

#[tokio::test]
async fn a_shell_holds_a_slots_memory_and_is_refused_while_the_budget_is_full() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 60), record_image(11, 60)];
    let dir = TestDir::new("shell");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.memory_max = Some(6 << 30);
    engine.recover().await.unwrap();

    let tty = engine.open_shell(10, SIZE).await.unwrap();
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert_eq!(budget.memory_in_use, 4 << 30);
    assert_eq!(
        budget.shell_memory,
        [api::ShellMemory {
            job: 10,
            memory: 4 << 30
        }]
    );
    let body = docker.0.lock().unwrap().shells[&tty.container].clone();
    assert_eq!(body.image.as_deref(), Some("runnerd-record:10"));
    assert_eq!(body.cmd.unwrap()[1], "/home/runner/_work/r/r");
    let mounts: Vec<(String, Option<String>)> = body
        .host_config
        .unwrap()
        .mounts
        .unwrap()
        .into_iter()
        .map(|mount| (mount.target.unwrap(), mount.source))
        .collect();
    assert!(
        mounts.contains(&("/state".to_owned(), None)),
        "the shell's changes go to a volume of its own, not a slot's: {mounts:?}"
    );
    assert!(
        mounts.contains(&("/base".to_owned(), Some("runnerd-app-base".to_owned()))),
        "{mounts:?}"
    );

    let refused = engine.open_shell(11, SIZE).await.unwrap_err();
    assert!(
        refused.to_string().starts_with("the budget is full"),
        "{refused}"
    );
    // No runner starts in the memory the shell holds.
    github.0.lock().unwrap().queued = vec![job(5)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());

    engine.close_shell(&tty.container).await;
    engine.publish();
    assert_eq!(handle.status.borrow().budget.memory_in_use, 0);
    assert!(
        docker
            .calls()
            .contains(&format!("remove {}", tty.container))
    );
}

#[tokio::test]
async fn under_measured_admission_no_shell_opens_while_the_slice_is_at_the_high_water_mark() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 60)];
    let dir = TestDir::new("shell");
    let config = recording().replace(
        "isolation = \"sysbox\"",
        "isolation = \"sysbox\"\nadmission = \"measured\"",
    );
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(10 << 30);
    engine.recover().await.unwrap();
    let slice_at = |memory| {
        handle.store.host_sample(api::HostSample {
            at: crate::time::now_ms() / 1000,
            cpu: 0,
            memory,
        });
    };

    // 9.2G of 10G: past 90%, though the 4G shell fits beside no runner.
    slice_at((92 << 30) / 10);
    let refused = engine.open_shell(10, SIZE).await.unwrap_err();
    assert!(refused.to_string().contains("90%"), "{refused}");
    slice_at(3 << 30);
    assert!(
        engine.open_shell(10, SIZE).await.is_ok(),
        "opens once under"
    );
}

#[tokio::test]
async fn a_shell_whose_process_exited_is_removed_and_one_left_by_an_earlier_run_is_not_taken_for_a_runner()
 {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 60)];
    let dir = TestDir::new("shell-gone");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    let tty = engine.open_shell(10, SIZE).await.unwrap();
    docker
        .0
        .lock()
        .unwrap()
        .containers
        .iter_mut()
        .for_each(|container| container.running = false);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(
        engine.records.shells.is_empty(),
        "the exited shell is forgotten"
    );
    assert!(
        docker
            .calls()
            .contains(&format!("remove {}", tty.container))
    );
    drop((engine, handle));

    // A shell carries its record's runner labels, as a commit keeps the container's.
    let mut left = runner_container(1, 7, true);
    left.id = "runnerd-shell-10-1".to_owned();
    left.name = left.id.clone();
    left.labels
        .insert(record::SHELL_LABEL.to_owned(), "10".to_owned());
    docker.0.lock().unwrap().containers = vec![left];
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    engine.publish();
    assert_eq!(runner_in(&handle, 1), None);
    assert!(
        docker
            .calls()
            .contains(&"remove runnerd-shell-10-1".to_owned())
    );
}

#[tokio::test]
async fn a_shell_left_by_an_earlier_run_that_docker_fails_to_remove_is_removed_on_a_later_tick() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let mut left = runner_container(1, 7, true);
    left.id = "runnerd-shell-10-1".to_owned();
    left.name = left.id.clone();
    left.labels
        .insert(record::SHELL_LABEL.to_owned(), "10".to_owned());
    {
        let mut state = docker.0.lock().unwrap();
        state.containers = vec![left];
        state.fail_removes = 1;
    }
    let dir = TestDir::new("shell-left");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();
    assert_eq!(docker.0.lock().unwrap().containers.len(), 1);
    engine.tick(Shutdown::No).await;
    assert_eq!(docker.0.lock().unwrap().containers.len(), 0);
}

#[tokio::test]
async fn failures_committed_at_once_count_together_against_min_free() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    // Room for one record of 2G (twice the 1G its layer holds) above the 10G floor, not for two.
    docker.0.lock().unwrap().free = Some(13 << 30);
    let dir = TestDir::new("floor-together");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    {
        let mut state = docker.0.lock().unwrap();
        state.containers = vec![runner_container(1, 7, true), runner_container(2, 8, true)];
        for (container, job) in [("container-1", 1), ("container-2", 2)] {
            state
                .container_captures
                .insert(container.to_owned(), format!("@@job {job} 1\n"));
        }
    }
    {
        let mut state = github.0.lock().unwrap();
        state.runners = vec![
            runner(7, &identity("ci", ID_A).runner_name("app", 1, 1), true),
            runner(8, &identity("ci", ID_A).runner_name("app", 2, 1), true),
        ];
        for job in [1, 2] {
            state
                .jobs
                .insert(job, details(job, "completed", Some("failure")));
        }
    }
    engine.recover().await.unwrap();
    until_state(&handle, 2, JobState::Running).await;
    {
        let mut state = docker.0.lock().unwrap();
        state.runner_log = Some(FAILED.to_owned());
        state.written = 1 << 30;
        state
            .containers
            .iter_mut()
            .for_each(|container| container.running = false);
    }
    until_recorded(&mut engine).await;

    let commits = docker
        .calls()
        .iter()
        .filter(|call| call.starts_with("commit"))
        .count();
    assert_eq!(
        commits, 1,
        "the second failure finds the first's disk taken"
    );
    let skipped: Vec<String> = [note(&handle, 1).await, note(&handle, 2).await]
        .into_iter()
        .filter_map(|note| note?.skipped)
        .collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].contains("records.min_free"), "{skipped:?}");
}

#[tokio::test(start_paused = true)]
async fn shells_get_containers_of_their_own_read_only_caches_and_close_after_eight_hours() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 60)];
    let dir = TestDir::new("shell-caches");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();

    let first = engine.open_shell(10, SIZE).await.unwrap();
    let second = engine.open_shell(10, SIZE).await.unwrap();
    assert_ne!(first.container, second.container);
    let body = docker.0.lock().unwrap().shells[&first.container].clone();
    for mount in body.host_config.unwrap().mounts.unwrap() {
        let target = mount.target.unwrap();
        assert_eq!(
            mount.read_only == Some(true),
            target != "/state",
            "{target}: only the shell's own /state is writable"
        );
    }
    let pnpm = engine.names.pnpm();
    let refused = engine.clean_cache(&pnpm).await.unwrap_err();
    assert!(
        refused.to_string().contains("a shell"),
        "a shared cache a shell mounts stays: {refused}"
    );
    let refused = engine.request_reseed("app").unwrap_err();
    assert!(
        matches!(refused, ControlError::Conflict(_)),
        "a reseed waits for the shell, and says so: {refused}"
    );

    tokio::time::advance(Duration::from_hours(8) + Duration::from_secs(1)).await;
    engine.tick(Shutdown::No).await;
    assert!(
        engine.records.shells.is_empty(),
        "forgotten shells are closed"
    );
    let calls = docker.calls();
    for tty in [&first, &second] {
        assert!(
            calls.contains(&format!("remove {}", tty.container)),
            "{calls:?}"
        );
    }
}

#[tokio::test]
async fn a_failed_jobs_runner_a_stopped_daemon_did_not_keep_becomes_its_record_at_the_next_start() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("crash");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    job_ends(&github, &docker, &mut engine, &handle, FAILED).await;
    drop((engine, handle));

    // The earlier daemon stopped before it collected or committed job 1's runner, and after it committed job 9's.
    docker.0.lock().unwrap().images = vec![record_image(9, 60)];
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &recording());
    let store = handle.store.clone();
    store.queued(
        "o/r",
        &QueuedJob {
            id: 9,
            run_id: 9,
            name: "build".to_owned(),
            ..QueuedJob::default()
        },
        crate::time::now_ms(),
    );
    store.flush().await;
    engine.recover().await.unwrap();
    until_recorded(&mut engine).await;

    let kept = note(&handle, 1).await.unwrap();
    assert!(
        kept.created_at.is_some(),
        "job 1's record is made: {kept:?}"
    );
    let made = note(&handle, 9).await.unwrap();
    assert_eq!(made.bytes, Some(1 << 30), "the record found is noted");
    let calls = docker.calls();
    let commit = calls
        .iter()
        .position(|call| call == "commit container-1 runnerd-record:1");
    let remove = calls.iter().position(|call| call == "remove container-1");
    assert!(
        commit.is_some() && commit < remove,
        "the stopped runner is committed, then goes: {calls:?}"
    );
}

#[tokio::test]
async fn no_shell_opens_while_the_shared_caches_it_mounts_are_trimmed() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().images = vec![record_image(10, 60)];
    let dir = TestDir::new("shell-trim");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &recording());
    engine.recover().await.unwrap();

    engine.trim = Some(tokio::spawn(std::future::pending()));
    let refused = engine.open_shell(10, SIZE).await.unwrap_err();
    assert!(
        matches!(refused, ControlError::Conflict(_)),
        "a shell waits for the trim: {refused}"
    );
    assert!(engine.records.shells.is_empty(), "no shell is open");
}

#[tokio::test(start_paused = true)]
async fn a_left_runner_docker_cannot_read_is_asked_about_less_and_less_and_never_removed() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unread");
    {
        let mut state = docker.0.lock().unwrap();
        state.containers = vec![runner_container(2, 8, false)];
        state
            .container_logs
            .insert("container-2".to_owned(), FAILED.to_owned());
        state
            .diag
            .insert("container-2".to_owned(), docker::tests::diag_archive(5));
        // Docker never manages to read its diagnostic files.
        state.fail_files = u32::MAX;
    }
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &recording());
    let poll = engine.config.host.poll_interval;
    let start = tokio::time::Instant::now();
    let mut reads = Vec::new();
    let mut read = |docker: &FakeDocker| {
        let count = docker
            .calls()
            .iter()
            .filter(|call| call.starts_with("files container-2 "))
            .count();
        if count > reads.len() {
            reads.push(start.elapsed());
        }
    };
    engine.recover().await.unwrap();
    read(&docker);
    // A tick each poll interval for an hour.
    while start.elapsed() < Duration::from_secs(3600) {
        tokio::time::advance(poll).await;
        engine.tick(Shutdown::No).await;
        read(&docker);
    }
    let gaps: Vec<u64> = reads
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]).as_secs())
        .collect();
    assert_eq!(
        gaps[..5],
        [15, 30, 60, 120, 240],
        "it is read again a poll interval later, then twice as long each time: {reads:?}"
    );
    assert!(
        gaps[5..].iter().all(|gap| (480..=615).contains(gap)),
        "never more than 10 minutes apart: {gaps:?}"
    );
    assert!(
        !docker.calls().contains(&"remove container-2".to_owned()),
        "the runner is never removed"
    );
}
