//! Records of failed jobs (`crate::record`): keeps a failed job's stopped runner container as an image before it is
//! removed, keeps the records within `[records]`, removes them on request, and runs the shells opened in them, whose
//! memory counts against the budget like a slot's. It must not serve a shell's terminal: the daemon does, with the
//! `Tty` `open_shell` returns, and closes it with `Control::close_shell`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use super::control::ControlError;
use super::{Engine, Occupant};
use crate::admission::HIGH_WATER;
use crate::api;
use crate::config::ByteSize;
use crate::docker::{DockerApi, Tty, TtySize, cpu_shares, is_not_found};
use crate::github::GithubApi;
use crate::record;
use crate::scheduler::SlotRef;
use crate::time;

/// A record being made: its container checked and measured, then committed and removed, each in a task of its own
/// so the engine's loop does not wait for Docker's walk of the container's files.
#[derive(Debug)]
pub(super) struct Recording {
    record: api::Record,
    container: String,
    /// The disk its commit is expected to take, once measured: what the records made after it count against
    /// `min_free` until it is done.
    estimate: u64,
    stage: Stage,
}

#[derive(Debug)]
enum Stage {
    /// Its changes checked for secrets, and its size and Docker's free disk measured.
    Checking(JoinHandle<Result<Checked, String>>),
    /// The commit, then the container's removal; the record's size.
    Committing(JoinHandle<Result<u64, String>>),
}

impl Stage {
    fn is_finished(&self) -> bool {
        match self {
            Stage::Checking(task) => task.is_finished(),
            Stage::Committing(task) => task.is_finished(),
        }
    }
}

/// What checking a container found: it may be kept, at about `incoming` bytes, with `free` bytes on Docker's disk
/// when known.
#[derive(Debug)]
struct Checked {
    incoming: u64,
    free: Option<u64>,
}

/// The records of failed jobs and the shells open in them.
#[derive(Debug)]
pub(super) struct RecordState {
    /// Oldest first.
    list: Vec<api::Record>,
    /// Whether `list` is what Docker has: until Docker lists them, no record is made.
    pub(super) listed: bool,
    /// Records being made, by job.
    pub(super) recordings: BTreeMap<u64, Recording>,
    /// Shells open in records, by container ID.
    pub(super) shells: BTreeMap<String, Shell>,
    /// Shells opened by this engine, which tells their containers' names apart.
    opened: u64,
    /// What `Handle::records` shows.
    status: watch::Sender<api::Records>,
    /// A recording's tasks say here that they are done, so the loop moves them on without waiting for a tick.
    done: mpsc::UnboundedSender<()>,
    pub(super) finished: mpsc::UnboundedReceiver<()>,
}

impl RecordState {
    /// No record known yet, and the receiver of what `publish_records` shows.
    pub(super) fn new() -> (RecordState, watch::Receiver<api::Records>) {
        let (status, receiver) = watch::channel(api::Records::default());
        let (done, finished) = mpsc::unbounded_channel();
        let state = RecordState {
            list: Vec::new(),
            listed: false,
            recordings: BTreeMap::new(),
            shells: BTreeMap::new(),
            opened: 0,
            status,
            done,
            finished,
        };
        (state, receiver)
    }
}

/// A shell open in a record.
#[derive(Debug, Clone)]
pub(super) struct Shell {
    job: u64,
    /// The profile the record's job ran in, whose snapshot and shared caches the shell mounts.
    pub(super) profile: String,
    /// Its container's memory cap, which counts against the budget.
    pub(super) memory: u64,
    opened: Instant,
}

/// The longest a shell stays open: a forgotten one holds a slot's memory and keeps its record from going.
const MAX_SHELL: Duration = Duration::from_hours(8);

/// Which records a removal asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordSelection {
    /// The record of one job.
    Job(u64),
    /// Every record without a shell open.
    All,
    /// The records older than this, without a shell open.
    OlderThan(Duration),
}

/// What a shell container runs, as root: the profile's snapshot under the cache layers as a runner sees them, with
/// the shell's own changes in its anonymous /state, then a login shell as `runner` in the directory `$1`.
const SHELL_SCRIPT: &str = r#"
. /usr/local/lib/runnerd-layers.sh && mount_layers > /dev/null 2>&1
install -d -o runner -g runner -m 700 "$XDG_RUNTIME_DIR" 2> /dev/null
cd "$1" 2> /dev/null || cd /home/runner
exec setpriv --reuid=runner --regid=runner --init-groups -- env HOME=/home/runner USER=runner LOGNAME=runner bash -l
"#;

/// The disk Docker takes per byte a container's layer holds once committed: the containerd image store keeps the
/// layer's archive besides its unpacked copy.
const COMMIT_FACTOR: u64 = 2;

impl<G: GithubApi + 'static, D: DockerApi + 'static> Engine<G, D> {
    /// Whether the profile of a slot keeps its failed jobs' runners.
    fn records_failures(&self, profile: &str) -> bool {
        self.config
            .profiles
            .get(profile)
            .or_else(|| self.retired.get(profile))
            .is_some_and(|profile| profile.record_failures)
    }

    /// Starts keeping a stopped runner container whose job failed as the job's record: checks and measures it, makes
    /// room within the limits, then commits and removes it, in the background. Returns whether it took the container
    /// over; if not, the caller removes it.
    pub(super) async fn begin_record(&mut self, slot: &SlotRef, occupant: &Occupant) -> bool {
        // The capture's last report may have named the job after `occupant` was taken.
        let job = self
            .occupied()
            .find(|(_, candidate)| candidate.runner_name == occupant.runner_name)
            .and_then(|(_, candidate)| candidate.job)
            .or(occupant.job);
        let Some(job) = job else {
            return false;
        };
        if !self.records_failures(&slot.profile) {
            return false;
        }
        if !self.records.listed {
            self.list_records().await;
            if !self.records.listed {
                let why = "Docker did not list the records kept".to_owned();
                self.skip_record(job, &occupant.repository, why);
                return false;
            }
        }
        let summary = self
            .store
            .job(job)
            .await
            .ok()
            .flatten()
            .map(|job| job.summary);
        let record = api::Record {
            job,
            repository: occupant.repository.clone(),
            profile: slot.profile.clone(),
            workflow: summary
                .as_ref()
                .and_then(|summary| summary.workflow.clone()),
            name: summary.map(|summary| summary.name),
            created_at: time::now_ms() / 1000,
            bytes: 0,
            image: record::image(job),
        };
        info!(
            "{}: keeping job {job}'s runner as {}",
            occupant.repository, record.image
        );
        let docker = self.docker.clone();
        let container = occupant.container.clone();
        let done = self.records.done.clone();
        let check = tokio::spawn(async move {
            let checked = check(&*docker, &container).await;
            let _ = done.send(());
            checked
        });
        self.records.recordings.insert(
            job,
            Recording {
                record,
                container: occupant.container.clone(),
                estimate: 0,
                stage: Stage::Checking(check),
            },
        );
        true
    }

    /// Notes that a stopped runner container an earlier daemon left was not kept, if its profile keeps failed jobs'
    /// runners, its job failed and no record of it was made: no daemon ran when it stopped, or one stopped while it
    /// made the record.
    pub(super) async fn note_lost_record(&self, profile: &str, job: Option<u64>, container: &str) {
        let Some(job) = job else {
            return;
        };
        if !self.records_failures(profile)
            || self.records.list.iter().any(|record| record.job == job)
        {
            return;
        }
        if let Ok(log) = self.docker.logs(container).await
            && record::failed(&log)
        {
            self.store.record_skipped(
                job,
                "runnerd was not running when the runner stopped, or stopped while keeping it"
                    .to_owned(),
            );
        }
    }

    /// Notes in the history why a failed job has no record.
    fn skip_record(&self, job: u64, repository: &str, why: String) {
        warn!("{repository}: no record of job {job}: {why}");
        self.store.record_skipped(job, why);
    }

    /// Makes room for a checked container within the limits, removing the records in its way, then commits it in
    /// the background; or notes why it is not kept and removes it.
    async fn make_room_and_commit(
        &mut self,
        record: api::Record,
        container: String,
        checked: Checked,
    ) {
        let job = record.job;
        // Records being committed take their disk too.
        let pending: u64 = self
            .records
            .recordings
            .values()
            .map(|other| other.estimate)
            .sum();
        let room = record::make_room(
            &self.records.list,
            &self.shell_jobs(),
            &self.config.records,
            checked.incoming,
            checked.free.map(|free| free.saturating_sub(pending)),
            time::now_ms() / 1000,
        );
        let removed = match room {
            Ok(removals) => {
                let mut removed = Ok(());
                for (old, why) in removals {
                    if let Err(error) = self.remove_record(old, why).await {
                        removed = Err(error.to_string());
                        break;
                    }
                }
                removed
            }
            Err(why) => Err(why),
        };
        if let Err(why) = removed {
            self.skip_record(job, &record.repository, why);
            self.remove_container(&container).await;
            return;
        }
        let labels = labels(&record);
        let docker = self.docker.clone();
        let image = record.image.clone();
        let done = self.records.done.clone();
        let committed_container = container.clone();
        let commit = tokio::spawn(async move {
            let container = committed_container;
            let committed = docker.commit(&container, &image, labels).await;
            if let Err(error) = docker.remove(&container).await
                && !is_not_found(&error)
            {
                warn!("cannot remove {container}: {error}");
            }
            let _ = done.send(());
            committed.map_err(|error| error.to_string())
        });
        self.records.recordings.insert(
            job,
            Recording {
                record,
                container,
                estimate: checked.incoming,
                stage: Stage::Committing(commit),
            },
        );
    }

    async fn remove_container(&self, container: &str) {
        if let Err(error) = self.docker.remove(container).await
            && !is_not_found(&error)
        {
            warn!("cannot remove {container}: {error}");
        }
    }

    /// Moves the recordings whose task finished on: a checked container to its commit, a committed one into the
    /// records, kept within their limits with its real size.
    pub(super) async fn collect_recordings(&mut self) {
        while self.records.finished.try_recv().is_ok() {}
        let done: Vec<u64> = self
            .records
            .recordings
            .iter()
            .filter(|(_, recording)| recording.stage.is_finished())
            .map(|(job, _)| *job)
            .collect();
        for job in done {
            let Some(recording) = self.records.recordings.remove(&job) else {
                continue;
            };
            let Recording {
                record,
                container,
                stage,
                ..
            } = recording;
            let task = match stage {
                Stage::Checking(task) => {
                    let checked = task
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|checked| checked);
                    match checked {
                        Ok(checked) => self.make_room_and_commit(record, container, checked).await,
                        Err(why) => {
                            self.skip_record(job, &record.repository, why);
                            self.remove_container(&container).await;
                        }
                    }
                    continue;
                }
                Stage::Committing(task) => task,
            };
            match task
                .await
                .map_err(|error| error.to_string())
                .and_then(|committed| committed)
            {
                Ok(bytes) => self.keep_record(record, bytes).await,
                Err(error) => self.skip_record(
                    job,
                    &record.repository,
                    format!("Docker did not commit it: {error}"),
                ),
            }
        }
    }

    /// Adds a committed record, and removes the records past the limits with its real size.
    async fn keep_record(&mut self, mut record: api::Record, bytes: u64) {
        let job = record.job;
        record.bytes = bytes;
        self.name_from_history(&mut record).await;
        info!(
            "{}: kept job {job}'s runner as {} ({})",
            record.repository,
            record.image,
            ByteSize(bytes)
        );
        let removals = record::make_room(
            &self.records.list,
            &self.shell_jobs(),
            &self.config.records,
            bytes,
            None,
            record.created_at,
        );
        self.store.record_made(record.clone());
        self.records.list.push(record);
        let removals = removals.unwrap_or_else(|_| vec![(job, api::RecordRemoval::Size)]);
        for (old, why) in removals {
            if let Err(error) = self.remove_record(old, why).await {
                warn!("{error}");
            }
        }
    }

    /// Fills in a record's workflow and job name from the history, which may have learnt them from GitHub only after
    /// the commit started.
    async fn name_from_history(&self, record: &mut api::Record) {
        if record.workflow.is_some() && record.name.is_some() {
            return;
        }
        if let Ok(Some(job)) = self.store.job(record.job).await {
            record.workflow = record.workflow.take().or(job.summary.workflow);
            record.name.get_or_insert(job.summary.name);
        }
    }

    /// Lists the record images Docker has; until it answers, no record is made.
    pub(super) async fn list_records(&mut self) {
        match self.docker.images(record::LABEL).await {
            Ok(images) => {
                let mut records: Vec<api::Record> = images
                    .into_iter()
                    .filter_map(|image| {
                        let label = |name: &str| image.labels.get(name).cloned();
                        Some(api::Record {
                            job: label(record::LABEL)?.parse().ok()?,
                            repository: label("runnerd.repository").unwrap_or_default(),
                            profile: label("runnerd.profile").unwrap_or_default(),
                            workflow: label("runnerd.workflow"),
                            name: label("runnerd.name"),
                            created_at: label("runnerd.created-at")
                                .and_then(|at| at.parse().ok())
                                .unwrap_or(0),
                            bytes: image.bytes,
                            image: image.reference,
                        })
                    })
                    .collect();
                records.sort_by_key(|record| (record.created_at, record.job));
                for record in &mut records {
                    self.name_from_history(record).await;
                    // Committed before an earlier daemon stopped, which did not get to note it.
                    let noted = self.store.job(record.job).await.ok().flatten();
                    if noted
                        .is_some_and(|job| job.record.is_none_or(|note| note.created_at.is_none()))
                    {
                        self.store.record_made(record.clone());
                    }
                }
                self.records.list = records;
                self.records.listed = true;
            }
            Err(error) => warn!("cannot list the records: {error}"),
        }
    }

    /// Removes the records past `max_age`.
    pub(super) async fn expire_records(&mut self) {
        let expired = record::expired(
            &self.records.list,
            &self.shell_jobs(),
            &self.config.records,
            time::now_ms() / 1000,
        );
        for (job, why) in expired {
            if let Err(error) = self.remove_record(job, why).await {
                warn!("{error}");
            }
        }
    }

    /// Removes one record's image, and its layers with it.
    async fn remove_record(
        &mut self,
        job: u64,
        why: api::RecordRemoval,
    ) -> Result<u64, ControlError> {
        let Some(index) = self
            .records
            .list
            .iter()
            .position(|record| record.job == job)
        else {
            return Err(ControlError::NotFound(format!(
                "job {job} has no record; `runnerctl records` lists them"
            )));
        };
        let image = self.records.list[index].image.clone();
        self.docker.remove_image(&image).await.map_err(|error| {
            ControlError::Failed(format!("cannot remove the record of job {job}: {error}"))
        })?;
        let removed = self.records.list.remove(index);
        info!("removed the record of job {job} ({why})");
        self.store.record_removed(job, time::now_ms(), why);
        Ok(removed.bytes)
    }

    /// Removes the selected records, but those with a shell open: one asked for by its job is refused instead.
    pub(super) async fn remove_records(
        &mut self,
        selection: &RecordSelection,
    ) -> Result<api::RemovedRecords, ControlError> {
        let open = self.shell_jobs();
        let now = time::now_ms() / 1000;
        let jobs: Vec<u64> = match selection {
            RecordSelection::Job(job) => {
                if open.contains(job) {
                    return Err(ControlError::Conflict(format!(
                        "a shell is open in the record of job {job}; remove it once the shell exits"
                    )));
                }
                vec![*job]
            }
            RecordSelection::All => self.records.list.iter().map(|record| record.job).collect(),
            RecordSelection::OlderThan(age) => self
                .records
                .list
                .iter()
                .filter(|record| now.saturating_sub(record.created_at) > age.as_secs())
                .map(|record| record.job)
                .collect(),
        };
        let mut removed = api::RemovedRecords::default();
        for job in jobs.into_iter().filter(|job| !open.contains(job)) {
            removed.bytes += self.remove_record(job, api::RecordRemoval::User).await?;
            removed.jobs.push(job);
        }
        Ok(removed)
    }

    /// The jobs whose record has a shell open.
    fn shell_jobs(&self) -> BTreeSet<u64> {
        self.records
            .shells
            .values()
            .map(|shell| shell.job)
            .collect()
    }

    /// The profiles whose snapshot and shared caches a shell mounts.
    pub(super) fn shell_profiles(&self) -> BTreeSet<&str> {
        self.records
            .shells
            .values()
            .map(|shell| shell.profile.as_str())
            .collect()
    }

    /// The shells' part of the memory in use.
    pub(super) fn shell_memory(&self) -> u64 {
        self.records.shells.values().map(|shell| shell.memory).sum()
    }

    /// The shells' memory by job, for the published budget.
    pub(super) fn shell_budget(&self) -> Vec<api::ShellMemory> {
        self.records
            .shells
            .values()
            .map(|shell| api::ShellMemory {
                job: shell.job,
                memory: shell.memory,
            })
            .collect()
    }

    /// Shows the records as they are now, if they changed.
    pub(super) fn publish_records(&self) {
        let records = self.records_status();
        self.records.status.send_if_modified(|current| {
            let changed = *current != records;
            *current = records;
            changed
        });
    }

    /// Every record and the limits, as `GET /v1/records` shows them.
    fn records_status(&self) -> api::Records {
        let limits = &self.config.records;
        api::Records {
            records: self.records.list.clone(),
            limits: api::RecordLimits {
                max_total: limits.max_total.0,
                max_count: limits.max_count,
                max_age: limits.max_age.as_secs(),
                min_free: limits.min_free.0,
            },
            shells: self.shell_jobs().into_iter().collect(),
        }
    }

    /// Starts a shell in a job's record, with the record's profile's limits, if the budget has room for them as for
    /// a slot of the profile.
    pub(super) async fn open_shell(
        &mut self,
        job: u64,
        size: TtySize,
    ) -> Result<Tty, ControlError> {
        let Some(record) = self
            .records
            .list
            .iter()
            .find(|record| record.job == job)
            .cloned()
        else {
            return Err(ControlError::NotFound(format!(
                "job {job} has no record; `runnerctl records` lists them"
            )));
        };
        let profile = self.config.profiles.get(&record.profile);
        let Some(profile) = profile.or_else(|| self.retired.get(&record.profile)) else {
            return Err(ControlError::Invalid(format!(
                "the record of job {job} ran in profile {}, which the config no longer has",
                record.profile
            )));
        };
        let (memory, cpus) = (profile.memory.0, profile.cpus);
        // A reseed replaces the snapshot the shell's layers sit on.
        if self.reseeds.contains_key(&record.profile) {
            return Err(ControlError::Conflict(format!(
                "{} is reseeding its snapshot, which a shell mounts; open it once the reseed is done",
                record.profile
            )));
        }
        // A trim removes the shared caches' volumes, which a shell mounts.
        if self.trim.as_ref().is_some_and(|trim| !trim.is_finished()) {
            return Err(ControlError::Conflict(
                "the shared caches, which a shell mounts, are being trimmed; open it once the trim is done"
                    .to_owned(),
            ));
        }
        if let Some(max) = self.memory_max {
            let in_use = self.memory_in_use();
            if in_use.saturating_add(memory) > max {
                return Err(ControlError::Conflict(format!(
                    "the budget is full: {} of the slice's {} is held, and a shell takes {} as a slot of {} \
                     does; open it once a job finishes",
                    ByteSize(in_use),
                    ByteSize(max),
                    ByteSize(memory),
                    record.profile
                )));
            }
        }
        // Under measured admission nothing starts at the high-water mark, a shell no more than a runner.
        if self.admitting(&mut [], None).held {
            return Err(ControlError::Conflict(format!(
                "the slice is at {HIGH_WATER}% of its MemoryMax, where measured admission starts nothing; open it \
                 once it falls under"
            )));
        }
        let body = self.shell_body(&record, memory, cpus);
        self.records.opened += 1;
        let name = format!(
            "runnerd-shell-{job}-{}-{}",
            time::now_ms() / 1000,
            self.records.opened
        );
        let tty = self
            .docker
            .shell(&name, body, size)
            .await
            .map_err(|error| ControlError::Failed(format!("cannot start a shell: {error}")))?;
        info!("opened a shell in the record of job {job}");
        self.records.shells.insert(
            tty.container.clone(),
            Shell {
                job,
                profile: record.profile,
                memory,
                opened: Instant::now(),
            },
        );
        Ok(tty)
    }

    /// The container of a shell in `record`: the record's image under the profile's caps, its snapshot and shared
    /// caches mounted read-only, and a terminal.
    fn shell_body(&self, record: &api::Record, memory: u64, cpus: u32) -> ContainerCreateBody {
        let workspace = record::workspace(&record.repository);
        let limit = i64::try_from(memory).unwrap_or(i64::MAX);
        let mut mounts = vec![Mount {
            target: Some("/state".to_owned()),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        }];
        if self.config.profiles.contains_key(&record.profile) {
            // The snapshot and the shared caches as the job saw them, read-only: running jobs share them.
            mounts.extend(
                crate::cache::runner_mounts(&self.names, &record.profile, 1)
                    .into_iter()
                    .filter(|mount| mount.target.as_deref() != Some("/state"))
                    .map(|mount| Mount {
                        read_only: Some(true),
                        ..mount
                    }),
            );
        }
        let mut host_config = HostConfig {
            cgroup_parent: Some(self.config.host.slice.clone()),
            memory: Some(limit),
            memory_swap: Some(limit),
            nano_cpus: Some(i64::from(cpus) * 1_000_000_000),
            cpu_shares: Some(cpu_shares(cpus)),
            init: Some(true),
            mounts: Some(mounts),
            network_mode: Some(self.names.network()),
            ..Default::default()
        };
        self.isolation.configure(&mut host_config);
        ContainerCreateBody {
            image: Some(record.image.clone()),
            entrypoint: Some(vec![
                "bash".to_owned(),
                "-c".to_owned(),
                SHELL_SCRIPT.to_owned(),
            ]),
            cmd: Some(vec!["shell".to_owned(), workspace]),
            env: Some(vec!["TERM=xterm-256color".to_owned()]),
            tty: Some(true),
            open_stdin: Some(true),
            stdin_once: Some(true),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            labels: Some(HashMap::from([(
                record::SHELL_LABEL.to_owned(),
                record.job.to_string(),
            )])),
            host_config: Some(host_config),
            ..Default::default()
        }
    }

    /// Removes a shell's container and frees its memory. One Docker does not remove now goes at a later tick.
    pub(super) async fn close_shell(&mut self, container: &str) {
        let Some(shell) = self.records.shells.get(container) else {
            return;
        };
        let job = shell.job;
        match self.docker.remove(container).await {
            Err(error) if !is_not_found(&error) => {
                warn!("cannot remove the shell in the record of job {job}: {error}");
            }
            _ => {
                info!("closed the shell in the record of job {job}");
                self.records.shells.remove(container);
            }
        }
    }

    /// Removes the shells whose process exited, whatever became of their terminal, and those open longer than
    /// `MAX_SHELL`.
    pub(super) async fn collect_shells(&mut self) {
        let shells: Vec<(String, Instant)> = self
            .records
            .shells
            .iter()
            .map(|(container, shell)| (container.clone(), shell.opened))
            .collect();
        for (container, opened) in shells {
            if opened.elapsed() > MAX_SHELL {
                info!("closing {container}: a shell is open for 8 hours at most");
                self.close_shell(&container).await;
                continue;
            }
            let ended = match self.docker.inspect(&container).await {
                Ok(state) => !state.running,
                Err(error) => is_not_found(&error),
            };
            if ended {
                self.close_shell(&container).await;
            }
        }
    }

    /// Removes every shell: on shutdown, and those an earlier daemon left, which no terminal is attached to.
    pub(super) async fn close_shells(&mut self) {
        let containers: Vec<String> = self.records.shells.keys().cloned().collect();
        for container in containers {
            self.close_shell(&container).await;
        }
    }
}

/// The labels a record's image carries, which `list_records` reads back.
fn labels(record: &api::Record) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (record::LABEL.to_owned(), record.job.to_string()),
        ("runnerd.repository".to_owned(), record.repository.clone()),
        ("runnerd.profile".to_owned(), record.profile.clone()),
        (
            "runnerd.created-at".to_owned(),
            record.created_at.to_string(),
        ),
    ]);
    if let Some(workflow) = &record.workflow {
        labels.insert("runnerd.workflow".to_owned(), workflow.clone());
    }
    if let Some(name) = &record.name {
        labels.insert("runnerd.name".to_owned(), name.clone());
    }
    labels
}

/// Checks that a stopped runner container may be kept, and measures it: refuses one whose changes still hold a
/// secret, and estimates the disk its commit takes. Docker walks the container's files for both, so it runs in a
/// task.
async fn check(docker: &impl DockerApi, container: &str) -> Result<Checked, String> {
    let changed = docker
        .changes(container)
        .await
        .map_err(|error| format!("Docker did not list its files: {error}"))?;
    let secrets = record::secrets_left(changed.iter().map(String::as_str));
    if !secrets.is_empty() {
        return Err(format!(
            "its runner left files a record must not keep ({}): its image did not delete them; use the image of \
             runnerd {}, or rebuild image_stage from its image/",
            record::listed(&secrets),
            env!("CARGO_PKG_VERSION")
        ));
    }
    let incoming = docker
        .written(container)
        .await
        .map_err(|error| format!("Docker did not measure it: {error}"))?
        .saturating_mul(COMMIT_FACTOR);
    let free = match docker.free_space().await {
        Ok(free) => Some(free),
        Err(error) => {
            warn!("cannot measure Docker's free disk: {error}");
            None
        }
    };
    Ok(Checked { incoming, free })
}
