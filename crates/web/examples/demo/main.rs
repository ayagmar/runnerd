//! The dashboard against made-up data, for working on its design: `cargo run -p runnerd-web --example demo -- DIR
//! [ADDR [ORIGIN...]]` serves a fake daemon's socket in DIR and the dashboard on ADDR (default 127.0.0.1:7478),
//! taking writes from each ORIGIN as from `[web] origins` too, and sends a sample of each running job and of the
//! slice every 2 seconds, and a log line every second. Open the first link in `DIR/runnerd.web`.

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::expect_used,
    reason = "a development tool: it prints where to look and stops on any failure"
)]

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runnerd_core::api::{
    Admission, AdmissionPolicy, AutoPlan, Basis, CpuAdmission, CpuBudget, Event, EventEnvelope,
    Holder, HostBudget, HostMemory, HostSample, JobPeak, JobState, PoolStatus, ProfileStatus,
    Resize, RunnerStatus, Sample, SizingChange, SizingHold, SizingRefused, SlotStatus, Waiting,
};

mod fake;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("usage: demo DIR [ADDR]"));
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7478".to_owned());
    let proxies = args.map(|origin| origin.parse().unwrap()).collect();
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("runnerd.sock");
    let _ = std::fs::remove_file(&socket);
    let fake = fake::Fake::new(fake::Fixtures::new(now()));
    burst(&fake);
    warm(&fake);
    auto_sizing(&fake);
    small_pool(&fake);
    measured(&fake);
    downtime(&fake);
    busy_desktop(&fake);
    scene(&fake);
    queue(&fake);
    let scenes = Scenes {
        start_every: start_every(),
        drop_every: every("RUNNERD_DEMO_DROP_EVERY"),
        finish_after: every("RUNNERD_DEMO_FINISH_AFTER"),
    };
    // About what the doctor's reads of the host take with a few repositories.
    fake.fixtures.lock().unwrap().doctor_delay = Duration::from_millis(700);
    fake::serve(&socket, fake.clone());
    let listener = runnerd_web::listen(&addr).await.unwrap().unwrap();
    let sessions = runnerd_web::Sessions::open(dir.join("dashboard-session")).unwrap();
    let web = runnerd_web::spawn(
        listener,
        socket.clone(),
        proxies,
        peers(),
        sessions,
        std::future::pending::<()>(),
    );
    println!(
        "login links in {}",
        runnerd_web::login_file(&socket).display()
    );
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut n = 0u64;
    loop {
        tick.tick().await;
        n += 1;
        let at = now();
        let mut events = Vec::new();
        {
            let mut fixtures = fake.fixtures.lock().unwrap();
            let running: Vec<u64> = fixtures
                .jobs
                .iter()
                .filter(|job| job.summary.state == JobState::Running)
                .map(|job| job.summary.id)
                .collect();
            for id in running {
                let line = fixtures.logs.get(&id).map_or(0, Vec::len) as u64;
                let text = format!("[{n:>4}] progress of the running step");
                fixtures.logs.entry(id).or_default().push(text.clone());
                if let Some(job) = fixtures.jobs.iter_mut().find(|job| job.summary.id == id) {
                    job.summary.log.lines += 1;
                    job.summary.log.bytes += text.len() as u64 + 1;
                }
                events.push(Event::LogLine {
                    job: id,
                    line,
                    text,
                });
                if n.is_multiple_of(2) {
                    let series = fixtures.samples.get(&id);
                    let first = series.and_then(|s| s.first()).map(|s| s.memory);
                    let last = series.and_then(|s| s.last()).cloned();
                    if let (Some(first), Some(last)) = (first, last) {
                        let base = first * 3 / 2;
                        let sample = Sample {
                            at,
                            cpu: 1500 + n * 6151 % 5200,
                            memory: base + (n * 7919 % 97) * (base / 600),
                            network_received: last.network_received + 400_000,
                            network_sent: last.network_sent + 40_000,
                        };
                        fixtures.samples.entry(id).or_default().push(sample.clone());
                        events.push(Event::JobSample { job: id, sample });
                    }
                }
            }
            if n.is_multiple_of(2) {
                events.push(host_sample(&mut fixtures, n, at));
            }
        }
        events.extend(scenes.at_second(&fake, n, at));
        for event in events {
            let _ = fake.events.send(EventEnvelope {
                id: None,
                at,
                event,
            });
        }
        if web.is_finished() {
            return;
        }
    }
}

/// The slice's sample of second `n`, kept and announced.
fn host_sample(fixtures: &mut fake::Fixtures, n: u64, at: u64) -> Event {
    let base = 9 << 30;
    let sample = HostSample {
        at,
        cpu: 2500 + n * 4271 % 4800,
        memory: base + (n * 104_729 % 89) * (base / 300),
    };
    fixtures.host.push(sample.clone());
    Event::HostSample { sample }
}

/// `RUNNERD_DEMO_START_EVERY`, when it is a number of seconds.
fn start_every() -> Option<u64> {
    every("RUNNERD_DEMO_START_EVERY")
}

/// What the environment asks to happen over time, by second.
struct Scenes {
    start_every: Option<u64>,
    drop_every: Option<u64>,
    finish_after: Option<u64>,
}

impl Scenes {
    /// What happens in second `n`, at `at`: its events.
    fn at_second(&self, fake: &fake::Fake, n: u64, at: u64) -> Vec<Event> {
        let mut events = Vec::new();
        if self
            .start_every
            .is_some_and(|every| n.is_multiple_of(every))
        {
            events.extend(start_oldest(fake));
        }
        if self.finish_after == Some(n) {
            events.extend(finish_running(fake, at));
        }
        if self.drop_every.is_some_and(|every| n.is_multiple_of(every)) {
            events.push(drop_status(fake));
        }
        events
    }
}

/// The daemon away for a moment: the next two reads of the status drop, and the budget's event, returned, asks for
/// one.
fn drop_status(fake: &fake::Fake) -> Event {
    let mut fixtures = fake.fixtures.lock().unwrap();
    fixtures.drops.insert("/v1/status".to_owned(), 2);
    Event::BudgetChanged {
        budget: fixtures.status.budget.clone(),
    }
}

/// Every running job completes at `at`, successfully; their events.
fn finish_running(fake: &fake::Fake, at: u64) -> Vec<Event> {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let mut events = Vec::new();
    for job in &mut fixtures.jobs {
        if job.summary.state == JobState::Running {
            job.summary.state = JobState::Completed;
            job.summary.conclusion = Some("success".to_owned());
            job.summary.timeline.finished_at = Some(at);
            events.push(Event::JobFinished {
                job: job.summary.clone(),
            });
        }
    }
    events
}

/// The environment variable `name`, when it is a number of seconds: `RUNNERD_DEMO_START_EVERY`;
/// `RUNNERD_DEMO_DROP_EVERY`, how often the overview's status read drops twice before it answers; and
/// `RUNNERD_DEMO_FINISH_AFTER`, when every running job completes.
fn every(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|every| every.parse::<u64>().ok())
        .filter(|every| *every > 0)
}

/// A burst on storefront, as pushes to a pull request bring: three more jobs, which wait for a node slot behind the
/// one its starting runner will take.
fn burst(fake: &fake::Fake) {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let first = fixtures.status.queued[0].clone();
    for (n, (name, waited)) in [("a11y", 35), ("lighthouse", 22), ("unit", 9)]
        .into_iter()
        .enumerate()
    {
        let mut job = first.clone();
        job.id += 1 + n as u64;
        name.clone_into(&mut job.name);
        job.queued_at = now() - waited;
        job.url = None;
        job.waiting = Some(Waiting::NoSlot {
            profile: "node".to_owned(),
        });
        fixtures.status.queued.push(job);
    }
    let queued = fixtures.status.queued.len();
    for repository in &mut fixtures.status.repositories {
        if repository.name == first.repository {
            repository.queued_jobs = queued;
        }
    }
}

/// `RUNNERD_DEMO_QUEUED=N`: a queue of N jobs in place of the scene's, oldest first, for the overview's cut and the
/// queue's page, and whichever the queue holds, those the history records (`record_queued`). Most are storefront's matrix shards, whose names repeat; every fourth is ledger's; every ninth asks
/// for labels no pool serves and has no GitHub page, as a job the history does not hold.
fn queue(fake: &fake::Fake) {
    if let Some(count) = std::env::var("RUNNERD_DEMO_QUEUED")
        .ok()
        .and_then(|count| count.parse::<u64>().ok())
    {
        queue_of(fake, count);
    }
    record_queued(fake);
}

fn queue_of(fake: &fake::Fake, count: u64) {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let template = fixtures.status.queued[0].clone();
    let at = now();
    fixtures.status.queued = (0..count)
        .map(|n| {
            let mut job = template.clone();
            job.id = 130_000_000_000 + n;
            job.queued_at = at - 1800 + n * 20;
            job.url = Some(format!(
                "https://github.com/halyard/storefront/actions/runs/40018663903/job/{}",
                job.id
            ));
            job.name = format!("test ({})", n % 3 + 1);
            // Jobs of one name whose branches differ only at their end, as a phone must still tell apart.
            job.branch = Some(format!("feature/checkout-redesign-payments-step-{n}"));
            job.waiting = Some(Waiting::NoSlot {
                profile: "node".to_owned(),
            });
            if n % 4 == 3 {
                "halyard/ledger".clone_into(&mut job.repository);
                job.profile = Some("jvm".to_owned());
                "build".clone_into(&mut job.name);
                job.waiting = Some(Waiting::NoSlot {
                    profile: "jvm".to_owned(),
                });
            }
            if n % 9 == 8 {
                job.url = None;
                job.profile = None;
                "gpu-render".clone_into(&mut job.name);
                job.waiting = Some(Waiting::NoPool {
                    labels: vec!["self-hosted".to_owned(), "gpu".to_owned()],
                });
            }
            job
        })
        .collect();
    let fixtures = &mut *fixtures;
    for repository in &mut fixtures.status.repositories {
        repository.queued_jobs = fixtures
            .status
            .queued
            .iter()
            .filter(|job| job.repository == repository.name)
            .count();
    }
}

/// The history holds each queued job a pool serves, as the daemon records what its listings find; one no pool serves
/// it lacks, so its row opens nothing here.
fn record_queued(fake: &fake::Fake) {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let Some(template) = fixtures.jobs.first().cloned() else {
        return;
    };
    let recorded: Vec<_> = fixtures
        .status
        .queued
        .iter()
        .filter(|queued| !matches!(queued.waiting, Some(Waiting::NoPool { .. })))
        .filter(|queued| fixtures.jobs.iter().all(|job| job.summary.id != queued.id))
        .map(|queued| {
            let mut job = template.clone();
            let summary = &mut job.summary;
            summary.id = queued.id;
            summary.repository.clone_from(&queued.repository);
            summary.run_id = queued.run_id;
            summary.workflow.clone_from(&queued.workflow);
            summary.name.clone_from(&queued.name);
            summary.branch.clone_from(&queued.branch);
            summary.url.clone_from(&queued.url);
            summary.state = JobState::Queued;
            summary.conclusion = None;
            summary.runner = None;
            summary.timeline = runnerd_core::api::Timeline {
                queued_at: queued.queued_at,
                ..runnerd_core::api::Timeline::default()
            };
            job.steps.clear();
            job
        })
        .collect();
    fixtures.jobs.extend(recorded);
    fixtures
        .jobs
        .sort_by_key(|job| std::cmp::Reverse(job.summary.id));
}

/// `RUNNERD_DEMO_START_EVERY=SECS`: every SECS seconds the oldest queued job the history holds leaves the queue as a
/// runner takes it, with its event.
fn start_oldest(fake: &fake::Fake) -> Option<Event> {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let fixtures = &mut *fixtures;
    // Only a job the history holds starts: one no pool serves never does.
    let oldest = fixtures
        .status
        .queued
        .iter()
        .position(|queued| fixtures.jobs.iter().any(|job| job.summary.id == queued.id))?;
    let left = fixtures.status.queued.remove(oldest);
    for repository in &mut fixtures.status.repositories {
        if repository.name == left.repository {
            repository.queued_jobs = repository.queued_jobs.saturating_sub(1);
        }
    }
    let job = fixtures
        .jobs
        .iter_mut()
        .find(|job| job.summary.id == left.id)?;
    job.summary.state = JobState::Running;
    Some(Event::JobStarted {
        job: job.summary.clone(),
    })
}

/// A warm runner in jvm's slot 2: ledger's last job there ended 10 minutes ago, and its runner waits for the next.
fn warm(fake: &fake::Fake) {
    let mut fixtures = fake.fixtures.lock().unwrap();
    let at = now() - 600;
    let jvm = fixtures
        .status
        .profiles
        .iter_mut()
        .find(|profile| profile.name == "jvm")
        .unwrap();
    jvm.slots[1].runner = Some(RunnerStatus {
        repository: "halyard/ledger".to_owned(),
        name: format!("runnerd-3f9a-forge-jvm-2-{at}"),
        runner_id: 4100 + at % 1000,
        container: format!("{at:x}c0ffee"),
        busy: false,
        started_at: at,
        reserved: 9 << 30,
        warm: true,
    });
}

/// Auto sizing on, at most once in 3 days a profile: it lowered jvm two days ago, which holds jvm a day more, and its
/// next look lowers node a step.
fn auto_sizing(fake: &fake::Fake) {
    const GIB: u64 = 1 << 30;
    let mut fixtures = fake.fixtures.lock().unwrap();
    let at = now();
    fixtures.auto_plan = Some(AutoPlan {
        every: 300,
        interval: 3 * 86_400,
        raise_interval: 3600,
        max_step: 25,
        next: vec![SizingChange {
            at,
            profile: "node".to_owned(),
            from: Resize {
                memory: 7 * GIB,
                slots: 2,
                cpus: None,
            },
            to: Resize {
                memory: 5376 << 20,
                slots: 2,
                cpus: None,
            },
            reason: "lower memory to 3.25G from 7G, its largest peak + 1G for page cache"
                .to_owned(),
            uncertain: false,
        }],
        held: vec![SizingHold {
            profile: "jvm".to_owned(),
            changed: fixtures.now - 2 * 86_400,
            until: fixtures.now + 86_400,
            lower_until: fixtures.now + 86_400,
        }],
        blocked: None,
        // Worded as core's sizing::auto words a raise the cap holds back and the CPU budget cannot give.
        refused: vec![SizingRefused {
            profile: "jvm".to_owned(),
            reason: format!(
                "13 runs of halyard/ledger {} averaged {} of its 8 CPUs (median): the cap holds them back; 9 CPUs \
                 do not fit the CPU budget with every profile's warm runners and one more runner at the \
                 largest cap",
                runnerd_core::api::job_title(Some("CI"), "build", None),
                runnerd_core::api::cores(7400),
            ),
        }],
        refused_slots: Vec::new(),
        waited_before_change: Vec::new(),
    });
}

/// The newest finished ledger build waited 9 hours, 8 of them while runnerd was not running to follow it.
fn downtime(fake: &fake::Fake) {
    let mut fixtures = fake.fixtures.lock().unwrap();
    if let Some(job) = fixtures
        .jobs
        .iter_mut()
        .filter(|job| {
            job.summary.state == JobState::Completed && job.summary.repository == "halyard/ledger"
        })
        .max_by_key(|job| job.summary.timeline.queued_at)
    {
        job.summary.timeline.queued_at -= 9 * 3600;
        job.summary.timeline.unseen = 8 * 3600 + 10 * 60;
    }
}

/// storefront split in two pools: its node jobs wait for a node slot while a `small` runner, 1G for its light jobs,
/// waits warm, and a job asking for a mistyped label waits for a pool, the oldest since nothing starts it.
fn small_pool(fake: &fake::Fake) {
    const GIB: u64 = 1 << 30;
    let mut fixtures = fake.fixtures.lock().unwrap();
    let at = now();
    let status = &mut fixtures.status;
    let warm = at - 260;
    status.profiles.push(ProfileStatus {
        name: "small".to_owned(),
        image: "runnerd-runner:node".to_owned(),
        cpus: 2,
        memory: GIB,
        reseeding: false,
        slots: vec![
            SlotStatus {
                slot: 1,
                runner: Some(RunnerStatus {
                    repository: "halyard/storefront".to_owned(),
                    name: format!("runnerd-3f9a-forge-small-1-{warm}"),
                    runner_id: 4100 + warm % 1000,
                    container: format!("{warm:x}c0ffee"),
                    busy: false,
                    started_at: warm,
                    reserved: GIB,
                    warm: true,
                }),
            },
            SlotStatus {
                slot: 2,
                runner: None,
            },
        ],
    });
    let node = ["self-hosted", "linux", "x64", "node"]
        .map(str::to_owned)
        .to_vec();
    for repository in &mut status.repositories {
        if repository.name == "halyard/storefront" {
            repository.pools = vec![
                PoolStatus {
                    profile: "node".to_owned(),
                    labels: node.clone(),
                    queued_jobs: repository.queued_jobs,
                },
                PoolStatus {
                    profile: "small".to_owned(),
                    labels: vec!["storefront-small".to_owned()],
                    queued_jobs: 0,
                },
            ];
            repository.queued_jobs += 1;
        }
    }
    for job in &mut status.queued {
        job.profile = Some("node".to_owned());
    }
    let mut lint = status.queued[0].clone();
    lint.id += 10;
    "lint".clone_into(&mut lint.name);
    lint.workflow = Some("CI".to_owned());
    lint.url = None;
    lint.queued_at = at - 290;
    lint.profile = None;
    lint.waiting = Some(Waiting::NoPool {
        labels: vec!["storefornt-small".to_owned()],
    });
    status.queued.insert(0, lint);
}

/// Measured admission with a CPU budget: ledger's build reserved its predicted 7.75G and 3.6 CPUs under its 9G and
/// 8 CPU caps, and storefront's runners 4G under their 7G.
fn measured(fake: &fake::Fake) {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    let mut fixtures = fake.fixtures.lock().unwrap();
    fixtures.config =
        fixtures
            .config
            .replacen("admission = \"caps\"", "admission = \"measured\"", 1);
    let ledger = 7 * GIB + 768 * MIB;
    let status = &mut fixtures.status;
    for profile in &mut status.profiles {
        for runner in profile
            .slots
            .iter_mut()
            .filter_map(|slot| slot.runner.as_mut())
        {
            match (profile.name.as_str(), runner.warm) {
                ("jvm", false) => runner.reserved = ledger,
                ("node", _) => runner.reserved = 4 * GIB,
                _ => {}
            }
        }
    }
    let reserved: u64 = status
        .profiles
        .iter()
        .flat_map(|profile| &profile.slots)
        .filter_map(|slot| slot.runner.as_ref())
        .map(|runner| runner.reserved)
        .sum();
    status.budget.admission = AdmissionPolicy::Measured;
    status.budget.memory_in_use = reserved + status.budget.mirror_memory;
    status.budget.cpu = Some(CpuBudget {
        cpus: 32_000,
        budget: 32_000,
        in_use: 3_600 + 8_000 + 2 * 2_400 + 2_000,
        held: false,
    });
    let predicted = Basis::Job {
        runs: 212,
        peak: JobPeak::P95,
    };
    let job = fixtures
        .jobs
        .iter_mut()
        .find(|job| job.summary.id == 120_055_991_700)
        .unwrap();
    job.admission = Some(Admission {
        policy: AdmissionPolicy::Measured,
        reserved: ledger,
        basis: predicted,
        free: Some(17 * GIB),
        slice: Some(6 * GIB + 256 * MIB),
        cpu: Some(CpuAdmission {
            reserved: 3_600,
            basis: predicted,
            free: 22_000,
            waited: None,
        }),
    });
}

/// A desktop's java and emulator hold the host's memory beside the slice.
fn busy_desktop(fake: &fake::Fake) {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    let mut fixtures = fake.fixtures.lock().unwrap();
    let budget = &mut fixtures.status.budget;
    let slice = 11 * GIB;
    let available = 23 * GIB;
    let limit = slice + available - 4 * GIB;
    budget.host = Some(HostBudget {
        memory: HostMemory {
            at: now(),
            total: 64 * GIB,
            available,
            slice,
            some: None,
            full: None,
            holders: vec![
                Holder {
                    name: "java".to_owned(),
                    memory: 9523 * MIB,
                },
                Holder {
                    name: "qemu-system-x86".to_owned(),
                    memory: 5939 * MIB,
                },
            ],
            ceiling: None,
            swap: None,
        },
        limit,
        spare: limit.saturating_sub(budget.memory_in_use),
        stalled: false,
        credit: None,
    });
}

/// `RUNNERD_DEMO_SCENE`, for looking at states the default scene lacks: `idle`, no runner at all under measured
/// admission, so every slot holds none of the budget; `many`, a profile of 18 more slots, each with a warm runner
/// holding 1G, so the lanes no longer fit at their floor; `profiles`, six caps from 1G to 64G on Capacity's scale;
/// `long`, the first jobs' names and branches near 100 characters long; `bounds`, the Maven store past its bound
/// beside a 900G mirror; `empty`, a daemon that has run no job yet, for the pages' empty states.
fn scene(fake: &fake::Fake) {
    const GIB: u64 = 1 << 30;
    let Ok(scene) = std::env::var("RUNNERD_DEMO_SCENE") else {
        return;
    };
    let mut fixtures = fake.fixtures.lock().unwrap();
    let at = now();
    let status = &mut fixtures.status;
    match scene.as_str() {
        "idle" => {
            for slot in status
                .profiles
                .iter_mut()
                .flat_map(|profile| &mut profile.slots)
            {
                slot.runner = None;
            }
            status.queued.clear();
            for repository in &mut status.repositories {
                repository.queued_jobs = 0;
                for pool in &mut repository.pools {
                    pool.queued_jobs = 0;
                }
            }
            // What the busy scene reserved goes with its runners: memory but the mirror's, the host's spare
            // worked out from it, and every CPU.
            status.budget.memory_in_use = status.budget.mirror_memory;
            if let Some(host) = &mut status.budget.host {
                host.spare = host.limit.saturating_sub(status.budget.memory_in_use);
            }
            if let Some(cpu) = &mut status.budget.cpu {
                cpu.in_use = 0;
            }
            for job in &mut fixtures.jobs {
                if job.summary.state == JobState::Running {
                    job.summary.state = JobState::Completed;
                }
            }
        }
        "many" => {
            let slots = (1..=18)
                .map(|slot| SlotStatus {
                    slot,
                    runner: Some(RunnerStatus {
                        repository: "halyard/ledger".to_owned(),
                        name: format!("runnerd-3f9a-forge-matrix-{slot}-{at}"),
                        runner_id: 5000 + u64::from(u32::try_from(slot).unwrap()),
                        container: format!("{at:x}{slot}"),
                        busy: false,
                        started_at: at - 60 * slot as u64,
                        reserved: GIB,
                        warm: true,
                    }),
                })
                .collect();
            status.profiles.push(ProfileStatus {
                name: "matrix".to_owned(),
                image: "runnerd-runner:node".to_owned(),
                cpus: 1,
                memory: GIB,
                reseeding: false,
                slots,
            });
            status.budget.memory_in_use += 18 * GIB;
        }
        "empty" => empty(&mut fixtures),
        "bounds" => {
            // The Maven store past its bound, and a mirror far larger than every other volume.
            for volume in &mut fixtures.caches.volumes {
                match volume.name.as_str() {
                    "runnerd-maven" => volume.bytes = 6 * GIB + (512 << 20),
                    "runnerd-mirror" => volume.bytes = 900 * GIB,
                    _ => {}
                }
            }
        }
        "long" => {
            // Names and branches as long as GitHub lets them be, in the first rows.
            for job in fixtures.jobs.iter_mut().take(3) {
                job.summary.name = format!("integration tests{}", " with a matrix entry".repeat(4));
                job.summary.branch =
                    Some(format!("feature/{}", "long-running-migration-".repeat(4)));
            }
        }
        "profiles" => {
            // Caps from 1G to 64G on one scale: a small cap's label has less room than itself.
            let mut sizing = fake::sizing(&fixtures);
            let template = sizing.profiles[0].clone();
            sizing.profiles = [1, 2, 4, 9, 16, 64]
                .into_iter()
                .enumerate()
                .map(|(index, gib)| {
                    let mut profile = template.clone();
                    profile.name = format!("p{index}-{gib}g");
                    profile.memory = gib * GIB;
                    profile
                })
                .collect();
            fixtures.sizing = Some(sizing);
        }
        other => println!("RUNNERD_DEMO_SCENE: no scene {other}, so the default one"),
    }
}

/// A daemon that has run no job yet: no history, no insights, no caches measured.
fn empty(fixtures: &mut fake::Fixtures) {
    let at = fixtures.now;
    fixtures.jobs.clear();
    fixtures.samples.clear();
    fixtures.logs.clear();
    fixtures.stats.clear();
    fixtures.cache_history.clear();
    fixtures.caches = runnerd_core::api::Caches {
        measured_at: None,
        volumes: vec![],
        build: None,
        actions: None,
        grype: None,
    };
    fixtures.insights = Some(runnerd_core::api::Insights {
        since: at - 14 * 86_400,
        until: at,
        insights: vec![],
        waits: vec![],
        host: vec![],
        disk: None,
        queue: vec![],
    });
    for profile in &mut fixtures.status.profiles {
        for slot in &mut profile.slots {
            slot.runner = None;
        }
    }
    // No profile has run a job: each is a chip on Capacity, not a lane.
    let mut sizing = fake::sizing(fixtures);
    for profile in &mut sizing.profiles {
        profile.peaks = runnerd_core::api::Peaks::default();
        profile.wait = runnerd_core::api::Spread::default();
        profile.slot_wait = runnerd_core::api::Spread::default();
        profile.starts = runnerd_core::api::Starts::default();
        profile.busy = 0;
        profile.advice = runnerd_core::api::Advice::TooFew { jobs: 0 };
        profile.revert = None;
        profile.hold = None;
        profile.jobs.clear();
    }
    fixtures.sizing = Some(sizing);
}

/// Another machine's dashboard, as `[web] peers` links it from every page's header.
fn peers() -> Vec<runnerd_core::config::Peer> {
    vec![runnerd_core::config::Peer {
        name: "skiff".to_owned(),
        origin: "https://skiff.tail0b5e.ts.net".parse().unwrap(),
    }]
}
