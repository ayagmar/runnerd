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
    Resize, RunnerStatus, Sample, SizingChange, SizingHold, SlotStatus, Waiting,
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
    busy_desktop(&fake);
    fake::serve(&socket, fake.clone());
    let listener = runnerd_web::listen(&addr).await.unwrap().unwrap();
    let sessions = runnerd_web::Sessions::open(dir.join("dashboard-session")).unwrap();
    let web = runnerd_web::spawn(
        listener,
        socket.clone(),
        proxies,
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
                let base = 9 << 30;
                let sample = HostSample {
                    at,
                    cpu: 2500 + n * 4271 % 4800,
                    memory: base + (n * 104_729 % 89) * (base / 300),
                };
                fixtures.host.push(sample.clone());
                events.push(Event::HostSample { sample });
            }
        }
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
        refused: Vec::new(),
    });
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
        },
        limit,
        spare: limit.saturating_sub(budget.memory_in_use),
        stalled: false,
    });
}
