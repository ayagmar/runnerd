//! What runners wrote without syncing, over generated sequences on the fakes (ADR 0141): runners started with syncs
//! skipped or kept, of images with and without `runnerd-skip-syncs`, jobs ending, clean stops, daemon deaths, orderly
//! reboots with runners busy, host crashes, a crash after a stop that left runners writing, reloads and restarts with
//! `skip_syncs` turned over, and Docker refusing to remove a volume, or holding it in a leftover container. The host's
//! `sync(2)` is a counter and its journal a set of the boots that ended in an orderly shutdown: what runners wrote is
//! on disk once a sync ran, or once the host shut down in order. After every restart:
//!
//! - `a_crash_empties_what_it_may_have_torn`: a crash while what a runner wrote unsynced was not yet on disk empties
//!   every slot's volume Docker has, of a lowered slot count and a removed profile too, and the pnpm and Maven
//!   volumes, before any runner starts; none starts while Docker refuses one. Snapshots and the mirror stay.
//! - `nothing_is_emptied_without_cause`: nothing is emptied unless the host crashed since a runner skipped its syncs,
//!   with no stop between that left no runner running: an orderly reboot keeps the caches, busy runners or not.
//! - `only_a_filtered_image_arms`: a runner skips its syncs only if its image carries the filter, and only once the
//!   mark says so on disk.
//!
//! A failure prints its seed and steps; `RUNNERD_UNSYNCED_SEED` replays one seed. The cases after it each take one
//! edge on its own: what the file holds, a boot ID that cannot be read, and Docker refusing for good.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// What a crash may tear and the guard empties: the config's slots, the shared stores, and the slot volumes Docker
/// still has from before (`LEFT`).
const TORN: [&str; 6] = [
    "runnerd-app-slot-1",
    "runnerd-app-slot-2",
    "runnerd-pnpm",
    "runnerd-maven",
    "runnerd-app-slot-3",
    "runnerd-gone-slot-1",
];

/// The volumes Docker has that the config does not list: a slot past a lowered `slots`, a slot and the snapshot of
/// a removed profile, and the mirror's storage. Runners of before may have written the slots unsynced; the snapshot
/// and the mirror keep their syncs and are never emptied.
const LEFT: [&str; 4] = [
    "runnerd-app-slot-3",
    "runnerd-gone-slot-1",
    "runnerd-gone-base",
    "runnerd-mirror",
];

/// The profile's image, as `image_stage = "app"` names it.
const IMAGE: &str = "runnerd-runner:app";

const CASES: u64 = 64;
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
    /// The daemon stops on its signal, leaving busy runners, and the host shuts down in order, which stops them and
    /// writes everything, then boots.
    Reboot,
    /// The host loses power: what was not written is lost, and it boots.
    Crash,
    /// A signal's stop that leaves busy runners writing, then the host loses power.
    StopCrash,
    /// As `Crash`, while a container that is not runnerd's mounts the pnpm volume at the first tick after.
    CrashRefused,
    /// As `CrashRefused`, and the daemon dies before the tick after it is gone, then starts again.
    CrashRefusedDie,
    /// As `Crash`, with a stopped seed container of runnerd's left mounting slot 1's volume.
    CrashLeftover,
    /// A restart with `skip_syncs` turned over.
    Toggle,
    /// A reload with `skip_syncs` turned over.
    Reload,
    /// The profile's image is replaced by one with the filter if it lacked it, or one without if it had it.
    Image,
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
        match self.below(22) {
            0..=5 => Step::Job,
            6..=8 => Step::EndJobs,
            9 => Step::Stop,
            10 => Step::Die,
            11..=12 => Step::Reboot,
            13..=14 => Step::Crash,
            15 => Step::StopCrash,
            16 => Step::CrashRefused,
            17 => Step::CrashRefusedDie,
            18 => Step::CrashLeftover,
            19 => Step::Toggle,
            20 => Step::Reload,
            _ => Step::Image,
        }
    }
}

/// How the host came to the boot a restart starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boot {
    /// The same boot.
    Same,
    /// A boot after an orderly shutdown.
    Orderly,
    /// A boot after a crash; `lost` is whether it lost what a runner wrote unsynced.
    Crashed { lost: bool },
}

/// What mounts a torn volume when the host is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Nothing,
    /// A container that is not runnerd's mounts the pnpm volume until the first tick is done.
    Foreign,
    /// As `Foreign`, and the daemon dies after that tick.
    ForeignThenDie,
    /// A stopped seed container of runnerd's mounts slot 1's volume.
    Leftover,
}

/// The host: its syncs, by count, and the boots its journal shows ended in an orderly shutdown.
#[derive(Clone, Default)]
struct Host {
    flushes: Arc<AtomicUsize>,
    ended: Arc<Mutex<BTreeSet<String>>>,
}

impl Host {
    fn durability(&self) -> Durability {
        let (flushes, ended) = (self.flushes.clone(), self.ended.clone());
        Durability::new(
            move || {
                flushes.fetch_add(1, Ordering::SeqCst);
            },
            move |boot| ended.lock().unwrap().contains(boot),
        )
    }
}

struct Case {
    github: FakeGithub,
    docker: FakeDocker,
    dir: TestDir,
    host: Host,
    /// The host's `sync(2)` calls the model has seen.
    flushes_seen: usize,
    boots: u32,
    skip: bool,
    engine: Engine<FakeGithub, FakeDocker>,
    handle: Handle,
    next_job: u64,
    /// What a runner wrote unsynced may not be on disk: one skipped its syncs since the last sync, or still runs.
    unwritten: bool,
    /// A runner skipped its syncs since the last stop that left no runner running, orderly shutdown or emptying.
    since_clean: bool,
    steps: Vec<Step>,
}

/// What broke, with the steps that led to it.
type Failure = String;

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

impl Case {
    fn build(&self) -> (Engine<FakeGithub, FakeDocker>, Handle) {
        engine_durable(
            &self.github,
            &self.docker,
            &self.dir,
            &config(self.skip),
            self.host.durability(),
        )
    }

    async fn new(seed: u64) -> Case {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new(&format!("unsynced-{seed}"));
        boot(&dir, "boot-0");
        let host = Host::default();
        docker
            .0
            .lock()
            .unwrap()
            .listed_volumes
            .extend(LEFT.map(str::to_owned));
        let (mut engine, handle) =
            engine_durable(&github, &docker, &dir, &config(true), host.durability());
        engine.recover().await.unwrap();
        Case {
            github,
            docker,
            dir,
            host,
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

    /// Notes the host's syncs since the last look: what runners wrote before is on disk, and what the runners still
    /// running write after is not.
    fn note_flushes(&mut self) -> bool {
        let count = self.host.flushes.load(Ordering::SeqCst);
        let flushed = count > self.flushes_seen;
        self.flushes_seen = count;
        if flushed {
            self.unwritten = self.skipping_runners_run();
        }
        flushed
    }

    fn calls(&self) -> usize {
        self.docker.0.lock().unwrap().calls.len()
    }

    fn skips(state: &DockerState, name: &str) -> bool {
        state
            .envs
            .get(name)
            .is_some_and(|env| env.iter().any(|var| var == docker::SKIP_SYNCS))
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
                Some((from + at, Case::skips(&state, name)))
            })
            .collect()
    }

    fn skipping_runners_run(&self) -> bool {
        let state = self.docker.0.lock().unwrap();
        state.containers.iter().any(|container| {
            container.running
                && container.labels.contains_key("runnerd.job")
                && Case::skips(&state, &container.name)
        })
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

    fn mark(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.0.join("unsynced")).ok()
    }

    /// A tick, noting the runners it started that skip their syncs; each must be of an image with the filter, under
    /// `skip_syncs`, with the mark on disk.
    async fn tick(&mut self) -> Result<(), Failure> {
        let from = self.calls();
        let lacking = self.docker.0.lock().unwrap().lacking_filter.contains(IMAGE);
        self.engine.tick(Shutdown::No).await;
        if self.runners_started(from).iter().any(|(_, skips)| *skips) {
            if lacking || !self.skip {
                return Err(self.fail(&format!(
                    "only_a_filtered_image_arms: a runner skips its syncs with skip_syncs = {} and an image {}",
                    self.skip,
                    if lacking { "without the filter" } else { "with it" }
                )));
            }
            if !self.mark().is_some_and(|mark| mark.starts_with("since ")) {
                return Err(self.fail(
                    "only_a_filtered_image_arms: a runner skips its syncs with no mark on disk",
                ));
            }
            self.unwritten = true;
            self.since_clean = true;
        }
        Ok(())
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
        github.runners.clear();
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
    /// owed. Busy runners it leaves keep writing.
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

    /// The host boots again: no container survives it. An orderly shutdown before it wrote everything, once its
    /// containers stopped, and its journal says so.
    fn boot_again(&mut self, orderly: bool) {
        if orderly {
            self.host
                .ended
                .lock()
                .unwrap()
                .insert(format!("boot-{}", self.boots));
            self.unwritten = false;
            self.since_clean = false;
        }
        self.boots += 1;
        boot(&self.dir, &format!("boot-{}", self.boots));
        self.docker.0.lock().unwrap().containers.clear();
        let mut github = self.github.0.lock().unwrap();
        github.queued.clear();
        github.runners.clear();
    }

    fn hold(&self, held: Held) {
        let holder = |name: &str, label: &str| Container {
            id: name.to_owned(),
            name: name.to_owned(),
            running: label.is_empty(),
            labels: if label.is_empty() {
                HashMap::new()
            } else {
                HashMap::from([(label.to_owned(), String::new())])
            },
        };
        let mut state = self.docker.0.lock().unwrap();
        match held {
            Held::Nothing => {}
            Held::Foreign | Held::ForeignThenDie => {
                state
                    .holders
                    .insert("runnerd-pnpm".to_owned(), vec![holder("someones-own", "")]);
            }
            Held::Leftover => {
                state.holders.insert(
                    "runnerd-app-slot-1".to_owned(),
                    vec![holder("runnerd-app-seed", "runnerd.seed")],
                );
            }
        }
    }

    /// A new daemon in the boot `boot` says: it recovers, and its first tick has a job queued, so a runner would
    /// start in the tick that empties the caches.
    async fn restart(&mut self, boot: Boot, held: Held) -> Result<(), Failure> {
        self.handle.store.flush().await;
        (self.engine, self.handle) = self.build();
        let crashed = matches!(boot, Boot::Crashed { .. });
        let may_empty = crashed && self.since_clean;
        self.hold(held);
        let from = self.calls();
        if self.engine.recover().await.is_err() {
            return Err(self.fail("recovery failed"));
        }
        self.queue();
        self.tick().await?;
        if matches!(held, Held::Foreign | Held::ForeignThenDie) && may_empty {
            if !self.runners_started(from).is_empty() {
                return Err(self.fail(
                    "a_crash_empties_what_it_may_have_torn: a runner started while Docker refused a torn volume",
                ));
            }
            self.docker.0.lock().unwrap().holders.clear();
            if held == Held::ForeignThenDie {
                self.die();
                self.handle.store.flush().await;
                (self.engine, self.handle) = self.build();
                if self.engine.recover().await.is_err() {
                    return Err(self.fail("recovery failed"));
                }
            }
            self.tick().await?;
        }
        self.docker.0.lock().unwrap().holders.clear();
        let emptied = self.emptied(from);
        if emptied.is_empty() {
            if boot == (Boot::Crashed { lost: true }) {
                return Err(self.fail(
                    "a_crash_empties_what_it_may_have_torn: a crash lost what runners wrote, and nothing was emptied",
                ));
            }
            return Ok(());
        }
        if !may_empty {
            return Err(self.fail(&format!(
                "nothing_is_emptied_without_cause: {emptied:?} emptied after a {boot:?} boot"
            )));
        }
        if self.docker.calls()[from..].iter().any(|call| {
            call == "remove volume runnerd-gone-base" || call == "remove volume runnerd-mirror"
        }) {
            return Err(self.fail(
                "a_crash_empties_what_it_may_have_torn: a snapshot or the mirror, which keep their syncs, was emptied",
            ));
        }
        let removed: BTreeSet<&str> = emptied.iter().map(|(_, name)| *name).collect();
        if removed.len() != TORN.len() {
            return Err(self.fail(&format!(
                "a_crash_empties_what_it_may_have_torn: only {removed:?} emptied"
            )));
        }
        let last_removal = emptied.iter().map(|(at, _)| *at).max().unwrap_or(0);
        if self
            .runners_started(from)
            .iter()
            .any(|(at, _)| *at < last_removal)
        {
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

    async fn crash(&mut self, held: Held) -> Result<(), Failure> {
        self.die();
        let lost = self.unwritten;
        self.boot_again(false);
        self.restart(Boot::Crashed { lost }, held).await
    }

    async fn step(&mut self, step: Step) -> Result<(), Failure> {
        self.steps.push(step);
        match step {
            Step::Job => {
                self.queue();
                self.tick().await?;
                // Its runner takes it: a stop leaves it running.
                for runner in &mut self.github.0.lock().unwrap().runners {
                    runner.busy = true;
                }
            }
            Step::EndJobs => {
                self.end_jobs();
                self.tick().await?;
            }
            Step::Stop => {
                self.stop().await?;
                self.restart(Boot::Same, Held::Nothing).await?;
            }
            Step::Die => {
                self.die();
                self.restart(Boot::Same, Held::Nothing).await?;
            }
            Step::Reboot => {
                self.stop().await?;
                self.boot_again(true);
                self.restart(Boot::Orderly, Held::Nothing).await?;
            }
            Step::Crash => self.crash(Held::Nothing).await?,
            Step::StopCrash => {
                self.stop().await?;
                self.crash(Held::Nothing).await?;
            }
            Step::CrashRefused => self.crash(Held::Foreign).await?,
            Step::CrashRefusedDie => self.crash(Held::ForeignThenDie).await?,
            Step::CrashLeftover => self.crash(Held::Leftover).await?,
            Step::Toggle => {
                self.die();
                self.skip = !self.skip;
                self.restart(Boot::Same, Held::Nothing).await?;
            }
            Step::Reload => {
                self.skip = !self.skip;
                self.engine
                    .reload(Config::parse(&config(self.skip)).unwrap(), None, None);
                self.tick().await?;
            }
            Step::Image => {
                let mut state = self.docker.0.lock().unwrap();
                if !state.lacking_filter.remove(IMAGE) {
                    state.lacking_filter.insert(IMAGE.to_owned());
                }
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

/// An engine in a host now in boot `boot-1` whose `unsynced` file holds `mark`, or none, and whose journal shows
/// `ended` ended in an orderly shutdown; recovered, with a job queued and one tick run. Returns the volumes the tick
/// removed and whether a runner started.
async fn after_mark(
    name: &str,
    mark: Option<&[u8]>,
    ended: &[&str],
) -> (TestDir, FakeDocker, Vec<String>, bool) {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker
        .0
        .lock()
        .unwrap()
        .listed_volumes
        .extend(LEFT.map(str::to_owned));
    let dir = TestDir::new(name);
    boot(&dir, "boot-1");
    if let Some(mark) = mark {
        std::fs::write(dir.0.join("unsynced"), mark).unwrap();
    }
    let host = Host::default();
    host.ended
        .lock()
        .unwrap()
        .extend(ended.iter().map(|boot| (*boot).to_owned()));
    let (mut engine, _handle) = engine_durable(&github, &docker, &dir, CONFIG, host.durability());
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    engine.tick(Shutdown::No).await;
    let calls = docker.calls();
    let mut removed: Vec<String> = calls
        .iter()
        .filter_map(|call| call.strip_prefix("remove volume "))
        .map(str::to_owned)
        .collect();
    removed.sort();
    let started = calls
        .iter()
        .any(|call| call.starts_with("start ") && !call.ends_with("-seed"));
    (dir, docker, removed, started)
}

#[tokio::test]
async fn the_mark_a_daemon_left_is_read_whatever_it_holds() {
    let mut all: Vec<String> = TORN.iter().map(|torn| (*torn).to_owned()).collect();
    all.sort();
    for (mark, ended, emptied, why) in [
        (None, &[][..], false, "no file: nothing is owed"),
        (
            Some(&b"since boot-1\n"[..]),
            &[][..],
            false,
            "a mark of this boot: the runners that wrote it may still run",
        ),
        (
            Some(&b"since boot-0\n"[..]),
            &["boot-0"][..],
            false,
            "a mark of a boot that ended in an orderly shutdown, as the daemon before this change wrote it",
        ),
        (
            Some(&b"since boot-0\n"[..]),
            &[][..],
            true,
            "a mark of a boot the journal shows no orderly end of",
        ),
        (
            Some(&b"torn\n"[..]),
            &["boot-0"][..],
            true,
            "torn, as a daemon left it mid-emptying",
        ),
        (
            Some(&b""[..]),
            &[][..],
            true,
            "empty: what it held cannot be told",
        ),
        (
            Some(&b"since \n"[..]),
            &[][..],
            true,
            "a mark that names no boot",
        ),
        (
            Some(&b"\xff\xfe since boot-0"[..]),
            &["boot-0"][..],
            true,
            "not text this module writes",
        ),
    ] {
        let (dir, _docker, removed, started) = after_mark("unsynced-mark", mark, ended).await;
        if emptied {
            assert_eq!(removed, all, "{why}: every torn volume is emptied");
            assert_eq!(
                std::fs::read_to_string(dir.0.join("unsynced")).unwrap(),
                "since boot-1\n",
                "{why}: once emptied, only what the runner started after writes is owed"
            );
        } else {
            assert_eq!(removed, Vec::<String>::new(), "{why}: nothing is emptied");
        }
        assert!(started, "{why}: a runner starts in the same tick");
    }
}

#[tokio::test]
async fn a_mark_that_cannot_be_read_empties_the_caches() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = TestDir::new("unsynced-unreadable-probe");
    let file = dir.0.join("unsynced");
    std::fs::write(&file, "since boot-0\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        // Root reads it all the same.
        return;
    }
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    docker
        .0
        .lock()
        .unwrap()
        .listed_volumes
        .extend(LEFT.map(str::to_owned));
    let dir = TestDir::new("unsynced-unreadable");
    boot(&dir, "boot-1");
    let file = dir.0.join("unsynced");
    std::fs::write(&file, "since boot-1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let removed: Vec<String> = docker
        .calls()
        .iter()
        .filter_map(|call| call.strip_prefix("remove volume "))
        .map(str::to_owned)
        .collect();
    assert_eq!(removed.len(), TORN.len(), "unreadable, it reads as torn");
    assert!(!file.exists(), "once emptied, nothing is owed");
}

#[tokio::test]
async fn without_a_boot_id_no_runner_skips_and_a_mark_needs_the_journal() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-no-boot");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    std::fs::remove_file(dir.0.join("proc/sys/kernel/random/boot_id")).unwrap();
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    engine.tick(Shutdown::No).await;
    {
        let state = docker.0.lock().unwrap();
        assert_eq!(state.envs.len(), 1, "one runner started");
        assert!(
            state
                .envs
                .values()
                .all(|env| !env.iter().any(|var| var == docker::SKIP_SYNCS)),
            "with no boot ID a later crash could not be told, so the runner keeps its syncs"
        );
    }
    assert!(!dir.0.join("unsynced").exists(), "and nothing is marked");

    for (ended, emptied) in [(&["boot-0"][..], false), (&[][..], true)] {
        let (github, docker) = (FakeGithub::default(), FakeDocker::default());
        let dir = TestDir::new("unsynced-no-boot-mark");
        std::fs::write(dir.0.join("unsynced"), "since boot-0\n").unwrap();
        let host = Host::default();
        host.ended
            .lock()
            .unwrap()
            .extend(ended.iter().map(|boot| (*boot).to_owned()));
        let (mut engine, _handle) =
            engine_durable(&github, &docker, &dir, CONFIG, host.durability());
        std::fs::remove_file(dir.0.join("proc/sys/kernel/random/boot_id")).unwrap();
        engine.recover().await.unwrap();
        engine.tick(Shutdown::No).await;
        let removed = docker
            .calls()
            .iter()
            .filter(|call| call.starts_with("remove volume "))
            .count();
        assert_eq!(
            removed > 0,
            emptied,
            "a mark read with no boot ID is kept if the journal shows its boot ended in order ({ended:?}), and \
             emptied if not"
        );
    }
}

#[tokio::test]
async fn an_image_without_the_filter_never_arms_the_mark() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-unfiltered");
    docker
        .0
        .lock()
        .unwrap()
        .lacking_filter
        .insert(IMAGE.to_owned());
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    engine.tick(Shutdown::No).await;
    let state = docker.0.lock().unwrap();
    assert_eq!(state.envs.len(), 1, "one runner started");
    assert!(
        state
            .envs
            .values()
            .all(|env| !env.iter().any(|var| var == docker::SKIP_SYNCS)),
        "skip_syncs is on, and a runner of an image without the filter is never asked"
    );
    assert!(
        !dir.0.join("unsynced").exists(),
        "and nothing is marked, so no boot after empties anything"
    );
}

#[tokio::test]
async fn a_volume_docker_keeps_refusing_stops_runners_for_a_bounded_number_of_ticks() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-refused");
    boot(&dir, "boot-1");
    std::fs::write(dir.0.join("unsynced"), "since boot-0\n").unwrap();
    docker.0.lock().unwrap().holders.insert(
        "runnerd-pnpm".to_owned(),
        vec![Container {
            id: "someones-own".to_owned(),
            name: "someones-own".to_owned(),
            running: true,
            labels: HashMap::new(),
        }],
    );
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let started =
        |docker: &FakeDocker| docker.calls().iter().any(|call| call.starts_with("start "));
    for tick in 1..super::super::unsynced::REFUSALS {
        engine.tick(Shutdown::No).await;
        assert!(!started(&docker), "no runner starts at refused tick {tick}");
    }
    engine.tick(Shutdown::No).await;
    assert!(
        started(&docker),
        "after the last refused try, runners start on what the caches hold"
    );
    let calls = docker.calls();
    assert!(
        !calls.iter().any(|call| call == "remove someones-own"),
        "a container that is not runnerd's is never removed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("unsynced")).unwrap(),
        "since boot-1\n",
        "and only what the runners started after write is owed"
    );
}

#[tokio::test]
async fn an_outage_counts_no_refusal() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-outage");
    boot(&dir, "boot-1");
    std::fs::write(dir.0.join("unsynced"), "torn\n").unwrap();
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    docker.set_outage(Outage::Refused);
    for _ in 0..=super::super::unsynced::REFUSALS {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(
        std::fs::read_to_string(dir.0.join("unsynced")).unwrap(),
        "torn\n",
        "however long Docker is out, the caches stay owed"
    );
    docker.set_outage(Outage::None);
    engine.tick(Shutdown::No).await;
    assert!(
        !dir.0.join("unsynced").exists(),
        "and are emptied once it answers"
    );
}

#[tokio::test]
async fn a_docker_error_on_a_torn_volume_counts_no_refusal() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-500");
    boot(&dir, "boot-1");
    std::fs::write(dir.0.join("unsynced"), "torn\n").unwrap();
    docker
        .0
        .lock()
        .unwrap()
        .volumes_failing
        .insert("runnerd-app-slot-1".to_owned());
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    for _ in 0..=super::super::unsynced::REFUSALS {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(
        std::fs::read_to_string(dir.0.join("unsynced")).unwrap(),
        "torn\n",
        "a 500 is Docker's error, not a container left on the volume: the caches stay owed"
    );
    assert!(
        !docker.calls().iter().any(|call| call.starts_with("start ")),
        "and no runner or seed starts on the torn slot"
    );
}

#[tokio::test]
async fn a_slot_volume_held_past_the_last_try_has_its_layers_started_over_before_any_runner() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("unsynced-held-slot");
    boot(&dir, "boot-1");
    std::fs::write(dir.0.join("unsynced"), "since boot-0\n").unwrap();
    {
        let mut state = docker.0.lock().unwrap();
        state.holders.insert(
            "runnerd-app-slot-1".to_owned(),
            vec![Container {
                id: "someones-own".to_owned(),
                name: "someones-own".to_owned(),
                running: true,
                labels: HashMap::new(),
            }],
        );
        // Starting the layers over fails until the test lets it.
        state.start_over_exit = 1;
    }
    let (mut engine, handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued.push(job(1));
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let runner_started = |docker: &FakeDocker| {
        docker
            .calls()
            .iter()
            .any(|call| call.starts_with("start ") && !call.ends_with("-start-over"))
    };
    let started_over = |docker: &FakeDocker| {
        docker
            .calls()
            .iter()
            .any(|call| call == "start runnerd-start-over")
    };
    for _ in 0..super::super::unsynced::REFUSALS + 2 {
        engine.tick(Shutdown::No).await;
    }
    assert!(
        started_over(&docker) && !runner_started(&docker),
        "past the last try, no runner starts while the held slot's layers cannot be started over"
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("unsynced")).unwrap(),
        "torn\n",
        "and the caches stay torn on disk"
    );

    // A restart keeps them torn: it tries the volume again, then the layers.
    drop(handle);
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    docker.0.lock().unwrap().start_over_exit = 0;
    for tick in 1..super::super::unsynced::REFUSALS {
        engine.tick(Shutdown::No).await;
        assert!(
            !runner_started(&docker),
            "after a restart, no runner starts at refused tick {tick}"
        );
    }
    let from = docker.calls().len();
    engine.tick(Shutdown::No).await;
    let calls = docker.calls()[from..].to_vec();
    let over = calls
        .iter()
        .position(|call| call == "start runnerd-start-over")
        .expect("the held slot's layers are started over at the last try");
    let runner = calls
        .iter()
        .position(|call| call.starts_with("start ") && !call.ends_with("-start-over"))
        .expect("then runners start");
    assert!(over < runner, "before any runner: {calls:?}");
    let mounts = docker.0.lock().unwrap().mounts["runnerd-start-over"].clone();
    assert_eq!(
        mounts
            .iter()
            .map(|mount| (mount.source.clone(), mount.target.clone()))
            .collect::<Vec<_>>(),
        [(
            Some("runnerd-app-slot-1".to_owned()),
            Some("/state".to_owned())
        )],
        "on the held volume"
    );
}
