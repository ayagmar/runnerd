//! The engine's invariants over generated sequences of events, on the fakes: jobs queued with every kind of labels,
//! taken, finished, cancelled and lost; runners that fail to start, before or after Docker made their container,
//! wait or exit idle; Docker calls that fail once; log captures that fail or break off; samples of the slice and the
//! host, short of memory or stalling, also between two starts of one tick; reseeds asked for; reloads, restarts and stops;
//! time passing. After each
//! tick the named checks below must hold, and once the sequence drains no runner may be left.
//!
//! The sequences come from a seeded generator, so a failure replays: it prints its seed and the shortest sequence
//! it shrank to. `RUNNERD_INVARIANT_CASES` runs more cases than the default, `RUNNERD_INVARIANT_SEED` one seed.
//!
//! A change that must not change behaviour is checked by its trace: `RUNNERD_INVARIANT_TRACE=<directory>` makes
//! `the_trace_of_each_seed` write what each case asked of GitHub and Docker and its status, step by step, and its
//! history's jobs. Run it on the base commit and on the change and compare the directories; never sort them, as the
//! order is what it shows. See `TRACE_UNSTABLE`.

use std::fmt::Write as _;
use std::panic::AssertUnwindSafe;
use std::time::SystemTime;

use futures_util::FutureExt as _;

use super::*;
use crate::admission::host;
use crate::admission::pooled::{self, POOL_AFTER};
use crate::api::{Basis, HoldCause, JobPeak};
use crate::config::Admission;
use crate::observe::metrics::Events;
use crate::scheduler::engine::clock::Clock;
use crate::scheduler::engine::control::RunningJobs;
use crate::scheduler::{Decision, IDLE_TIMEOUT, PRIORITY_AFTER};
use crate::store::{RunnerRecord, SampleRecord};
use crate::time;

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// The slice's `MemoryMax`: two heavy runners, or one and a light one, not two and a light one: a runner counted
/// at less than it holds lets one start too many.
const MEMORY_MAX: u64 = 8 * G + 512 * M;

/// The slice's CPUs, in millicores: one heavy runner and one light one under `cpu_overcommit = 1.0`, where memory
/// has room for two heavy ones, so CPU binds before memory.
const SLICE_CPUS: u64 = 3000;

/// What the host has, in all.
const HOST_TOTAL: u64 = 64 * G;

/// The host's margin for page cache (ADR 0055): the larger of 2G and a sixteenth of the host.
const HOST_MARGIN: u64 = HOST_TOTAL / 16;

/// How a seed container's start ends in the names of what a tick started.
const SEED_START: &str = "'s seed container";

/// Cases run by default; `RUNNERD_INVARIANT_CASES` sets another number.
const CASES: u64 = 64;

/// Events in a case.
const EVENTS: usize = 40;

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
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is a config choice a reload flips on its own"
)]
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
    /// `[host] pooled_count`.
    pooled_count: bool,
    /// `[host] pooled_host`.
    pooled_host: bool,
    /// The `heavy` profile's memory, in gigabytes.
    heavy_memory: u64,
    /// `[caches] mirror`'s `enabled`.
    mirror: bool,
    /// The `heavy` profile's `record_failures`.
    record_failures: bool,
    /// Whether the engine has a build cache, which it is given at its start.
    build_cache: bool,
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
        pooled_count: true,
        pooled_host: true,
        heavy_memory: 4,
        mirror: false,
        record_failures: false,
        build_cache: false,
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
        if !self.pooled_count {
            host.push_str("pooled_count = false\n");
        }
        if !self.pooled_host {
            host.push_str("pooled_host = false\n");
        }
        let mut config = format!(
            r#"
[host]
slice = "ci.slice"
isolation = "sysbox"
{host}
[caches]
mirror = {{ enabled = {}, memory = "512M", cpus = 1, ttl = "168h" }}
pnpm = {{ max_size = "5G" }}
maven = {{ max_size = "5G" }}

[profiles.heavy]
image_stage = "app"
cpus = 2
memory = "{}G"
slots = {}
record_failures = {}
"#,
            self.mirror, self.heavy_memory, self.heavy_slots, self.record_failures
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
    PooledCount,
    PooledHost,
    HeavyMemory,
    Mirror,
    RecordFailures,
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
    /// Someone removes a running job's container, and Docker takes until after the next tick or pass: it refuses its
    /// log and a second removal meanwhile.
    Removing(usize),
    /// The next runner fails to start in Docker.
    StartFails,
    /// The next runner fails to start after Docker made and started its container, which GitHub may give a job
    /// meanwhile; the start's removal of it may fail too, which leaves it running.
    StartFailsLate {
        takes_job: bool,
        cleanup_fails: bool,
    },
    /// The next log capture fails to start, or prints what it has and breaks off.
    CaptureFails {
        mid_stream: bool,
    },
    /// An idle runner exits at once, without a job.
    ExitsAtOnce(usize),
    /// An idle runner exits without a job after waiting `IDLE_TIMEOUT`.
    ExitsIdle(usize),
    /// An idle runner connected and exits without a job, as when its job is cancelled before it begins.
    ExitsConnected(usize),
    /// Docker fails the next inspect of a runner's container.
    InspectFails(usize),
    /// Docker fails the first inspect of the next runner container started.
    StartedInspectFails,
    /// Docker fails the next remove.
    RemoveFails,
    /// The sampler reads the slice and the host: busy runners use `percent` of their caps, the host has
    /// `available` gigabytes available, and its tasks stall or not.
    Sample {
        percent: u64,
        available: u64,
        stalled: bool,
    },
    /// As `Sample`, while the next runner starts: after the plan, and before the tick's later starts.
    SampleDuringStart {
        percent: u64,
        available: u64,
        stalled: bool,
    },
    /// Someone asks to reseed the heavy (`light` false) or the light profile.
    Reseed {
        light: bool,
    },
    Reload(Change),
    /// The daemon restarts: a new engine on the same history and Docker.
    Restart,
    /// The daemon restarts and a signal stops it before its first tick, so before any listing says which of the
    /// runners it adopted are idle; then it starts again.
    RestartThenStop,
    /// As `Restart`, while Docker fails the first inspect of a runner's container.
    RestartInspectFails(usize),
    /// The daemon stops, then starts again: on a signal, or with `runnerd stop` once Docker is gone.
    Stop {
        docker_gone: bool,
    },
    /// Time passes: idle runners waited `IDLE_TIMEOUT`, holds after failures ran out.
    Elapse,
    /// Running jobs pass their first minute, and the predictor says what each reservation goes by from here on.
    Settle,
    /// Each repository's `build` has runs behind it in the heavy pool, so its prediction goes by them.
    PastRuns,
    /// Each repository's `build` has runs behind it in the heavy pool peaking at 3G, and `o/r`'s `lint` in the light
    /// pool at 768M: reservations near their caps, whose headroom can pass what the host keeps for growth.
    PastLargeRuns,
    Tick,
    /// Docker says runners stopped, and a pass between ticks frees their slots and starts runners from the last
    /// tick's listings (ADR 0095).
    Pass,
    /// A running job's runner and container go, and GitHub goes on saying the job is in progress.
    RunnerGoes(usize),
    /// As `RunnerGoes`, and GitHub goes on listing the job queued.
    RunnerGoesStillListed(usize),
    /// GitHub says a job whose runner went completed, and stops listing it.
    OpenJobCompletes(usize),
    /// A forced pause of `o/r` or `o/s`: their runs are cancelled.
    ForcedPause {
        second: bool,
    },
    Resume {
        second: bool,
    },
    /// GitHub lists a runner of this machine's, busy, that no slot holds: its deletes answer `Busy`.
    BusyOrphan,
    /// The heavy profile's snapshot measures over `snapshot_max` at the next check, which no runner is in the way of.
    OversizedSnapshot,
    /// As `Restart`, while Docker fails the restart's first read of a container's log.
    RestartLogsFail,
    /// The daemon restarts with a build cache.
    UseBuildCache,
    /// A running job saves to the build cache while GitHub names no runner for it.
    SaveUnnamed(usize),
}

fn generate(rng: &mut Rng) -> Vec<Step> {
    (0..EVENTS)
        .map(|_| match rng.below(112) {
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
            96 => Step::Removing(rng.index()),
            41..=42 => Step::StartFails,
            43 => Step::ExitsAtOnce(rng.index()),
            44 => Step::ExitsIdle(rng.index()),
            95 => Step::ExitsConnected(rng.index()),
            45..=46 => Step::InspectFails(rng.index()),
            47 => Step::RemoveFails,
            48..=54 => Step::Sample {
                percent: [10, 50, 90, 120][rng.index() % 4],
                available: [3, 6, 8, 9, 20, 50][rng.index() % 6],
                stalled: rng.below(5) == 0,
            },
            55..=62 => Step::Reload(match rng.below(10) {
                0 => Change::HeavySlots(1 + u32::try_from(rng.below(3)).unwrap()),
                1 => Change::LightSlots(1 + u32::try_from(rng.below(3)).unwrap()),
                2 => Change::LightPool,
                3 => Change::SecondRepository,
                4 => Change::LightProfile,
                5 => Change::Policy,
                6 => Change::PooledCount,
                7 => Change::HeavyMemory,
                8 => Change::PooledHost,
                _ => Change::CpuOvercommit,
            }),
            63..=64 => Step::Restart,
            65 => Step::RestartThenStop,
            66..=67 => Step::RestartInspectFails(rng.index()),
            68..=71 => Step::Elapse,
            72..=73 => Step::Reseed {
                light: rng.below(2) == 0,
            },
            74 => Step::StartedInspectFails,
            75..=76 => Step::StartFailsLate {
                takes_job: rng.below(2) == 0,
                cleanup_fails: rng.below(2) == 0,
            },
            77..=78 => Step::SampleDuringStart {
                percent: [10, 50, 90, 120][rng.index() % 4],
                available: [3, 8, 20, 50][rng.index() % 4],
                stalled: rng.below(5) == 0,
            },
            79..=80 => Step::CaptureFails {
                mid_stream: rng.below(2) == 0,
            },
            81..=82 => Step::Stop {
                docker_gone: rng.below(2) == 0,
            },
            83..=89 => Step::Pass,
            90..=93 => Step::Settle,
            100..=101 => Step::RunnerGoes(rng.index()),
            102 => Step::RunnerGoesStillListed(rng.index()),
            103..=104 => Step::OpenJobCompletes(rng.index()),
            105 => Step::ForcedPause {
                second: rng.below(3) == 0,
            },
            106 => Step::Resume {
                second: rng.below(3) == 0,
            },
            107 => Step::BusyOrphan,
            108 => Step::OversizedSnapshot,
            109 => Step::Reload(Change::Mirror),
            110 => Step::Reload(Change::RecordFailures),
            111 => Step::RestartLogsFail,
            _ => Step::Tick,
        })
        .collect()
}

/// What GitHub's listing of queued jobs shows of a job a runner just took.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Listing {
    /// It no longer lists it.
    Current,
    /// It still lists it until the next tick, as it does for a few seconds.
    Stale,
}

/// When an idle runner exits without a job.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Exit {
    AtOnce,
    AfterIdleTimeout,
    Connected,
}

/// A runner the harness made exit without a job, until a tick or a pass frees its slot.
#[derive(Debug, Clone)]
struct ExitWatch {
    container: String,
    repository: String,
    profile: String,
    exit: Exit,
    /// Its pool was held, or a streak of runners that left was under way, when it exited.
    quiet_before: bool,
}

/// Where a job is, as GitHub knows it.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Queued,
    /// A runner of this machine runs it.
    Running {
        container: String,
    },
    /// Its runner went and GitHub has not said it ended: `ran` as for `Done`, once it does.
    Open {
        ran: Option<(String, String)>,
    },
    /// Over: `ran` is the profile and repository of the runner that ran it, if one of this machine's did.
    Done {
        ran: Option<(String, String)>,
    },
}

#[derive(Debug, Clone)]
struct JobModel {
    repository: &'static str,
    /// `lint` for a light job, `build` for any other: a third job beside each repository's `build`, so the host's
    /// credit, which keeps the two largest jobs' headroom, can stand (ADR 0108).
    name: &'static str,
    labels: Vec<String>,
    phase: Phase,
    /// The profile and repository of the runner that took it.
    taken_by: Option<(String, String)>,
    /// A tick saw it queued, so the engine follows it.
    listed: bool,
    /// A tick saw it running once it was listed and no longer queued: GitHub's details then name its runner,
    /// whatever its log capture said.
    named_by_github: bool,
}

/// A reading of the host the harness gave the sampler.
#[derive(Debug, Clone, Copy)]
struct HostReading {
    /// Unix seconds.
    at: u64,
    slice: u64,
    available: u64,
    stalled: bool,
}

impl HostReading {
    /// Whether admission still goes by it at `now` (ms): it is from the last three sample intervals.
    fn fresh(&self, now: u64) -> bool {
        self.at + 3 * api::SAMPLE_INTERVAL >= now / 1000
    }
}

/// A broken invariant: which, at which step, and what was seen.
#[derive(Debug, Clone)]
struct Violation {
    invariant: &'static str,
    step: usize,
    detail: String,
}

/// A case: the engine on the fakes, and what GitHub and Docker hold as the world sees it.
/// How many checks saw what they exist for: none of either in all cases, and they check nothing.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    barriers: usize,
    credits: usize,
    /// Checks that found the host's credit standing (ADR 0108).
    host_credits: usize,
    /// Ticks that left the larger margin standing with waiters, none fitting the plain one.
    bands: usize,
    /// Ticks that gave it up for a job that fits the plain one, which the next plan starts.
    released: usize,
    /// Runners that exited without a job and whose slots were freed.
    exits_freed: usize,
    /// Jobs a runner started for that had not taken it yet.
    runners_starting: usize,
    /// Ticks that ended with a job whose runner went and that GitHub has not said ended.
    closing: usize,
    /// Ticks that ended with such a job still followed as queued too.
    closing_and_queued: usize,
    /// Ticks that ended with a runner's build cache uploads waiting for GitHub's word.
    ended_uploads: usize,
    /// Ticks that ended with a cancelled run's runner waiting to exit.
    cancelling: usize,
    /// Ticks that ended with a busy registration no slot holds.
    busy_orphans: usize,
    /// Ticks that ended with a snapshot marked over its limit.
    oversized: usize,
    /// Ticks that ended with a leftover container Docker could not say what to do with.
    unread: usize,
    /// Ticks that ended with the registry mirror running.
    mirrors: usize,
    /// Ticks that ended with a pool's memory and CPU waits on different jobs.
    split_waits: usize,
    /// Ticks that ended with a capture running beside its next start's memory of where it broke.
    captures_with_recaptures: usize,
}

impl Seen {
    fn add(&mut self, case: Seen) {
        self.barriers += case.barriers;
        self.exits_freed += case.exits_freed;
        self.runners_starting += case.runners_starting;
        self.closing += case.closing;
        self.closing_and_queued += case.closing_and_queued;
        self.ended_uploads += case.ended_uploads;
        self.cancelling += case.cancelling;
        self.busy_orphans += case.busy_orphans;
        self.oversized += case.oversized;
        self.unread += case.unread;
        self.mirrors += case.mirrors;
        self.split_waits += case.split_waits;
        self.captures_with_recaptures += case.captures_with_recaptures;
    }

    /// The states no generated case reached, so nothing checks them.
    fn unreached(&self) -> Vec<&'static str> {
        [
            ("a barrier", self.barriers),
            ("a runner that left without a job", self.exits_freed),
            ("a runner started before its job", self.runners_starting),
            ("a job whose runner went, still open", self.closing),
            (
                "a job closing and followed as queued",
                self.closing_and_queued,
            ),
            ("uploads waiting for GitHub", self.ended_uploads),
            ("a cancelled run's runner", self.cancelling),
            ("a busy orphan registration", self.busy_orphans),
            ("an oversized snapshot", self.oversized),
            ("an unread leftover", self.unread),
            ("the mirror running", self.mirrors),
            (
                "a pool's memory and CPU waits on different jobs",
                self.split_waits,
            ),
            (
                "a capture beside a recapture",
                self.captures_with_recaptures,
            ),
        ]
        .into_iter()
        .filter_map(|(state, count)| (count == 0).then_some(state))
        .collect()
    }
}

struct Case {
    /// What the checks saw.
    seen: Seen,
    /// What the age of a reading goes by, the engine's too, stopped: only `Elapse` writes an old one, however slow the
    /// machine. A runner's start goes by the system's clock, so never reads old.
    clock: Clock,
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
    /// Jobs the last tick listed queued that were cancelled since: a pass plans from that listing.
    cancelled_since_tick: BTreeSet<u64>,
    /// Docker calls checked so far.
    calls_seen: usize,
    /// The last reading of the host this daemon's store has, which a restart loses.
    host: Option<HostReading>,
    /// The reading a `SampleDuringStart` took, until the tick it was taken in checks the starts after it.
    host_during_start: Arc<Mutex<Option<HostReading>>>,
    /// Log captures `CaptureFails` failed, by container, as of the start of the last tick.
    capture_faults: HashMap<String, u32>,
    /// Runners that exited without a job, whose slots the engine has not freed yet. A restart forgets them, as it
    /// forgets holds: recovery removes such a container without a verdict on its pool.
    exits: Vec<ExitWatch>,
    /// Each busy runner as the tick or pass began: its container, repository and profile. One that ends in it proves
    /// its pool works and ends any hold.
    busy_before: Vec<(String, String, String)>,
    /// Runners whose job ended since the engine last looked, as in `busy_before`: one that took and finished a job
    /// between two looks was never seen busy.
    ended_between: Vec<(String, String, String)>,
    /// Jobs GitHub goes on listing queued after their runner went.
    listed_after_runner: BTreeSet<u64>,
    /// Containers of runs a forced pause cancelled: the engine may remove them, ending their jobs.
    forced: BTreeSet<String>,
    /// Registrations `BusyOrphan` made.
    orphans: Vec<u64>,
    /// What the case asked of the fakes, step by step, when traced.
    trace: Option<Vec<String>>,
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
    fn new() -> Case {
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
        let clock = Clock::stopped();
        let (engine, handle) = Case::engine(&github, &docker, &dir, shape, clock);
        Case {
            seen: Seen::default(),
            clock,
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
            cancelled_since_tick: BTreeSet::new(),
            calls_seen: 0,
            host: None,
            host_during_start: Arc::default(),
            capture_faults: HashMap::new(),
            exits: Vec::new(),
            busy_before: Vec::new(),
            ended_between: Vec::new(),
            listed_after_runner: BTreeSet::new(),
            forced: BTreeSet::new(),
            orphans: Vec::new(),
            trace: None,
            step: 0,
        }
    }

    fn engine(
        github: &FakeGithub,
        docker: &FakeDocker,
        dir: &TestDir,
        shape: Shape,
        clock: Clock,
    ) -> (Engine<FakeGithub, FakeDocker>, Handle) {
        let (mut engine, handle) = if shape.build_cache {
            let (engine, handle, _cache) =
                build_cache::engine_with_cache(github, docker, dir, &shape.toml());
            (engine, handle)
        } else {
            engine_with(github, docker, dir, &shape.toml())
        };
        engine.clock = clock;
        engine.memory_max = Some(MEMORY_MAX);
        engine.cpus = Some(SLICE_CPUS);
        (engine, handle)
    }

    /// Recovers as `Engine::run` does: again until Docker answers.
    async fn recover(&mut self) -> Result<(), Violation> {
        for _ in 0..10 {
            if self.engine.recover().await.is_ok() {
                return Ok(());
            }
        }
        violation(
            self,
            "recovery_succeeds",
            "Docker never answered".to_owned(),
        )
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

    /// When traced, records what this step asked of the fakes, once the background work it began has run.
    async fn trace_step(&mut self, step: &str) {
        if self.trace.is_none() {
            return;
        }
        self.settle_background().await;
        let status = steady(&steady_status(&self.status()).to_string());
        let github = std::mem::take(&mut self.github.0.lock().unwrap().trace);
        let (docker, execs) = {
            let mut state = self.docker.0.lock().unwrap();
            (
                std::mem::take(&mut state.trace),
                std::mem::take(&mut state.exec_trace),
            )
        };
        if let Some(trace) = self.trace.as_mut() {
            trace.push(format!("-- {step}"));
            trace.push(format!("status {status}"));
            trace.extend(github.iter().map(|call| format!("github {}", steady(call))));
            trace.extend(docker.iter().map(|call| format!("docker {}", steady(call))));
            trace.extend(execs.iter().map(|call| format!("capture {}", steady(call))));
        }
    }

    /// Waits until what the step set going has stopped calling the fakes: until the calls stop growing for
    /// `QUIET_ROUNDS` rounds in a row, each round letting blocking threads and the runtime's tasks run.
    async fn settle_background(&mut self) {
        let mut quiet = 0;
        let mut last = (usize::MAX, usize::MAX);
        while quiet < QUIET_ROUNDS {
            self.handle.store.flush().await;
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
            let now = (self.github.0.lock().unwrap().trace.len(), {
                let state = self.docker.0.lock().unwrap();
                state.trace.len() + state.exec_trace.len()
            });
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
        }
    }

    /// The trace, with what the case left in its history's jobs table.
    async fn finished_trace(mut self) -> Vec<String> {
        self.handle.store.flush().await;
        let mut trace = self.trace.take().unwrap_or_default();
        trace.push("-- jobs".to_owned());
        let db = rusqlite::Connection::open(self.dir.0.join("history.db")).unwrap();
        let mut rows = db
            .prepare(
                "SELECT id, repository, state, conclusion, github_completed, runner, \
                 started_at IS NOT NULL, finished_at IS NOT NULL, slot_waited > 0, host_waited > 0 \
                 FROM jobs ORDER BY id",
            )
            .unwrap();
        let jobs = rows
            .query_map([], |row| {
                Ok(format!(
                    "{} {} {} {:?} {} {:?} {} {} slot_waited {} host_waited {}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?.map(|name| steady(&name)),
                    row.get::<_, bool>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, bool>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            })
            .unwrap();
        trace.extend(jobs.map(Result::unwrap));
        trace
    }

    fn status(&mut self) -> api::Status {
        self.engine.publish();
        self.handle.status.borrow().clone()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per step, which split up would only be listed twice"
    )]
    async fn apply(&mut self, step: Step) -> Result<(), Violation> {
        match step {
            Step::Queue { second, labels } => {
                let repository = if second { "o/s" } else { "o/r" };
                let id = self.next_job;
                self.next_job += 1;
                let name = if matches!(labels, Labels::Light) {
                    "lint"
                } else {
                    "build"
                };
                let labels = labels.of(repository);
                let mut github = self.github.0.lock().unwrap();
                github.queued.push(QueuedJob {
                    id,
                    run_id: id,
                    name: name.to_owned(),
                    labels: labels.clone(),
                    created_at: time::now_ms(),
                    ..QueuedJob::default()
                });
                github.queued_on.insert(id, repository.to_owned());
                self.jobs.insert(
                    id,
                    JobModel {
                        repository,
                        name,
                        labels,
                        phase: Phase::Queued,
                        taken_by: None,
                        listed: false,
                        named_by_github: false,
                    },
                );
            }
            Step::Assign(pick) => self.assign(pick, Listing::Current).await,
            Step::AssignListed(pick) => self.assign(pick, Listing::Stale).await,
            Step::Finish(pick) => {
                let running = self.running();
                if let Some((id, container)) = pick_from(&running, pick).cloned() {
                    self.end_job(id, &container, "success");
                    // As for `Lose`: Docker runs no capture in a container that stopped.
                    if (self.unobserved(&container) || self.capture_unread(id, &container))
                        && let Some(job) = self.jobs.get_mut(&id)
                    {
                        job.phase = Phase::Done { ran: None };
                    }
                    let name = self.jobs[&id].name;
                    let mut docker = self.docker.0.lock().unwrap();
                    docker
                        .container_logs
                        .insert(container.clone(), format!("Running job: {name}\n"));
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
                    let job = self.jobs.get_mut(&id).unwrap();
                    job.phase = Phase::Done { ran: None };
                    if job.listed {
                        self.cancelled_since_tick.insert(id);
                    }
                }
            }
            Step::Lose(pick) => self.lose(pick),
            Step::Removing(pick) => self.removing(pick),
            Step::StartFails => self.docker.0.lock().unwrap().fail_starts += 1,
            Step::StartFailsLate {
                takes_job,
                cleanup_fails,
            } => {
                let failure = LateFailure {
                    takes_job,
                    cleanup_fails,
                };
                self.docker
                    .0
                    .lock()
                    .unwrap()
                    .late_failures
                    .push_back(failure);
            }
            Step::CaptureFails { mid_stream } => {
                let mut docker = self.docker.0.lock().unwrap();
                *if mid_stream {
                    &mut docker.break_captures
                } else {
                    &mut docker.fail_execs
                } += 1;
            }
            Step::ExitsAtOnce(pick) => self.exit_idle(pick, Exit::AtOnce),
            Step::ExitsIdle(pick) => self.exit_idle(pick, Exit::AfterIdleTimeout),
            Step::ExitsConnected(pick) => self.exit_idle(pick, Exit::Connected),
            Step::InspectFails(pick) => self.inspect_fails(pick),
            Step::StartedInspectFails => self.docker.0.lock().unwrap().fail_started_inspects += 1,
            Step::RemoveFails => self.docker.0.lock().unwrap().fail_removes += 1,
            Step::Sample {
                percent,
                available,
                stalled,
            } => {
                let docker = self.docker.0.lock().unwrap();
                let reading = sample(
                    &docker,
                    &self.handle.store,
                    &self.took_job,
                    self.clock.now_ms(),
                    percent,
                    available,
                    stalled,
                );
                drop(docker);
                self.host = Some(reading);
            }
            Step::SampleDuringStart {
                percent,
                available,
                stalled,
            } => self.sample_during_start(percent, available, stalled),
            // Refused for a profile reseeding already, one a reload removed and one a shell holds.
            Step::Reseed { light } => {
                let _ = self
                    .engine
                    .request_reseed(if light { "light" } else { "heavy" });
            }
            Step::Reload(change) => {
                self.reload(change).await?;
                self.tick().await?;
            }
            Step::Restart => {
                self.restart().await?;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::RestartThenStop => {
                self.restart().await?;
                self.stop(false).await?;
                self.restart().await?;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::Stop { docker_gone } => {
                self.stop(docker_gone).await?;
                self.restart().await?;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::RestartInspectFails(pick) => {
                self.inspect_fails(pick);
                self.restart().await?;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::Elapse => self.elapse(),
            Step::Settle => self.settle(),
            Step::PastRuns => {
                self.past_runs(&[("o/r", "build", "heavy", G), ("o/s", "build", "heavy", G)])
                    .await;
            }
            Step::PastLargeRuns => {
                let runs = [
                    ("o/r", "build", "heavy", 3 * G),
                    ("o/s", "build", "heavy", 3 * G),
                    ("o/r", "lint", "light", 768 * M),
                ];
                self.past_runs(&runs).await;
            }
            Step::Tick => self.tick().await?,
            Step::Pass => self.pass().await?,
            Step::RunnerGoes(pick) => self.runner_goes(pick, Listing::Current),
            Step::RunnerGoesStillListed(pick) => self.runner_goes(pick, Listing::Stale),
            Step::OpenJobCompletes(pick) => self.open_job_completes(pick),
            Step::ForcedPause { second } => self.forced_pause(second).await,
            Step::Resume { second } => self.resume(second).await,
            Step::BusyOrphan => self.busy_orphan(),
            Step::OversizedSnapshot => self.oversized_snapshot(),
            Step::RestartLogsFail => {
                self.docker.0.lock().unwrap().fail_logs = 1;
                self.restart().await?;
                // For the restart alone: a later log that cannot be read says nothing of why its runner left.
                self.docker.0.lock().unwrap().fail_logs = 0;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::UseBuildCache => {
                self.shape.build_cache = true;
                self.restart().await?;
                self.tick().await?;
                self.a_restart_takes_the_host_credit_back()?;
            }
            Step::SaveUnnamed(pick) => self.save_unnamed(pick).await,
        }
        self.no_removal_of_a_running_job()
    }

    /// A running job's runner and container go while GitHub goes on saying the job runs, and with `Listing::Stale`
    /// goes on listing it queued too.
    fn runner_goes(&mut self, pick: usize, listing: Listing) {
        let running = self.running();
        let Some((id, container)) = pick_from(&running, pick).cloned() else {
            return;
        };
        let ran = if self.unobserved(&container) || self.capture_unread(id, &container) {
            None
        } else {
            self.jobs[&id].taken_by.clone()
        };
        let job = self.jobs.get_mut(&id).unwrap();
        job.phase = Phase::Open { ran };
        let (name, labels) = (job.name, job.labels.clone());
        let mut docker = self.docker.0.lock().unwrap();
        let runner = docker
            .containers
            .iter()
            .find(|candidate| candidate.id == container)
            .map(|candidate| candidate.name.clone());
        docker
            .containers
            .retain(|candidate| candidate.id != container);
        drop(docker);
        let mut github = self.github.0.lock().unwrap();
        github
            .runners
            .retain(|registered| Some(&registered.name) != runner.as_ref());
        github.queued.retain(|queued| queued.id != id);
        if listing == Listing::Stale {
            github.queued.push(QueuedJob {
                id,
                run_id: id,
                name: name.to_owned(),
                labels,
                created_at: time::now_ms(),
                ..QueuedJob::default()
            });
            self.listed_after_runner.insert(id);
        }
        self.still_listed.remove(&id);
        drop(github);
        self.took_job.remove(&container);
    }

    /// The jobs whose runner went and that GitHub has not said ended.
    fn open(&self) -> Vec<u64> {
        self.jobs
            .iter()
            .filter(|(_, job)| matches!(job.phase, Phase::Open { .. }))
            .map(|(id, _)| *id)
            .collect()
    }

    fn open_job_completes(&mut self, pick: usize) {
        let open = self.open();
        let Some(id) = pick_from(&open, pick).copied() else {
            return;
        };
        let job = self.jobs.get_mut(&id).unwrap();
        let Phase::Open { ran } = job.phase.clone() else {
            return;
        };
        job.phase = Phase::Done { ran };
        let name = job.name;
        let mut github = self.github.0.lock().unwrap();
        let runner = github.jobs.get(&id).and_then(|job| job.runner_name.clone());
        let mut completed = details(id, "completed", Some("success"));
        completed.run_id = id;
        completed.name = name.to_owned();
        completed.runner_name = runner;
        github.jobs.insert(id, completed);
        github.queued.retain(|job| job.id != id);
        drop(github);
        self.listed_after_runner.remove(&id);
    }

    async fn forced_pause(&mut self, second: bool) {
        let repository = if second { "o/s" } else { "o/r" };
        let running = self.running();
        self.forced.extend(
            running
                .into_iter()
                .filter(|(id, _)| self.jobs[id].repository == repository)
                .map(|(_, container)| container),
        );
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .command(Command::Pause {
                target: Target::Repository(repository.to_owned()),
                jobs: RunningJobs::Kill,
                reply,
            })
            .await;
        // Refused for a repository a reload removed.
        let _ = answer.await;
    }

    async fn resume(&mut self, second: bool) {
        let repository = if second { "o/s" } else { "o/r" };
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .command(Command::Resume {
                target: Target::Repository(repository.to_owned()),
                reply,
            })
            .await;
        let _ = answer.await;
    }

    /// The jobs of containers a forced pause's kill removed are over.
    fn forced_jobs_end(&mut self) {
        let gone: Vec<(u64, String)> = {
            let docker = self.docker.0.lock().unwrap();
            self.running()
                .into_iter()
                .filter(|(_, container)| {
                    self.forced.contains(container)
                        && !docker.containers.iter().any(|c| c.id == *container)
                })
                .collect()
        };
        for (id, container) in gone {
            self.end_job(id, &container, "cancelled");
            self.jobs.get_mut(&id).unwrap().phase = Phase::Done { ran: None };
        }
    }

    fn busy_orphan(&mut self) {
        let name = format!(
            "{}orphan-{}",
            self.engine.runner_prefix(),
            self.orphans.len()
        );
        let mut github = self.github.0.lock().unwrap();
        github.registrations += 1;
        let id = 1000 + github.registrations;
        github.runners.push(Runner {
            id,
            name,
            status: "online".to_owned(),
            busy: true,
        });
        drop(github);
        self.orphans.push(id);
    }

    /// The heavy profile's snapshot is too large, which a check of the caches finds once no runner runs.
    fn oversized_snapshot(&mut self) {
        let volume = Names::new("runnerd").base("heavy");
        self.docker
            .0
            .lock()
            .unwrap()
            .volume_sizes
            .insert(volume, u64::MAX / 2);
        self.engine.make_trim_due();
    }

    /// A running job saves to the build cache while GitHub says it is queued on no runner.
    async fn save_unnamed(&mut self, pick: usize) {
        let running = self.running();
        let Some((id, container)) = pick_from(&running, pick).cloned() else {
            return;
        };
        let name = self
            .docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|candidate| candidate.id == container)
            .map(|candidate| candidate.name.clone());
        let Some(socket) = name.and_then(|name| build_cache::socket_of(&self.docker, &name)) else {
            return;
        };
        if let Some(details) = self.github.0.lock().unwrap().jobs.get_mut(&id) {
            details.status = "queued".to_owned();
            details.runner_name = None;
        }
        build_cache::save(&socket).await;
    }

    /// A running job's container vanishes, as when someone removes it.
    fn lose(&mut self, pick: usize) {
        if let Some(container) = self.end_running(pick) {
            self.docker
                .0
                .lock()
                .unwrap()
                .containers
                .retain(|candidate| candidate.id != container);
        }
    }

    /// Someone starts removing a running job's container.
    fn removing(&mut self, pick: usize) {
        if let Some(container) = self.end_running(pick) {
            let mut docker = self.docker.0.lock().unwrap();
            set_running(&mut docker, &container, false);
            docker.being_removed.insert(container);
        }
    }

    /// Docker finishes the removals someone began.
    fn removals_finish(&mut self) {
        let mut docker = self.docker.0.lock().unwrap();
        let removed = std::mem::take(&mut docker.being_removed);
        docker
            .containers
            .retain(|candidate| !removed.contains(&candidate.id));
    }

    /// Ends a running job whose container goes without a word from it, and says which container that is.
    fn end_running(&mut self, pick: usize) -> Option<String> {
        let running = self.running();
        let (id, container) = pick_from(&running, pick).cloned()?;
        self.end_job(id, &container, "failure");
        // Gone before Docker answered any inspect of it: nothing read its log, so nothing can name
        // the runner that ran the job.
        if (self.unobserved(&container) || self.capture_unread(id, &container))
            && let Some(job) = self.jobs.get_mut(&id)
        {
            job.phase = Phase::Done { ran: None };
        }
        Some(container)
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
    async fn assign(&mut self, pick: usize, listing: Listing) {
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
            if listing == Listing::Stale {
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
            running.name = self.jobs[&id].name.to_owned();
            running.runner_name = Some(container.name.clone());
            running.labels = self.jobs[&id].labels.clone();
            github.jobs.insert(id, running);
            assert!(runner_id.is_some(), "an assigned runner is registered");
        }
        {
            let mut docker = self.docker.0.lock().unwrap();
            docker
                .container_captures
                .insert(container.id.clone(), format!("@@job {id} {id}\n"));
            docker
                .diag
                .insert(container.id.clone(), docker::tests::diag_archive(id));
        }
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

    /// GitHub completes a running job, and its ephemeral runner leaves. Its stopped container's log still names the
    /// job, as Docker keeps it until the container is removed; the engine names a slot's later runners otherwise, so
    /// none takes its log.
    ///
    /// It also leaves GitHub's queued listing, even if `AssignListed` left it there: the harness assumes a stale
    /// listing never outlives its job. Where it does, as for a runner killed during its job's setup within GitHub's
    /// few seconds of lag, the engine no longer counts the job as taken once its runner exits, and starts a second
    /// runner for it, which waits out `IDLE_TIMEOUT`: one idle runner, rare enough to leave.
    fn end_job(&mut self, id: u64, container: &str, conclusion: &str) {
        let job = self.jobs.get_mut(&id).unwrap();
        job.phase = Phase::Done {
            ran: job.taken_by.clone(),
        };
        let by_container = self
            .docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|candidate| candidate.id == container)
            .map(|candidate| candidate.name.clone());
        let mut github = self.github.0.lock().unwrap();
        let name = github
            .jobs
            .get(&id)
            .and_then(|details| details.runner_name.clone())
            .or(by_container);
        let mut completed = details(id, "completed", Some(conclusion));
        completed.run_id = id;
        completed.name = job.name.to_owned();
        completed.runner_name = name.clone();
        github.jobs.insert(id, completed);
        // GitHub lists a job queued a few seconds after a runner took it, never once it completed.
        github.queued.retain(|job| job.id != id);
        self.still_listed.remove(&id);
        github
            .runners
            .retain(|runner| Some(&runner.name) != name.as_ref());
        drop(github);
        self.took_job.remove(container);
        if let Some((slot, occupant)) = self
            .engine
            .occupied()
            .find(|(_, occupant)| occupant.container == container)
        {
            let repository = occupant.repository.clone();
            self.ended_between
                .push((container.to_owned(), repository, slot.profile));
        }
    }

    /// An idle runner's container exits without a job: at once, after `IDLE_TIMEOUT`, or having connected.
    fn exit_idle(&mut self, pick: usize, exit: Exit) {
        let idle = self.idle_containers();
        let Some(container) = pick_from(&idle, pick).cloned() else {
            return;
        };
        if exit == Exit::AfterIdleTimeout {
            for occupant in self.engine.occupants_mut() {
                if occupant.container == container.id {
                    occupant.started_at -= IDLE_TIMEOUT;
                }
            }
        }
        let watch = self
            .engine
            .occupied()
            .find(|(_, occupant)| occupant.container == container.id)
            .map(|(slot, occupant)| ExitWatch {
                container: container.id.clone(),
                repository: occupant.repository.clone(),
                profile: slot.profile,
                // `Elapse` aged it: the engine reads that as having waited, and so as having connected.
                exit: match occupant.started_at.elapsed() {
                    Ok(waited) if exit == Exit::AtOnce && waited >= IDLE_TIMEOUT => {
                        Exit::AfterIdleTimeout
                    }
                    _ => exit,
                },
                quiet_before: false,
            });
        self.exits.extend(watch.map(|watch| ExitWatch {
            quiet_before: self.pool_is_quiet(&watch.repository, &watch.profile),
            ..watch
        }));
        let log = match exit {
            Exit::Connected => "Listening for Jobs\n",
            Exit::AtOnce | Exit::AfterIdleTimeout => "",
        };
        let mut docker = self.docker.0.lock().unwrap();
        docker
            .container_logs
            .insert(container.id.clone(), log.to_owned());
        set_running(&mut docker, &container.id, false);
    }

    /// Notes the busy runners, as a tick or a pass begins.
    fn note_busy(&mut self) {
        self.busy_before = self
            .engine
            .occupied()
            .filter(|(_, occupant)| {
                occupant.listed_busy()
                    || occupant.named_job().is_some()
                    || self.took_job.contains(&occupant.container)
            })
            .map(|(slot, occupant)| {
                let repository = occupant.repository.clone();
                (occupant.container.clone(), repository, slot.profile)
            })
            .collect();
        self.busy_before.append(&mut self.ended_between);
    }

    /// Whether a pool is neither held now nor in a streak of runners that left without a job.
    fn pool_is_quiet(&self, repository: &str, profile: &str) -> bool {
        self.engine
            .pool_is_quiet(repository, profile, SystemTime::now())
    }

    /// Invariant 8 (ADR 0104), as a tick or a pass frees slots: a pool whose runner exited without connecting is held
    /// back, so a pool whose runners keep failing to start backs off, and a runner that connected and left a quiet
    /// pool brings no hold, as it says nothing of the next start. Another runner of the pool failing in the meantime
    /// is a hold of its own, so that pool proves nothing.
    fn exits_hold_only_what_failed_to_start(&mut self) -> Result<(), Violation> {
        let in_slots: BTreeSet<String> = self
            .engine
            .occupied()
            .map(|(_, occupant)| occupant.container.clone())
            .collect();
        let (waiting, freed): (Vec<ExitWatch>, Vec<ExitWatch>) = std::mem::take(&mut self.exits)
            .into_iter()
            .partition(|watch| in_slots.contains(&watch.container));
        self.exits = waiting;
        let now = SystemTime::now();
        for watch in &freed {
            let Some(hold) = self.engine.pool_hold(&watch.repository, &watch.profile) else {
                continue;
            };
            let standing = hold.held.filter(|held| held.until > now);
            let job_ended = self
                .busy_before
                .iter()
                .any(|(container, repository, profile)| {
                    !in_slots.contains(container)
                        && *repository == watch.repository
                        && *profile == watch.profile
                });
            match watch.exit {
                Exit::AtOnce if standing.is_none() && !job_ended => {
                    let detail = format!("{watch:?} failed to start and {hold:?} holds nothing");
                    return violation(self, "exits_hold_only_what_failed_to_start", detail);
                }
                Exit::Connected if watch.quiet_before && standing.is_some() => {
                    let failed = freed.iter().any(|other| {
                        other.exit == Exit::AtOnce
                            && other.repository == watch.repository
                            && other.profile == watch.profile
                    });
                    let cause = standing.map(|held| held.cause);
                    if !failed && matches!(cause, Some(HoldCause::NoJob | HoldCause::IdleExits)) {
                        let detail = format!("{watch:?} connected and {hold:?} holds its pool");
                        return violation(self, "exits_hold_only_what_failed_to_start", detail);
                    }
                }
                _ => {}
            }
            self.seen.exits_freed += 1;
        }
        Ok(())
    }

    /// Takes a `sample` while the next runner starts.
    fn sample_during_start(&mut self, percent: u64, available: u64, stalled: bool) {
        let (store, took_job, taken, clock) = (
            self.handle.store.clone(),
            self.took_job.clone(),
            self.host_during_start.clone(),
            self.clock,
        );
        self.docker.0.lock().unwrap().on_start = Some(Box::new(move |docker| {
            let now = clock.now_ms();
            let reading = sample(docker, &store, &took_job, now, percent, available, stalled);
            *taken.lock().unwrap() = Some(reading);
        }));
    }

    /// Whether the engine has not watched `container` yet, as Docker failed every inspect of it.
    fn unobserved(&self, container: &str) -> bool {
        self.engine
            .occupied()
            .any(|(_, occupant)| occupant.container == container && !occupant.is_observed())
    }

    /// Whether job `id`, running in `container`, may end without the engine ever reading its log: its log capture
    /// failed and no tick has started it again since, or it failed twice, after which the next start waits longer
    /// than a sequence runs. Not once GitHub named its runner to the engine (`named_by_github`).
    fn capture_unread(&self, id: u64, container: &str) -> bool {
        let faults = self.capture_faults_of(container);
        let retried = self.capture_faults.get(container).copied().unwrap_or(0);
        !self.jobs[&id].named_by_github && (faults >= 2 || faults > retried)
    }

    /// How many log captures in `container` failed so far.
    fn capture_faults_of(&self, container: &str) -> u32 {
        self.docker
            .0
            .lock()
            .unwrap()
            .capture_faults
            .get(container)
            .copied()
            .unwrap_or(0)
    }

    async fn reload(&mut self, change: Change) -> Result<(), Violation> {
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
            Change::PooledCount => shape.pooled_count = !shape.pooled_count,
            Change::PooledHost => shape.pooled_host = !shape.pooled_host,
            // Down and back: a restart whose inspect fails must still count a runner at the cap it started with.
            Change::HeavyMemory => shape.heavy_memory = if shape.heavy_memory == 4 { 2 } else { 4 },
            Change::Mirror => shape.mirror = !shape.mirror,
            Change::RecordFailures => shape.record_failures = !shape.record_failures,
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.engine
            .command(Command::ReplaceConfig {
                config: Box::new(Config::parse(&shape.toml()).unwrap()),
                memory_max: Some(MEMORY_MAX),
                cpus: Some(SLICE_CPUS),
                commit: Commit::new(|| Ok(None)),
                running: RunningSlots::Drain,
                author: Author::Owner,
                reply,
            })
            .await;
        let applied = answer.await;
        if applied != Ok(Ok(Vec::new())) {
            return violation(
                self,
                "a_reload_applies",
                format!("{change:?} answered {applied:?}"),
            );
        }
        self.shape = shape;
        Ok(())
    }

    /// Invariant 7: a stop on a signal, or one Docker fails, ends at once and leaves every container that runs a job
    /// running, for the next start.
    async fn stop(&mut self, docker_gone: bool) -> Result<(), Violation> {
        let running = self.running();
        let mode = if docker_gone {
            self.docker.set_outage(Outage::Refused);
            Shutdown::Graceful
        } else {
            Shutdown::Detach
        };
        self.handle.control.shutdown(mode);
        // Far past what a stop takes, so only a stop that hangs fails it, however slow the machine.
        let stopped = tokio::time::timeout(Duration::from_secs(300), self.engine.schedule()).await;
        self.docker.set_outage(Outage::None);
        if stopped.is_err() {
            return violation(self, "a_stop_ends", format!("{mode:?} never ended"));
        }
        let left: BTreeSet<String> = self
            .running_containers()
            .into_iter()
            .map(|container| container.id)
            .collect();
        let gone: Vec<&(u64, String)> = running
            .iter()
            .filter(|(_, container)| !left.contains(container))
            .collect();
        if !gone.is_empty() {
            let detail = format!("{mode:?} took the containers of running jobs {gone:?}");
            return violation(self, "a_stop_leaves_running_jobs", detail);
        }
        if mode == Shutdown::Detach {
            self.a_detach_deregisters_idle_runners()?;
        }
        Ok(())
    }

    /// ADR 0091: a detaching stop leaves no runner that took no job registered, adopted ones too, even before a
    /// listing said they were idle: one left would take a job with no daemon to watch it.
    fn a_detach_deregisters_idle_runners(&self) -> Result<(), Violation> {
        let registered: BTreeSet<String> = self
            .github
            .0
            .lock()
            .unwrap()
            .runners
            .iter()
            .map(|runner| runner.name.clone())
            .collect();
        let idle: Vec<String> = self
            .idle_containers()
            .into_iter()
            .map(|container| container.name)
            .filter(|name| registered.contains(name))
            .collect();
        if idle.is_empty() {
            return Ok(());
        }
        violation(
            self,
            "a_detach_deregisters_idle_runners",
            format!("{idle:?} stay registered with no job"),
        )
    }

    async fn restart(&mut self) -> Result<(), Violation> {
        self.engine.abort_captures();
        self.handle.store.flush().await;
        let (engine, handle) = Case::engine(
            &self.github,
            &self.docker,
            &self.dir,
            self.shape,
            self.clock,
        );
        self.engine = engine;
        self.handle = handle;
        // The store keeps the host's reading in memory only, and a sample still to come goes to the old store.
        self.host = None;
        self.exits.clear();
        self.docker.0.lock().unwrap().on_start = None;
        self.recover().await
    }

    fn elapse(&mut self) {
        // The sampler stopped: the last readings are older than admission goes by.
        let old = self.clock.now_ms() - 1000 * (3 * api::SAMPLE_INTERVAL + 1);
        let docker = self.docker.0.lock().unwrap();
        let reading = sample(
            &docker,
            &self.handle.store,
            &self.took_job,
            old,
            50,
            50,
            false,
        );
        drop(docker);
        self.host = Some(reading);
        for occupant in self.engine.occupants_mut() {
            occupant.started_at -= IDLE_TIMEOUT;
        }
        for watch in &mut self.exits {
            if watch.exit == Exit::AtOnce {
                watch.exit = Exit::AfterIdleTimeout;
            }
        }
        let past = SystemTime::now() - Duration::from_secs(1);
        self.engine.expire_holds(past);
        self.engine.age_waits(PRIORITY_AFTER);
        self.engine.expire_cancelling();
        self.engine.make_unread_due();
        self.engine
            .age_ended_uploads(crate::scheduler::engine::build_cache::ENDED_WAIT);
    }

    fn settle(&mut self) {
        for occupant in self.engine.occupants_mut() {
            occupant.age_job_start(POOL_AFTER);
            if let Some(admission) = occupant.admission.as_mut() {
                admission.basis = Basis::Job {
                    runs: 10,
                    peak: JobPeak::P95,
                };
            }
        }
    }

    /// Ten finished runs of each repository's job in a profile, peaking as given, with IDs no generated job takes.
    async fn past_runs(&mut self, runs: &[(&str, &str, &str, u64)]) {
        let at = time::now_ms() - 3600 * 1000;
        let store = &self.handle.store;
        for (first, (repository, name, profile, peak)) in (1_u64..).map(|n| n * 1_000_000).zip(runs)
        {
            for id in first..first + 10 {
                let runner = format!("earlier-{id}");
                store.queued(
                    repository,
                    &QueuedJob {
                        id,
                        run_id: id,
                        name: (*name).to_owned(),
                        created_at: at,
                        ..QueuedJob::default()
                    },
                    at,
                );
                store.runner_started(RunnerRecord {
                    purpose: None,
                    name: runner.clone(),
                    repository: (*repository).to_owned(),
                    profile: (*profile).to_owned(),
                    slot: 1,
                    container: format!("c-{id}"),
                    started_at: at,
                    memory_cap: Some(if *profile == "heavy" { 4 * G } else { G }),
                    cpu_cap: Some(2000),
                    admission: None,
                });
                store.claimed(id, id, &runner, at + 1000);
                store.samples(vec![SampleRecord {
                    runner: runner.clone(),
                    at: at + 2000,
                    cpu: 0,
                    cpu_usage: 0,
                    memory: *peak,
                    network_received: 0,
                    network_sent: 0,
                    cpu_held: crate::observe::metrics::CpuHeld::default(),
                    events: Events::default(),
                }]);
                let mut ran = details(id, "completed", Some("success"));
                ran.run_id = id;
                ran.name = (*name).to_owned();
                ran.runner_name = Some(runner.clone());
                ran.steps = Vec::new();
                store.details(ran);
                store.runner_removed(&runner, at + 60_000);
            }
        }
        store.flush().await;
        self.engine.forget_predictor();
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

    /// What GitHub knows of the runners whose start failed after Docker made their container: a job one took runs
    /// on in its container if the start left it, and fails once GitHub notices its runner gone otherwise.
    fn failed_late(&mut self) {
        let failed = std::mem::take(&mut self.docker.0.lock().unwrap().late_started);
        for (container, job) in failed {
            let Some(id) = job else { continue };
            let left = self
                .running_containers()
                .into_iter()
                .find(|candidate| candidate.id == container);
            if let Some(left) = left {
                let label = |name: &str| left.labels.get(name).cloned().unwrap_or_default();
                self.took_job.insert(container.clone());
                let job = self.jobs.get_mut(&id).unwrap();
                job.phase = Phase::Running { container };
                job.taken_by = Some((label("runnerd.profile"), label("runnerd.repository")));
            } else {
                self.jobs.get_mut(&id).unwrap().taken_by = None;
                self.end_job(id, &container, "failure");
            }
        }
    }

    async fn tick(&mut self) -> Result<(), Violation> {
        self.unregistered_runners_exit();
        let was_short = self.engine.host_is_short();
        let calls_before = self.docker.0.lock().unwrap().calls.len();
        let seeds_before: BTreeSet<String> = self.engine.seed_memories().into_keys().collect();
        self.capture_faults = self.docker.0.lock().unwrap().capture_faults.clone();
        for (id, job) in &mut self.jobs {
            match job.phase {
                Phase::Queued if self.shape.serves(job.repository, &job.labels).is_some() => {
                    job.listed = true;
                }
                Phase::Running { .. } if job.listed && !self.still_listed.contains(id) => {
                    job.named_by_github = true;
                }
                _ => {}
            }
        }
        self.cancelled_since_tick.clear();
        self.note_busy();
        self.engine.tick(Shutdown::No).await;
        self.removals_finish();
        self.failed_late();
        self.exits_hold_only_what_failed_to_start()?;
        let after = self.status();
        let listed = std::mem::take(&mut self.still_listed);
        self.github
            .0
            .lock()
            .unwrap()
            .queued
            .retain(|job| !listed.contains(&job.id));
        // From Docker's starts, not the runners' names: a slot's next runner may take the name of one that stopped
        // this second.
        let running: BTreeSet<String> = self
            .running_containers()
            .into_iter()
            .map(|container| container.name)
            .collect();
        let starts: Vec<String> = self.docker.0.lock().unwrap().calls[calls_before..]
            .iter()
            .filter_map(|call| call.strip_prefix("start "))
            .filter(|name| running.contains(*name))
            .map(str::to_owned)
            .collect();
        let mut started: BTreeSet<String> = starts.iter().cloned().collect();
        self.no_pool_gets_more_runners_than_its_queued_jobs(&after, &started)?;
        let seeds = self
            .engine
            .reseed_memory
            .keys()
            .filter(|profile| !seeds_before.contains(*profile))
            .map(|profile| format!("{profile}{SEED_START}"))
            .collect::<Vec<_>>();
        let read_during_start = self.starts_after_a_reading(&starts);
        let same_reading = read_during_start.is_none();
        let seeded = !seeds.is_empty();
        if let Some(after) = read_during_start {
            started = after;
        }
        started.extend(seeds);
        self.starts_fit_the_budget(&after, &started)?;
        self.docker_runs_no_runner_the_engine_lost(&after)?;
        self.only_unserved_jobs_wait_for_a_pool(&after)?;
        self.one_barrier_and_only_for_a_queued_job(&after)?;
        self.no_job_waits_for_a_slot_or_the_host_past_its_runners_start(&after)?;
        self.the_count_keeps_its_bounds(&after)?;
        if same_reading {
            self.the_band_follows_the_waiters(was_short, seeded)?;
        }
        self.note_states();
        self.no_removal_of_a_running_job()
    }

    /// Counts the states the checks exist for, as a tick or a pass leaves them.
    fn note_states(&mut self) {
        let counts = self.engine.state_counts();
        let seen = &mut self.seen;
        seen.closing += counts.closing;
        seen.closing_and_queued += counts.shared_with_queued;
        seen.ended_uploads += counts.ended_uploads;
        seen.cancelling += counts.cancelling;
        seen.busy_orphans += counts.busy_orphans;
        seen.oversized += counts.oversized;
        seen.unread += counts.unread;
        seen.split_waits += counts.split_waits;
        seen.captures_with_recaptures += counts.captures_with_recaptures;
        seen.mirrors += usize::from(
            self.docker
                .0
                .lock()
                .unwrap()
                .containers
                .iter()
                .any(|container| {
                    container.running && container.labels.contains_key("runnerd.mirror")
                }),
        );
    }

    /// The starts a reading taken while a runner started binds, if one was: those after it, seeds among them, which go
    /// last. The starts before it went by the reading the plan had.
    fn starts_after_a_reading(&mut self, starts: &[String]) -> Option<BTreeSet<String>> {
        let hooked = self.docker.0.lock().unwrap().hooked_start.take();
        let during = self.host_during_start.lock().unwrap().take();
        let (hooked, reading) = hooked.zip(during)?;
        self.host = Some(reading);
        let after = starts.iter().skip_while(|name| **name != hooked).skip(1);
        Some(after.cloned().collect())
    }

    /// A pass between ticks, after Docker said every runner may have stopped: the checks of a tick that bind a pass,
    /// which plans from the last tick's listings.
    async fn pass(&mut self) -> Result<(), Violation> {
        self.unregistered_runners_exit();
        let calls_before = self.docker.0.lock().unwrap().calls.len();
        self.engine.exits.any();
        self.note_busy();
        self.engine.turnover().await;
        self.removals_finish();
        self.failed_late();
        self.exits_hold_only_what_failed_to_start()?;
        let after = self.status();
        let running: BTreeSet<String> = self
            .running_containers()
            .into_iter()
            .map(|container| container.name)
            .collect();
        let order: Vec<String> = self.docker.0.lock().unwrap().calls[calls_before..]
            .iter()
            .filter_map(|call| call.strip_prefix("start "))
            .filter(|name| running.contains(*name))
            .map(str::to_owned)
            .collect();
        let starts: BTreeSet<String> = order.iter().cloned().collect();
        self.no_pass_starts_a_second_runner_for_a_job(&after, &starts)?;
        let bound = self.starts_after_a_reading(&order);
        self.starts_fit_the_budget(&after, bound.as_ref().unwrap_or(&starts))?;
        self.docker_runs_no_runner_the_engine_lost(&after)?;
        self.one_barrier_and_only_for_a_queued_job(&after)?;
        self.no_job_waits_for_a_slot_or_the_host_past_its_runners_start(&after)?;
        self.the_count_keeps_its_bounds(&after)?;
        self.note_states();
        self.no_removal_of_a_running_job()
    }

    /// ADR 0110: a job an idle runner will take stopped waiting for a slot and for the host's memory when that runner
    /// started, whether a tick or a pass started it, so neither wait sizing reads goes on to the next commit.
    fn no_job_waits_for_a_slot_or_the_host_past_its_runners_start(
        &mut self,
        status: &api::Status,
    ) -> Result<(), Violation> {
        let measured = &self.engine.measured;
        let still: Vec<u64> = status
            .queued
            .iter()
            .filter(|job| job.waiting == Some(api::Waiting::RunnerStarting))
            .map(|job| job.id)
            .inspect(|_| self.seen.runners_starting += 1)
            .filter(|job| {
                measured.slot_waiting.0.contains(job) || measured.host_waiting.0.contains(job)
            })
            .collect();
        if still.is_empty() {
            return Ok(());
        }
        violation(
            self,
            "no_job_waits_for_a_slot_or_the_host_past_its_runners_start",
            format!("jobs {still:?} have a runner starting and still wait for a slot or the host"),
        )
    }

    /// Invariant 1 for a pass: a pass that starts runners in a pool leaves it no more runners waiting for a job than
    /// the jobs queued now and those the listing it planned from showed queued that were cancelled since. A job that
    /// left the queue for one of this machine's runners is never counted twice: no job gets a second runner from
    /// a listing older than its first.
    fn no_pass_starts_a_second_runner_for_a_job(
        &self,
        status: &api::Status,
        started: &BTreeSet<String>,
    ) -> Result<(), Violation> {
        let idle: BTreeSet<String> = self.idle_containers().into_iter().map(|c| c.id).collect();
        let pools: BTreeSet<(String, String)> = runners_of(status)
            .filter(|(_, runner)| started.contains(&runner.name))
            .map(|(profile, runner)| (runner.repository.clone(), profile.to_owned()))
            .collect();
        for (repository, profile) in pools {
            let waiting = runners_of(status)
                .filter(|(in_profile, runner)| {
                    *in_profile == profile
                        && runner.repository == repository
                        && idle.contains(&runner.container)
                })
                .count();
            let wanted = self
                .queued()
                .into_iter()
                .chain(self.cancelled_since_tick.iter().copied())
                .chain(self.listed_after_runner.iter().copied())
                .filter(|id| {
                    let job = &self.jobs[id];
                    job.repository == repository
                        && self.shape.serves(job.repository, &job.labels) == Some(profile.as_str())
                })
                .count();
            if waiting > wanted {
                return violation(
                    self,
                    "no_pass_starts_a_second_runner_for_a_job",
                    format!(
                        "{repository} in {profile}: {waiting} runners wait for {wanted} jobs queued or cancelled"
                    ),
                );
            }
        }
        Ok(())
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
                .chain(self.listed_after_runner.iter().copied())
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

    /// Invariant 2: a tick that starts a runner or a seed container leaves what runs within the slice's
    /// `MemoryMax`, the CPU budget under `cpu_overcommit` and what the host leaves the slice, and starts nothing
    /// while the host stalls short of memory (ADR 0055). What runs may be past it otherwise: nothing is stopped for
    /// it.
    fn starts_fit_the_budget(
        &self,
        status: &api::Status,
        started: &BTreeSet<String>,
    ) -> Result<(), Violation> {
        if started.is_empty() {
            return Ok(());
        }
        let budget = &status.budget;
        // A seed container reserves no CPU, so it starts past a CPU budget a reload lowered under the runners in.
        let runner_started = started.iter().any(|name| !name.ends_with(SEED_START));
        // Under caps, what Docker runs them with, whatever the engine counts.
        if self.shape.admission == Admission::Caps {
            let running = self.running_containers();
            let docker = self.docker.0.lock().unwrap();
            let caps: u64 = running
                .iter()
                .map(|container| docker.memory.get(&container.id).copied().unwrap_or(0))
                .sum();
            let cpus: u64 = running
                .iter()
                .map(|container| docker.cpus.get(&container.id).copied().unwrap_or(0))
                .sum();
            drop(docker);
            // The CPU budget is the slice's CPUs times 1.0; seeds and the mirror reserve none.
            if runner_started && self.shape.cpu_overcommit && cpus > SLICE_CPUS {
                return violation(
                    self,
                    "starts_fit_the_budget",
                    format!("started {started:?} with CPU limits of {cpus} in {SLICE_CPUS}"),
                );
            }
            let seeds: u64 = budget.seed_memory.iter().map(|seed| seed.memory).sum();
            if caps.saturating_add(seeds) > MEMORY_MAX {
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
            && budget.counted() > max
        {
            return violation(
                self,
                "starts_fit_the_budget",
                format!(
                    "started {started:?} with {} of {max} counted, seed containers {:?}",
                    budget.counted(),
                    budget.seed_memory
                ),
            );
        }
        if let Some(cpu) = &budget.cpu
            && runner_started
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
        // From the harness's own reading, whatever the engine reports: the engine's margin is at least this one.
        let now = self.clock.now_ms();
        if let Some(reading) = self.host.filter(|reading| reading.fresh(now)) {
            let limit = reading
                .slice
                .saturating_add(reading.available)
                .saturating_sub(HOST_MARGIN);
            // The seed containers that run, at their caps; not the room a waiting one holds.
            let seeds: u64 = self.engine.seed_memories().values().sum();
            let credit = host_credit(budget);
            let reserved = runners_of(status)
                .map(|(_, runner)| runner.reserved)
                .fold(seeds, u64::saturating_add)
                .saturating_sub(credit);
            if reserved > limit {
                return violation(
                    self,
                    "starts_fit_the_budget",
                    format!("started {started:?} with {reserved} reserved past the host's {limit}"),
                );
            }
            if reading.stalled && reading.available < 2 * HOST_MARGIN {
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

    /// Invariant 11 (ADR 0099, 0108): both credits stay in their bounds through restarts, reloads, stale readings,
    /// stalls and lost runners. What the status shows is what a plan goes by; neither count reads under what eligible
    /// runners use with all the rest in full, and the host's keeps at least the 6G floor of their headroom, or all of it;
    /// the host's credit stands only with `pooled_host`, a fresh reading of the slice and the host, and no stall; and
    /// what a drain leaves reads neither credit.
    fn the_count_keeps_its_bounds(&mut self, status: &api::Status) -> Result<(), Violation> {
        let budget = &status.budget;
        let credit = budget.pooled.map_or(0, |pooled| pooled.credit);
        let host_credit = host_credit(budget);
        let measured = self.shape.admission == Admission::Measured;
        if credit > pooled::MOST_CREDIT || (credit > 0 && !(measured && self.shape.pooled_count)) {
            let detail = format!("{budget:?} under {:?}", self.shape);
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        if host_credit > 0 && !(measured && self.shape.pooled_host) {
            let detail = format!("a host credit of {host_credit} under {:?}", self.shape);
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        self.plans_read_the_count(budget, host_credit)?;
        let committed = self.engine.committed_memory();
        let hosted = budget.memory_in_use - host_credit;
        if credit == 0 && host_credit == 0 {
            return Ok(());
        }
        let now = SystemTime::now();
        let fresh_after = self
            .clock
            .now_ms()
            .saturating_sub(3000 * api::SAMPLE_INTERVAL);
        let mut floor = committed;
        let mut headroom = 0;
        for (_, occupant) in self.engine.occupied() {
            let sample = self.handle.store.latest_sample(&occupant.runner_name);
            let settled = occupant
                .job_found_at()
                .and_then(|started| now.duration_since(started).ok())
                .is_some_and(|ran| ran >= POOL_AFTER);
            let predicted = occupant.admission.is_some_and(|admission| {
                matches!(
                    admission.basis,
                    Basis::Job { .. } | Basis::Profile { .. } | Basis::ProfileMax { .. }
                )
            });
            let known = occupant.has_key() && occupant.was_started_here();
            let takes = self.engine.takes(occupant);
            match sample.filter(|sample| sample.at >= fresh_after) {
                Some(sample) if settled && predicted && known => {
                    floor += sample.memory;
                    headroom += takes.saturating_sub(sample.memory);
                }
                _ => floor += takes,
            }
        }
        let slice = self
            .handle
            .store
            .latest_host_sample()
            .filter(|sample| sample.at * 1000 >= fresh_after)
            .map(|sample| sample.memory);
        if host_credit > 0 {
            self.seen.host_credits += 1;
            let kept = headroom.min(pooled::HOST_RESERVE_FLOOR);
            if hosted < floor + kept {
                let detail =
                    format!("{budget:?} counts {hosted} for the host, under {floor} and {kept}");
                return violation(self, "the_count_keeps_its_bounds", detail);
            }
            let read = self
                .host
                .filter(|reading| reading.fresh(self.clock.now_ms()));
            let stalls = budget.host.as_ref().is_none_or(|host| host.stalled);
            if read.is_none() || slice.is_none() || stalls {
                let detail = format!(
                    "a host credit of {host_credit} beside the reading {read:?}, the slice's {slice:?} and {:?}",
                    budget.host
                );
                return violation(self, "the_count_keeps_its_bounds", detail);
            }
        }
        if credit == 0 {
            return Ok(());
        }
        self.seen.credits += 1;
        if budget.counted() < floor {
            let detail = format!("{budget:?} counts under {floor}");
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        let host = budget.host.as_ref();
        let keeps = HOST_MARGIN + HOST_TOTAL / 4;
        if host.is_none_or(|host| host.stalled || host.memory.available < keeps + credit) {
            let detail = format!("{credit} of credit beside the host's {host:?}");
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        let room = slice.map(|slice| {
            (MEMORY_MAX * 90 / 100)
                .saturating_sub(slice)
                .saturating_sub(pooled::HIGH_WATER_RESERVE)
        });
        if room.is_none_or(|room| credit > room) {
            let detail = format!("{credit} of credit beside the slice's {slice:?}");
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        Ok(())
    }

    /// Part of invariant 11: a plan reads the counts the status shows, the host's limit the plain count less the
    /// host's credit, and a drain neither credit.
    fn plans_read_the_count(
        &mut self,
        budget: &api::Budget,
        host_credit: u64,
    ) -> Result<(), Violation> {
        let admitting = self.engine.admitting(&mut [], None);
        let plan = self.engine.plan_budget(&admitting);
        let left = budget
            .memory_max
            .map(|max| crate::scheduler::left(max, budget.counted()));
        if !admitting.held && plan.memory != left {
            let detail = format!("a plan reads {:?}, the status {left:?}", plan.memory);
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        // The host's limit reads the plain count less the host's credit.
        let hosted = budget.memory_in_use - host_credit;
        let host = admitting
            .host
            .as_ref()
            .map(|host| host::left(host.limit, hosted, host.stalled));
        if !admitting.held && plan.host != host {
            let detail = format!(
                "a plan's host reads {:?}, the host's count {host:?}",
                plan.host
            );
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        let running = self.engine.occupied().next().is_some();
        let committed = self.engine.committed_memory();
        let drained = admitting.host.as_ref().map(|host| {
            let limit = host::limit(&host.memory, running || host.stalled);
            crate::scheduler::left(limit, committed)
        });
        if plan.drained.host != drained {
            let detail = format!("a drain leaves {:?}, not {drained:?}", plan.drained.host);
            return violation(self, "the_count_keeps_its_bounds", detail);
        }
        Ok(())
    }

    /// Invariant 11a (ADR 0108, challenge finding 6): a restarted daemon saw none of the runners it adopted start, so
    /// none counts at its use: the host's credit is gone at its first tick, and nothing starts on it until the plain
    /// count fits, which `starts_fit_the_budget` reads.
    fn a_restart_takes_the_host_credit_back(&mut self) -> Result<(), Violation> {
        let status = self.status();
        if host_credit(&status.budget) > 0 {
            let detail = format!("{:?} after a restart", status.budget.host);
            return violation(self, "a_restart_takes_the_host_credit_back", detail);
        }
        Ok(())
    }

    /// Invariant 12 (ADR 0102), after a tick: a stall keeps the larger margin, and no reading leaves none. A plan by what
    /// the tick remembered shows a job waiting for the host only if the margin stands while it fits the plain one not
    /// at all; and with nothing to start, a host no waiter fits keeps it. The plan may start one and leave others
    /// waiting: the tick after decides for those. A reading taken during the tick's starts binds the next tick, so the
    /// caller skips this then. A seed started after the tick's last plan only raises the count, which can leave a host no
    /// waiter fits at the plain margin for one tick (`seeded`): that plan starts nothing, and the next tick decides.
    fn the_band_follows_the_waiters(
        &mut self,
        was_short: bool,
        seeded: bool,
    ) -> Result<(), Violation> {
        let short = self.engine.host_is_short();
        let (plan, admitting) = self.engine.plan();
        let Some(host) = admitting.host.as_ref() else {
            if short {
                let detail = "the larger margin with no reading".to_owned();
                return violation(self, "the_band_follows_the_waiters", detail);
            }
            return Ok(());
        };
        if host.stalled {
            if !short {
                let detail = format!("a stall without the larger margin: {:?}", host.memory);
                return violation(self, "the_band_follows_the_waiters", detail);
            }
            return Ok(());
        }
        let in_use = self.engine.count(self.engine.host_remembered()).plain;
        let room = host::left(host::limit(&host.memory, false), in_use, false);
        let needs: Vec<i64> = plan
            .waiting
            .values()
            .filter_map(|reason| match reason {
                api::Waiting::HostMemory { needs, .. } => i64::try_from(*needs).ok(),
                _ => None,
            })
            .collect();
        let fits = needs.iter().any(|needs| *needs <= room);
        let starts = plan
            .decisions
            .iter()
            .any(|decision| matches!(decision, Decision::Start { .. }));
        if short && fits {
            let detail = format!("{needs:?} wait, {room} is left by the plain margin");
            return violation(self, "the_band_follows_the_waiters", detail);
        }
        if !short && !fits && !needs.is_empty() && !starts && !seeded {
            let detail = format!("{needs:?} wait, {room} is left, and the margin is plain");
            return violation(self, "the_band_follows_the_waiters", detail);
        }
        self.seen.bands += usize::from(short && !needs.is_empty());
        self.seen.released += usize::from(was_short && !short && starts);
        Ok(())
    }

    /// Invariant 2a (ADR 0096): at most one job holds younger ones back, memory and CPU together. It is queued, older
    /// than each of them and says what it lacks itself; and no wait is kept for a job a listing that succeeded no
    /// longer shows, so no hold outlives its job.
    fn one_barrier_and_only_for_a_queued_job(
        &mut self,
        status: &api::Status,
    ) -> Result<(), Violation> {
        let behind = |reason: &Option<api::Waiting>| match reason {
            Some(api::Waiting::BehindMemory { job } | api::Waiting::BehindCpu { job }) => {
                Some(*job)
            }
            _ => None,
        };
        let owners: BTreeSet<u64> = status
            .queued
            .iter()
            .filter_map(|job| behind(&job.waiting))
            .collect();
        self.seen.barriers += owners.len();
        if owners.len() > 1 {
            return violation(
                self,
                "one_barrier_and_only_for_a_queued_job",
                format!("jobs wait behind each of {owners:?}"),
            );
        }
        for owner in &owners {
            let Some(queued) = status.queued.iter().find(|job| job.id == *owner) else {
                return violation(
                    self,
                    "one_barrier_and_only_for_a_queued_job",
                    format!("jobs wait behind {owner}, which no longer waits"),
                );
            };
            if behind(&queued.waiting).is_some() {
                return violation(
                    self,
                    "one_barrier_and_only_for_a_queued_job",
                    format!("{owner} holds others back and says {:?}", queued.waiting),
                );
            }
            if let Some(younger) = status
                .queued
                .iter()
                .find(|job| behind(&job.waiting).is_some() && job.id <= *owner)
            {
                return violation(
                    self,
                    "one_barrier_and_only_for_a_queued_job",
                    format!(
                        "{} is older than {owner}, which it waits behind",
                        younger.id
                    ),
                );
            }
        }
        for listed in self.engine.listed_repositories() {
            let (name, queued) = (listed.name, listed.queued);
            let waits = listed.waits.iter();
            let kept = waits.flat_map(|waits| waits.memory.iter().chain(&waits.cpu));
            if let Some(wait) = kept
                .into_iter()
                .find(|wait| !queued.iter().any(|job| job.id == wait.job))
            {
                return violation(
                    self,
                    "one_barrier_and_only_for_a_queued_job",
                    format!("{name} keeps a wait for job {}, listed no more", wait.job),
                );
            }
            if queued.is_empty() && listed.waits.iter().any(|waits| waits.served) {
                return violation(
                    self,
                    "one_barrier_and_only_for_a_queued_job",
                    format!("{name} keeps a served mark with nothing queued"),
                );
            }
        }
        Ok(())
    }

    /// Invariant 3: runnerd never removes a container whose job still runs, whatever restarts, reloads and failed
    /// Docker calls came before.
    fn no_removal_of_a_running_job(&mut self) -> Result<(), Violation> {
        self.forced_jobs_end();
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
        while !self.open().is_empty() {
            self.open_job_completes(0);
        }
        // What GitHub lists of this machine that no slot holds goes when its job does.
        let orphans = std::mem::take(&mut self.orphans);
        self.github
            .0
            .lock()
            .unwrap()
            .runners
            .retain(|runner| !orphans.contains(&runner.id));
        for _ in 0..6 {
            self.elapse();
            self.tick().await?;
        }
        self.no_runner_is_left()?;
        self.history_names_the_pool_that_ran_each_job().await?;
        self.no_job_stays_running().await
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

    /// Invariant 7, once drained: no job the history holds stays running, as every runner has gone.
    async fn no_job_stays_running(&mut self) -> Result<(), Violation> {
        for id in self.jobs.keys().copied().collect::<Vec<_>>() {
            let recorded = self.handle.store.job(id).await.unwrap();
            if let Some(job) = recorded.filter(|job| job.summary.state == JobState::Running) {
                return violation(
                    self,
                    "no_job_stays_running",
                    format!("job {id} is still running on {:?}", job.summary.runner),
                );
            }
        }
        Ok(())
    }
}

/// The sampler reads the slice and the host: the runners that took a job (`took_job`) use `percent` of their caps,
/// the others 100M; the host has `available` gigabytes available, and its tasks stall or not.
fn sample(
    docker: &DockerState,
    store: &Store,
    took_job: &BTreeSet<String>,
    now: u64,
    percent: u64,
    available: u64,
    stalled: bool,
) -> HostReading {
    let mut slice = 0;
    let mut samples = Vec::new();
    for container in docker
        .containers
        .iter()
        .filter(|container| container.running && container.labels.contains_key("runnerd.job"))
    {
        let cap = docker.memory.get(&container.id).copied().unwrap_or(G);
        let memory = if took_job.contains(&container.id) {
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
            cpu_held: crate::observe::metrics::CpuHeld::default(),
            events: Events::default(),
        });
    }
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
        ceiling: None,
        swap: None,
    });
    HostReading {
        at: now / 1000,
        slice,
        available: available * G,
        stalled,
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

/// What the host's limit takes off the plain count in `budget` (ADR 0108).
fn host_credit(budget: &api::Budget) -> u64 {
    budget
        .host
        .as_ref()
        .and_then(|host| host.credit)
        .map_or(0, |credit| credit.bytes)
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

/// The status as a trace compares it: what it says, without the figures a run's clock decides. A seed's reserve and
/// whether a reseed still runs depend on which background task finished first, which is not what a step did.
fn steady_status(status: &api::Status) -> serde_json::Value {
    fn blank(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Number(_) => *value = 0.into(),
            serde_json::Value::Array(items) => items.iter_mut().for_each(blank),
            serde_json::Value::Object(fields) => {
                fields.retain(|name, _| !UNSTEADY_STATUS.contains(&name.as_str()));
                fields.values_mut().for_each(blank);
            }
            _ => {}
        }
    }
    let mut value = serde_json::to_value(status).unwrap();
    blank(&mut value);
    value
}

/// A runner's name without the second it was started in, which is all of a trace the machine's clock decides.
fn steady(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if bytes[i] == b'-' && run == 10 {
            i += 11;
        } else {
            out.push(char::from(bytes[i]));
            i += 1;
        }
    }
    out
}

/// Runs a sequence from a fresh start, then drains it; the first broken invariant.
async fn run(steps: &[Step]) -> Result<Seen, Violation> {
    run_case(steps, Case::new()).await.map(|case| case.seen)
}

async fn run_case(steps: &[Step], mut case: Case) -> Result<Case, Violation> {
    case.recover().await?;
    case.tick().await?;
    case.trace_step("start").await;
    for (index, step) in steps.iter().enumerate() {
        case.step = index;
        case.apply(*step).await?;
        case.trace_step(&format!("{index} {step:?}")).await;
    }
    case.step = steps.len();
    case.drain().await?;
    case.trace_step("drained").await;
    Ok(case)
}

/// What a generated case runs: its seed's steps, after the scenarios every so many seeds start from.
fn steps_of(seed: u64) -> Vec<Step> {
    let mut steps = generate(&mut Rng(seed));
    if seed % 2 == 1 {
        steps.splice(0..0, TWO_JOBS_SETTLED);
    }
    match seed % 8 {
        0 => drop(steps.splice(0..0, SLOT_FREED_BETWEEN_TICKS)),
        2 => drop(steps.splice(0..0, BAND_RELEASED[..BAND_STANDS].iter().copied())),
        4 => drop(steps.splice(0..0, THREE_JOBS_SETTLED)),
        6 => drop(steps.splice(0..0, BAND_RELEASED)),
        _ => {}
    }
    if seed % 32 == 2 {
        steps.splice(0..0, WAITS_ON_DIFFERENT_JOBS);
    }
    if seed % 2 == 1 {
        let rarely: &[Step] = match seed % 16 {
            1 => &A_JOB_STAYS_OPEN,
            3 => &A_JOB_STAYS_LISTED,
            5 => &THE_MIRROR_RUNS,
            7 => &A_FORCED_PAUSE,
            9 => &A_BUSY_ORPHAN,
            11 => &AN_OVERSIZED_SNAPSHOT,
            13 => &AN_UNREAD_LEFTOVER,
            _ => &UPLOADS_WAIT_FOR_GITHUB,
        };
        // Two ticks, for what the scenario left to end, so the slots are free for what starts next.
        steps.splice(0..0, rarely.iter().copied().chain([Step::Tick, Step::Tick]));
    }
    steps
}

/// `run`, with a panic reported as the broken invariant `no_panic`, so it keeps its seed and shrinks like any other;
/// its step is not known, and says 0.
async fn attempt(steps: &[Step]) -> Result<Seen, Violation> {
    match AssertUnwindSafe(run(steps)).catch_unwind().await {
        Ok(outcome) => outcome,
        Err(panic) => Err(Violation {
            invariant: "no_panic",
            step: 0,
            detail: panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default(),
        }),
    }
}

/// The shortest sequence found that still breaks what `found` broke: drops chunks of steps, then single steps, while
/// it does.
async fn shrink(mut steps: Vec<Step>, found: Violation) -> (Vec<Step>, Violation) {
    let mut last = found;
    let mut chunk = steps.len() / 2;
    while chunk >= 1 {
        let mut start = 0;
        while start < steps.len() {
            let mut fewer = steps.clone();
            fewer.drain(start..(start + chunk).min(steps.len()));
            match attempt(&fewer).await {
                Err(found) if found.invariant == last.invariant => {
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

/// What a case in four starts from: under measured admission a light runner runs and a heavy job waits for a host no
/// margin fits; the host then has room between the margins, so the band lets the job go and the next plan starts it.
const BAND_RELEASED: [Step; 10] = [
    Step::Reload(Change::Policy),
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    Step::Sample {
        percent: 10,
        available: 20,
        stalled: false,
    },
    Step::Tick,
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
    Step::Sample {
        percent: 10,
        available: 9,
        stalled: false,
    },
    Step::Tick,
    Step::Tick,
];

/// How many of `BAND_RELEASED`'s steps leave the larger margin standing, for the steps after them to work on.
const BAND_STANDS: usize = 7;

/// What every eighth case starts from: three light jobs for two light slots, so the third waits for a slot until the
/// first job ends and the pass after it starts the third's runner, between ticks (ADR 0110).
const SLOT_FREED_BETWEEN_TICKS: [Step; 7] = [
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    Step::Tick,
    Step::Assign(0),
    Step::Finish(0),
    Step::Pass,
];

/// What every other case starts from: under measured admission, a job of `o/r` and one of `o/s` past their first minute
/// and using a tenth of what they reserve, so the steps after it find a credit to take back. A tick takes what the
/// captures found, as a job's first minute starts there, before it can go by.
const TWO_JOBS_SETTLED: [Step; 11] = [
    Step::PastRuns,
    Step::Reload(Change::Policy),
    Step::Queue {
        second: false,
        labels: Labels::Heavy,
    },
    Step::Queue {
        second: true,
        labels: Labels::Heavy,
    },
    Step::Tick,
    Step::Assign(0),
    Step::Assign(0),
    Step::Tick,
    Step::Settle,
    Step::Sample {
        percent: 10,
        available: 50,
        stalled: false,
    },
    Step::Tick,
];

/// What a case in eight starts from: under measured admission, `o/r`'s and `o/s`'s `build` and `o/r`'s `lint`,
/// predicted near their caps, past their first minute and using a tenth of their caps: their headroom passes what the
/// two largest keep, so the host's credit stands for the steps after it to work on (ADR 0108).
const THREE_JOBS_SETTLED: [Step; 17] = [
    Step::PastLargeRuns,
    Step::Reload(Change::Policy),
    Step::Queue {
        second: false,
        labels: Labels::Heavy,
    },
    Step::Queue {
        second: true,
        labels: Labels::Heavy,
    },
    Step::Tick,
    Step::Assign(0),
    Step::Assign(0),
    Step::Tick,
    Step::Settle,
    Step::Sample {
        percent: 10,
        available: 50,
        stalled: false,
    },
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::Settle,
    Step::Sample {
        percent: 10,
        available: 50,
        stalled: false,
    },
    Step::Tick,
];

const HEAVY: Step = Step::Queue {
    second: false,
    labels: Labels::Heavy,
};

/// A sample of a host with room, or `available` gigabytes of it.
const fn host_has(available: u64) -> Step {
    Step::Sample {
        percent: 10,
        available,
        stalled: false,
    }
}

/// Each scenario below starts one case in sixteen, ends with nothing running, and reaches a state no chance sequence
/// reliably does; `Seen` proves it was.
const A_JOB_STAYS_OPEN: [Step; 7] = [
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::RunnerGoes(0),
    Step::Tick,
    Step::OpenJobCompletes(0),
];

const A_JOB_STAYS_LISTED: [Step; 8] = [
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::RunnerGoesStillListed(0),
    Step::Tick,
    Step::Tick,
    Step::OpenJobCompletes(0),
];

const THE_MIRROR_RUNS: [Step; 7] = [
    Step::Reload(Change::Mirror),
    HEAVY,
    Step::Tick,
    Step::Tick,
    Step::Assign(0),
    Step::Finish(0),
    Step::Tick,
];

/// One plan leaves a pool's memory and CPU waits on different jobs. With a heavy runner up, the next heavy job has the
/// memory it needs and lacks CPU; a light job behind it fits and starts, which leaves the last heavy job short of memory,
/// and memory is what `why_waits` asks about first.
const WAITS_ON_DIFFERENT_JOBS: [Step; 16] = [
    Step::Reload(Change::CpuOvercommit),
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    HEAVY,
    Step::Queue {
        second: false,
        labels: Labels::Light,
    },
    HEAVY,
    Step::Tick,
    // The light runner takes the light job and the heavy ones are cancelled, so what comes next finds the slots free.
    Step::Assign(0),
    Step::Cancel(0),
    Step::Cancel(0),
    Step::Finish(0),
    Step::Finish(0),
    Step::Reload(Change::CpuOvercommit),
    Step::Tick,
];

const A_FORCED_PAUSE: [Step; 9] = [
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::ForcedPause { second: false },
    Step::Tick,
    Step::Elapse,
    Step::Tick,
    Step::Resume { second: false },
];

const A_BUSY_ORPHAN: [Step; 2] = [Step::BusyOrphan, Step::Tick];

const AN_OVERSIZED_SNAPSHOT: [Step; 5] = [
    Step::OversizedSnapshot,
    Step::Tick,
    host_has(3),
    Step::Tick,
    host_has(50),
];

const AN_UNREAD_LEFTOVER: [Step; 8] = [
    Step::Reload(Change::RecordFailures),
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::Finish(0),
    Step::RestartLogsFail,
    Step::Tick,
];

const UPLOADS_WAIT_FOR_GITHUB: [Step; 11] = [
    Step::UseBuildCache,
    HEAVY,
    Step::Tick,
    Step::Assign(0),
    Step::Tick,
    Step::SaveUnnamed(0),
    Step::RunnerGoes(0),
    Step::Tick,
    Step::Elapse,
    Step::OpenJobCompletes(0),
    Step::Tick,
];

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "names the failing seed before shrinking, which takes a while and may itself fail"
)]
async fn the_scheduler_keeps_its_invariants_over_generated_sequences() {
    let seeds: Vec<u64> = match env_u64("RUNNERD_INVARIANT_SEED") {
        Some(seed) => vec![seed],
        None => (0..env_u64("RUNNERD_INVARIANT_CASES").unwrap_or(CASES)).collect(),
    };
    let mut seen = Seen::default();
    // Cases at once: each waits now and then for its captures, which the others use.
    for batch in seeds.chunks(64) {
        let runs = batch.iter().map(|&seed| async move {
            let steps = steps_of(seed);
            (seed, attempt(&steps).await, steps)
        });
        for (seed, outcome, steps) in futures_util::future::join_all(runs).await {
            if let Ok(case) = &outcome {
                seen.add(*case);
                assert!(
                    seed % 8 != 2 || case.bands > 0,
                    "seed {seed} starts from a job no margin fits, so the larger margin stands"
                );
                assert!(
                    seed % 8 != 6 || case.released > 0,
                    "seed {seed} starts from a job between the margins, so the band lets it go"
                );
                assert!(
                    seed % 2 == 0 || case.credits > 0,
                    "seed {seed} starts from two settled jobs, so it reaches a credit"
                );
                assert!(
                    seed % 8 != 4 || case.host_credits > 0,
                    "seed {seed} starts from three settled jobs, so it reaches a host credit"
                );
            }
            if let Err(found) = outcome {
                // Before shrinking, which may itself fail: the seed alone replays it.
                eprintln!(
                    "seed {seed} breaks {} at step {}: {}; shrinking",
                    found.invariant, found.step, found.detail
                );
                let (invariant, detail) = (found.invariant, found.detail.clone());
                let (shortest, violation) = shrink(steps, found).await;
                panic!(
                    "seed {seed} breaks {invariant}: {detail}\nshrunk to {shortest:#?}\nwhich breaks it at step {}: {}",
                    violation.step, violation.detail
                );
            }
        }
    }
    assert!(
        seen.unreached().is_empty() || seeds.len() == 1,
        "the generated runs never reach {:?}, so nothing checks them",
        seen.unreached()
    );
}

/// Status fields a trace leaves out (`steady_status`).
const UNSTEADY_STATUS: &[&str] = &["seed_memory", "reseeding"];

/// How many rounds without a call to a fake say that a step's background work has ended.
const QUIET_ROUNDS: u32 = 8;

/// Seeds whose trace differed between runs of one binary, over eight runs of each of the first 64 seeds: a comparison
/// leaves them out. Seed 21 differed once, in where a capture's end reads the runner's files.
const TRACE_UNSTABLE: &[u64] = &[21];

/// Writes each seed's trace under `RUNNERD_INVARIANT_TRACE`, for comparing two commits (see the module's documentation).
#[tokio::test]
async fn the_trace_of_each_seed() {
    let Some(directory) = std::env::var_os("RUNNERD_INVARIANT_TRACE") else {
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    for seed in (0..env_u64("RUNNERD_INVARIANT_CASES").unwrap_or(CASES))
        .filter(|seed| !TRACE_UNSTABLE.contains(seed))
    {
        let mut case = Case::new();
        case.trace = Some(Vec::new());
        let case = run_case(&steps_of(seed), case)
            .await
            .unwrap_or_else(|found| {
                panic!("seed {seed} breaks {}: {}", found.invariant, found.detail)
            });
        let trace = case.finished_trace().await;
        std::fs::write(directory.join(format!("{seed}.trace")), trace.join("\n")).unwrap();
    }
}

/// Runs `steps` as the generated test does, and returns what broke.
async fn broken(steps: &[Step]) -> Option<(&'static str, String)> {
    run(steps)
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
async fn a_job_that_ends_before_its_broken_capture_comes_back_keeps_its_runner_in_the_history() {
    // Shrunk from seeds 4523 and 4496. The capture breaks and Docker cannot inspect the runner, so the job ends first.
    // The stopped container's worker log still names it, and nothing else links the two.
    let broke = [
        Step::Queue {
            second: false,
            labels: Labels::Light,
        },
        Step::CaptureFails { mid_stream: true },
        Step::RestartInspectFails(0),
        Step::InspectFails(1),
        Step::InspectFails(6),
    ];
    // A tick finds it stopped.
    let tick = [Step::Tick, Step::Tick, Step::Assign(3)];
    assert_eq!(broken(&[&broke[..], &tick].concat()).await, None);
    // A pass reaps it before a tick looks.
    let pass = [
        Step::StartFails,
        Step::Tick,
        Step::Tick,
        Step::Assign(3),
        Step::Finish(0),
        Step::Pass,
    ];
    assert_eq!(broken(&[&broke[..], &pass].concat()).await, None);
    // A restart finds it stopped, its capture gone with the last daemon.
    let restart = [
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Reload(Change::Policy),
        Step::InspectFails(5),
        Step::CaptureFails { mid_stream: false },
        Step::Reload(Change::SecondRepository),
        Step::AssignListed(4),
        Step::Finish(4),
        Step::RestartInspectFails(5),
    ];
    assert_eq!(broken(&restart).await, None);
}

#[tokio::test]
async fn a_runner_whose_inspect_fails_after_a_reload_lowered_its_memory_counts_at_its_own_cap() {
    // Shrunk from seed 487: two heavy runners started at 4G; a reload lowers the profile to 2G; a restart cannot read
    // them. Counted at 2G each, the next start would fit caps that add past the budget.
    let heavy = |second| Step::Queue {
        second,
        labels: Labels::Heavy,
    };
    let steps = [
        heavy(false),
        heavy(true),
        Step::Tick,
        Step::Assign(0),
        Step::Assign(0),
        Step::Reload(Change::HeavyMemory),
        Step::RestartInspectFails(7),
        heavy(false),
        Step::Reload(Change::HeavySlots(3)),
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
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
async fn a_job_taking_a_waiting_runners_room_still_fits_the_cpu_budget() {
    // Two heavy runners hold 4000 millicores when a reload turns on a CPU budget of 3000. Once a job ends, a queued
    // one may stop the runner left waiting only if its room pays the 1000 past the budget back first.
    let steps = [
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Queue {
            second: true,
            labels: Labels::Heavy,
        },
        Step::Tick,
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Reload(Change::CpuOvercommit),
        Step::AssignListed(1),
        Step::InspectFails(1),
        Step::Finish(2),
        Step::Tick,
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
async fn a_seed_container_starts_past_a_cpu_budget_a_reload_lowered() {
    // Shrunk from seed 1006. The runners in hold 4000 millicores when a reload turns on a CPU budget of 3000, and the
    // tick that follows starts a seed container, which reserves none.
    let steps = [
        Step::Assign(6),
        Step::Queue {
            second: false,
            labels: Labels::Light,
        },
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Reload(Change::HeavyMemory),
        Step::AssignListed(4),
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Finish(5),
        Step::Reload(Change::LightSlots(1)),
        Step::Reload(Change::HeavySlots(3)),
        Step::Reload(Change::CpuOvercommit),
    ];
    assert_eq!(broken(&steps).await, None);
}

#[tokio::test]
async fn a_job_taking_a_waiting_runners_room_still_fits_the_slice_under_measured_admission() {
    let steps = [
        // Three heavy slots, so the third job lacks only memory.
        Step::Reload(Change::Policy),
        Step::Reload(Change::HeavySlots(3)),
        // o/s gets a runner, then its job is cancelled: the runner waits for nothing.
        Step::Queue {
            second: true,
            labels: Labels::Heavy,
        },
        Step::Tick,
        Step::Cancel(0),
        // o/r's runner takes a job and uses past its cap: the two take more than the slice's 8.5G.
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Tick,
        Step::Assign(0),
        Step::Sample {
            percent: 120,
            available: 50,
            stalled: false,
        },
        // Stopping the waiting runner pays that back first, which leaves no room for another 4G.
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Tick,
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

/// A runner whose log capture fails takes a queued job just before a poll that lists it busy while GitHub still
/// lists its job queued: no second runner starts for that job.
#[tokio::test]
async fn a_runner_first_listed_busy_beside_its_job_still_listed_gets_no_twin() {
    let steps = [
        Step::StartedInspectFails,
        Step::CaptureFails { mid_stream: false },
        Step::Queue {
            second: false,
            labels: Labels::SelfHosted,
        },
        Step::Restart,
        Step::AssignListed(2),
        Step::Reload(Change::LightPool),
    ];
    assert_eq!(broken(&steps).await, None);
}

/// A start whose runner took a listed job and then failed, leaving nothing in Docker, names no job; a pass after
/// another runner went must not start a runner for that job from the listing that still shows it.
#[tokio::test]
async fn a_pass_starts_no_runner_for_a_job_a_failed_start_may_have_taken() {
    let steps = [
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        Step::Queue {
            second: false,
            labels: Labels::Heavy,
        },
        // The first start leaves its container in its slot; the next, in the tick after the restart, takes job 1
        // after that tick's listing, then fails and leaves nothing.
        Step::StartFailsLate {
            takes_job: false,
            cleanup_fails: true,
        },
        Step::Sample {
            percent: 120,
            available: 8,
            stalled: false,
        },
        Step::StartFailsLate {
            takes_job: true,
            cleanup_fails: false,
        },
        Step::Tick,
        Step::Restart,
        // Job 2 runs in the container the first start left, which someone removes: a pass frees its slot.
        Step::Assign(7),
        Step::Lose(5),
        Step::Pass,
    ];
    assert_eq!(broken(&steps).await, None);
}
