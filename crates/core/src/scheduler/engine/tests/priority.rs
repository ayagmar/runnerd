//! The barrier for a starved job (ADR 0096) through the engine: which paths commit a wait and which do not, what ends
//! one, what the barrier reads of the budgets, and how long a job really waits behind it. Each test backdates a wait
//! to pass its limit, as the clock does in the field.

use super::*;
use crate::scheduler::{MEMORY_PRIORITY_AFTER, Wait};

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

    /// As if the wait began `MEMORY_PRIORITY_AFTER` earlier.
    fn age(&mut self) {
        self.engine
            .repositories
            .get_mut("o/big")
            .and_then(|state| state.waits.get_mut("big"))
            .and_then(|waits| waits.memory.as_mut())
            .expect("o/big's next job waits")
            .since -= MEMORY_PRIORITY_AFTER;
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
        since: std::time::SystemTime::now() - MEMORY_PRIORITY_AFTER,
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
        since: std::time::SystemTime::now() - MEMORY_PRIORITY_AFTER,
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

/// A restart loses the wait, so a job restarts its clock: nothing stale can hold a queue after one, and its age
/// comes back in `MEMORY_PRIORITY_AFTER`.
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
        wait.since.elapsed().unwrap_or_default() < MEMORY_PRIORITY_AFTER,
        "not carried over"
    );
}
