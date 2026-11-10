//! A batch of a plan's starts, one launcher for a tick's `apply` and a pass's `refill` (ADR 0125, 0126). Each start is
//! launched on the engine, which checks it still fits, admits it, prepares its slot's caches, names it and marks its
//! slot `Starting`; a task of its own then registers its runner with GitHub, one registration of the batch at a time,
//! starts its container and inspects it; and the engine folds the task's result back into the slot and its records. Up
//! to `CONCURRENT_STARTS` tasks run at once. It must not decide which slots start, which `plan` does, nor let any
//! reader but the budgets' and the launcher's see a start in flight: a batch ends only once each of its tasks is
//! folded, and the engine answers no command, plans nothing and publishes nothing until it does.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::Semaphore;
use tokio::task::{JoinError, JoinSet};
use tracing::{info, warn};

use super::admission::Admitting;
use super::control::Control;
use super::runners::THIS_SPEC;
use super::slot::Slot;
use super::{Engagement, Engine, Footprint, Occupant, Purpose, Shutdown, unix_secs};
use crate::api::{self, Basis, HoldCause};
use crate::docker::{DockerApi, RunnerSpec, RunnerState};
use crate::github::{self, GithubApi, Runner};
use crate::isolation::{self, Isolation};
use crate::scheduler::{Decision, Pool, SlotRef, StartFor};

/// How many starts of a batch are in flight at once: their Docker work overlaps (ADR 0126).
pub(super) const CONCURRENT_STARTS: usize = 4;

/// How many of a batch's runner registrations GitHub is asked for at once. GitHub asks for its mutating requests to
/// be made one after another, and a secondary rate limit it answers stops every request until it lifts (ADR 0126).
pub(super) const POST_LANE: usize = 1;

/// What a start came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) enum StartOutcome {
    /// Its runner is in its slot.
    Started,
    /// No runner is: its pool or profile went, a stop was asked for, or GitHub or Docker failed, which holds its pool
    /// back unless only preparing the caches failed. A container Docker kept holds the slot until it is removed.
    Failed,
}

/// What became of each start a batch was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Carried {
    /// It started, or failed: either way it is done with.
    Done(StartOutcome),
    /// It waits for the next plan: it, or a start before it, no longer fits what runs, or a warm runner it replaces
    /// took a job.
    Deferred,
    /// Nothing of it is done with: a stop, GitHub's used-up rate limit or a failure that holds its pool back since the
    /// plan kept it from launching, or GitHub refused its registration for the rate limit. Its job waits on as it did.
    Skipped,
}

/// A start in flight: named and admitted, with its slot's caches prepared, registering its runner or creating its
/// container. It lives in its slot only while its batch runs (`Slot`), so it survives no restart or reload.
#[derive(Debug, Clone)]
pub(super) struct Starting {
    repository: String,
    runner_name: String,
    purpose: Purpose,
    /// What it reserves of memory and CPU.
    admission: api::Admission,
    /// Its container's memory cap.
    memory: u64,
    /// Its container's CPU limit, in millicores.
    cpus: u64,
    /// What `held_by` reads as a fresh runner's start, and its runner's start once placed.
    launched_at: SystemTime,
    /// The labels its runner registers with.
    labels: Vec<String>,
    image: String,
}

impl Starting {
    /// What it takes of the budgets: what the runner it becomes takes when it has just started.
    pub(super) fn footprint(&self) -> Footprint<'_> {
        Footprint {
            runner_name: &self.runner_name,
            memory: self.memory,
            reserved: self.admission.reserved,
            cpus: self.cpus,
            reserved_cpu: self.admission.cpu.map_or(self.cpus, |cpu| cpu.reserved),
            started_at: self.launched_at,
            key: None,
            job_found_here: None,
            basis: Some(self.admission.basis),
        }
    }

    pub(super) fn repository(&self) -> &str {
        &self.repository
    }

    pub(super) fn launched_at(&self) -> SystemTime {
        self.launched_at
    }

    /// The runner it became once Docker made `container` for its registration `runner_id`.
    pub(super) fn into_occupant(self, runner_id: u64, container: String) -> Occupant {
        Occupant {
            repository: self.repository,
            runner_name: self.runner_name,
            runner_id,
            container,
            started_at: self.launched_at,
            memory: self.memory,
            cpus: self.cpus,
            admission: Some(self.admission),
            labels: Some(self.labels),
            image: Some(self.image),
            spec: Some(THIS_SPEC.to_owned()),
            engagement: Engagement::started(self.purpose),
        }
    }
}

#[cfg(test)]
impl Starting {
    /// A start of `repository`'s runner `runner_name` for a job, launched now, admitted with `admission` into a
    /// profile of `memory` and `cpus` millicores.
    pub(super) fn new(
        repository: &str,
        runner_name: &str,
        admission: api::Admission,
        memory: u64,
        cpus: u64,
    ) -> Starting {
        Starting {
            repository: repository.to_owned(),
            runner_name: runner_name.to_owned(),
            purpose: Purpose::Job,
            admission,
            memory,
            cpus,
            launched_at: SystemTime::now(),
            labels: vec!["self-hosted".to_owned(), "app".to_owned()],
            image: "runnerd-runner:app".to_owned(),
        }
    }
}

/// The labels of a runner container: what a restart adopts it by.
fn container_labels(
    name: &str,
    repository: &str,
    slot: usize,
    runner_id: u64,
    labels: &[String],
    image: &str,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("runnerd.job".to_owned(), name.to_owned()),
        ("runnerd.repository".to_owned(), repository.to_owned()),
        ("runnerd.slot".to_owned(), slot.to_string()),
        ("runnerd.runner-id".to_owned(), runner_id.to_string()),
        (super::runners::LABELS.to_owned(), labels.join(",")),
        (super::runners::IMAGE.to_owned(), image.to_owned()),
        (super::runners::SPEC.to_owned(), THIS_SPEC.to_owned()),
    ])
}

/// What a start's task sends GitHub and Docker.
struct Request {
    repository: String,
    /// Its slot's number, from 1.
    number: usize,
    /// The labels its runner registers with.
    labels: Vec<String>,
    /// Its container, without the registration's ID and JIT config, which the task adds.
    spec: RunnerSpec,
}

/// What a start's task came to.
enum StartRun {
    /// GitHub registered no runner.
    Unregistered(github::Error),
    /// A stop was asked for before its registration was sent, or while GitHub registered it: no registration is left,
    /// as the task deleted the one GitHub made.
    Stopped,
    /// Docker failed and kept no container.
    NoContainer {
        runner: Runner,
        error: String,
        may_have_run: bool,
    },
    /// Docker made its container.
    Container {
        runner: Runner,
        container: String,
        made: Made,
    },
}

/// How Docker's start of a container it made ended.
enum Made {
    /// It runs, as Docker's inspect of it says, or not if that failed.
    Running(Result<RunnerState, crate::docker::Error>),
    /// Docker failed after making it, and kept it.
    Failed(String),
}

/// A start's I/O, in a task of its own: registers its runner once `lane` lets it, unless a stop was asked for by then,
/// deletes the registration if one was asked for meanwhile, starts its container and inspects it once it runs. It
/// decides nothing.
async fn run<G: GithubApi, D: DockerApi>(
    github: Arc<G>,
    docker: Arc<D>,
    control: Arc<Control>,
    isolation: &'static dyn Isolation,
    lane: Arc<Semaphore>,
    request: Request,
) -> StartRun {
    let (repository, name) = (request.repository.as_str(), request.spec.name.clone());
    let registered = {
        // Held for the registration alone: Docker's work of the batch's other starts goes on meanwhile.
        let Ok(_turn) = lane.acquire().await else {
            // Never closed: its batch owns it.
            return StartRun::Stopped;
        };
        // A registration sent after a stop was asked for would only be deleted again, holding the stop back.
        if control.mode() != Shutdown::No {
            return StartRun::Stopped;
        }
        github.jit_config(repository, &name, &request.labels).await
    };
    let (runner, jit_config) = match registered {
        Ok(registered) => registered,
        Err(error) => return StartRun::Unregistered(error),
    };
    // A stop asked for while GitHub registered it: its runner would take a job no daemon watches.
    if control.mode() != Shutdown::No {
        info!("{repository}: stopping; deleting {name} before it starts");
        if let Err(error) = github.delete_runner(repository, runner.id).await {
            warn!("{repository}: cannot delete {name}: {error}");
        }
        return StartRun::Stopped;
    }
    let spec = RunnerSpec {
        labels: container_labels(
            &name,
            repository,
            request.number,
            runner.id,
            &request.labels,
            &request.spec.image,
        ),
        jit_config: Some(jit_config),
        ..request.spec
    };
    match docker.start(&spec, isolation).await {
        Ok(container) => {
            let inspected = docker.inspect(&container).await;
            StartRun::Container {
                runner,
                container,
                made: Made::Running(inspected),
            }
        }
        Err(failed) => match failed.container {
            Some(container) => StartRun::Container {
                runner,
                container,
                made: Made::Failed(failed.error.to_string()),
            },
            None => StartRun::NoContainer {
                runner,
                error: failed.error.to_string(),
                may_have_run: failed.may_have_run,
            },
        },
    }
}

/// The runners batches launched, by name, in launch order, for a test to tell the launches a reading taken
/// mid-batch binds from those before it, whose registration or Docker start may come later.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(super) struct Launches(Arc<std::sync::Mutex<Vec<String>>>);

#[cfg(test)]
impl Launches {
    fn note(&self, runner: &str) {
        crate::sync::lock(&self.0).push(runner.to_owned());
    }

    /// How many runners were launched so far.
    pub(super) fn count(&self) -> usize {
        crate::sync::lock(&self.0).len()
    }

    /// The runners launched after the first `count`, in order.
    pub(super) fn after(&self, count: usize) -> Vec<String> {
        crate::sync::lock(&self.0)
            .get(count..)
            .unwrap_or_default()
            .to_vec()
    }
}

/// A start the launcher is done with or let go, for what follows its end.
struct Launched {
    slot: SlotRef,
    repository: String,
    /// The job it was started for; `None` for a warm runner.
    job: Option<u64>,
}

/// What the launcher did with a start.
enum Launch {
    /// It waits for the next plan.
    Deferred,
    /// A stop was asked for while its caches and name were readied: nothing of it runs.
    Stopped,
    /// It failed before its task: its pool or profile went, or its slot's caches are not ready.
    Failed(Launched),
    /// Its slot is `Starting`, and its task is to run.
    Running(Launched, Box<Request>),
}

/// A batch's tasks, each with the index of its start and what the launcher kept of it.
struct InFlight {
    tasks: JoinSet<StartRun>,
    starts: HashMap<tokio::task::Id, (usize, Launched)>,
}

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Carries out `starts`, a plan's start decisions in its order, `CONCURRENT_STARTS` at a time; returns what
    /// became of each. Each launches against what runs and what is in flight by then, once the starts that already
    /// came back are folded. None launches once a stop is asked for, while GitHub's rate limit is used up, or into a
    /// pool a failure holds back since the plan; once one is deferred, the later ones are too, as they counted on its
    /// room. Returns once every start it launched is folded: the tasks are its own, and go with it.
    pub(super) async fn start_batch(
        &mut self,
        starts: Vec<Decision>,
        admitting: &mut Admitting,
    ) -> Vec<Carried> {
        let mut carried = vec![Carried::Skipped; starts.len()];
        let mut in_flight = InFlight {
            tasks: JoinSet::new(),
            starts: HashMap::new(),
        };
        let lane = Arc::new(Semaphore::new(POST_LANE));
        let mut deferred = false;
        let mut starts = starts.into_iter().enumerate();
        loop {
            while in_flight.tasks.len() < CONCURRENT_STARTS
                && let Some((index, start)) = starts.next()
            {
                // What came back already decides this one: a hold, the rate limit, the room a failure gave back.
                while let Some(joined) = in_flight.tasks.try_join_next_with_id() {
                    self.fold_joined(joined, &mut in_flight, &mut carried).await;
                }
                // A runner started after a stop was asked for could take a job a detaching stop leaves running.
                if self.control.mode() != Shutdown::No || self.github_limit().is_some() {
                    continue;
                }
                // Its pool's other starts in flight go on; its repository's other pools, and other repositories, too.
                if self.held_since_the_plan(&start) {
                    continue;
                }
                if deferred {
                    carried[index] = Carried::Deferred;
                    continue;
                }
                match self.launch(start, admitting).await {
                    Launch::Deferred => {
                        deferred = true;
                        carried[index] = Carried::Deferred;
                    }
                    Launch::Stopped => {}
                    Launch::Failed(launched) => {
                        self.started(&launched, StartOutcome::Failed);
                        carried[index] = Carried::Done(StartOutcome::Failed);
                    }
                    Launch::Running(launched, request) => {
                        #[cfg(test)]
                        self.launches.note(&request.spec.name);
                        let task = in_flight.tasks.spawn(run(
                            self.github.clone(),
                            self.docker.clone(),
                            self.control.clone(),
                            self.isolation,
                            lane.clone(),
                            *request,
                        ));
                        in_flight.starts.insert(task.id(), (index, launched));
                    }
                }
            }
            // Awaited here, never while answering commands: only the batch's own launches see a start in flight.
            let Some(joined) = in_flight.tasks.join_next_with_id().await else {
                break;
            };
            self.fold_joined(joined, &mut in_flight, &mut carried).await;
        }
        carried
    }

    /// Folds a task of the batch that ended, and notes what became of its start.
    async fn fold_joined(
        &mut self,
        joined: Result<(tokio::task::Id, StartRun), JoinError>,
        in_flight: &mut InFlight,
        carried: &mut [Carried],
    ) {
        let (id, ran) = match joined {
            Ok(done) => done,
            Err(error) => {
                // As if it had panicked here.
                if let Ok(panic) = error.try_into_panic() {
                    std::panic::resume_unwind(panic);
                }
                // Cancelled, which only happens while the runtime shuts down, which drops this batch too.
                return std::future::pending::<()>().await;
            }
        };
        let Some((index, launched)) = in_flight.starts.remove(&id) else {
            return;
        };
        let became = self.fold(&launched.slot, ran).await;
        if let Carried::Done(outcome) = became {
            self.started(&launched, outcome);
        }
        carried[index] = became;
    }

    /// Whether a failure holds back the pool `start` is of: as the plan starts nothing in a held pool, it came since.
    fn held_since_the_plan(&self, start: &Decision) -> bool {
        match start {
            Decision::Start {
                slot, repository, ..
            } => self.backing_off(repository, &slot.profile, SystemTime::now()),
            Decision::Stop { .. } => false,
        }
    }

    /// Launches a plan's `Decision::Start`, unless a warm runner it replaces is still there: that one took a job before
    /// its stop, and the start waits for the next plan. So does one that no longer fits what runs and is in flight.
    async fn launch(&mut self, start: Decision, admitting: &mut Admitting) -> Launch {
        let Decision::Start {
            slot,
            repository,
            memory,
            cpu,
            for_,
            replacing,
            behind,
        } = start
        else {
            return Launch::Deferred;
        };
        if replacing.iter().any(|slot| self.holds_runner(slot)) {
            info!("{repository}: a warm runner it would replace took a job; planning again");
            return Launch::Deferred;
        }
        // The plan's budgets are those of its start: the starts before this one are counted since, and so are what
        // ran meanwhile.
        if !self.still_fits(admitting, &slot.profile, memory, cpu) {
            info!(
                "{repository}: its runner in {} no longer fits what runs now; planning again",
                slot.profile
            );
            return Launch::Deferred;
        }
        if let Some(owner) = behind {
            info!(
                "{repository}: a runner in {} starts behind the barrier of job {owner}",
                slot.profile
            );
        }
        let pool = Pool {
            repository,
            profile: slot.profile.clone(),
        };
        self.refresh_free(admitting);
        let mut admission = admitting.admit(&pool, memory, cpu);
        let repository = pool.repository;
        let (purpose, job) = if let StartFor::Job(job) = for_ {
            let held_since = self
                .repositories
                .listed(&repository)
                .and_then(|state| state.waits(&slot.profile)?.cpu())
                // The pool's wait for CPU can be another job's, as memory and CPU may each hold a different one.
                .filter(|wait| wait.job == job)
                .map(|wait| wait.since);
            if let Some((cpu, since)) = admission.cpu.as_mut().zip(held_since) {
                cpu.waited = Some(since.elapsed().unwrap_or_default().as_secs());
            }
            (Purpose::Job, Some(job))
        } else {
            // Its profile's caps, whatever its repository's queued jobs predict.
            admission.basis = Basis::Cap;
            if let Some(cpu) = admission.cpu.as_mut() {
                cpu.basis = Basis::Cap;
            }
            let purpose = self.warm_purpose(&Pool {
                repository: repository.clone(),
                profile: slot.profile.clone(),
            });
            (purpose, None)
        };
        let launched = Launched {
            slot,
            repository,
            job,
        };
        self.launch_admitted(launched, admission, purpose).await
    }

    /// Launches an admitted start: prepares its slot's caches, names it, opens its build cache and marks its slot
    /// `Starting`, for its task to run.
    async fn launch_admitted(
        &mut self,
        launched: Launched,
        admission: api::Admission,
        purpose: Purpose,
    ) -> Launch {
        let (slot, repository) = (&launched.slot, launched.repository.as_str());
        // Its pool's labels: those of the repository's entry in the slot's profile.
        let Some(labels) = self
            .config
            .pool_in(repository, &slot.profile)
            .map(|pool| pool.labels.clone())
        else {
            return Launch::Failed(launched);
        };
        let now = SystemTime::now();
        if !self.caches_ready(slot, now).await {
            return Launch::Failed(launched);
        }
        let number = slot.index + 1;
        let secs = unix_secs(now).max(self.named + 1);
        self.named = secs;
        // GitHub and Docker learn the name only once the history keeps its second, which a restart names later than.
        if !self.store.runner_named(secs).await {
            warn!("{repository}: no runner starts until the history records its name");
            return Launch::Failed(launched);
        }
        let name = self.identity.runner_name(&slot.profile, number, secs);
        self.start_mirror().await;
        // The batch checked before this launch's awaits, which a stop may have come during.
        if self.control.mode() != Shutdown::No {
            return Launch::Stopped;
        }
        let Some(profile) = self.config.profiles.get(&slot.profile) else {
            return Launch::Failed(launched);
        };
        let image = profile.image.reference();
        // Its build cache opens here, before GitHub registers its runner: the fold closes it if no container comes.
        let spec = RunnerSpec {
            name: name.clone(),
            image: image.clone(),
            profile: slot.profile.clone(),
            slice: self.config.host.slice.clone(),
            cpus: profile.cpus,
            memory: profile.memory,
            env: self.runner_env(),
            mounts: self.runner_mounts(&slot.profile, number, &name, repository),
            network: Some(self.names.network()),
            labels: std::collections::BTreeMap::new(),
            entrypoint: None,
            jit_config: None,
        };
        let starting = Starting {
            repository: repository.to_owned(),
            runner_name: name,
            purpose,
            admission,
            memory: spec.memory.0,
            cpus: u64::from(spec.cpus) * 1000,
            launched_at: now,
            labels: labels.clone(),
            image,
        };
        let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        else {
            self.close_build_cache(&spec.name);
            return Launch::Failed(launched);
        };
        entry.launch(starting);
        let request = Request {
            repository: repository.to_owned(),
            number,
            labels,
            spec,
        };
        Launch::Running(launched, Box::new(request))
    }

    /// Puts what a start's task came to in its slot and records it: its runner, or the container of a failed start
    /// that Docker kept, holds the slot; otherwise the slot is free, the build cache its start opened is closed, and
    /// GitHub's or Docker's failure holds back its pool. A registration refused for the rate limit is carried as
    /// skipped, as a start the limit kept from launching is.
    async fn fold(&mut self, slot: &SlotRef, ran: StartRun) -> Carried {
        let Some(starting) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
            .and_then(Slot::end_start)
        else {
            return Carried::Done(StartOutcome::Failed);
        };
        let repository = starting.repository.clone();
        let outcome = match ran {
            // Not the pool's fault: no runner starts anywhere until the limit resets.
            StartRun::Unregistered(github::Error::RateLimited { until }) => {
                self.close_build_cache(&starting.runner_name);
                self.github_limited(until);
                return Carried::Skipped;
            }
            StartRun::Unregistered(error) => {
                self.close_build_cache(&starting.runner_name);
                let failure = format!("cannot register a runner: {error}");
                warn!("{repository}: {failure}");
                self.hold(&repository, &slot.profile, HoldCause::Register, failure);
                StartOutcome::Failed
            }
            StartRun::Stopped => {
                self.close_build_cache(&starting.runner_name);
                StartOutcome::Failed
            }
            StartRun::NoContainer {
                runner,
                error,
                may_have_run,
            } => {
                let failure = isolation::start_failure(&starting.runner_name, &error);
                warn!("{repository}: {failure}");
                self.recheck_image(&starting.image);
                // Its runner may have taken a listed job, which no capture names.
                if may_have_run {
                    self.spend(&repository, &slot.profile);
                }
                self.abandon(&slot.profile, &repository, &runner, failure);
                StartOutcome::Failed
            }
            StartRun::Container {
                runner,
                container,
                made,
            } => {
                let made = match made {
                    Made::Running(inspected) => Ok(inspected),
                    Made::Failed(error) => {
                        let failure = isolation::start_failure(&starting.runner_name, &error);
                        warn!("{repository}: {failure}");
                        self.recheck_image(&starting.image);
                        Err(failure)
                    }
                };
                let purpose = starting.purpose;
                let occupant = starting.into_occupant(runner.id, container);
                self.place(slot, occupant, purpose, made).await
            }
        };
        Carried::Done(outcome)
    }

    /// Puts a runner a start made in its slot and watches it, or after a start that failed, its container that
    /// Docker kept: it holds the slot, with its registration, until `collect_finished` confirms its removal, which
    /// keeps a second runner off the slot's volumes. If it runs, the next tick watches it as a runner.
    async fn place(
        &mut self,
        slot: &SlotRef,
        occupant: Occupant,
        purpose: Purpose,
        made: Result<Result<RunnerState, crate::docker::Error>, String>,
    ) -> StartOutcome {
        if let Some(entry) = self
            .slots
            .get_mut(&slot.profile)
            .and_then(|slots| slots.get_mut(slot.index))
        {
            entry.place(occupant.clone());
        }
        let (repository, name, number) =
            (&occupant.repository, &occupant.runner_name, slot.index + 1);
        match made {
            Ok(inspected) => {
                let warm = match purpose {
                    Purpose::Job => "",
                    Purpose::Warm => " warm, for jobs not queued yet",
                    Purpose::Burst => " warm, for a job its runs are expected to queue",
                };
                info!(
                    "{repository}: started {name} in {} slot {number}{warm}",
                    slot.profile
                );
                self.watch_inspected(slot, &occupant, inspected).await;
                StartOutcome::Started
            }
            Err(failure) => {
                warn!(
                    "{repository}: {name} may be left in Docker; {} slot {number} stays its until it is removed",
                    slot.profile
                );
                self.hold(repository, &slot.profile, HoldCause::Start, failure);
                StartOutcome::Failed
            }
        }
    }

    /// What follows a start for a job, whatever it came to: only a runner in its slot ends the job's wait. A start
    /// that failed served nothing, even one that left a container there; its backoff takes the pool's wait, and the
    /// job waits for neither a slot nor the host's memory since.
    fn started(&mut self, launched: &Launched, outcome: StartOutcome) {
        let Some(job) = launched.job else {
            return;
        };
        let (slot, repository) = (&launched.slot, &launched.repository);
        if self.holds_runner(slot) {
            self.note_started(job);
        }
        if outcome == StartOutcome::Started
            && !self.backing_off(repository, &slot.profile, SystemTime::now())
            && let Some(waits) = self
                .repositories
                .listed_mut(repository)
                .and_then(|state| state.waits_mut(&slot.profile))
            && waits.started()
        {
            info!(
                "{repository}: a barrier served its pool in {}: its next jobs hold sooner until its queue empties",
                slot.profile
            );
        }
    }
}
