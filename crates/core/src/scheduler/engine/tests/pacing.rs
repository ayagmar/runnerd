//! How soon the engine starts runners for queued jobs on the fakes: idle runners it removes in the same tick do not
//! hold the starts back.

use super::*;
use crate::scheduler::IDLE_TIMEOUT;

/// `o/r` with an idle runner in slot 1 that waited past `IDLE_TIMEOUT`, whose log capture ends only with its
/// container, and a job of `o/s` queued for slot 2.
async fn idle_runner_beside_a_queued_job(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    idle_runner(github, docker);
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
    }
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(5)];
        state.queued_on.insert(5, "o/s".to_owned());
    }
    let (mut engine, handle) = engine_with(github, docker, dir, &two_repositories());
    engine.recover().await.unwrap();
    for occupant in engine.slots.values_mut().flatten().flatten() {
        occupant.started_at -= IDLE_TIMEOUT;
    }
    (engine, handle)
}

#[tokio::test(start_paused = true)]
async fn removing_an_idle_runner_does_not_hold_back_a_start_for_another_repository() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("pacing");
    let (mut engine, handle) = idle_runner_beside_a_queued_job(&github, &docker, &dir).await;

    let began = tokio::time::Instant::now();
    engine.tick(Shutdown::No).await;
    let took = began.elapsed();

    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
    assert_eq!(registered(&github.calls()).len(), 1, "o/s got its runner");
    // The capture of a runner that is still up ends only once its container goes: waiting for it before the
    // removal held every later start of the tick back by the whole drain.
    assert!(
        took < super::super::history::CAPTURE_DRAIN,
        "the tick took {took:?}"
    );
    // The history says why the idle runner went, and why the new one started.
    handle.store.flush().await;
    let history = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    let runners: Vec<(String, Option<String>, Option<String>)> = history
        .prepare("SELECT repository, purpose, stopped_for FROM runners ORDER BY started_at")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        runners,
        [
            ("o/r".to_owned(), None, Some("expired".to_owned())),
            ("o/s".to_owned(), Some("job".to_owned()), None)
        ]
    );
}

/// A workload on the fakes, with the latencies the owner's host shows, for the figures ADR 0092 gives: how long
/// `o/s`'s jobs wait from their queueing to their runner's container start, and what the engine asks GitHub.
/// Every `PERIOD`, three jobs of `o/s` are queued that run `RUN`, and three of `o/r` that another machine takes
/// `TAKEN_AFTER` later, which leaves the runners started for them idle: they are removed as surplus while later
/// jobs of `o/s` wait. A runner's log capture ends only once its container goes, as a real runner's does.
#[tokio::test(start_paused = true)]
#[ignore = "prints a workload's figures; run with --ignored --nocapture"]
#[allow(clippy::print_stdout, reason = "the figures are its output")]
async fn workload_figures() {
    const PERIOD: Duration = Duration::from_secs(40);
    const RUN: Duration = Duration::from_secs(25);
    const TAKEN_AFTER: Duration = Duration::from_secs(20);
    const LENGTH: Duration = Duration::from_mins(12);
    const POLL: Duration = Duration::from_secs(15);

    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("workload");
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        // A request to the API, from the journal's gaps: a listing, a registration, a delete.
        state.latency = Duration::from_millis(300);
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
        state.latency = DockerLatency {
            start: Duration::from_millis(1000),
            remove_running: Duration::from_millis(1000),
        };
    }
    let config = two_repositories().replace("slots = 2", "slots = 8");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();

    let began = tokio::time::Instant::now();
    let waits: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let world = tokio::spawn(world(
        github.clone(),
        docker.clone(),
        waits.clone(),
        began,
        [PERIOD, RUN, TAKEN_AFTER, LENGTH],
    ));
    while began.elapsed() < LENGTH {
        engine.tick(Shutdown::No).await;
        // Another machine took `o/r`'s jobs: its idle runners are surplus at once.
        for occupant in engine.slots.values_mut().flatten().flatten() {
            if occupant.repository == "o/r" && occupant.job.is_none() && !occupant.busy {
                occupant.started_at = std::time::SystemTime::now() - IDLE_TIMEOUT;
            }
        }
        // A runner's name has its start's second on the wall clock, which Tokio's paused one does not move: one
        // wall second between ticks keeps each slot's runners apart.
        std::thread::sleep(Duration::from_millis(1001));
        tokio::time::sleep(POLL).await;
    }
    world.abort();
    drop(handle);

    let mut waits = waits.lock().unwrap().clone();
    waits.sort();
    let at =
        |percent: usize| waits[(waits.len() * percent / 100).min(waits.len() - 1)].as_secs_f64();
    let calls = github.calls();
    let count = |prefix: &str| calls.iter().filter(|call| call.starts_with(prefix)).count();
    println!(
        "o/s jobs {}: queued to container start p50 {:.1}s, p90 {:.1}s, max {:.1}s",
        waits.len(),
        at(50),
        at(90),
        at(100)
    );
    println!(
        "GitHub: {} queued listings, {} runner listings, {} registrations, {} deletes",
        count("queued "),
        count("runners "),
        count("register "),
        count("delete ")
    );
}

/// What happens on GitHub and in the runners while the workload's engine ticks: jobs queued and taken elsewhere,
/// each `o/s` runner up takes the oldest job queued, and its container stops and its registration goes once the
/// job ran. Records each `o/s`
/// job's wait from its queueing to its runner's container start in `waits`.
async fn world(
    github: FakeGithub,
    docker: FakeDocker,
    waits: Arc<Mutex<Vec<Duration>>>,
    began: tokio::time::Instant,
    [period, run, taken_after, length]: [Duration; 4],
) {
    let mut world = World {
        next_id: 1,
        next_burst: began,
        queued_at: HashMap::new(),
        taken_elsewhere: Vec::new(),
        ends: Vec::new(),
        assigned: BTreeSet::new(),
    };
    loop {
        let now = tokio::time::Instant::now();
        if now >= world.next_burst && now - began < length {
            world.burst(&github, now, taken_after);
            world.next_burst += period;
        }
        world.step(&github, &docker, &waits, now, run);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The workload's state between steps.
struct World {
    next_id: u64,
    next_burst: tokio::time::Instant,
    /// When each `o/s` job was queued.
    queued_at: HashMap<u64, tokio::time::Instant>,
    /// When each `o/r` job is taken by another machine.
    taken_elsewhere: Vec<(tokio::time::Instant, u64)>,
    /// When each busy container stops, and its runner's name.
    ends: Vec<(tokio::time::Instant, String, String)>,
    /// The containers that took a job.
    assigned: BTreeSet<String>,
}

impl World {
    fn burst(&mut self, github: &FakeGithub, now: tokio::time::Instant, taken_after: Duration) {
        let mut state = github.0.lock().unwrap();
        for repository in ["o/s", "o/s", "o/s", "o/r", "o/r", "o/r"] {
            let id = self.next_id;
            self.next_id += 1;
            state.queued.push(job(id));
            state.queued_on.insert(id, repository.to_owned());
            if repository == "o/r" {
                self.taken_elsewhere.push((now + taken_after, id));
            } else {
                self.queued_at.insert(id, now);
            }
        }
    }

    fn step(
        &mut self,
        github: &FakeGithub,
        docker: &FakeDocker,
        waits: &Mutex<Vec<Duration>>,
        now: tokio::time::Instant,
        run: Duration,
    ) {
        let mut docker_state = docker.0.lock().unwrap();
        let mut github_state = github.0.lock().unwrap();
        let gone: Vec<u64> = self
            .taken_elsewhere
            .iter()
            .filter(|(at, _)| *at <= now)
            .map(|(_, id)| *id)
            .collect();
        github_state.queued.retain(|job| !gone.contains(&job.id));
        self.taken_elsewhere.retain(|(at, _)| *at > now);
        // Each `o/s` runner up and idle takes the oldest `o/s` job queued.
        let up: Vec<(String, String)> = docker_state
            .containers
            .iter()
            .filter(|container| {
                container.running
                    && container
                        .labels
                        .get("runnerd.repository")
                        .map(String::as_str)
                        == Some("o/s")
                    && !self.assigned.contains(&container.id)
            })
            .map(|container| (container.id.clone(), container.name.clone()))
            .collect();
        for (id, name) in up {
            let Some(job) = github_state
                .queued
                .iter()
                .find(|job| github_state.queued_on.get(&job.id).map(String::as_str) == Some("o/s"))
                .map(|job| job.id)
            else {
                break;
            };
            github_state.queued.retain(|queued| queued.id != job);
            for runner in &mut github_state.runners {
                if runner.name == name {
                    runner.busy = true;
                }
            }
            let mut running = details(job, "in_progress", None);
            running.runner_name = Some(name.clone());
            github_state.jobs.insert(job, running);
            docker_state
                .container_captures
                .insert(id.clone(), format!("@@job {job} {job}\n"));
            let started = docker_state
                .started_on_clock
                .get(&name)
                .copied()
                .unwrap_or(now);
            waits
                .lock()
                .unwrap()
                .push(started.saturating_duration_since(self.queued_at[&job]));
            self.assigned.insert(id.clone());
            self.ends.push((now + run, id, name));
        }
        // A runner whose job ended exits, and GitHub forgets it, as an ephemeral runner's registration goes with
        // its job.
        for (_, id, name) in self.ends.iter().filter(|(at, _, _)| *at <= now) {
            if let Some(container) = docker_state
                .containers
                .iter_mut()
                .find(|container| container.id == *id)
            {
                container.running = false;
            }
            github_state.runners.retain(|runner| runner.name != *name);
        }
        self.ends.retain(|(at, _, _)| *at > now);
    }
}
