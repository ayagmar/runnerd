//! The barrier for a starved job (ADR 0096) through the engine: which paths commit a wait and which do not, what ends
//! one, what the barrier reads of the budgets, and how long a job really waits behind it. Each test backdates a wait
//! to pass its limit, as the clock does in the field.

use super::*;
use crate::admission::{Key, Predictor};
use crate::api::HoldCause;
use crate::observe::metrics::{CpuHeld, Events};
use crate::scheduler::{Decision, Hold, PRIORITY_AFTER, SERVED_PRIORITY_AFTER, Wait, Waits};
use crate::sizing::{Measure, Ran};
use crate::store::{RunnerRecord, SampleRecord};

const G: u64 = 1 << 30;

/// Two profiles of one slice, `big` (8G) and `small` (1G), with o/big and o/small on them.
fn big_and_small() -> String {
    r#"
[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.big]
image_stage = "app"
cpus = 2
memory = "8G"
slots = 2

[profiles.small]
image_stage = "app"
cpus = 1
memory = "1G"
slots = 4

[[repositories]]
name = "o/big"
profile = "big"
labels = ["self-hosted", "big"]
warm = 0

[[repositories]]
name = "o/small"
profile = "small"
labels = ["self-hosted", "small"]
warm = 0
"#
    .to_owned()
}

fn big(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: id,
        name: "large".to_owned(),
        labels: vec!["self-hosted".to_owned(), "big".to_owned()],
        ..QueuedJob::default()
    }
}

fn small(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: id,
        name: "small".to_owned(),
        labels: vec!["self-hosted".to_owned(), "small".to_owned()],
        ..QueuedJob::default()
    }
}

/// An engine with a slice of `memory_max`, past recovery, whose jobs are on the repository their labels name.
struct Fixture {
    github: FakeGithub,
    docker: FakeDocker,
    engine: Engine<FakeGithub, FakeDocker>,
    handle: Handle,
    _dir: TestDir,
}

impl Fixture {
    async fn new(memory_max: u64) -> Fixture {
        Fixture::with(&big_and_small(), memory_max).await
    }

    async fn with(config: &str, memory_max: u64) -> Fixture {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("priority"),
        );
        let (mut engine, handle) = engine_with(&github, &docker, &dir, config);
        engine.memory_max = Some(memory_max);
        engine.recover().await.unwrap();
        Fixture {
            github,
            docker,
            engine,
            handle,
            _dir: dir,
        }
    }

    /// GitHub lists `jobs`, each on the repository of its pool.
    fn queue(&self, jobs: Vec<QueuedJob>) {
        let mut state = self.github.0.lock().unwrap();
        for job in &jobs {
            let on = if job.name == "large" {
                "o/big"
            } else {
                "o/small"
            };
            state.queued_on.insert(job.id, on.to_owned());
        }
        state.queued = jobs;
    }

    /// Every runner listed takes a job.
    fn all_busy(&self) {
        for runner in &mut self.github.0.lock().unwrap().runners {
            runner.busy = true;
        }
    }

    fn wait(&self) -> Option<Wait> {
        self.engine.repositories["o/big"]
            .waits
            .get("big")
            .and_then(|waits| waits.memory)
    }

    /// What o/big's pool keeps.
    fn waits(&self) -> Waits {
        let kept = self.engine.repositories["o/big"].waits.get("big");
        kept.copied().unwrap_or_default()
    }

    /// As if the wait began `PRIORITY_AFTER` earlier.
    fn age(&mut self) {
        self.age_by(PRIORITY_AFTER);
    }

    /// As if the wait began `by` earlier.
    fn age_by(&mut self, by: Duration) {
        self.engine
            .repositories
            .get_mut("o/big")
            .and_then(|state| state.waits.get_mut("big"))
            .and_then(|waits| waits.memory.as_mut())
            .expect("o/big's next job waits")
            .since -= by;
    }

    /// The host's memory read now: 32G, whose 2G margin grows to 3G while the host is short, with `room` of it for the
    /// slice and what is available.
    fn host(&self, room: u64) {
        self.handle.store.host_memory(api::HostMemory {
            at: crate::time::now_ms() / 1000,
            total: 32 * G,
            available: room,
            slice: 0,
            some: Some(api::Pressure::default()),
            full: Some(api::Pressure::default()),
            holders: Vec::new(),
            ceiling: None,
            swap: None,
        });
    }

    /// Job 10 of o/big does not fit and waits past its limit; then job 11 of o/small, 1G, comes after it.
    async fn younger_job_after_a_starved_one(&mut self) {
        self.queue(vec![big(10)]);
        self.engine.tick(Shutdown::No).await;
        self.age();
        self.queue(vec![big(10), small(11)]);
        self.engine.tick(Shutdown::No).await;
    }

    /// How many runners of `repository` run.
    fn runners_of(&mut self, repository: &str) -> usize {
        self.engine.publish();
        self.handle
            .status
            .borrow()
            .profiles
            .iter()
            .flat_map(|profile| &profile.slots)
            .filter_map(|slot| slot.runner.as_ref())
            .filter(|runner| runner.repository == repository)
            .count()
    }

    fn reason(&mut self, job: u64) -> Option<api::Waiting> {
        self.engine.publish();
        self.handle
            .status
            .borrow()
            .queued
            .iter()
            .find(|queued| queued.id == job)
            .and_then(|queued| queued.waiting.clone())
    }

    /// Two busy small runners in a 9G slice leave too little for job 10 of o/big, which has waited past its limit.
    async fn starved() -> Fixture {
        let mut fixture = Fixture::new(9 * G).await;
        fixture.queue((1..=2).map(small).collect());
        fixture.engine.tick(Shutdown::No).await;
        assert_eq!(fixture.runners_of("o/small"), 2);
        fixture.queue(vec![big(10)]);
        fixture.all_busy();
        fixture.engine.tick(Shutdown::No).await;
        assert_eq!(fixture.wait().map(|wait| wait.job), Some(10));
        fixture.age();
        fixture
    }

    /// Job 10 owned a barrier and its runner started once the small ones ended, in a slice that leaves job 12 of its
    /// pool waiting. The runner has not taken its job yet.
    async fn served() -> Fixture {
        let mut fixture = Fixture::starved().await;
        fixture.queue(vec![big(10), big(12)]);
        fixture.engine.tick(Shutdown::No).await;
        fixture.docker.0.lock().unwrap().containers.clear();
        fixture.engine.tick(Shutdown::No).await;
        assert_eq!(fixture.runners_of("o/big"), 1);
        assert!(fixture.waits().served, "the barrier served its pool");
        fixture
    }
}

/// Reading the status every second must neither start a wait nor age one: only what can start a runner may.
#[tokio::test]
async fn reading_the_status_commits_no_wait() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue((1..=2).map(small).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.all_busy();
    fixture.queue(vec![big(10)]);
    // Listed by a tick's listing alone, with no plan of a tick after it.
    fixture.engine.repositories.get_mut("o/big").unwrap().queued = vec![big(10)];
    fixture.engine.repositories.get_mut("o/big").unwrap().listed = true;
    fixture.engine.publish();
    assert_eq!(fixture.wait(), None, "a status plan started no wait");
}

/// ADR 0095's passes start runners between ticks: a wait kept only by ticks misses their plans.
#[tokio::test]
async fn a_pass_between_ticks_commits_a_wait_from_the_listing_it_plans_from() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue((1..=3).map(small).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.all_busy();
    fixture.engine.tick(Shutdown::No).await;
    // A job of the tick's listing, with a newer listing yet to answer; one small runner ends.
    fixture.engine.repositories.get_mut("o/big").unwrap().queued = vec![big(10)];
    fixture.engine.repositories.get_mut("o/big").unwrap().listed = true;
    fixture.engine.listing += 1;
    fixture.docker.0.lock().unwrap().containers.remove(0);
    fixture.engine.exits.any();
    fixture.engine.turnover().await;
    assert_eq!(
        fixture.wait().map(|wait| wait.job),
        Some(10),
        "the pass held job 10 for memory"
    );
}

/// The bug this exists for, whole: small runners keep taking what frees up, then the large job's runner starts.
#[tokio::test]
async fn younger_jobs_stop_taking_memory_and_the_starved_pool_starts_once_it_drained() {
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(
        fixture.reason(11),
        Some(api::Waiting::BehindMemory { job: 10 })
    );
    assert_eq!(
        fixture.runners_of("o/small"),
        2,
        "no younger runner started"
    );
    // The running jobs end.
    fixture.docker.0.lock().unwrap().containers.clear();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert_eq!(
        fixture.wait(),
        None,
        "its runner in its slot ended the wait"
    );
}

/// Copying the CPU rule ended the wait before the start: a start that fails would make the starved job young again,
/// and repeatedly so where preparing the caches fails with no backoff.
#[tokio::test]
async fn a_start_that_fails_preparing_its_caches_keeps_the_wait() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue(vec![big(10)]);
    fixture
        .engine
        .repositories
        .get_mut("o/big")
        .unwrap()
        .waits
        .entry("big".to_owned())
        .or_default()
        .memory = Some(Wait {
        job: 10,
        since: std::time::SystemTime::now() - PRIORITY_AFTER,
    });
    fixture.docker.0.lock().unwrap().fail_volumes = 1;
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 0, "no runner went in its slot");
    assert_eq!(fixture.wait().map(|wait| wait.job), Some(10));
}

/// A listing that failed shows no job: reading that as gone would make the job young, and keeping the barrier would
/// let an invisible job stop every other repository for as long as GitHub fails.
#[tokio::test]
async fn a_failed_listing_keeps_the_wait_and_suspends_the_barrier() {
    let mut fixture = Fixture::starved().await;
    let wait = fixture.wait();
    fixture
        .github
        .0
        .lock()
        .unwrap()
        .failed_queued
        .insert("o/big".to_owned());
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait(), wait, "its age is kept");
    assert_eq!(fixture.runners_of("o/small"), 3, "younger jobs went on");
    fixture.github.0.lock().unwrap().failed_queued.clear();
    fixture.all_busy();
    // The runner listed busy for the first time may have taken job 12.
    fixture.queue(vec![big(10), small(12), small(13)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(
        fixture.reason(13),
        Some(api::Waiting::BehindMemory { job: 10 }),
        "the same job holds again on the next listing that succeeds"
    );
}

/// A hold that outlives its job would stop a queue for nobody.
#[tokio::test]
async fn a_job_gone_from_a_listing_that_succeeded_ends_the_wait_and_the_barrier() {
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait(), None);
    assert_eq!(fixture.runners_of("o/small"), 3);
}

/// Pausing is the owner saying this repository does not go now: it must not hold others, nor come back senior.
#[tokio::test]
async fn pausing_the_starved_repository_ends_the_wait() {
    let mut fixture = Fixture::starved().await;
    fixture
        .engine
        .pause(&one("o/big"), RunningJobs::Keep)
        .await
        .unwrap();
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait(), None);
    assert_eq!(fixture.runners_of("o/small"), 3);
}

/// A pool that cannot register runners would hold every younger job for a start that fails.
#[tokio::test]
async fn a_pool_backing_off_after_failed_runners_loses_its_wait() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue(vec![big(10)]);
    fixture
        .engine
        .repositories
        .get_mut("o/big")
        .unwrap()
        .waits
        .entry("big".to_owned())
        .or_default()
        .memory = Some(Wait {
        job: 10,
        since: std::time::SystemTime::now() - PRIORITY_AFTER,
    });
    fixture.github.0.lock().unwrap().register_fails = true;
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait(), None);
    fixture.github.0.lock().unwrap().register_fails = false;
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
}

/// Waits are keyed by profile: without the prune, a pool recreated there later inherits a stranger's age.
#[tokio::test]
async fn a_reload_that_removes_the_pool_drops_its_wait() {
    let mut fixture = Fixture::starved().await;
    let without = big_and_small().replace(
        "[[repositories]]\nname = \"o/big\"\nprofile = \"big\"\nlabels = [\"self-hosted\", \"big\"]\nwarm = 0\n",
        "[[repositories]]\nname = \"o/big\"\nprofile = \"small\"\nlabels = [\"self-hosted\", \"big\"]\nwarm = 0\n",
    );
    fixture
        .engine
        .reload(Config::parse(&without).unwrap(), Some(9 * G), None);
    assert!(
        fixture.engine.repositories["o/big"].waits.is_empty(),
        "the pool in `big` is gone"
    );
}

/// A reload that shrinks a pool's runner re-decides at the next plan: nothing of the old size is kept.
#[tokio::test]
async fn a_reload_that_makes_the_runner_past_the_slice_lifts_the_barrier() {
    let mut fixture = Fixture::starved().await;
    let larger = big_and_small().replace("memory = \"8G\"", "memory = \"10G\"");
    fixture
        .engine
        .reload(Config::parse(&larger).unwrap(), Some(9 * G), None);
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 3, "no drain could serve it");
    assert!(
        matches!(fixture.reason(10), Some(api::Waiting::Memory { .. })),
        "{:?}",
        fixture.reason(10)
    );
}

/// The mirror stays when runners end: a runner under `MemoryMax` that cannot fit beside it must not stop the queue.
#[tokio::test]
async fn a_runner_that_fits_the_slice_but_not_beside_the_mirror_erects_no_barrier() {
    let config = big_and_small().replace(
        "mirror = { enabled = false, memory = \"512M\"",
        "mirror = { enabled = true, memory = \"2G\"",
    );
    let mut fixture = Fixture::with(&config, 9 * G).await;
    fixture.queue(vec![big(10)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait().map(|wait| wait.job), Some(10));
    fixture.age();
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
}

/// Between the plain margin and a short host's the runner fits only an empty slice, which a warm runner keeps from
/// happening: its room cannot pay for the job, so nothing ends, and a barrier there would stop every queue for good.
#[tokio::test]
async fn a_runner_that_fits_only_a_slice_with_no_runner_erects_no_barrier_while_a_warm_one_stays() {
    let config = big_and_small()
        + r#"
[[repositories]]
name = "o/warm"
profile = "small"
labels = ["self-hosted", "warmed"]
warm = 1
"#;
    let mut fixture = Fixture::with(&config, 24 * G).await;
    // o/warm ran a job lately, so it is active.
    let lately = QueuedJob {
        id: 1,
        run_id: 1,
        name: "warmed".to_owned(),
        labels: vec!["self-hosted".to_owned(), "warmed".to_owned()],
        created_at: crate::time::now_ms() - 600_000,
        ..QueuedJob::default()
    };
    fixture
        .handle
        .store
        .queued("o/warm", &lately, crate::time::now_ms());
    fixture.handle.store.flush().await;
    // 8.5G by the plain margin and 7.5G while short, with the 8G runner between.
    fixture.host(21 * G / 2);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/warm"), 1, "a 1G warm runner runs");
    fixture.engine.measured.host.short = true;
    fixture.younger_job_after_a_starved_one().await;
    assert!(
        matches!(fixture.reason(10), Some(api::Waiting::HostMemory { .. })),
        "{:?}",
        fixture.reason(10)
    );
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
    assert_eq!(fixture.runners_of("o/warm"), 1, "its room would not do");
    assert!(
        fixture.engine.measured.host.short,
        "beside the warm runner it fits no margin, so the larger one stands"
    );
}

/// Programs outside the slice hold the memory: no runner's end gives it back.
#[tokio::test]
async fn a_host_too_short_for_the_runner_after_a_full_drain_erects_no_barrier() {
    let mut fixture = Fixture::new(24 * G).await;
    fixture.host(7 * G);
    fixture.younger_job_after_a_starved_one().await;
    assert!(
        matches!(fixture.reason(10), Some(api::Waiting::HostMemory { .. })),
        "{:?}",
        fixture.reason(10)
    );
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
}

/// A seed container stays when runners end, as the mirror does.
#[tokio::test]
async fn a_runner_that_fits_the_slice_but_not_beside_a_seed_container_erects_no_barrier() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture
        .engine
        .reseed_memory
        .insert("small".to_owned(), 2 * G);
    fixture.younger_job_after_a_starved_one().await;
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
}

/// A shell stays when runners end, as the mirror does.
#[tokio::test]
async fn a_runner_that_fits_the_slice_but_not_beside_a_shell_erects_no_barrier() {
    let config = big_and_small().replace("slots = 4", "slots = 4\nrecord_failures = true");
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("priority"),
    );
    let mut image = records::record_image(99, 60);
    image
        .labels
        .insert("runnerd.profile".to_owned(), "small".to_owned());
    docker.0.lock().unwrap().images = vec![image];
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.memory_max = Some(17 * G / 2);
    engine.recover().await.unwrap();
    engine
        .open_shell(99, docker::TtySize { cols: 80, rows: 24 })
        .await
        .unwrap();
    let mut fixture = Fixture {
        github,
        docker,
        engine,
        handle,
        _dir: dir,
    };
    fixture.younger_job_after_a_starved_one().await;
    assert_eq!(fixture.runners_of("o/small"), 1, "younger jobs go on");
}

/// A runner no capture named hides the pool's oldest job from the plan, which does not say it took that one: reading
/// that as gone would make a job that may well still wait young again.
#[tokio::test]
async fn a_runner_not_yet_named_leaves_the_oldest_jobs_wait() {
    let mut fixture = Fixture::new(17 * G).await;
    fixture.queue(vec![big(9)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    let wait = Wait {
        job: 10,
        since: std::time::SystemTime::now() - PRIORITY_AFTER,
    };
    fixture
        .engine
        .repositories
        .get_mut("o/big")
        .unwrap()
        .waits
        .entry("big".to_owned())
        .or_default()
        .memory = Some(wait);
    fixture.queue(vec![big(10)]);
    fixture.all_busy();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait(), Some(wait), "its age is kept");
}

/// A restart loses the wait, so a job restarts its clock: nothing stale can hold a queue after one, and its age
/// comes back in `PRIORITY_AFTER`.
#[tokio::test]
async fn a_restart_starts_the_wait_again() {
    let fixture = Fixture::starved().await;
    let (github, docker) = (fixture.github.clone(), fixture.docker.clone());
    let dir = TestDir::new("restarted");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &big_and_small());
    engine.memory_max = Some(9 * G);
    engine.recover().await.unwrap();
    fixture.queue(vec![big(10), small(11)]);
    engine.tick(Shutdown::No).await;
    let wait = engine.repositories["o/big"]
        .waits
        .get("big")
        .and_then(|waits| waits.memory)
        .expect("the first plan held it again");
    assert!(
        wait.since.elapsed().unwrap_or_default() < PRIORITY_AFTER,
        "not carried over"
    );
}

/// Two minutes of waiting is what put the barrier up too often: a first job has the whole first limit to find room.
#[tokio::test]
async fn a_pools_first_job_holds_only_after_the_first_limit() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue((1..=2).map(small).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.all_busy();
    fixture.queue(vec![big(10)]);
    fixture.engine.tick(Shutdown::No).await;
    fixture.age_by(PRIORITY_AFTER.saturating_sub(Duration::from_secs(5)));
    fixture.queue(vec![big(10), small(11)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 3, "the job goes by it");
    fixture.age_by(Duration::from_secs(5));
    fixture.all_busy();
    // The runner listed busy for the first time may have taken job 12.
    fixture.queue(vec![big(10), small(12), small(13)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(
        fixture.reason(13),
        Some(api::Waiting::BehindMemory { job: 10 })
    );
}

/// With the first limit for every job, a pool of many large jobs gets one runner per limit.
#[tokio::test]
async fn once_a_barrier_served_a_pool_its_next_job_holds_after_the_short_limit() {
    let mut fixture = Fixture::served().await;
    assert_eq!(fixture.wait().map(|wait| wait.job), Some(12));
    fixture.age_by(SERVED_PRIORITY_AFTER.saturating_sub(Duration::from_secs(5)));
    fixture.all_busy();
    fixture.queue(vec![big(12), small(13)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 1, "not yet");
    fixture.age_by(Duration::from_secs(5));
    fixture.all_busy();
    // The runner listed busy for the first time may have taken job 14.
    fixture.queue(vec![big(12), small(14), small(15)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(
        fixture.reason(15),
        Some(api::Waiting::BehindMemory { job: 12 })
    );
}

/// Reading a failed listing, which shows no job, as an empty queue would drop the mark whenever GitHub stumbles, and a
/// mark that outlives the queue would make the next burst's first job hold early.
#[tokio::test]
async fn the_mark_stays_through_a_failed_listing_and_goes_when_a_listing_shows_the_queue_empty() {
    let mut fixture = Fixture::served().await;
    fixture.all_busy();
    let failed = |fixture: &Fixture| {
        let mut state = fixture.github.0.lock().unwrap();
        state.failed_queued.insert("o/big".to_owned());
    };
    failed(&fixture);
    fixture.queue(Vec::new());
    fixture.engine.tick(Shutdown::No).await;
    assert!(fixture.waits().served, "a failed listing says nothing");
    fixture.github.0.lock().unwrap().failed_queued.clear();
    fixture.queue(vec![big(13)]);
    fixture.engine.tick(Shutdown::No).await;
    assert!(
        fixture.waits().served,
        "job 12 went elsewhere, 13 is queued"
    );
    fixture.queue(Vec::new());
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.waits(), Waits::default(), "the queue emptied");
}

/// A mark with no wait beside it is still a mark: the next job of the listing may not be short yet.
#[tokio::test]
async fn the_mark_outlives_the_wait_it_came_from() {
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![big(10)]);
    fixture.engine.tick(Shutdown::No).await;
    fixture.docker.0.lock().unwrap().containers.clear();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert_eq!(fixture.wait(), None, "nothing waits");
    assert!(fixture.waits().served, "job 10 is listed still");
    fixture.queue(vec![big(12)]);
    fixture.engine.tick(Shutdown::No).await;
    assert!(fixture.waits().served, "the queue never emptied");
}

/// A barrier that gave its pool no runner proved nothing about the jobs behind its owner.
#[tokio::test]
async fn a_barrier_whose_job_went_elsewhere_served_nobody() {
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![big(10), big(12)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.waits().owner, Some(10));
    fixture.queue(vec![big(12)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.waits().owner, None);
    fixture.docker.0.lock().unwrap().containers.clear();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert!(!fixture.waits().served, "job 12 owned no barrier");

    // The tick that shows the job gone brings the room too, as a cancelled push does, or a large owner leaving a
    // smaller runner: the plan starts job 12 before that tick's waits are committed.
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![big(10), big(12)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.waits().owner, Some(10));
    fixture.queue(vec![big(12)]);
    fixture.docker.0.lock().unwrap().containers.clear();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert!(!fixture.waits().served, "the same tick");
}

/// A job that waited past the limit and then found room had no barrier to thank, as with a queue held by its slots.
#[tokio::test]
async fn a_start_no_barrier_came_before_serves_nobody() {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue(vec![big(10)]);
    let waits = fixture.engine.repositories.get_mut("o/big").unwrap();
    waits.waits.entry("big".to_owned()).or_default().memory = Some(Wait {
        job: 10,
        since: std::time::SystemTime::now() - PRIORITY_AFTER,
    });
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert_eq!(fixture.waits(), Waits::default());
}

/// A start that failed gave nothing, and the plan that retries has no barrier in it to say whose the job was.
#[tokio::test]
async fn only_a_runner_in_its_slot_serves_the_pool() {
    let mut fixture = Fixture::starved().await;
    fixture.queue(vec![big(10), big(12)]);
    fixture.engine.tick(Shutdown::No).await;
    fixture.docker.0.lock().unwrap().containers.clear();
    fixture.docker.0.lock().unwrap().fail_volumes = 1;
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 0, "no runner went in its slot");
    assert!(!fixture.waits().served);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/big"), 1);
    assert!(fixture.waits().served, "the retry was for the same owner");
}

/// Each of these keeps a pool from starting a runner, for longer than a mark should speak for its queue.
#[tokio::test]
async fn pausing_backing_off_or_preparing_takes_the_mark() {
    async fn stopped(stop: impl FnOnce(&mut Fixture)) {
        let mut fixture = Fixture::served().await;
        stop(&mut fixture);
        fixture.engine.tick(Shutdown::No).await;
        assert_eq!(fixture.waits(), Waits::default(), "nothing is left behind");
    }
    let mut paused = Fixture::served().await;
    paused
        .engine
        .pause(&one("o/big"), RunningJobs::Keep)
        .await
        .unwrap();
    paused.engine.tick(Shutdown::No).await;
    assert_eq!(paused.waits(), Waits::default(), "pausing");
    stopped(|fixture| {
        let state = fixture.engine.repositories.get_mut("o/big").unwrap();
        state.holds.entry("big".to_owned()).or_default().held = Some(Hold {
            until: std::time::SystemTime::now() + Duration::from_secs(60),
            cause: HoldCause::NoJob,
        });
    })
    .await;
    stopped(|fixture| {
        fixture.engine.reseed_requested.insert("big".to_owned());
    })
    .await;
}

/// A pool recreated in the profile must not inherit a stranger's mark.
#[tokio::test]
async fn a_reload_that_removes_the_pool_takes_the_mark() {
    let mut fixture = Fixture::served().await;
    let moved = big_and_small().replace(
        "[[repositories]]\nname = \"o/big\"\nprofile = \"big\"\nlabels = [\"self-hosted\", \"big\"]\nwarm = 0\n",
        "[[repositories]]\nname = \"o/big\"\nprofile = \"small\"\nlabels = [\"self-hosted\", \"big\"]\nwarm = 0\n",
    );
    fixture
        .engine
        .reload(Config::parse(&moved).unwrap(), Some(9 * G), None);
    assert_eq!(fixture.waits(), Waits::default());
}

/// The mark lives as the waits do: a restart leaves nothing to read back, and the next job waits the first limit.
#[tokio::test]
async fn a_restart_loses_the_mark() {
    let fixture = Fixture::served().await;
    let (github, docker) = (fixture.github.clone(), fixture.docker.clone());
    let dir = TestDir::new("restarted");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &big_and_small());
    engine.memory_max = Some(9 * G);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let waits = engine.repositories["o/big"].waits.get("big");
    assert!(!waits.is_some_and(|waits| waits.served));
}

/// A 1G job of o/small named `name`.
fn named(id: u64, name: &str) -> QueuedJob {
    QueuedJob {
        name: name.to_owned(),
        ..small(id)
    }
}

/// `ms` since the epoch as GitHub writes a time.
fn rfc3339(ms: u64) -> String {
    let (days, secs) = (ms / 86_400_000, ms / 1000 % 86_400);
    // Days to a civil date, from Howard Hinnant's `civil_from_days`.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let of_era = shifted % 146_097;
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * of_year + 2) / 153;
    let day = of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (secs / 3600, secs / 60 % 60, secs % 60);
    format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

impl Fixture {
    /// `count` successful runs of o/small's `name` in the history, from ID `first`, each keeping its runner `minutes`.
    async fn ran(&self, name: &str, first: u64, count: u64, minutes: u64) {
        let store = &self.handle.store;
        let at = crate::time::now_ms() - 24 * 3600 * 1000;
        for id in first..first + count {
            let runner = format!("earlier-{id}");
            let job = QueuedJob {
                created_at: at,
                ..named(id, name)
            };
            store.queued("o/small", &job, at);
            store.runner_started(RunnerRecord {
                purpose: None,
                name: runner.clone(),
                repository: "o/small".to_owned(),
                profile: "small".to_owned(),
                slot: 1,
                container: format!("c-{id}"),
                started_at: at,
                memory_cap: Some(G),
                cpu_cap: Some(1000),
                admission: None,
            });
            store.claimed(id, id, &runner, at + 1000);
            store.samples(vec![SampleRecord {
                runner: runner.clone(),
                at: at + 2000,
                cpu: 0,
                cpu_usage: 0,
                memory: G / 2,
                network_received: 0,
                network_sent: 0,
                cpu_held: CpuHeld::default(),
                events: Events {
                    oom: Some(0),
                    oom_kill: Some(0),
                    sock_throttled: None,
                    local_oom: None,
                },
            }]);
            store.details(JobDetails {
                id,
                run_id: id,
                run_attempt: 1,
                name: name.to_owned(),
                workflow_name: None,
                head_branch: None,
                html_url: None,
                labels: Vec::new(),
                created_at: None,
                status: "completed".to_owned(),
                conclusion: Some("success".to_owned()),
                started_at: None,
                completed_at: Some(rfc3339(at + minutes * 60_000)),
                runner_name: Some(runner.clone()),
                steps: Vec::new(),
            });
            store.runner_removed(&runner, at + minutes * 60_000);
        }
        store.flush().await;
    }

    /// The running runners run `name` of o/small, and the engine reads the history again, as it does each minute.
    fn running(&mut self, name: &str) {
        for occupant in self.engine.slots.values_mut().flatten().flatten() {
            occupant.key = Some(Key {
                repository: "o/small".to_owned(),
                workflow: None,
                name: name.to_owned(),
            });
        }
        self.engine.measured.predictor = None;
    }

    /// Two 1G runners that usually run 30 minutes hold back job 10's 8G; job 11 of o/small, `name`, comes after it.
    async fn behind_the_barrier(name: &str) -> Fixture {
        let mut fixture = Fixture::starved().await;
        fixture.ran("small", 100, 3, 30).await;
        fixture.ran("quick", 200, 3, 2).await;
        fixture.ran("rare", 300, 2, 2).await;
        fixture.ran("slow", 400, 3, 45).await;
        fixture.running("small");
        fixture.queue(vec![big(10), named(11, name)]);
        fixture
    }
}

/// Whole, under the default admission: the history's run times reach the plan, and the job needs no reason to start.
#[tokio::test]
async fn a_job_that_usually_ends_before_the_room_gathers_starts_behind_the_barrier() {
    let mut fixture = Fixture::behind_the_barrier("quick").await;
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 3, "job 11's runner started");
    assert_eq!(fixture.runners_of("o/big"), 0);
    assert!(
        matches!(fixture.reason(10), Some(api::Waiting::Memory { .. })),
        "the owner keeps its reason"
    );
    assert_eq!(
        fixture.wait().map(|wait| wait.job),
        Some(10),
        "a start behind the barrier ends its own pool's wait, not the owner's"
    );
}

#[tokio::test]
async fn a_job_that_would_outlast_the_room_or_has_too_few_runs_stays_behind_the_barrier() {
    for name in ["slow", "rare"] {
        let mut fixture = Fixture::behind_the_barrier(name).await;
        fixture.engine.tick(Shutdown::No).await;
        assert_eq!(fixture.runners_of("o/small"), 2, "{name} stays queued");
        assert_eq!(
            fixture.reason(11),
            Some(api::Waiting::BehindMemory { job: 10 }),
            "{name} says what it says behind a strict barrier"
        );
    }
}

/// GitHub may give the runner either job: the long one speaks for the pool, though the short one is older.
#[tokio::test]
async fn a_pool_with_short_and_long_jobs_queued_is_judged_by_the_long_one() {
    let mut fixture = Fixture::behind_the_barrier("quick").await;
    fixture.queue(vec![big(10), named(11, "quick"), named(12, "slow")]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.runners_of("o/small"), 2);
    assert_eq!(
        fixture.reason(11),
        Some(api::Waiting::BehindMemory { job: 10 })
    );
}

/// A runner no capture named hides the pool's oldest job from the plan, but it may have taken another: the new runner
/// may still get the hidden job, which goes by its run time all the same. Three 1G runners take 3G of 9G, so the 8G owner
/// needs two ends: the unnamed one's, now, and a named one's in 30 minutes.
async fn a_runner_not_named_hides(name: &str) -> Fixture {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue((1..=3).map(small).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.queue(vec![big(10)]);
    fixture.all_busy();
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait().map(|wait| wait.job), Some(10));
    fixture.age();
    fixture.ran("small", 100, 3, 30).await;
    fixture.ran("quick", 200, 3, 2).await;
    fixture.ran("slow", 400, 3, 45).await;
    fixture.running("small");
    fixture.queue(vec![big(10), named(11, name), named(12, "quick")]);
    let unnamed = fixture
        .engine
        .slots
        .values_mut()
        .flatten()
        .flatten()
        .find(|occupant| occupant.repository == "o/small")
        .unwrap();
    unnamed.job = None;
    unnamed.key = None;
    unnamed.busy = false;
    fixture.engine.tick(Shutdown::No).await;
    fixture
}

#[tokio::test]
async fn a_job_hidden_for_a_runner_not_yet_named_counts_towards_its_pools_longest() {
    let mut short = a_runner_not_named_hides("quick").await;
    assert_eq!(short.runners_of("o/small"), 4, "a short job passes");
    let mut long = a_runner_not_named_hides("slow").await;
    assert_eq!(long.runners_of("o/small"), 3, "job 12 stays behind");
}

/// Three busy runners, and one ends: the pass plans with the listing it has, by the rule a tick goes by.
async fn a_pass_behind_the_barrier(name: &str) -> Fixture {
    let mut fixture = Fixture::new(9 * G).await;
    fixture.queue((1..=3).map(small).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.all_busy();
    fixture.queue((1..=3).map(small).chain([big(10)]).collect());
    fixture.engine.tick(Shutdown::No).await;
    fixture.queue(vec![big(10)]);
    fixture.engine.tick(Shutdown::No).await;
    assert_eq!(fixture.wait().map(|wait| wait.job), Some(10));
    fixture.age();
    fixture.ran("small", 100, 3, 30).await;
    fixture.ran("quick", 200, 3, 2).await;
    fixture.ran("slow", 400, 3, 45).await;
    fixture.running("small");
    fixture.engine.refresh_predictor().await;
    let listed = fixture.engine.repositories.get_mut("o/small").unwrap();
    // The runner that ended may have taken the older one, which the plan then leaves out.
    listed.queued = vec![named(11, name), named(12, name)];
    listed.listed = true;
    fixture.engine.listing += 1;
    fixture.docker.0.lock().unwrap().containers.remove(0);
    fixture.engine.exits.any();
    fixture.engine.turnover().await;
    fixture
}

/// ADR 0095's passes run between ticks: a backfill that only ticks checked would let them take the owner's room.
#[tokio::test]
async fn a_pass_between_ticks_goes_by_the_same_rule_as_a_tick() {
    let mut passed = a_pass_behind_the_barrier("quick").await;
    assert_eq!(passed.runners_of("o/small"), 3, "two left, and job 12's");
    let mut held = a_pass_behind_the_barrier("slow").await;
    assert_eq!(held.runners_of("o/small"), 2, "two left");
    assert_eq!(
        held.reason(12),
        Some(api::Waiting::BehindMemory { job: 10 })
    );
}

/// Reading run times under caps must not start predicting CPU there: its runners reserve their profile's `cpus`.
#[tokio::test]
async fn under_caps_a_history_changes_no_reservation() {
    let config = big_and_small().replace("[caches]", "cpu_overcommit = 1.0\n\n[caches]");
    let mut fixture = Fixture::with(&config, 9 * G).await;
    fixture.engine.cpus = Some(8000);
    let run = Ran {
        repository: "o/small".to_owned(),
        workflow: None,
        name: "small".to_owned(),
        profile: "small".to_owned(),
        memory_cap: Some(G),
        cpu_cap: Some(1000),
        memory_peak: G / 2,
        cpu_peak: 200,
        cpu_average: Some(100),
        queued_at: 0,
        started_at: Some(0),
        runner_started_at: 0,
        finished_at: Some(60_000),
        oom_killed: false,
        hit_cap: Some(false),
        own_cap: Some(false),
        measure: Measure::WithoutPageCache,
        succeeded: true,
        failed: false,
        run_id: 1,
        run_attempt: 1,
        branch: None,
        reserved: None,
        host_waited: 0,
        unseen: 0,
        cpu_periods: None,
        cpu_throttled: None,
        slot_waited: 0,
    };
    let mut predictor = Predictor::new(&fixture.engine.config.sizing);
    for _ in 0..20 {
        predictor.learn(&run, "small", G, 60_000);
    }
    fixture.engine.measured.predictor = Some((predictor, std::time::SystemTime::now()));
    fixture.queue(vec![small(1)]);
    fixture.engine.tick(Shutdown::No).await;
    let (plan, _) = fixture.engine.plan();
    assert_eq!(plan.decisions, [] as [Decision; 0]);
    assert_eq!(
        fixture.engine.cpu_in_use(),
        1000,
        "the runner's whole 1 CPU"
    );
}
