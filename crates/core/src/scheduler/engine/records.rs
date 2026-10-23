//! Records of failed jobs (`crate::record`): keeps a failed job's stopped runner container as an image before it is
//! removed, keeps the records within `[records]`, removes them on request, and runs the shells opened in them, whose
//! memory counts against the budget like a slot's. It must not serve a shell's terminal: the daemon does, with the
//! `Tty` `open_shell` returns, and closes it with `Control::close_shell`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::control::ControlError;
use super::{Engine, Occupant};
use crate::api;
use crate::config::ByteSize;
use crate::docker::{DockerApi, Tty, TtySize, is_not_found};
use crate::github::GithubApi;
use crate::record;
use crate::scheduler::SlotRef;
use crate::time;

/// A record being committed, with what its `api::Record` needs once the commit is done.
#[derive(Debug)]
pub(super) struct Recording {
    record: api::Record,
    /// The commit, then the container's removal; the record's size.
    commit: JoinHandle<Result<u64, String>>,
}

/// The records of failed jobs and the shells open in them.
#[derive(Debug)]
pub(super) struct RecordState {
    /// Oldest first.
    list: Vec<api::Record>,
    /// Whether `list` is what Docker has: until Docker lists them, no record is made.
    pub(super) listed: bool,
    /// Records being committed, by job.
    pub(super) recordings: BTreeMap<u64, Recording>,
    /// Shells open in records, by container ID.
    pub(super) shells: BTreeMap<String, Shell>,
    /// What `Handle::records` shows.
    status: watch::Sender<api::Records>,
}

impl RecordState {
    /// No record known yet, and the receiver of what `publish_records` shows.
    pub(super) fn new() -> (RecordState, watch::Receiver<api::Records>) {
        let (status, receiver) = watch::channel(api::Records::default());
        let state = RecordState {
            list: Vec::new(),
            listed: false,
            recordings: BTreeMap::new(),
            shells: BTreeMap::new(),
            status,
        };
        (state, receiver)
    }
}

/// A shell open in a record.
#[derive(Debug, Clone)]
pub(super) struct Shell {
    job: u64,
    /// Its container's memory cap, which counts against the budget.
    pub(super) memory: u64,
}

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

    /// Starts keeping a stopped runner container whose job failed as the job's record: makes room within the
    /// limits, then commits and removes it in the background. Returns whether it took the container over; if not,
    /// the caller removes it, and the history says why no record was made.
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
        if let Err(why) = self.make_room_for(&occupant.container).await {
            warn!("{}: no record of job {job}: {why}", occupant.repository);
            self.store.record_skipped(job, why);
            return false;
        }
        self.commit(job, slot, occupant).await;
        true
    }

    /// Checks that a runner container may be kept, and removes the records in its way; or says why it is not kept.
    async fn make_room_for(&mut self, container: &str) -> Result<(), String> {
        if !self.records.listed {
            self.list_records().await;
            if !self.records.listed {
                return Err("Docker did not list the records kept".to_owned());
            }
        }
        let changed = self
            .docker
            .changes(container)
            .await
            .map_err(|error| format!("Docker did not list its files: {error}"))?;
        let secrets = record::secrets_left(changed.iter().map(String::as_str));
        if !secrets.is_empty() {
            return Err(format!(
                "its runner left secret files ({}); a runner image of this release deletes them",
                secrets.join(", ")
            ));
        }
        let incoming = self
            .docker
            .written(container)
            .await
            .map_err(|error| format!("Docker did not measure it: {error}"))?
            .saturating_mul(COMMIT_FACTOR);
        let free = match self.docker.free_space().await {
            Ok(free) => Some(free),
            Err(error) => {
                warn!("cannot measure Docker's free disk: {error}");
                None
            }
        };
        let removals = record::make_room(
            &self.records.list,
            &self.shell_jobs(),
            &self.config.records,
            incoming,
            free,
            time::now_ms() / 1000,
        )?;
        for (old, why) in removals {
            self.remove_record(old, why)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Commits a failed job's runner container as its record, then removes the container, in the background.
    async fn commit(&mut self, job: u64, slot: &SlotRef, occupant: &Occupant) {
        let summary = self
            .store
            .job(job)
            .await
            .ok()
            .flatten()
            .map(|job| job.summary);
        let now = time::now_ms() / 1000;
        let record = api::Record {
            job,
            repository: occupant.repository.clone(),
            profile: slot.profile.clone(),
            workflow: summary
                .as_ref()
                .and_then(|summary| summary.workflow.clone()),
            name: summary.map(|summary| summary.name),
            created_at: now,
            bytes: 0,
            image: record::image(job),
        };
        let mut labels = BTreeMap::from([
            (record::LABEL.to_owned(), job.to_string()),
            ("runnerd.repository".to_owned(), record.repository.clone()),
            ("runnerd.profile".to_owned(), record.profile.clone()),
            ("runnerd.created-at".to_owned(), now.to_string()),
        ]);
        if let Some(workflow) = &record.workflow {
            labels.insert("runnerd.workflow".to_owned(), workflow.clone());
        }
        if let Some(name) = &record.name {
            labels.insert("runnerd.name".to_owned(), name.clone());
        }
        info!(
            "{}: keeping job {job}'s runner as {}",
            occupant.repository, record.image
        );
        let docker = self.docker.clone();
        let container = occupant.container.clone();
        let image = record.image.clone();
        let commit = tokio::spawn(async move {
            let committed = docker.commit(&container, &image, labels).await;
            if let Err(error) = docker.remove(&container).await
                && !is_not_found(&error)
            {
                warn!("cannot remove {container}: {error}");
            }
            committed.map_err(|error| error.to_string())
        });
        self.records
            .recordings
            .insert(job, Recording { record, commit });
    }

    /// Takes the records whose commit finished, and keeps the records within their limits with the real sizes.
    pub(super) async fn collect_recordings(&mut self) {
        let done: Vec<u64> = self
            .records
            .recordings
            .iter()
            .filter(|(_, recording)| recording.commit.is_finished())
            .map(|(job, _)| *job)
            .collect();
        for job in done {
            let Some(Recording { mut record, commit }) = self.records.recordings.remove(&job)
            else {
                continue;
            };
            let bytes = match commit.await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(error)) => {
                    warn!(
                        "{}: cannot keep job {job}'s runner: {error}",
                        record.repository
                    );
                    self.store
                        .record_skipped(job, format!("Docker did not commit it: {error}"));
                    continue;
                }
                Err(error) => {
                    warn!(
                        "{}: cannot keep job {job}'s runner: {error}",
                        record.repository
                    );
                    self.store.record_skipped(job, error.to_string());
                    continue;
                }
            };
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
        let workspace = record::workspace(&record.repository);
        let limit = i64::try_from(memory).unwrap_or(i64::MAX);
        let mut mounts = vec![Mount {
            target: Some("/state".to_owned()),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        }];
        if self.config.profiles.contains_key(&record.profile) {
            mounts.extend(
                crate::cache::runner_mounts(&self.names, &record.profile, 1)
                    .into_iter()
                    .filter(|mount| mount.target.as_deref() != Some("/state")),
            );
        }
        let mut host_config = HostConfig {
            cgroup_parent: Some(self.config.host.slice.clone()),
            memory: Some(limit),
            memory_swap: Some(limit),
            nano_cpus: Some(i64::from(cpus) * 1_000_000_000),
            init: Some(true),
            mounts: Some(mounts),
            network_mode: Some(self.names.network()),
            ..Default::default()
        };
        self.isolation.configure(&mut host_config);
        let body = ContainerCreateBody {
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
                job.to_string(),
            )])),
            host_config: Some(host_config),
            ..Default::default()
        };
        let name = format!("runnerd-shell-{job}-{}", time::now_ms() / 1000);
        let tty = self
            .docker
            .shell(&name, body, size)
            .await
            .map_err(|error| ControlError::Failed(format!("cannot start a shell: {error}")))?;
        info!("opened a shell in the record of job {job}");
        self.records
            .shells
            .insert(tty.container.clone(), Shell { job, memory });
        Ok(tty)
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

    /// Removes the shells whose process exited, whatever became of their terminal.
    pub(super) async fn collect_shells(&mut self) {
        let containers: Vec<String> = self.records.shells.keys().cloned().collect();
        for container in containers {
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
