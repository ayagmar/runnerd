//! What runners wrote without syncing, over generated sequences on the fakes (ADR 0141): runners started with syncs
//! skipped or kept, jobs ending, clean stops, daemon deaths, clean reboots, host crashes, reloads of `skip_syncs`, and
//! Docker refusing to remove a volume. The host's `sync(2)` is a counter: what runners wrote is on disk once it ran,
//! or once the host shut down cleanly. After every restart:
//!
//! - `a_crash_empties_what_it_may_have_torn`: a crash while what a runner wrote unsynced was not yet on disk empties
//!   every slot's volume and the pnpm and Maven volumes, before any runner starts; none starts while Docker refuses
//!   one.
//! - `nothing_is_emptied_without_cause`: nothing is emptied unless the host booted again since a runner skipped its
//!   syncs, with no stop between that left no runner running.
//!
//! A failure prints its seed and steps; `RUNNERD_UNSYNCED_SEED` replays one seed.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// What a crash may tear and the guard empties.
const TORN: [&str; 4] = [
    "runnerd-app-slot-1",
    "runnerd-app-slot-2",
    "runnerd-pnpm",
    "runnerd-maven",
];

const CASES: u64 = 48;
const STEPS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A job is queued and a tick starts its runner if a slot is free.
    Job,
    /// Every runner's job ends and its container goes.
    EndJobs,
    /// A signal's stop, then a start in the same boot.
    Stop,
    /// The daemon is killed and started again in the same boot.
    Die,
    /// The daemon stops on its signal and the host shuts down cleanly, writing everything, then boots.
    Reboot,
    /// The host loses power: what was not written is lost, and it boots.
    Crash,
    /// As `Crash`, while Docker refuses to remove the pnpm volume at the first tick after.
    CrashRefused,
    /// A reload turns `skip_syncs` over, as a restart with the other config does.
    Toggle,
}

/// A xorshift generator: a seed replays its case.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn step(&mut self) -> Step {
        match self.below(16) {
            0..=4 => Step::Job,
            5..=7 => Step::EndJobs,
            8 => Step::Stop,
            9 => Step::Die,
            10..=11 => Step::Reboot,
            12..=13 => Step::Crash,
            14 => Step::CrashRefused,
            _ => Step::Toggle,
        }
    }
}

struct Case {
    github: FakeGithub,
    docker: FakeDocker,
    dir: TestDir,
    flushes: Arc<AtomicUsize>,
    /// The host's `sync(2)` calls the model has seen.
    flushes_seen: usize,
    boots: u32,
    skip: bool,
    engine: Engine<FakeGithub, FakeDocker>,
    handle: Handle,
    next_job: u64,
    /// A runner skipped its syncs since what was written last reached the disk: a crash now loses it.
    unwritten: bool,
    /// A runner skipped its syncs since the last stop that left no runner running, or the last emptying.
    since_clean: bool,
    steps: Vec<Step>,
}

/// What broke, with the steps that led to it.
type Failure = String;

impl Case {
    fn config(skip: bool) -> String {
        if skip {
            CONFIG.to_owned()
        } else {
            CONFIG.replace(
                "admission = \"caps\"",
                "admission = \"caps\"\nskip_syncs = false",
            )
        }
    }

    fn build(
        github: &FakeGithub,
        docker: &FakeDocker,
        dir: &TestDir,
        flushes: &Arc<AtomicUsize>,
        skip: bool,
    ) -> (Engine<FakeGithub, FakeDocker>, Handle) {
        let flushes = flushes.clone();
        engine_flushing(
            github,
            docker,
            dir,
            &Case::config(skip),
            Flush::new(move || {
                flushes.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    async fn new(seed: u64) -> Case {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new(&format!("unsynced-{seed}"));
        boot(&dir, "boot-0");
        let flushes = Arc::new(AtomicUsize::new(0));
        let (mut engine, handle) = Case::build(&github, &docker, &dir, &flushes, true);
        engine.recover().await.unwrap();
        Case {
            github,
            docker,
            dir,
            flushes,
            flushes_seen: 0,
            boots: 0,
            skip: true,
            engine,
            handle,
            next_job: 1,
            unwritten: false,
            since_clean: false,
            steps: Vec::new(),
        }
    }

    fn fail(&self, what: &str) -> Failure {
        format!("{what}\nsteps: {:?}", self.steps)
    }

    /// Notes the host's syncs since the last look: what runners wrote before is on disk.
    fn note_flushes(&mut self) -> bool {
        let count = self.flushes.load(Ordering::SeqCst);
        let flushed = count > self.flushes_seen;
        self.flushes_seen = count;
        if flushed {
            self.unwritten = false;
        }
        flushed
    }

    fn calls(&self) -> usize {
        self.docker.0.lock().unwrap().calls.len()
    }

    /// The runner containers started by calls from `from` on, and whether each skips its syncs.
    fn runners_started(&self, from: usize) -> Vec<(usize, bool)> {
        let state = self.docker.0.lock().unwrap();
        state.calls[from..]
            .iter()
            .enumerate()
            .filter_map(|(at, call)| {
                let name = call.strip_prefix("start ")?;
                if name.ends_with("-seed") || name.ends_with("-measure") {
                    return None;
                }
                let skips = state
                    .envs
                    .get(name)
                    .is_some_and(|env| env.iter().any(|var| var == docker::SKIP_SYNCS));
                Some((from + at, skips))
            })
            .collect()
    }

    /// The torn volumes removed by calls from `from` on, each at its call.
    fn emptied(&self, from: usize) -> Vec<(usize, &'static str)> {
        let state = self.docker.0.lock().unwrap();
        state.calls[from..]
            .iter()
            .enumerate()
            .filter_map(|(at, call)| {
                let name = call.strip_prefix("remove volume ")?;
                TORN.iter()
                    .find(|torn| **torn == name)
                    .map(|torn| (from + at, *torn))
            })
            .collect()
    }

    fn queue(&mut self) {
        let id = self.next_job;
        self.next_job += 1;
        let mut github = self.github.0.lock().unwrap();
        github.queued.push(job(id));
        github.jobs.insert(id, details(id, "in_progress", None));
    }

    /// A tick, noting the runners it started that skip their syncs.
    async fn tick(&mut self) {
        let from = self.calls();
        self.engine.tick(Shutdown::No).await;
        if self.runners_started(from).iter().any(|(_, skips)| *skips) {
            self.unwritten = true;
            self.since_clean = true;
        }
    }

    fn end_jobs(&self) {
        self.docker
            .0
            .lock()
            .unwrap()
            .containers
            .retain(|container| !container.labels.contains_key("runnerd.job"));
        let mut github = self.github.0.lock().unwrap();
        github.queued.clear();
        for details in github.jobs.values_mut() {
            details.status = "completed".to_owned();
            details.conclusion = Some("success".to_owned());
        }
    }

    fn runners_running(&self) -> bool {
        self.docker
            .0
            .lock()
            .unwrap()
            .containers
            .iter()
            .any(|container| container.running && container.labels.contains_key("runnerd.job"))
    }

    /// A signal's stop, to its end: once it leaves no runner running, what runners wrote is on disk and nothing is
    /// owed.
    async fn stop(&mut self) -> Result<(), Failure> {
        self.handle.control.shutdown(Shutdown::Detach);
        if tokio::time::timeout(Duration::from_secs(60), self.engine.schedule())
            .await
            .is_err()
        {
            return Err(self.fail("the stop never ended"));
        }
        let flushed = self.note_flushes();
        if !self.runners_running() {
            if self.since_clean && !flushed {
                return Err(
                    self.fail("a stop that left no runner said nothing is owed without a sync")
                );
            }
            self.since_clean = false;
        }
        Ok(())
    }

    fn die(&mut self) {
        self.engine.abort_captures();
    }

    /// The host boots again: no container survives it.
    fn boot_again(&mut self) {
        self.boots += 1;
        boot(&self.dir, &format!("boot-{}", self.boots));
        self.docker.0.lock().unwrap().containers.clear();
        let mut github = self.github.0.lock().unwrap();
        github.queued.clear();
        github.runners.clear();
    }

    /// A new daemon in the boot there is now: it recovers, and its first tick has a job queued, so a runner would
    /// start in the tick that empties the caches. `lost` says what a crash lost; `marked` that the guard should know.
    async fn restart(&mut self, lost: bool, booted: bool, refused: bool) -> Result<(), Failure> {
        self.handle.store.flush().await;
        let (engine, handle) = Case::build(
            &self.github,
            &self.docker,
            &self.dir,
            &self.flushes,
            self.skip,
        );
        self.engine = engine;
        self.handle = handle;
        let may_empty = booted && self.since_clean;
        if refused {
            self.docker
                .0
                .lock()
                .unwrap()
                .volumes_in_use
                .insert("runnerd-pnpm".to_owned());
        }
        let from = self.calls();
        if self.engine.recover().await.is_err() {
            return Err(self.fail("recovery failed"));
        }
        self.queue();
        self.tick().await;
        if refused && may_empty {
            if !self.runners_started(from).is_empty() {
                return Err(self.fail(
                    "a_crash_empties_what_it_may_have_torn: a runner started while Docker refused a torn volume",
                ));
            }
            self.docker.0.lock().unwrap().volumes_in_use.clear();
            self.tick().await;
        }
        self.docker.0.lock().unwrap().volumes_in_use.clear();
        let emptied = self.emptied(from);
        if emptied.is_empty() {
            if lost {
                return Err(self.fail(
                    "a_crash_empties_what_it_may_have_torn: a crash lost what runners wrote, and nothing was emptied",
                ));
            }
            return Ok(());
        }
        if !may_empty {
            return Err(self.fail(&format!(
                "nothing_is_emptied_without_cause: {emptied:?} emptied with no boot since runners skipped their syncs"
            )));
        }
        let removed: BTreeSet<&str> = emptied.iter().map(|(_, name)| *name).collect();
        if removed.len() != TORN.len() {
            return Err(self.fail(&format!(
                "a_crash_empties_what_it_may_have_torn: only {removed:?} emptied"
            )));
        }
        let last_removal = emptied.iter().map(|(at, _)| *at).max().unwrap_or(0);
        let started_before: Vec<_> = self
            .runners_started(from)
            .into_iter()
            .filter(|(at, _)| *at < last_removal)
            .collect();
        if !started_before.is_empty() {
            return Err(self.fail(
                "a_crash_empties_what_it_may_have_torn: a runner started before the torn volumes were emptied",
            ));
        }
        // Emptied and synced: nothing of before is owed. The tick's own starts are after it.
        self.note_flushes();
        let skipping = self.runners_started(from).iter().any(|(_, skips)| *skips);
        self.unwritten = skipping;
        self.since_clean = skipping;
        Ok(())
    }

    async fn step(&mut self, step: Step) -> Result<(), Failure> {
        self.steps.push(step);
        match step {
            Step::Job => {
                self.queue();
                self.tick().await;
            }
            Step::EndJobs => {
                self.end_jobs();
                self.tick().await;
            }
            Step::Stop => {
                self.stop().await?;
                self.restart(false, false, false).await?;
            }
            Step::Die => {
                self.die();
                self.restart(false, false, false).await?;
            }
            Step::Reboot => {
                self.stop().await?;
                // A clean shutdown writes everything.
                self.unwritten = false;
                self.boot_again();
                self.restart(false, true, false).await?;
            }
            Step::Crash | Step::CrashRefused => {
                self.die();
                let lost = self.unwritten;
                self.boot_again();
                self.restart(lost, true, step == Step::CrashRefused).await?;
            }
            Step::Toggle => {
                self.die();
                self.skip = !self.skip;
                self.restart(false, false, false).await?;
            }
        }
        Ok(())
    }
}

async fn run(seed: u64) -> Result<(), Failure> {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut case = Case::new(seed).await;
    for _ in 0..STEPS {
        let step = rng.step();
        case.step(step).await?;
    }
    Ok(())
}

#[tokio::test]
async fn what_runners_wrote_unsynced_is_emptied_after_a_crash_and_only_then() {
    let seeds: Vec<u64> = match std::env::var("RUNNERD_UNSYNCED_SEED") {
        Ok(seed) => vec![seed.parse().unwrap()],
        Err(_) => (1..=CASES).collect(),
    };
    for seed in seeds {
        if let Err(failure) = run(seed).await {
            panic!("seed {seed}: {failure}");
        }
    }
}
