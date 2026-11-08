//! How soon the engine starts runners for queued jobs on the fakes: idle runners it removes in the same tick do not
//! hold the starts back, and a slot whose job ended takes the next listed job within seconds, through the engine's
//! own loop (ADR 0095).

use tokio::time::Instant;

use super::*;
use crate::scheduler::IDLE_TIMEOUT;
use crate::scheduler::engine::STOP_DEADLINE;

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
    for occupant in engine.occupants_mut() {
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

    let began = Instant::now();
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

/// What a burst's world saw.
#[derive(Debug, Default)]
struct Seen {
    /// Each job's wait from its queueing to its runner's container start.
    waits: Vec<Duration>,
    /// Each runner start that took the room of one whose job ended while a listed job waited: when that one's
    /// container stopped, and how long until this one's container started.
    turnovers: Vec<(Instant, Duration)>,
    /// Runner containers started.
    starts: usize,
    /// Jobs runners took.
    taken: usize,
    /// Runner starts past the slice's memory, what ran then.
    over_budget: Vec<u64>,
}

/// A runner of the burst with a job.
struct Running {
    container: String,
    name: String,
    job: u64,
    /// When its capture names its job.
    named_at: Instant,
    ends_at: Instant,
}

/// The world around an engine that runs its own loop: `o/r`'s jobs, each taken by an idle runner of this machine
/// `boot` after its container started, named by its log capture `named_after` later, and ending `run` to twice that
/// after it was taken, when its container stops and Docker's events say so, twice, beside one for a container runnerd never ran.
/// GitHub drops an ended runner's registration, as an ephemeral runner's goes with its job.
struct Burst {
    github: FakeGithub,
    docker: FakeDocker,
    boot: Duration,
    run: Duration,
    named_after: Duration,
    /// The slice's `MemoryMax`.
    budget: u64,
    /// Whether removing a runner's container takes 5 to 25 s, different for each, so the slots' generations drift
    /// apart rather than end together.
    slow_removals: bool,
    queued_at: HashMap<u64, Instant>,
    /// The jobs a listing of queued jobs showed.
    listed: BTreeSet<u64>,
    /// Listings of queued jobs so far, and when the world first saw each.
    listings: usize,
    listed_at: Vec<Instant>,
    running: Vec<Running>,
    /// The runner containers seen so far, and those that took a job.
    started: BTreeSet<String>,
    assigned: BTreeSet<String>,
    /// When runners stopped while a listed job waited, oldest first: the next starts take their room.
    freed: VecDeque<Instant>,
    seen: Seen,
}

impl Burst {
    fn new(github: &FakeGithub, docker: &FakeDocker, budget: u64) -> Burst {
        Burst {
            github: github.clone(),
            docker: docker.clone(),
            boot: Duration::from_secs(6),
            run: Duration::from_secs(10),
            named_after: Duration::from_secs(1),
            budget,
            slow_removals: false,
            queued_at: HashMap::new(),
            listed: BTreeSet::new(),
            listings: 0,
            listed_at: Vec::new(),
            running: Vec::new(),
            started: BTreeSet::new(),
            assigned: BTreeSet::new(),
            freed: VecDeque::new(),
            seen: Seen::default(),
        }
    }

    /// GitHub queues `count` jobs of `o/r`.
    fn queue(&mut self, count: u64) {
        let mut github = self.github.0.lock().unwrap();
        let first = github
            .queued
            .iter()
            .map(|job| job.id)
            .max()
            .unwrap_or(0)
            .max(self.queued_at.keys().copied().max().unwrap_or(0))
            + 1;
        for id in first..first + count {
            github.queued.push(job(id));
            github.queued_on.insert(id, "o/r".to_owned());
            self.queued_at.insert(id, Instant::now());
        }
    }

    /// Whether every job queued so far ran.
    fn done(&self) -> bool {
        self.seen.taken == self.queued_at.len() && self.running.is_empty()
    }

    /// One step of the world, at `now`.
    fn step(&mut self, now: Instant) {
        let (github, docker) = (self.github.clone(), self.docker.clone());
        let mut github = github.0.lock().unwrap();
        let mut docker = docker.0.lock().unwrap();
        let listings = github
            .calls
            .iter()
            .filter(|call| call.starts_with("queued "))
            .count();
        if listings > self.listings {
            self.listings = listings;
            self.listed_at.push(now);
            self.listed.extend(github.queued.iter().map(|job| job.id));
        }
        self.see_starts(&mut docker, now);
        self.take_jobs(&mut github, &mut docker, now);
        self.end_jobs(&mut github, &mut docker, now);
    }

    /// Each runner container started since: what runs then fits the slice, and it takes a stopped one's room.
    fn see_starts(&mut self, docker: &mut DockerState, now: Instant) {
        // Each runner container started since: what runs then fits the slice, and it takes a stopped one's room.
        let new: Vec<String> = docker
            .containers
            .iter()
            .filter(|container| {
                container.labels.contains_key("runnerd.job")
                    && !self.started.contains(&container.id)
            })
            .map(|container| container.id.clone())
            .collect();
        for id in new {
            self.started.insert(id.clone());
            self.seen.starts += 1;
            if self.slow_removals {
                let step = u64::try_from(self.seen.starts * 7 % 5).unwrap();
                docker
                    .slow_removes
                    .insert(id.clone(), Duration::from_secs(5 + step * 5));
            }
            let using: u64 = docker
                .containers
                .iter()
                .filter(|container| {
                    container.running && container.labels.contains_key("runnerd.job")
                })
                .filter_map(|container| docker.memory.get(&container.id))
                .sum();
            if using > self.budget {
                self.seen.over_budget.push(using);
            }
            let name = docker
                .containers
                .iter()
                .find(|container| container.id == id)
                .map(|container| container.name.clone())
                .unwrap_or_default();
            let at = docker.started_on_clock.get(&name).copied().unwrap_or(now);
            if let Some(stopped) = self.freed.pop_front() {
                self.seen
                    .turnovers
                    .push((stopped, at.saturating_duration_since(stopped)));
            }
        }
    }

    /// Each runner up `boot` and idle takes the oldest job queued.
    fn take_jobs(&mut self, github: &mut GithubState, docker: &mut DockerState, now: Instant) {
        let idle: Vec<(String, String)> = docker
            .containers
            .iter()
            .filter(|container| {
                container.running
                    && container.labels.contains_key("runnerd.job")
                    && !self.assigned.contains(&container.id)
                    && docker
                        .started_on_clock
                        .get(&container.name)
                        .is_some_and(|at| now.saturating_duration_since(*at) >= self.boot)
            })
            .map(|container| (container.id.clone(), container.name.clone()))
            .collect();
        for (container, name) in idle {
            let Some(job) = github.queued.iter().map(|job| job.id).min() else {
                break;
            };
            github.queued.retain(|queued| queued.id != job);
            for runner in &mut github.runners {
                if runner.name == name {
                    runner.busy = true;
                }
            }
            let mut details = details(job, "in_progress", None);
            details.runner_name = Some(name.clone());
            github.jobs.insert(job, details);
            let started = docker.started_on_clock.get(&name).copied().unwrap_or(now);
            self.seen
                .waits
                .push(started.saturating_duration_since(self.queued_at[&job]));
            self.seen.taken += 1;
            self.assigned.insert(container.clone());
            self.running.push(Running {
                container,
                name,
                job,
                named_at: now + self.named_after,
                // From `run` to twice it, so jobs end apart, as a burst's do.
                ends_at: now + self.run + self.run * u32::try_from(job * 7 % 11).unwrap() / 10,
            });
        }
    }

    /// Names the jobs of runners up `named_after`, and ends those whose time came.
    fn end_jobs(&mut self, github: &mut GithubState, docker: &mut DockerState, now: Instant) {
        for running in &self.running {
            if running.named_at <= now {
                docker
                    .container_captures
                    .entry(running.container.clone())
                    .or_insert_with(|| format!("@@job {0} {0}\n", running.job));
            }
        }
        let (ended, left): (Vec<Running>, Vec<Running>) = std::mem::take(&mut self.running)
            .into_iter()
            .partition(|running| running.ends_at <= now);
        self.running = left;
        // Listed jobs no runner up waits for, past the one the world may be giving a runner: the room of a runner
        // that stops now is theirs.
        let idle = docker
            .containers
            .iter()
            .filter(|container| {
                container.running
                    && container.labels.contains_key("runnerd.job")
                    && !self.assigned.contains(&container.id)
            })
            .count();
        let listed = github
            .queued
            .iter()
            .filter(|job| self.listed.contains(&job.id))
            .count();
        let waiting = listed > idle + 1;
        for running in ended {
            github.runners.retain(|runner| runner.name != running.name);
            let mut details = details(running.job, "completed", Some("success"));
            details.runner_name = Some(running.name.clone());
            github.jobs.insert(running.job, details);
            if let Some(container) = docker
                .containers
                .iter_mut()
                .find(|container| container.id == running.container)
            {
                container.running = false;
            }
            for id in [
                running.container.as_str(),
                running.container.as_str(),
                "not-a-runner",
            ] {
                docker
                    .events
                    .retain(|events| events.send(Ok(id.to_owned())).is_ok());
            }
            if waiting {
                self.freed.push_back(now);
            }
        }
    }
}

/// The fakes for a burst, with the latencies the owner's host shows (ADR 0092), GitHub's listings taking
/// `listing_lag` more.
fn burst_fakes(listing_lag: Duration) -> (FakeGithub, FakeDocker) {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    {
        let mut state = github.0.lock().unwrap();
        state.strict = true;
        // A request to the API, from the journal's gaps: a listing, a registration, a delete.
        state.latency = Duration::from_millis(300);
        state.listing_latency = listing_lag;
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
        // Slot 1's jobs change little: no reseed holds the profile back.
        state.measured = Some(format!("slot 1 changed {} bytes\n", 1 << 20));
        state.latency = DockerLatency {
            start: Duration::from_millis(1000),
            remove_running: Duration::from_millis(1000),
            remove_stopped: Duration::from_millis(300),
        };
    }
    (github, docker)
}

/// Steps `burst` every 100ms until `until` says it is over, or 30 minutes passed.
async fn play(burst: &mut Burst, until: impl Fn(&Burst, Instant) -> bool) {
    let began = Instant::now();
    loop {
        let now = Instant::now();
        burst.step(now);
        if until(burst, now) || now - began > Duration::from_mins(30) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Docker's events follow runner containers as they stop, through a stream that breaks off and two that fail to
/// connect, as when Docker restarts. A slot whose job ended while listed jobs waited takes the next one within
/// seconds, not at the next poll, whatever the listings, a removal or a capture take; no job gets two runners and
/// no start passes the slice's memory. The bug it catches: an exit noticed only by the next poll's inspect, a pass
/// that plans from a listing still showing a job a runner took, and one started while the slice was full.
#[tokio::test(start_paused = true)]
async fn a_slot_whose_job_ended_takes_the_next_listed_job_within_seconds() {
    const JOBS: u64 = 16;
    const G: u64 = 1 << 30;
    let (github, docker) = burst_fakes(Duration::from_secs(3));
    let dir = TestDir::new("turnover");
    let config = CONFIG.replace("slots = 2", "slots = 4");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    // Three 4G runners fit, not four: memory binds before slots.
    engine.memory_max = Some(12 * G + G / 2);
    let mut burst = Burst::new(&github, &docker, 12 * G + G / 2);
    burst.queue(JOBS);
    let run = tokio::spawn(engine.run());
    let broke = Instant::now() + Duration::from_secs(40);
    play(&mut burst, |burst, now| {
        now >= broke && !burst.running.is_empty()
    })
    .await;
    {
        docker.0.lock().unwrap().fail_events = 2;
        docker.break_events();
    }
    play(&mut burst, Burst::done_at).await;
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();

    let seen = &burst.seen;
    assert_eq!(
        seen.taken,
        usize::try_from(JOBS).unwrap(),
        "every job ran: {seen:?}"
    );
    assert_eq!(
        seen.starts,
        usize::try_from(JOBS).unwrap(),
        "no job got a second runner: {seen:?}"
    );
    assert!(
        seen.over_budget.is_empty(),
        "starts passed the slice: {seen:?}"
    );
    // Every slot freed while the events were followed; the others by the next poll or the reconnect's pass.
    let followed: Vec<Duration> = seen
        .turnovers
        .iter()
        .filter(|(stopped, _)| *stopped < broke || *stopped > broke + Duration::from_secs(10))
        .map(|(_, took)| *took)
        .collect();
    assert!(followed.len() >= 8, "too few slots turned over: {seen:?}");
    assert!(
        followed.iter().all(|took| *took <= Duration::from_secs(4)),
        "a slot waited for its next runner: {followed:?}"
    );
    assert!(
        seen.turnovers
            .iter()
            .all(|(_, took)| *took <= Duration::from_secs(20)),
        "a slot freed while Docker's events were down waited past a poll: {:?}",
        seen.turnovers
    );
    assert!(
        docker
            .0
            .lock()
            .unwrap()
            .events
            .iter()
            .all(tokio::sync::mpsc::UnboundedSender::is_closed),
        "the engine stopped following Docker's events as it stopped"
    );
}

/// Waits until the history stored what the engine sent it, with the clock held: its writer is a thread of its own,
/// which a paused clock does not wait for, so under load a tick could read the history before a write it follows.
async fn stored(store: &Store) {
    let store = store.clone();
    let runtime = tokio::runtime::Handle::current();
    // A blocking task holds a paused clock while it runs.
    tokio::task::spawn_blocking(move || runtime.block_on(store.flush()))
        .await
        .unwrap();
}

/// Six of eight slots run long jobs while GitHub takes 1.8 s a request, as on the owner's host, and a job queues.
/// Its runner's container starts within a start's time of the listing that showed it. Then that job's capture names
/// it, and another job queues: it starts as fast, though the tick that lists it is the first to see the job taken.
/// The bugs it catches: the tick reading every running job's details from GitHub, a few requests at a time, before it
/// planned, which held each start back by about half a second per running job; and reading those of a job a runner
/// took since the last tick, though the history's record of the listing that showed it names it.
#[tokio::test(start_paused = true)]
async fn a_job_listed_beside_running_jobs_starts_without_waiting_for_their_reads() {
    /// How long after the first listing that showed it the `nth` job's runner started.
    fn took(burst: &Burst, queued: Instant, nth: usize) -> Duration {
        let started = queued + burst.seen.waits[nth];
        let listed = burst
            .listed_at
            .iter()
            .copied()
            .find(|at| *at >= queued)
            .unwrap();
        started.saturating_duration_since(listed)
    }
    let (github, docker) = burst_fakes(Duration::ZERO);
    github.0.lock().unwrap().latency = Duration::from_millis(1800);
    let dir = TestDir::new("listed-start");
    let config = CONFIG.replace("slots = 2", "slots = 8");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let mut burst = Burst::new(&github, &docker, u64::MAX);
    burst.run = Duration::from_hours(1);
    burst.queue(6);
    let run = tokio::spawn(engine.run());
    // The six run and their captures named their jobs, a few ticks ago.
    let settled = Instant::now() + Duration::from_mins(2);
    play(&mut burst, |_, now| now >= settled).await;
    assert_eq!(burst.running.len(), 6, "six jobs run: {:?}", burst.seen);

    burst.queue(1);
    let seventh = Instant::now();
    play(&mut burst, |burst, _| burst.seen.taken == 7).await;
    // Its capture names it; the next tick is the first to take it.
    let named = Instant::now() + burst.named_after;
    play(&mut burst, |_, now| now >= named).await;
    stored(&handle.store).await;
    burst.queue(1);
    let eighth = Instant::now();
    play(&mut burst, |burst, _| burst.seen.taken == 8).await;
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();

    // A start is its registration, 1.8 s, and its container's start, 1 s; the world sees a listing within 0.1 s of
    // its answer. Reading the six jobs before the plan took two rounds of requests more, and the seventh one more.
    for (nth, queued) in [(6, seventh), (7, eighth)] {
        let took = took(&burst, queued, nth);
        assert!(
            took <= Duration::from_secs(3),
            "job {}'s runner started {took:?} after the listing that showed it",
            nth + 1
        );
    }
}

/// Four long jobs run while GitHub takes 9 s to read a job, as a slow GitHub does: each tick ends with those reads.
/// Listings still come every `poll_interval` (15 s), counted from each tick's start. Then reads take 20 s, longer
/// than the interval, and listings come as each tick ends, 2 s apart. The bugs they catch: the next tick counted
/// from the last one's end, so that every slow read pushed the next listing out by its whole time; and ticks run back
/// to back, with no rest for GitHub, once one outlasts the interval.
#[tokio::test(start_paused = true)]
async fn slow_job_reads_do_not_push_the_next_listing_out() {
    /// The gaps between the listings the world saw over `span` from now.
    async fn gaps(burst: &mut Burst, span: Duration) -> Vec<Duration> {
        let from = burst.listed_at.len();
        let until = Instant::now() + span;
        play(burst, |_, now| now >= until).await;
        burst.listed_at[from..]
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect()
    }
    let (github, docker) = burst_fakes(Duration::ZERO);
    github.0.lock().unwrap().job_latency = Duration::from_secs(9);
    let dir = TestDir::new("fixed-rate");
    let config = CONFIG.replace("slots = 2", "slots = 4");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let mut burst = Burst::new(&github, &docker, u64::MAX);
    burst.run = Duration::from_hours(1);
    burst.queue(4);
    let run = tokio::spawn(engine.run());
    let settled = Instant::now() + Duration::from_mins(2);
    play(&mut burst, |_, now| now >= settled).await;
    assert_eq!(burst.running.len(), 4, "four jobs run: {:?}", burst.seen);

    let read = github.0.lock().unwrap().job_reads;
    let paced = gaps(&mut burst, Duration::from_mins(3)).await;
    assert!(
        github.0.lock().unwrap().job_reads > read,
        "the ticks read the running jobs"
    );
    // The world steps every 100 ms.
    assert!(paced.len() >= 10, "too few listings: {paced:?}");
    assert!(
        paced
            .iter()
            .all(|gap| gap.abs_diff(Duration::from_secs(15)) <= Duration::from_millis(200)),
        "listings left `poll_interval`'s pace: {paced:?}"
    );

    github.0.lock().unwrap().job_latency = Duration::from_secs(20);
    let slow = gaps(&mut burst, Duration::from_mins(3)).await;
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
    // A tick reads the jobs for 20.3 s, then rests 2 s; one tick at the switch still read at 9 s.
    assert!(slow.len() >= 5, "too few listings: {slow:?}");
    assert!(
        slow.iter()
            .skip(1)
            .all(|gap| *gap >= Duration::from_millis(22_300)),
        "ticks ran back to back: {slow:?}"
    );
}

/// A running job's run is cancelled and its runner does not exit, so the tick `CANCEL_GRACE` later removes it before
/// it lists, and Docker takes 8 s to. The next listing still comes `poll_interval` after that tick's. The bug it
/// catches: the next tick counted from the last one's start, so a slow collect before the listing and a quick one
/// after brought two listings 7 s apart, closer than the interval.
#[tokio::test(start_paused = true)]
async fn a_slow_collect_does_not_bring_the_next_listing_sooner() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    docker
        .0
        .lock()
        .unwrap()
        .slow_removes
        .insert("container-1".to_owned(), Duration::from_secs(8));
    let dir = TestDir::new("slow-collect");
    let (engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    handle
        .control
        .pause(one("o/r"), RunningJobs::Kill)
        .await
        .unwrap();
    // The pause starts a tick at once, as every command does: pacing is measured from the listings after it.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut listed_at = Vec::new();
    let mut listings = github
        .calls()
        .iter()
        .filter(|call| call.starts_with("runners "))
        .count();
    let until =
        Instant::now() + crate::scheduler::engine::control::CANCEL_GRACE + Duration::from_mins(1);
    while Instant::now() < until {
        let now = github
            .calls()
            .iter()
            .filter(|call| call.starts_with("runners "))
            .count();
        if now > listings {
            listings = now;
            listed_at.push(Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();

    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
    let gaps: Vec<Duration> = listed_at.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(gaps.len() >= 8, "too few listings: {gaps:?}");
    // The world looks every 100 ms.
    assert!(
        gaps.iter().all(|gap| *gap >= Duration::from_millis(14_900)),
        "two listings came closer than `poll_interval`: {gaps:?}"
    );
}

/// Four slots work through a long backlog while each removal takes 5 to 25 s, so a slot's next runner ends while
/// another's release still waits, generation after generation. Polls keep their pace and a command is answered
/// within a pass's bound. The bug it catches: a pass taking every later exit for as long as some release waited,
/// which, with a backlog, held the engine until it drained: no poll, no command.
#[tokio::test(start_paused = true)]
async fn overlapping_slow_releases_leave_the_polls_and_commands_their_turn() {
    const JOBS: u64 = 48;
    let (github, docker) = burst_fakes(Duration::ZERO);
    let dir = TestDir::new("overlapping-releases");
    let config = CONFIG.replace("slots = 2", "slots = 4");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let mut burst = Burst::new(&github, &docker, u64::MAX);
    burst.boot = Duration::from_secs(1);
    burst.run = Duration::from_secs(3);
    burst.slow_removals = true;
    burst.queue(JOBS);
    let run = tokio::spawn(engine.run());
    let command = async {
        tokio::time::sleep(Duration::from_mins(1)).await;
        let sent = Instant::now();
        handle
            .control
            .replace_config(
                Config::parse(&config).unwrap(),
                None,
                None,
                Commit::new(|| Ok(None)),
                RunningSlots::Keep,
                Author::Owner,
            )
            .await
            .unwrap();
        sent.elapsed()
    };
    let ((), answered) = tokio::join!(play(&mut burst, Burst::done_at), command);
    let drained = Instant::now();
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();

    assert_eq!(
        burst.seen.taken,
        usize::try_from(JOBS).unwrap(),
        "every job ran: {:?}",
        burst.seen
    );
    assert!(
        answered < Duration::from_secs(90),
        "the command waited {answered:?}"
    );
    let gaps: Vec<Duration> = burst
        .listed_at
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .chain(burst.listed_at.last().map(|last| drained - *last))
        .collect();
    // A poll interval, the pass before the tick and the one the tick holds, each its join window and its slowest
    // removal.
    assert!(
        gaps.iter().all(|gap| *gap < Duration::from_secs(90)),
        "a poll waited for the backlog: {gaps:?}"
    );
}

/// The containers of `o/r`'s runners in the first profile's slots, as the status says.
fn runner_containers(handle: &Handle) -> Vec<String> {
    handle.status.borrow().profiles[0]
        .slots
        .iter()
        .filter_map(|slot| slot.runner.as_ref().map(|runner| runner.container.clone()))
        .collect()
}

/// A runner that stops before its start returned, and one someone else removed, free their slots on Docker's word
/// within seconds, with the next poll an hour away. The bugs it catches: the hint for a container the engine did not
/// track yet dropped, so its slot waits for the poll; a container gone by the pass's inspect kept as if it still ran.
#[tokio::test(start_paused = true)]
async fn runners_that_stop_at_once_or_are_removed_free_their_slots_before_the_next_poll() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    github.0.lock().unwrap().queued = vec![job(1), job(2)];
    docker.0.lock().unwrap().exit_at_start = 1;
    let dir = TestDir::new("early-exits");
    let config = CONFIG.replace("[host]\n", "[host]\npoll_interval = \"1h\"\n");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());

    tokio::time::sleep(Duration::from_secs(5)).await;
    let left = runner_containers(&handle);
    assert_eq!(
        left.len(),
        1,
        "the runner that stopped as it started left its slot: {:?}",
        docker.calls()
    );
    {
        let mut state = docker.0.lock().unwrap();
        state.containers.retain(|container| container.id != left[0]);
        state
            .events
            .retain(|events| events.send(Ok(left[0].clone())).is_ok());
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = runner_containers(&handle);
    assert!(
        !now.contains(&left[0]),
        "the removed runner left its slot: {now:?}"
    );

    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}

impl Burst {
    fn done_at(&self, _: Instant) -> bool {
        self.done()
    }
}

/// A burst for the figures ADR 0092 and 0095 give, with the latencies the owner's host shows: `BURSTS` bursts of
/// `JOBS` short jobs of `o/r` every `PERIOD`, more than its 8 slots take, through the engine's own loop. Prints how
/// long jobs waited from their queueing to their runner's container start, how long a slot whose job ended waited
/// for its next runner while listed jobs waited, and what the engine asked GitHub per hour; first with Docker's
/// events not followed, as before them, then followed.
#[tokio::test(start_paused = true)]
#[ignore = "prints a workload's figures; run with --ignored --nocapture"]
#[allow(clippy::print_stdout, reason = "the figures are its output")]
#[allow(
    clippy::cast_precision_loss,
    reason = "counts per hour, far below 2^52"
)]
async fn workload_figures() {
    const BURSTS: u64 = 6;
    const JOBS: u64 = 40;
    const PERIOD: Duration = Duration::from_mins(5);
    for followed in [false, true] {
        let (github, docker) = burst_fakes(Duration::ZERO);
        if !followed {
            docker.0.lock().unwrap().fail_events = u32::MAX;
        }
        let dir = TestDir::new("workload");
        let config = CONFIG.replace("slots = 2", "slots = 8");
        let (engine, handle) = engine_with(&github, &docker, &dir, &config);
        let mut burst = Burst::new(&github, &docker, u64::MAX);
        let run = tokio::spawn(engine.run());
        let began = Instant::now();
        for _ in 0..BURSTS {
            burst.queue(JOBS);
            let next = Instant::now() + PERIOD;
            play(&mut burst, |_, now| now >= next).await;
        }
        let hours = began.elapsed().as_secs_f64() / 3600.0;
        handle.shutdown(Shutdown::Force);
        run.await.unwrap();

        let figures = |mut durations: Vec<Duration>| {
            durations.sort();
            let at = |percent: usize| {
                durations[(durations.len() * percent / 100).min(durations.len() - 1)].as_secs_f64()
            };
            format!(
                "p50 {:.1}s, p90 {:.1}s, max {:.1}s over {}",
                at(50),
                at(90),
                at(100),
                durations.len()
            )
        };
        let seen = &burst.seen;
        println!(
            "Docker's events {}:",
            if followed { "followed" } else { "not followed" }
        );
        println!(
            "  queued to container start: {}",
            figures(seen.waits.clone())
        );
        println!(
            "  a slot's job ended to its next container start: {}",
            figures(seen.turnovers.iter().map(|(_, took)| *took).collect())
        );
        let calls = github.calls();
        let per_hour = |prefix: &str| {
            calls.iter().filter(|call| call.starts_with(prefix)).count() as f64 / hours
        };
        println!(
            "  GitHub per hour: {:.0} queued listings, {:.0} runner listings, {:.0} job reads, {:.0} registrations, \
             {:.0} deletes",
            per_hour("queued "),
            per_hour("runners "),
            github.0.lock().unwrap().job_reads as f64 / hours,
            per_hour("register "),
            per_hour("delete ")
        );
    }
}

/// Stops a runner container and says so in Docker's events.
fn stop_container(docker: &FakeDocker, id: &str) {
    let mut state = docker.0.lock().unwrap();
    for container in &mut state.containers {
        if container.id == id {
            container.running = false;
        }
    }
    state
        .events
        .retain(|events| events.send(Ok(id.to_owned())).is_ok());
}

/// GitHub completes `job`, which `runner` ran, and drops the runner's registration.
fn complete(github: &FakeGithub, job: u64, runner: &str) {
    let mut state = github.0.lock().unwrap();
    state.queued.retain(|queued| queued.id != job);
    state.runners.retain(|registered| registered.name != runner);
    let mut completed = details(job, "completed", Some("success"));
    completed.runner_name = Some(runner.to_owned());
    state.jobs.insert(job, completed);
}

/// A runner takes job 1 while GitHub still lists it queued, and its capture names it; the tick's read of that job
/// waits while the job ends and a pass releases the runner, then says it completed. The bug it catches: the end of
/// the job's history dropping the engine's knowledge that a runner of this machine took it, so the tick, still
/// holding a listing that shows it queued, started a second runner for it.
#[tokio::test(start_paused = true)]
async fn a_job_that_ended_while_the_tick_read_it_gets_no_second_runner() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    {
        let mut state = docker.0.lock().unwrap();
        state.captures_wait = Some(github.clone());
        state.capture_end = CaptureEnd::Container;
    }
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("closing");
    let (engine, handle) = engine_in(&github, &docker, &dir);
    let run = tokio::spawn(engine.run());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let name = registered(&github.calls())[0]
        .strip_prefix("register ")
        .unwrap()
        .to_owned();
    {
        let mut state = github.0.lock().unwrap();
        for runner in &mut state.runners {
            if runner.name == name {
                runner.busy = true;
            }
        }
        let mut running = details(1, "in_progress", None);
        running.runner_name = Some(name.clone());
        state.jobs.insert(1, running);
    }
    docker
        .0
        .lock()
        .unwrap()
        .container_captures
        .insert(name.clone(), "@@job 1 1\n".to_owned());
    github.hold_jobs();
    // The next poll lists job 1 queued still, and its read waits.
    tokio::time::sleep(Duration::from_secs(15)).await;
    complete(&github, 1, &name);
    stop_container(&docker, &name);
    tokio::time::sleep(Duration::from_secs(3)).await;
    github.release_jobs();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 1 got one runner: {:?}",
        github.calls()
    );
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}

/// With one slot, job 2 waits behind job 1's runner. A poll lists `o/r`'s queue, job 2 in it, while `o/s`'s listing
/// waits; meanwhile job 1 ends, a pass starts a runner that takes job 2, whose capture names nothing, and that runner
/// ends too. The bug it catches: the listing that began before that runner took job 2 clearing the engine's
/// suspicion of it once it answers, so the tick started a second runner for job 2.
#[tokio::test(start_paused = true)]
async fn a_listing_older_than_a_runners_unnamed_job_does_not_bring_the_job_back() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(2)];
        state.queued_on.insert(2, "o/r".to_owned());
        let mut running = details(1, "in_progress", None);
        running.runner_name = Some(state.runners[0].name.clone());
        state.jobs.insert(1, running);
    }
    let dir = TestDir::new("generation");
    let config = two_repositories().replace("slots = 2", "slots = 1");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());
    tokio::time::sleep(Duration::from_secs(5)).await;
    github.hold_queued("o/s");
    // The next poll: `o/r`'s queue answers with job 2, `o/s`'s waits.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let first = github.0.lock().unwrap().runners[0].name.clone();
    complete(&github, 1, &first);
    stop_container(&docker, "container-1");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let calls = github.calls();
    let started = registered(&calls);
    assert_eq!(
        started.len(),
        1,
        "a pass started a runner for job 2: {calls:?}"
    );
    let second = started[0].strip_prefix("register ").unwrap().to_owned();
    {
        let mut state = github.0.lock().unwrap();
        state.queued.clear();
        let mut ran = details(2, "completed", Some("success"));
        ran.runner_name = Some(second.clone());
        state.jobs.insert(2, ran);
        state.runners.retain(|runner| runner.name != second);
    }
    stop_container(&docker, &second);
    tokio::time::sleep(Duration::from_secs(3)).await;
    github.release_queued();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 2 got one runner: {:?}",
        github.calls()
    );
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}

/// How one runner's release is held up.
#[derive(Debug, Clone, Copy)]
enum Slow {
    /// Its log capture never ends: the release waits out `CAPTURE_DRAIN`.
    Capture,
    /// Docker takes a minute to remove its container.
    Removal,
}

/// When two runners end, the second at once or once the first's release began.
#[derive(Debug, Clone, Copy)]
enum Ends {
    Together,
    SlowFirst,
}

/// Two runners end while listed jobs wait; one's release is slow. The other's slot takes the next job within seconds,
/// and the slow one's slot stays held until Docker removed its container. The bugs it catches: a pass releasing the
/// runners one after the other before starting any, so an unrelated slow release held every slot; and a pass
/// waiting out the releases it began before it heeds a runner that ended since.
#[tokio::test(start_paused = true)]
async fn a_slow_release_does_not_hold_back_another_slots_next_runner() {
    for (slow, ends) in [
        (Slow::Capture, Ends::Together),
        (Slow::Removal, Ends::Together),
        (Slow::Capture, Ends::SlowFirst),
        (Slow::Removal, Ends::SlowFirst),
    ] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        running_job(&github, &docker, "o/r", 1, 1);
        running_job(&github, &docker, "o/r", 2, 2);
        {
            let mut state = github.0.lock().unwrap();
            state.queued = vec![job(3), job(4)];
            for id in [1, 2] {
                let name = state.runners[usize::try_from(id - 1).unwrap()].name.clone();
                let mut running = details(id, "in_progress", None);
                running.runner_name = Some(name);
                state.jobs.insert(id, running);
            }
        }
        {
            let mut state = docker.0.lock().unwrap();
            match slow {
                Slow::Capture => {
                    state.stuck_captures.insert("container-1".to_owned());
                }
                Slow::Removal => {
                    state
                        .slow_removes
                        .insert("container-1".to_owned(), Duration::from_mins(1));
                }
            }
        }
        let dir = TestDir::new("slow-release");
        let (engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
        let run = tokio::spawn(engine.run());
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            registered(&github.calls()).is_empty(),
            "both slots are busy"
        );
        for (id, slot) in [(1, 1), (2, 2)] {
            let name = github.0.lock().unwrap().runners[0].name.clone();
            complete(&github, id, &name);
            stop_container(&docker, &format!("container-{slot}"));
            if let Ends::SlowFirst = ends {
                // Slot 1's release begins and waits.
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(4)).await;

        let calls = github.calls();
        let started: Vec<&String> = registered(&calls);
        assert_eq!(
            started.len(),
            1,
            "{slow:?}, {ends:?}: slot 2 took a job, slot 1 waits for its release: {calls:?}"
        );
        assert!(
            started[0].contains("-app-2-"),
            "{slow:?}, {ends:?}: the runner started in slot 2: {started:?}"
        );
        handle.shutdown(Shutdown::Force);
        run.await.unwrap();
    }
}

/// Two runners end while listed jobs wait and GitHub does not answer deletes. Slot 1's runner logged its job's end, so
/// GitHub dropped its registration with the job: nothing deletes it. Slot 2's log stops before its job's end, as when
/// the runner was killed, so its registration is deleted, but its slot takes the next job on Docker's word alone. The
/// bugs it catches: every release waiting for GitHub's answer to a delete before freeing its slot, and a delete sent
/// for each runner whose job ended, which GitHub answers "not found".
#[tokio::test(start_paused = true)]
async fn a_slot_takes_its_next_job_without_waiting_for_the_runners_delete() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_job(&github, &docker, "o/r", 1, 1);
    running_job(&github, &docker, "o/r", 2, 2);
    github.0.lock().unwrap().queued = vec![job(3), job(4)];
    docker.0.lock().unwrap().container_logs.insert(
        "container-1".to_owned(),
        "Running job: build\nJob build completed with result: Succeeded\n".to_owned(),
    );
    let dir = TestDir::new("delete-off-turnover");
    let config = CONFIG.replace("[host]\n", "[host]\npoll_interval = \"1h\"\n");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        registered(&github.calls()).is_empty(),
        "both slots are busy"
    );
    github.hold_deletes();
    let ids: Vec<u64> = github
        .0
        .lock()
        .unwrap()
        .runners
        .iter()
        .map(|runner| runner.id)
        .collect();
    for (job, slot) in [(1, 1), (2, 2)] {
        let name = github.0.lock().unwrap().runners[0].name.clone();
        complete(&github, job, &name);
        stop_container(&docker, &format!("container-{slot}"));
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    let calls = github.calls();
    assert_eq!(
        registered(&calls).len(),
        2,
        "both slots took a listed job: {calls:?}"
    );
    assert!(
        !calls.contains(&format!("delete {}", ids[0])),
        "the runner whose job completed is not deleted: {calls:?}"
    );
    assert!(
        calls.contains(&format!("delete {}", ids[1])),
        "the runner whose log stops before its job's end is deleted: {calls:?}"
    );
    github.release_deletes();
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}

/// A runner whose log stops before its job's end is released while GitHub holds deletes, and a graceful stop is asked.
/// Returns the running engine, once the runner's delete was sent.
async fn stop_with_a_pending_delete(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> tokio::task::JoinHandle<()> {
    running_job(github, docker, "o/r", 1, 1);
    let config = CONFIG.replace("[host]\n", "[host]\npoll_interval = \"1h\"\n");
    let (engine, handle) = engine_with(github, docker, dir, &config);
    let run = tokio::spawn(engine.run());
    until_state(&handle, 1, JobState::Running).await;
    github.hold_deletes();
    let runner = github.0.lock().unwrap().runners[0].clone();
    complete(github, 1, &runner.name);
    stop_container(docker, "container-1");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        github.calls().contains(&format!("delete {}", runner.id)),
        "the released runner's delete is sent: {:?}",
        github.calls()
    );
    handle.shutdown(Shutdown::Graceful);
    run
}

/// The bug it catches: the stop returning while the delete still waits for GitHub, so the daemon's exit drops it and
/// the registration stays on GitHub, offline, until runnerd starts again.
#[tokio::test(start_paused = true)]
async fn a_stop_waits_for_a_released_runners_delete() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("stop-waits-for-delete");
    let run = stop_with_a_pending_delete(&github, &docker, &dir).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(!run.is_finished(), "the stop waits for the delete");
    github.release_deletes();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the stop ends once GitHub answers")
        .unwrap();
}

/// The bug it catches: a stop that never ends while GitHub does not answer a delete.
#[tokio::test(start_paused = true)]
async fn a_stop_waits_for_a_delete_github_does_not_answer_only_until_its_deadline() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("stop-delete-deadline");
    let run = stop_with_a_pending_delete(&github, &docker, &dir).await;
    tokio::time::timeout(STOP_DEADLINE + Duration::from_secs(10), run)
        .await
        .expect("the stop ends by its deadline")
        .unwrap();
}

/// A runner started for job 1 takes it, and its capture names it, with the next poll an hour away. The bug it
/// catches: status showing the runner starting, still waiting for its job, until the next listing said it was busy.
#[tokio::test(start_paused = true)]
async fn a_runner_whose_capture_named_its_job_shows_running_before_the_next_poll() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().captures_wait = Some(github.clone());
    github.0.lock().unwrap().queued = vec![job(1)];
    let dir = TestDir::new("named-job-status");
    let config = CONFIG.replace("[host]\n", "[host]\npoll_interval = \"1h\"\n");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());
    let state = |handle: &Handle| {
        let status = handle.status.borrow();
        api::Status::slot_state(&status.profiles[0], &status.profiles[0].slots[0])
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(state(&handle).word(), "starting", "{:?}", docker.calls());

    let name = registered(&github.calls())[0]
        .strip_prefix("register ")
        .unwrap()
        .to_owned();
    docker
        .0
        .lock()
        .unwrap()
        .container_captures
        .insert(name, "@@job 1 1\n".to_owned());
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(state(&handle).word(), "running");

    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}

/// With one slot, job 2 of `o/r` waits behind job 1's runner. A poll lists `o/r`'s queue, job 2 in it, while `o/s`'s
/// listing waits; meanwhile job 1 ends, a pass starts a runner that takes job 2 and names it, that runner ends too,
/// and `o/r` is removed from the config and added back. The bug it catches: the engine forgetting with the
/// repository that this machine ran job 2, so the listing that began before, once it answers, started a second
/// runner for it.
#[tokio::test(start_paused = true)]
async fn a_repository_removed_and_added_back_keeps_what_its_runners_took() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker.0.lock().unwrap().captures_wait = Some(github.clone());
    running_job(&github, &docker, "o/r", 1, 1);
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(2)];
        state.queued_on.insert(2, "o/r".to_owned());
        let mut running = details(1, "in_progress", None);
        running.runner_name = Some(state.runners[0].name.clone());
        state.jobs.insert(1, running);
    }
    let dir = TestDir::new("readded");
    let config = two_repositories().replace("slots = 2", "slots = 1");
    let (engine, handle) = engine_with(&github, &docker, &dir, &config);
    let run = tokio::spawn(engine.run());
    tokio::time::sleep(Duration::from_secs(5)).await;
    github.hold_queued("o/s");
    // The next poll: `o/r`'s queue answers with job 2, `o/s`'s waits.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let first = github.0.lock().unwrap().runners[0].name.clone();
    complete(&github, 1, &first);
    stop_container(&docker, "container-1");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let calls = github.calls();
    let started = registered(&calls);
    assert_eq!(
        started.len(),
        1,
        "a pass started a runner for job 2: {calls:?}"
    );
    let second = started[0].strip_prefix("register ").unwrap().to_owned();
    docker
        .0
        .lock()
        .unwrap()
        .container_captures
        .insert(second.clone(), "@@job 2 2\n".to_owned());
    tokio::time::sleep(Duration::from_secs(1)).await;
    complete(&github, 2, &second);
    stop_container(&docker, &second);
    tokio::time::sleep(Duration::from_secs(3)).await;
    for config in [
        config.replace("name = \"o/r\"", "name = \"o/t\""),
        config.clone(),
    ] {
        handle
            .control
            .replace_config(
                Config::parse(&config).unwrap(),
                None,
                None,
                Commit::new(|| Ok(None)),
                RunningSlots::Keep,
                Author::Owner,
            )
            .await
            .unwrap();
    }
    // GitHub still lists job 2 queued, as it does for a while after a runner took it.
    github.0.lock().unwrap().queued = vec![job(2)];
    github.release_queued();
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert_eq!(
        registered(&github.calls()).len(),
        1,
        "job 2 got one runner: {:?}",
        github.calls()
    );
    handle.shutdown(Shutdown::Force);
    run.await.unwrap();
}
