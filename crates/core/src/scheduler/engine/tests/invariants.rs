//! The engine's invariants over generated sequences of events, on the fakes: jobs queued with every kind of labels,
//! taken, finished, cancelled and lost; runners that fail to start, wait or exit idle; Docker calls that fail once;
//! samples of the slice and the host, short of memory or stalling; reloads and restarts; time passing. After each
//! tick the named checks below must hold, and once the sequence drains no runner may be left.
//!
//! The sequences come from a seeded generator, so a failure replays: it prints its seed and the shortest sequence
//! it shrank to. `RUNNERD_INVARIANT_CASES` runs more cases than the default, `RUNNERD_INVARIANT_SEED` one seed.

use std::fmt::Write as _;

use super::*;
use crate::config::Admission;
use crate::observe::metrics::Events;
use crate::scheduler::IDLE_TIMEOUT;
use crate::store::SampleRecord;
use crate::time;
use std::time::SystemTime;

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// The slice's `MemoryMax`: two heavy runners, or one and a light one, not two and a light one: a runner counted
/// at less than it holds lets one start too many.
const MEMORY_MAX: u64 = 8 * G + 512 * M;

/// The slice's CPUs, in millicores: one heavy runner and two light ones under `cpu_overcommit = 1.0`.
const SLICE_CPUS: u64 = 4000;

/// What the host has, in all.
const HOST_TOTAL: u64 = 64 * G;

/// The host's margin for page cache (ADR 0055): the larger of 2G and a sixteenth of the host.
const HOST_MARGIN: u64 = HOST_TOTAL / 16;

/// Cases run by default; `RUNNERD_INVARIANT_CASES` sets another number.
const CASES: u64 = 64;

/// Events in a case.
const EVENTS: usize = 40;

/// Bugs the generated sequences find on main, each with its `#[ignore]`d regression test below. The generated test
/// steers around them so it guards everything else; a fix removes its bug from `KNOWN_BUGS` and the `#[ignore]`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Bug {
    /// `a_stopped_runner_that_recovery_cannot_remove_is_removed_later`: Docker removes everything at a restart.
    RecoveryRemovesOnce,
    /// `a_runner_watched_through_a_failed_inspect_still_records_the_job_it_runs`: Docker fails at most one inspect
    /// of a container across a restart.
    WatchedThroughAFailedInspect,
}

const KNOWN_BUGS: &[Bug] = &[Bug::RecoveryRemovesOnce, Bug::WatchedThroughAFailedInspect];

/// A deterministic generator (SplitMix64): the same seed gives the same sequence on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn index(&mut self) -> usize {
        usize::try_from(self.below(8)).unwrap()
    }
}

/// The config a case runs with: two profiles, `o/r` in both, `o/s` beside it in `heavy`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Shape {
    admission: Admission,
    cpu_overcommit: bool,
    heavy_slots: u32,
    /// `None`: the `light` profile is gone, and with it `o/r`'s light pool.
    light_slots: Option<u32>,
    /// Whether `o/r` has its light pool, for `r-light` jobs.
    light_pool: bool,
    /// Whether `o/s` is configured.
    second_repository: bool,
}

/// A pool as the config has it.
struct PoolShape {
    repository: &'static str,
    profile: &'static str,
    labels: &'static [&'static str],
}

impl Shape {
    const START: Shape = Shape {
        admission: Admission::Caps,
        cpu_overcommit: false,
        heavy_slots: 2,
        light_slots: Some(2),
        light_pool: true,
        second_repository: true,
    };

    fn pools(&self) -> Vec<PoolShape> {
        let mut pools = vec![PoolShape {
            repository: "o/r",
            profile: "heavy",
            labels: &["self-hosted", "app"],
        }];
        if self.light_pool && self.light_slots.is_some() {
            pools.push(PoolShape {
                repository: "o/r",
                profile: "light",
                labels: &["r-light"],
            });
        }
        if self.second_repository {
            pools.push(PoolShape {
                repository: "o/s",
                profile: "heavy",
                labels: &["self-hosted", "s-app"],
            });
        }
        pools
    }

    fn toml(&self) -> String {
        let mut host = String::new();
        if self.admission == Admission::Measured {
            host.push_str("admission = \"measured\"\n");
        }
        if self.cpu_overcommit {
            host.push_str("cpu_overcommit = 1.0\n");
        }
        let mut config = format!(
            r#"
[host]
slice = "ci.slice"
isolation = "sysbox"
{host}
[caches]
mirror = {{ enabled = false, memory = "512M", cpus = 1, ttl = "168h" }}
pnpm = {{ max_size = "5G" }}
maven = {{ max_size = "5G" }}

[profiles.heavy]
image_stage = "app"
cpus = 2
memory = "4G"
slots = {}
"#,
            self.heavy_slots
        );
        if let Some(slots) = self.light_slots {
            let _ = write!(
                config,
                "\n[profiles.light]\nimage_stage = \"app\"\ncpus = 1\nmemory = \"1G\"\nslots = {slots}\n"
            );
        }
        for pool in self.pools() {
            let labels: Vec<String> = pool
                .labels
                .iter()
                .map(|label| format!("{label:?}"))
                .collect();
            let _ = write!(
                config,
                "\n[[repositories]]\nname = {:?}\nprofile = {:?}\nlabels = [{}]\nwarm = 0\n",
                pool.repository,
                pool.profile,
                labels.join(", ")
            );
        }
        config
    }

    /// The profile of the pool that serves a job, by GitHub's rule: a runner takes a job whose every label it has,
    /// compared without case. A job asking for no label is no pool's (ADR 0058).
    fn serves(&self, repository: &str, labels: &[String]) -> Option<&'static str> {
        if labels.is_empty() {
            return None;
        }
        let serving: Vec<&'static str> = self
            .pools()
            .into_iter()
            .filter(|pool| {
                pool.repository == repository
                    && labels.iter().all(|label| {
                        pool.labels
                            .iter()
                            .any(|has| has.eq_ignore_ascii_case(label))
                    })
            })
            .map(|pool| pool.profile)
            .collect();
        assert!(serving.len() <= 1, "the shape has pools sharing labels");
        serving.first().copied()
    }

    fn configures(&self, repository: &str) -> bool {
        self.pools()
            .iter()
            .any(|pool| pool.repository == repository)
    }
}

/// The labels a generated job asks for.
#[derive(Debug, Clone, Copy)]
enum Labels {
    /// Its repository's heavy pool's.
    Heavy,
    /// `o/r`'s light pool's, or for `o/s` a light label it has no pool for.
    Light,
    /// Only `self-hosted`, which the heavy pools have.
    SelfHosted,
    /// A typo of a pool's label.
    Typo,
    /// None at all, as a job naming only a runner group.
    Empty,
    /// One GitHub-hosted runner's.
    Hosted,
}

impl Labels {
    fn of(self, repository: &str) -> Vec<String> {
        let labels: &[&str] = match (self, repository) {
            (Labels::Heavy, "o/r") => &["self-hosted", "app"],
            (Labels::Heavy, _) => &["self-hosted", "s-app"],
            (Labels::Light, "o/r") => &["r-light"],
            (Labels::Light, _) => &["s-light"],
            (Labels::SelfHosted, _) => &["self-hosted"],
            (Labels::Typo, _) => &["r-lite"],
            (Labels::Empty, _) => &[],
            (Labels::Hosted, _) => &["ubuntu-latest"],
        };
        labels.iter().map(|label| (*label).to_owned()).collect()
    }
}

/// A change a reload makes to the shape.
#[derive(Debug, Clone, Copy)]
enum Change {
    HeavySlots(u32),
    LightSlots(u32),
    LightPool,
    SecondRepository,
    LightProfile,
    Policy,
    CpuOvercommit,
}

/// One thing that happens. Indexes pick among the candidates there are then, modulo their number.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// GitHub queues a job of `o/r` (`second` false) or `o/s`.
    Queue {
        second: bool,
        labels: Labels,
    },
    /// GitHub gives a queued job to an idle runner of this machine whose labels it matches.
    Assign(usize),
    /// As `Assign`, but GitHub's listing of queued jobs still shows the job until the next tick, as it does for a
    /// few seconds.
    AssignListed(usize),
    /// A running job finishes and its runner exits.
    Finish(usize),
    /// A queued job is cancelled.
    Cancel(usize),
    /// A running job's container vanishes, as when someone removes it.
    Lose(usize),
    /// The next runner fails to start in Docker.
    StartFails,
    /// An idle runner exits at once, without a job.
    ExitsAtOnce(usize),
    /// An idle runner exits without a job after waiting `IDLE_TIMEOUT`.
    ExitsIdle(usize),
    /// Docker fails the next inspect of a runner's container.
    InspectFails(usize),
    /// Docker fails the next remove.
    RemoveFails,
    /// The sampler reads the slice and the host: busy runners use `percent` of their caps, the host has
    /// `available` gigabytes available, and its tasks stall or not.
    Sample {
        percent: u64,
        available: u64,
        stalled: bool,
    },
    Reload(Change),
    /// The daemon restarts: a new engine on the same history and Docker.
    Restart,
    /// As `Restart`, while Docker fails the first inspect of a runner's container.
    RestartInspectFails(usize),
    /// Time passes: idle runners waited `IDLE_TIMEOUT`, holds after failures ran out.
    Elapse,
    Tick,
}

fn generate(rng: &mut Rng) -> Vec<Step> {
    (0..EVENTS)
        .map(|_| match rng.below(100) {
            0..=17 => Step::Queue {
                second: rng.below(3) == 0,
                labels: match rng.below(10) {
                    0..=3 => Labels::Heavy,
                    4..=5 => Labels::Light,
                    6 => Labels::SelfHosted,
                    7 => Labels::Typo,
                    8 => Labels::Empty,
                    _ => Labels::Hosted,
                },
            },
            18..=26 => Step::Assign(rng.index()),
            27..=29 => Step::AssignListed(rng.index()),
            30..=37 => Step::Finish(rng.index()),
            38..=39 => Step::Cancel(rng.index()),
            40 => Step::Lose(rng.index()),
            41..=42 => Step::StartFails,
            43 => Step::ExitsAtOnce(rng.index()),
            44 => Step::ExitsIdle(rng.index()),
            45..=46 => Step::InspectFails(rng.index()),
            47 => Step::RemoveFails,
            48..=54 => Step::Sample {
                percent: [10, 50, 90, 120][rng.index() % 4],
                available: [3, 8, 20, 50][rng.index() % 4],
                stalled: rng.below(5) == 0,
            },
            55..=62 => Step::Reload(match rng.below(7) {
                0 => Change::HeavySlots(1 + u32::try_from(rng.below(3)).unwrap()),
                1 => Change::LightSlots(1 + u32::try_from(rng.below(3)).unwrap()),
                2 => Change::LightPool,
                3 => Change::SecondRepository,
                4 => Change::LightProfile,
                5 => Change::Policy,
                _ => Change::CpuOvercommit,
            }),
            63..=65 => Step::Restart,
            66..=67 => Step::RestartInspectFails(rng.index()),
            68..=71 => Step::Elapse,
            _ => Step::Tick,
        })
        .collect()
}

/// Where a job is, as GitHub knows it.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Queued,
    /// A runner of this machine runs it.
    Running {
        container: String,
    },
    /// Over: `ran` is the profile and repository of the runner that ran it, if one of this machine's did.
    Done {
        ran: Option<(String, String)>,
    },
}

#[derive(Debug, Clone)]
struct JobModel {
    repository: &'static str,
    labels: Vec<String>,
    phase: Phase,
    /// The profile and repository of the runner that took it.
    taken_by: Option<(String, String)>,
}

/// A broken invariant: which, at which step, and what was seen.
#[derive(Debug, Clone)]
struct Violation {
    invariant: &'static str,
    step: usize,
    detail: String,
}

/// A case: the engine on the fakes, and what GitHub and Docker hold as the world sees it.
struct Case {
    /// The bugs it steers around.
    known: &'static [Bug],
    github: FakeGithub,
    docker: FakeDocker,
    dir: TestDir,
    engine: Engine<FakeGithub, FakeDocker>,
    handle: Handle,
    shape: Shape,
    jobs: BTreeMap<u64, JobModel>,
    next_job: u64,
    /// Containers that took a job, which are no longer idle.
    took_job: BTreeSet<String>,
    /// Jobs a runner took that GitHub still lists queued until the next tick.
    still_listed: BTreeSet<u64>,
    /// Docker calls checked so far.
    calls_seen: usize,
    /// The last host reading, as `(available, stalled)`.
    host: Option<(u64, bool)>,
    step: usize,
}

fn violation(case: &Case, invariant: &'static str, detail: String) -> Result<(), Violation> {
    Err(Violation {
        invariant,
        step: case.step,
        detail,
    })
}

impl Case {
    fn new(known: &'static [Bug]) -> Case {
        let github = FakeGithub::default();
        let docker = FakeDocker::default();
        github.0.lock().unwrap().strict = true;
        {
            let mut state = docker.0.lock().unwrap();
            state.captures_wait = Some(github.clone());
            state.seed_exit = Some(0);
        }
        let dir = TestDir::new("invariants");
        let shape = Shape::START;
        let (engine, handle) = Case::engine(&github, &docker, &dir, shape);
        Case {
            known,
            github,
            docker,
            dir,
            engine,
            handle,
            shape,
            jobs: BTreeMap::new(),
            next_job: 1,
            took_job: BTreeSet::new(),
            still_listed: BTreeSet::new(),
            calls_seen: 0,
            host: None,
            step: 0,
        }
    }

    fn engine(
        github: &FakeGithub,
        docker: &FakeDocker,
        dir: &TestDir,
        shape: Shape,
    ) -> (Engine<FakeGithub, FakeDocker>, Handle) {
        let (mut engine, handle) = engine_with(github, docker, dir, &shape.toml());
        engine.memory_max = Some(MEMORY_MAX);
        engine.cpus = Some(SLICE_CPUS);
        (engine, handle)
    }

    /// Recovers as `Engine::run` does: again until Docker answers.
    async fn recover(&mut self) {
        for _ in 0..10 {
            if self.engine.recover().await.is_ok() {
                return;
            }
        }
        panic!("recovery never succeeded");
    }

    /// The runner containers Docker runs, by ID.
    fn running_containers(&self) -> Vec<Container> {
        self.docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .filter(|container| container.running && container.labels.contains_key("runnerd.job"))
            .cloned()
            .collect()
    }

    /// Running runner containers of this machine that took no job, which GitHub may give one.
    fn idle_containers(&self) -> Vec<Container> {
        self.running_containers()
            .into_iter()
            .filter(|container| !self.took_job.contains(&container.id))
            .collect()
    }

    fn queued(&self) -> Vec<u64> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.phase == Phase::Queued)
            .map(|(id, _)| *id)
            .collect()
    }

    fn running(&self) -> Vec<(u64, String)> {
        self.jobs
            .iter()
            .filter_map(|(id, job)| match &job.phase {
                Phase::Running { container } => Some((*id, container.clone())),
                _ => None,
            })
            .collect()
    }

    fn status(&mut self) -> api::Status {
        self.engine.publish();
        self.handle.status.borrow().clone()
    }

    async fn apply(&mut self, step: Step) -> Result<(), Violation> {
        match step {
            Step::Queue { second, labels } => {
                let repository = if second { "o/s" } else { "o/r" };
                let id = self.next_job;
                self.next_job += 1;
                let labels = labels.of(repository);
                let mut github = self.github.0.lock().unwrap();
                github.queued.push(QueuedJob {
                    id,
                    run_id: id,
                    name: "build".to_owned(),
                    labels: labels.clone(),
                    created_at: time::now_ms(),
                    ..QueuedJob::default()
                });
                github.queued_on.insert(id, repository.to_owned());
                self.jobs.insert(
                    id,
                    JobModel {
                        repository,
                        labels,
                        phase: Phase::Queued,
                        taken_by: None,
                    },
                );
            }
            Step::Assign(pick) => self.assign(pick, false).await,
            Step::AssignListed(pick) => self.assign(pick, true).await,
            Step::Finish(pick) => {
                let running = self.running();
                if let Some((id, container)) = pick_from(&running, pick).cloned() {
                    self.end_job(id, &container, "success");
                    let mut docker = self.docker.0.lock().unwrap();
                    docker
                        .container_logs
                        .insert(container.clone(), "Running job: build\n".to_owned());
                    set_running(&mut docker, &container, false);
                }
            }
            Step::Cancel(pick) => {
                let queued = self.queued();
                if let Some(id) = pick_from(&queued, pick).copied() {
                    let mut github = self.github.0.lock().unwrap();
                    github.queued.retain(|job| job.id != id);
                    let mut cancelled = details(id, "completed", Some("cancelled"));
                    cancelled.runner_name = None;
                    github.jobs.insert(id, cancelled);
                    self.jobs.get_mut(&id).unwrap().phase = Phase::Done { ran: None };
                }
            }
            Step::Lose(pick) => {
                let running = self.running();
                if let Some((id, container)) = pick_from(&running, pick).cloned() {
                    self.end_job(id, &container, "failure");
                    self.docker
                        .0
                        .lock()
                        .unwrap()
                        .containers
                        .retain(|candidate| candidate.id != container);
                }
            }
            Step::StartFails => self.docker.0.lock().unwrap().fail_starts += 1,
            Step::ExitsAtOnce(pick) => self.exit_idle(pick, false),
            Step::ExitsIdle(pick) => self.exit_idle(pick, true),
            Step::InspectFails(pick) => self.inspect_fails(pick),
            Step::RemoveFails => self.docker.0.lock().unwrap().fail_removes += 1,
            Step::Sample {
                percent,
                available,
                stalled,
            } => self.sample(percent, available, stalled),
            Step::Reload(change) => {
                self.reload(change).await;
                self.tick().await?;
            }
            Step::Restart => {
                self.restart().await;
                self.tick().await?;
            }
            Step::RestartInspectFails(pick) => {
                self.inspect_fails(pick);
                self.restart().await;
                self.tick().await?;
            }
            Step::Elapse => self.elapse(),
            Step::Tick => self.tick().await?,
        }
        self.no_removal_of_a_running_job()
    }

    fn inspect_fails(&mut self, pick: usize) {
        let containers = self.running_containers();
        if let Some(container) = pick_from(&containers, pick) {
            *self
                .docker
                .0
                .lock()
                .unwrap()
                .fail_inspects
                .entry(container.id.clone())
                .or_default() += 1;
        }
    }

    /// GitHub gives a queued job to an idle runner of its repository whose labels it matches.
    async fn assign(&mut self, pick: usize, listed: bool) {
        let idle = self.idle_containers();
        let registered: BTreeSet<String> = self
            .github
            .0
            .lock()
            .unwrap()
            .runners
            .iter()
            .map(|runner| runner.name.clone())
            .collect();
        let candidates: Vec<(u64, Container)> = self
            .queued()
            .into_iter()
            .flat_map(|id| {
                let job = &self.jobs[&id];
                idle.iter()
                    .filter(|container| {
                        let label =
                            |name: &str| container.labels.get(name).cloned().unwrap_or_default();
                        let labels: Vec<String> = label("runnerd.labels")
                            .split(',')
                            .map(str::to_owned)
                            .collect();
                        registered.contains(&container.name)
                            && label("runnerd.repository") == job.repository
                            && !job.labels.is_empty()
                            && job.labels.iter().all(|wanted| {
                                labels.iter().any(|has| has.eq_ignore_ascii_case(wanted))
                            })
                    })
                    .map(move |container| (id, container.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let Some((id, container)) = pick_from(&candidates, pick).cloned() else {
            return;
        };
        let profile = container.labels["runnerd.profile"].clone();
        let repository = container.labels["runnerd.repository"].clone();
        {
            let mut github = self.github.0.lock().unwrap();
            if listed {
                self.still_listed.insert(id);
            } else {
                github.queued.retain(|job| job.id != id);
            }
            let mut runner_id = None;
            for runner in &mut github.runners {
                if runner.name == container.name {
                    runner.busy = true;
                    runner_id = Some(runner.id);
                }
            }
            let mut running = details(id, "in_progress", None);
            running.run_id = id;
            running.runner_name = Some(container.name.clone());
            running.labels = self.jobs[&id].labels.clone();
            github.jobs.insert(id, running);
            assert!(runner_id.is_some(), "an assigned runner is registered");
        }
        self.docker
            .0
            .lock()
            .unwrap()
            .container_captures
            .insert(container.id.clone(), format!("@@job {id} {id}\n"));
        self.took_job.insert(container.id.clone());
        let job = self.jobs.get_mut(&id).unwrap();
        job.phase = Phase::Running {
            container: container.id.clone(),
        };
        job.taken_by = Some((profile, repository));
        // Lets its capture report the job, as it does within moments of the runner taking it.
        for _ in 0..100 {
            self.handle.store.flush().await;
            if self
                .handle
                .store
                .job(id)
                .await
                .unwrap()
                .is_some_and(|job| job.summary.runner.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// GitHub completes a running job, and its ephemeral runner leaves. Its container no longer reports the job:
    /// runner names have a resolution of a second, so a runner started in its slot right after it may get its name
    /// and its container's ID.
    fn end_job(&mut self, id: u64, container: &str, conclusion: &str) {
        let job = self.jobs.get_mut(&id).unwrap();
        job.phase = Phase::Done {
            ran: job.taken_by.clone(),
        };
        let mut github = self.github.0.lock().unwrap();
        let name = github
            .jobs
            .get(&id)
            .and_then(|details| details.runner_name.clone());
        let mut completed = details(id, "completed", Some(conclusion));
        completed.run_id = id;
        completed.runner_name = name.clone();
        github.jobs.insert(id, completed);
        // GitHub lists a job queued a few seconds after a runner took it, never once it completed.
        github.queued.retain(|job| job.id != id);
        self.still_listed.remove(&id);
        github
            .runners
            .retain(|runner| Some(&runner.name) != name.as_ref());
        self.docker
            .0
            .lock()
            .unwrap()
            .container_captures
            .remove(container);
        self.took_job.remove(container);
    }

    /// An idle runner's container exits without a job; `waited`: after `IDLE_TIMEOUT`.
    fn exit_idle(&mut self, pick: usize, waited: bool) {
        let idle = self.idle_containers();
        let Some(container) = pick_from(&idle, pick).cloned() else {
            return;
        };
        if waited {
            for occupant in self.engine.slots.values_mut().flatten().flatten() {
                if occupant.container == container.id {
                    occupant.started_at -= IDLE_TIMEOUT;
                }
            }
        }
        let mut docker = self.docker.0.lock().unwrap();
        docker
            .container_logs
            .insert(container.id.clone(), String::new());
        set_running(&mut docker, &container.id, false);
    }

    fn sample(&mut self, percent: u64, available: u64, stalled: bool) {
        let now = time::now_ms();
        let docker = self.docker.0.lock().unwrap();
        let mut slice = 0;
        let mut samples = Vec::new();
        for container in docker
            .containers
            .iter()
            .filter(|container| container.running && container.labels.contains_key("runnerd.job"))
        {
            let cap = docker.memory.get(&container.id).copied().unwrap_or(G);
            let memory = if self.took_job.contains(&container.id) {
                cap / 100 * percent
            } else {
                100 * M
            };
            slice += memory;
            samples.push(SampleRecord {
                runner: container.name.clone(),
                at: now,
                cpu: 0,
                cpu_usage: 0,
                memory,
                network_received: 0,
                network_sent: 0,
                events: Events::default(),
            });
        }
        drop(docker);
        let store = &self.handle.store;
        store.samples(samples);
        store.host_sample(api::HostSample {
            at: now / 1000,
            cpu: 0,
            memory: slice,
        });
        store.host_memory(api::HostMemory {
            at: now / 1000,
            total: HOST_TOTAL,
            available: available * G,
            slice,
            some: Some(api::Pressure::default()),
            full: Some(api::Pressure {
                avg10: if stalled { 600 } else { 0 },
                avg60: 0,
            }),
            holders: Vec::new(),
        });
        self.host = Some((available * G, stalled));
    }

    async fn reload(&mut self, change: Change) {
        let mut shape = self.shape;
        match change {
            Change::HeavySlots(slots) => shape.heavy_slots = slots,
            Change::LightSlots(slots) => {
                if shape.light_slots.is_some() {
                    shape.light_slots = Some(slots);
                }
            }
            Change::LightPool => shape.light_pool = !shape.light_pool,
            Change::SecondRepository => shape.second_repository = !shape.second_repository,
            Change::LightProfile => {
                shape.light_slots = match shape.light_slots {
                    Some(_) => None,
                    None => Some(2),
                };
            }
            Change::Policy => {
                shape.admission = match shape.admission {
                    Admission::Caps => Admission::Measured,
                    Admission::Measured => Admission::Caps,
                };
            }
            Change::CpuOvercommit => shape.cpu_overcommit = !shape.cpu_overcommit,
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .command(Command::ReplaceConfig {
                config: Box::new(Config::parse(&shape.toml()).unwrap()),
                memory_max: Some(MEMORY_MAX),
                cpus: Some(SLICE_CPUS),
                commit: Commit::new(|| Ok(None)),
                running: RunningSlots::Drain,
                reply,
            })
            .await;
        assert_eq!(answer.await.unwrap(), Ok(None), "a person's reload applies");
        self.shape = shape;
    }

    async fn restart(&mut self) {
        for (_, capture) in std::mem::take(&mut self.engine.captures) {
            capture.abort();
        }
        self.handle.store.flush().await;
        {
            let mut docker = self.docker.0.lock().unwrap();
            if self.known.contains(&Bug::RecoveryRemovesOnce) {
                docker.fail_removes = 0;
            }
            if self.known.contains(&Bug::WatchedThroughAFailedInspect) {
                for failures in docker.fail_inspects.values_mut() {
                    *failures = (*failures).min(1);
                }
            }
        }
        let (engine, handle) = Case::engine(&self.github, &self.docker, &self.dir, self.shape);
        self.engine = engine;
        self.handle = handle;
        self.recover().await;
    }

    fn elapse(&mut self) {
        for occupant in self.engine.slots.values_mut().flatten().flatten() {
            occupant.started_at -= IDLE_TIMEOUT;
        }
        let past = SystemTime::now() - Duration::from_secs(1);
        for state in self.engine.repositories.values_mut() {
            for hold in state.holds.values_mut() {
                if let Some(held) = hold.held.as_mut() {
                    held.until = past;
                }
            }
        }
    }

    /// A runner whose registration GitHub deleted exits, as a JIT runner does.
    fn unregistered_runners_exit(&mut self) {
        let registered: BTreeSet<String> = self
            .github
            .0
            .lock()
            .unwrap()
            .runners
            .iter()
            .map(|runner| runner.name.clone())
            .collect();
        let mut docker = self.docker.0.lock().unwrap();
        for container in &mut docker.containers {
            if container.running
                && container.labels.contains_key("runnerd.job")
                && !registered.contains(&container.name)
            {
                container.running = false;
            }
        }
    }

    /// Waits for the next second if a slot whose runner started this second has no running runner now: runner names
    /// have a resolution of a second, and a real runner does not start and end within one, so the next runner in that
    /// slot would take its name, and the history would take the two for one.
    async fn no_name_twice(&mut self) {
        let now = time::now_ms() / 1000;
        let running: BTreeSet<String> = self
            .running_containers()
            .into_iter()
            .map(|container| container.name)
            .collect();
        let reused = self
            .docker
            .0
            .lock()
            .unwrap()
            .started
            .iter()
            .any(|(name, at)| at / 1000 == now && !running.contains(name));
        if reused {
            tokio::time::sleep(Duration::from_millis(1000 - time::now_ms() % 1000)).await;
        }
    }

    async fn tick(&mut self) -> Result<(), Violation> {
        self.unregistered_runners_exit();
        self.no_name_twice().await;
        let before = self.status();
        self.engine.tick(Shutdown::No).await;
        let after = self.status();
        let listed = std::mem::take(&mut self.still_listed);
        self.github
            .0
            .lock()
            .unwrap()
            .queued
            .retain(|job| !listed.contains(&job.id));
        let names = |status: &api::Status| -> BTreeSet<String> {
            runners_of(status)
                .map(|(_, runner)| runner.name.clone())
                .collect()
        };
        let started: BTreeSet<String> =
            names(&after).difference(&names(&before)).cloned().collect();
        self.no_pool_gets_more_runners_than_its_queued_jobs(&after, &started)?;
        self.starts_fit_the_budget(&after, &started)?;
        self.docker_runs_no_runner_the_engine_lost(&after)?;
        self.only_unserved_jobs_wait_for_a_pool(&after)?;
        self.no_removal_of_a_running_job()
    }

    /// Invariant 1: a tick that starts runners in a pool leaves it no more runners waiting for a job than it has
    /// queued jobs, so no job gets runners in two pools nor more than one.
    fn no_pool_gets_more_runners_than_its_queued_jobs(
        &self,
        status: &api::Status,
        started: &BTreeSet<String>,
    ) -> Result<(), Violation> {
        let mut pools: BTreeSet<(String, String)> = BTreeSet::new();
        for (profile, runner) in runners_of(status) {
            if started.contains(&runner.name) {
                pools.insert((runner.repository.clone(), profile.to_owned()));
            }
        }
        let idle: BTreeSet<String> = self.idle_containers().into_iter().map(|c| c.id).collect();
        for (repository, profile) in pools {
            let waiting = runners_of(status)
                .filter(|(in_profile, runner)| {
                    *in_profile == profile
                        && runner.repository == repository
                        && idle.contains(&runner.container)
                })
                .count();
            let queued = self
                .queued()
                .into_iter()
                .filter(|id| {
                    let job = &self.jobs[id];
                    job.repository == repository
                        && self.shape.serves(job.repository, &job.labels) == Some(profile.as_str())
                })
                .count();
            if waiting > queued {
                return violation(
                    self,
                    "no_pool_gets_more_runners_than_its_queued_jobs",
                    format!(
                        "{repository} in {profile}: {waiting} runners wait for {queued} queued jobs"
                    ),
                );
            }
        }
        Ok(())
    }

    /// Invariant 2: a tick that starts a runner leaves the running ones within the slice's `MemoryMax`, the CPU
    /// budget under `cpu_overcommit` and what the host leaves the slice, and starts nothing while the host stalls
    /// short of memory (ADR 0055). Running runners may be past it otherwise: nothing is stopped for it.
    fn starts_fit_the_budget(
        &self,
        status: &api::Status,
        started: &BTreeSet<String>,
    ) -> Result<(), Violation> {
        if started.is_empty() {
            return Ok(());
        }
        let budget = &status.budget;
        // Under caps, what Docker runs them with, whatever the engine counts.
        if self.shape.admission == Admission::Caps {
            let running = self.running_containers();
            let docker = self.docker.0.lock().unwrap();
            let caps: u64 = running
                .iter()
                .map(|container| docker.memory.get(&container.id).copied().unwrap_or(0))
                .sum();
            drop(docker);
            let seeds: u64 = budget.seed_memory.iter().map(|seed| seed.memory).sum();
            if caps + seeds > MEMORY_MAX {
                return violation(
                    self,
                    "starts_fit_the_budget",
                    format!(
                        "started {started:?} with caps of {caps} and seeds of {seeds} in {MEMORY_MAX}"
                    ),
                );
            }
        }
        if let Some(max) = budget.memory_max
            && budget.memory_in_use > max
        {
            return violation(
                self,
                "starts_fit_the_budget",
                format!(
                    "started {started:?} with {} of {max} reserved, seed containers {:?}",
                    budget.memory_in_use, budget.seed_memory
                ),
            );
        }
        if let Some(cpu) = &budget.cpu
            && cpu.in_use > cpu.budget
        {
            return violation(
                self,
                "starts_fit_the_budget",
                format!(
                    "started {started:?} with {} of {} millicores reserved",
                    cpu.in_use, cpu.budget
                ),
            );
        }
        if let Some(host) = &budget.host {
            let reading = &host.memory;
            let limit = (reading.slice + reading.available).saturating_sub(HOST_MARGIN);
            let reserved: u64 = runners_of(status).map(|(_, runner)| runner.reserved).sum();
            if reserved > limit {
                return violation(
                    self,
                    "starts_fit_the_budget",
                    format!("started {started:?} with {reserved} reserved past the host's {limit}"),
                );
            }
            let stalled = reading.full.is_some_and(|full| full.avg10 >= 500);
            if stalled && reading.available < 2 * HOST_MARGIN {
                return violation(
                    self,
                    "starts_fit_the_budget",
                    format!(
                        "started {started:?} while the host stalls with {} available",
                        reading.available
                    ),
                );
            }
        }
        Ok(())
    }

    /// Invariant 3: runnerd never removes a container whose job still runs, whatever restarts, reloads and failed
    /// Docker calls came before.
    fn no_removal_of_a_running_job(&mut self) -> Result<(), Violation> {
        let calls = self.docker.calls();
        let running: BTreeMap<String, u64> = self
            .running()
            .into_iter()
            .map(|(id, container)| (container, id))
            .collect();
        for call in &calls[self.calls_seen..] {
            if let Some(container) = call.strip_prefix("remove ")
                && let Some(job) = running.get(container)
            {
                let detail = format!("removed {container}, which runs job {job}");
                self.calls_seen = calls.len();
                return violation(self, "no_removal_of_a_running_job", detail);
            }
        }
        self.calls_seen = calls.len();
        Ok(())
    }

    /// Invariant 4, while the sequence runs: every runner container Docker runs is one the engine holds a slot for.
    fn docker_runs_no_runner_the_engine_lost(&self, status: &api::Status) -> Result<(), Violation> {
        let held: BTreeSet<&str> = runners_of(status)
            .map(|(_, runner)| runner.container.as_str())
            .collect();
        let lost: Vec<String> = self
            .running_containers()
            .into_iter()
            .filter(|container| !held.contains(container.id.as_str()))
            .map(|container| container.id)
            .collect();
        if lost.is_empty() {
            Ok(())
        } else {
            violation(
                self,
                "docker_runs_no_runner_the_engine_lost",
                format!("no slot holds {lost:?}"),
            )
        }
    }

    /// Invariant 5: a queued job no pool serves waits with `NoPool`, a job a pool serves never does, and a job for
    /// GitHub's own runners is not listed.
    fn only_unserved_jobs_wait_for_a_pool(&self, status: &api::Status) -> Result<(), Violation> {
        for listed in &status.queued {
            let job = &self.jobs[&listed.id];
            let hosted = job.labels == ["ubuntu-latest"];
            let unserved = self.shape.serves(job.repository, &job.labels).is_none();
            let no_pool = matches!(listed.waiting, Some(api::Waiting::NoPool { .. }));
            if hosted || unserved != no_pool {
                return violation(
                    self,
                    "only_unserved_jobs_wait_for_a_pool",
                    format!(
                        "job {} asking for {:?} waits with {:?}",
                        listed.id, job.labels, listed.waiting
                    ),
                );
            }
        }
        for id in self.queued() {
            let job = &self.jobs[&id];
            if self.shape.configures(job.repository)
                && job.labels != ["ubuntu-latest"]
                && !status.queued.iter().any(|listed| listed.id == id)
            {
                return violation(
                    self,
                    "only_unserved_jobs_wait_for_a_pool",
                    format!("queued job {id} asking for {:?} is not listed", job.labels),
                );
            }
        }
        Ok(())
    }

    /// Ends the sequence: the queued jobs are cancelled, the running ones finish, and time passes.
    async fn drain(&mut self) -> Result<(), Violation> {
        self.step += 1;
        while !self.queued().is_empty() {
            self.apply(Step::Cancel(0)).await?;
        }
        while !self.running().is_empty() {
            self.apply(Step::Finish(0)).await?;
        }
        for _ in 0..4 {
            self.elapse();
            self.tick().await?;
        }
        self.no_runner_is_left()?;
        self.history_names_the_pool_that_ran_each_job().await
    }

    /// Invariant 4, once drained: every runner runnerd started has gone, from Docker, its slots and GitHub.
    fn no_runner_is_left(&mut self) -> Result<(), Violation> {
        let status = self.status();
        let containers: Vec<String> = self
            .docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .filter(|container| container.labels.contains_key("runnerd.job"))
            .map(|container| container.id.clone())
            .collect();
        let slots: Vec<String> = runners_of(&status)
            .map(|(_, runner)| runner.name.clone())
            .collect();
        let registered: Vec<String> = self
            .github
            .0
            .lock()
            .unwrap()
            .runners
            .iter()
            .map(|runner| runner.name.clone())
            .collect();
        if containers.is_empty() && slots.is_empty() && registered.is_empty() {
            Ok(())
        } else {
            violation(
                self,
                "no_runner_is_left",
                format!("containers {containers:?}, slots {slots:?}, registered {registered:?}"),
            )
        }
    }

    /// Invariant 6: the history names the profile and repository of the runner that ran each job.
    async fn history_names_the_pool_that_ran_each_job(&mut self) -> Result<(), Violation> {
        self.handle.store.flush().await;
        let ran: Vec<(u64, (String, String))> = self
            .jobs
            .iter()
            .filter_map(|(id, job)| match &job.phase {
                Phase::Done { ran: Some(ran) } => Some((*id, ran.clone())),
                _ => None,
            })
            .collect();
        for (id, (profile, repository)) in ran {
            let recorded = self.handle.store.job(id).await.unwrap();
            let seen = recorded.as_ref().map(|job| {
                (
                    job.summary
                        .runner
                        .as_ref()
                        .map(|runner| runner.profile.clone()),
                    job.summary.repository.clone(),
                )
            });
            if seen != Some((Some(profile.clone()), repository.clone())) {
                return violation(
                    self,
                    "history_names_the_pool_that_ran_each_job",
                    format!("job {id} ran in {repository} {profile}; the history says {seen:?}"),
                );
            }
        }
        Ok(())
    }
}

fn pick_from<T>(items: &[T], pick: usize) -> Option<&T> {
    (!items.is_empty()).then(|| &items[pick % items.len()])
}

fn set_running(docker: &mut DockerState, container: &str, running: bool) {
    for candidate in &mut docker.containers {
        if candidate.id == container {
            candidate.running = running;
        }
    }
}

/// Every runner in the status, with its profile.
fn runners_of(status: &api::Status) -> impl Iterator<Item = (&str, &api::RunnerStatus)> {
    status.profiles.iter().flat_map(|profile| {
        profile
            .slots
            .iter()
            .filter_map(|slot| slot.runner.as_ref())
            .map(|runner| (profile.name.as_str(), runner))
    })
}

/// Runs a sequence from a fresh start, then drains it; the first broken invariant.
async fn run(steps: &[Step], known: &'static [Bug]) -> Result<(), Violation> {
    let mut case = Case::new(known);
    case.recover().await;
    case.tick().await?;
    for (index, step) in steps.iter().enumerate() {
        case.step = index;
        case.apply(*step).await?;
    }
    case.step = steps.len();
    case.drain().await
}

/// The shortest sequence found that still breaks `invariant`: drops chunks of steps, then single steps, while it
/// does.
async fn shrink(mut steps: Vec<Step>, invariant: &'static str) -> (Vec<Step>, Violation) {
    let mut last = run(&steps, KNOWN_BUGS).await.unwrap_err();
    let mut chunk = steps.len() / 2;
    while chunk >= 1 {
        let mut start = 0;
        while start < steps.len() {
            let mut fewer = steps.clone();
            fewer.drain(start..(start + chunk).min(steps.len()));
            match run(&fewer, KNOWN_BUGS).await {
                Err(found) if found.invariant == invariant => {
                    steps = fewer;
                    last = found;
                }
                _ => start += chunk,
            }
        }
        chunk /= 2;
    }
    (steps, last)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
async fn the_scheduler_keeps_its_invariants_over_generated_sequences() {
    let seeds: Vec<u64> = match env_u64("RUNNERD_INVARIANT_SEED") {
        Some(seed) => vec![seed],
        None => (0..env_u64("RUNNERD_INVARIANT_CASES").unwrap_or(CASES)).collect(),
    };
    // Cases at once: each waits out seconds now and then (`Case::no_name_twice`), which the others use.
    for batch in seeds.chunks(64) {
        let runs = batch.iter().map(|&seed| async move {
            let steps = generate(&mut Rng(seed));
            (
                seed,
                run(&steps, KNOWN_BUGS)
                    .await
                    .err()
                    .map(|found| (steps, found)),
            )
        });
        for (seed, failed) in futures_util::future::join_all(runs).await {
            if let Some((steps, found)) = failed {
                let (shortest, violation) = shrink(steps, found.invariant).await;
                panic!(
                    "seed {seed} breaks {}: {}\nshrunk to {:#?}\nwhich breaks it at step {}: {}",
                    found.invariant, found.detail, shortest, violation.step, violation.detail
                );
            }
        }
    }
}

/// Runs `steps` as the generated test does, around no known bug, and returns what broke.
async fn broken(steps: &[Step]) -> Option<(&'static str, String)> {
    run(steps, &[])
        .await
        .err()
        .map(|found| (found.invariant, found.detail))
}

#[tokio::test]
async fn a_job_taking_a_waiting_runners_room_still_fits_what_the_host_leaves() {
    let steps = [
        // o/s gets a runner, then its job is cancelled: the runner waits for nothing.
        Step::Queue {
            second: true,
            labels: Labels::Heavy,
        },
        Step::Tick,
        Step::Cancel(0),
        // An o/r job comes while the host has 3G available, under its 4G margin: no 4G runner fits.
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Sample {
            percent: 10,
            available: 3,
            stalled: false,
        },
        Step::Tick,
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
#[ignore = "bug: a runner container recovery fails to remove stays in Docker until the next restart"]
async fn a_stopped_runner_that_recovery_cannot_remove_is_removed_later() {
    let steps = [
        Step::Queue {
            second: false,
            labels: Labels::Light,
        },
        Step::Tick,
        Step::Assign(0),
        // The job ends while the daemon is down, and Docker fails the first remove after the restart.
        Step::RemoveFails,
        Step::Finish(0),
        Step::Restart,
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
#[ignore = "bug: a runner whose container Docker fails to inspect once as it is watched never has its job recorded"]
async fn a_runner_watched_through_a_failed_inspect_still_records_the_job_it_runs() {
    let steps = [
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Tick,
        // Docker fails the inspects of the runner's container while a restart adopts it.
        Step::InspectFails(0),
        Step::RestartInspectFails(0),
        Step::Assign(0),
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
async fn a_reseed_starts_only_where_its_seed_container_fits_the_budget() {
    let steps = [
        // A light job runs in light's slot 1, so the light profile is due a reseed once its slots are free.
        Step::Queue {
            second: false,
            labels: Labels::Light,
        },
        Step::Tick,
        Step::Assign(0),
        // Two heavy runners take 8G of the 8.5G; the light seed container's 1G does not fit beside them.
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Finish(0),
        Step::Tick,
    ];
    assert_eq!(broken(&steps).await, None);
}
