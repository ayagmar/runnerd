//! `runnerctl watch`: the daemon's state kept current from `/v1/events`. A snapshot (status with its queued jobs,
//! running jobs, the last finished ones) read after the stream is open, then each event applied to it. Draws nothing
//! itself: `run` hands the state to a drawing function, and `frame` turns it into text for `output::redraw`.
//! `runnerctl tui` keeps the same `State` under its own.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use runnerd_core::api::{
    Event, EventEnvelope, HostSample, Job, JobQuery, JobState, JobSummary, QueuedJob, Sample,
    Status, Step,
};

use crate::human::{self, Paint, Table, Tone, bytes, cores, duration};
use runnerd_core::client::{Client, ClientError, Events};

/// Finished jobs kept on screen.
const RECENT: usize = 5;

/// What the screen shows.
#[derive(Debug)]
pub(crate) struct State {
    /// Its `queued` jobs are kept current from `job.*` events, oldest first.
    pub(crate) status: Status,
    pub(crate) running: BTreeMap<u64, Job>,
    /// Each running job's last sample.
    pub(crate) samples: BTreeMap<u64, Sample>,
    pub(crate) host: Option<HostSample>,
    /// The last finished jobs, newest first.
    pub(crate) recent: Vec<JobSummary>,
}

impl State {
    /// Reads the snapshot. Open the event stream first, so nothing that happens meanwhile is missed.
    pub(crate) async fn read(client: &Client) -> Result<State, ClientError> {
        let status: Status = client.get("/v1/status").await?;
        let page = |state| JobQuery {
            state,
            limit: Some(100),
            ..JobQuery::default()
        };
        let running: runnerd_core::api::JobPage = client
            .get_query("/v1/jobs", &page(Some(JobState::Running)))
            .await?;
        let mut running_jobs = BTreeMap::new();
        for summary in running.jobs {
            let job: Job = client.get(&format!("/v1/jobs/{}", summary.id)).await?;
            running_jobs.insert(job.summary.id, job);
        }
        let latest: runnerd_core::api::JobPage = client
            .get_query(
                "/v1/jobs",
                &JobQuery {
                    limit: Some(20),
                    ..JobQuery::default()
                },
            )
            .await?;
        Ok(State {
            status,
            running: running_jobs,
            samples: BTreeMap::new(),
            host: None,
            recent: latest
                .jobs
                .into_iter()
                .filter(|job| job.state.is_final())
                .take(RECENT)
                .collect(),
        })
    }

    /// Applies one event. Returns a running job to read again: `job.started` carries no name for a job its runner
    /// took before the daemon saw it queued, and only the job's record gets it later.
    pub(crate) fn apply(&mut self, envelope: &EventEnvelope) -> Option<u64> {
        match &envelope.event {
            Event::JobQueued { job } => {
                let queued = &mut self.status.queued;
                if !queued.iter().any(|known| known.id == job.id) {
                    queued.push(queued_job(job));
                    // As the status orders them: IDs grow with time.
                    queued.sort_by_key(|known| known.id);
                }
            }
            Event::JobStarted { job } => {
                self.status.queued.retain(|known| known.id != job.id);
                let steps = self
                    .running
                    .remove(&job.id)
                    .map(|job| job.steps)
                    .unwrap_or_default();
                self.running.insert(
                    job.id,
                    Job {
                        admission: None,
                        summary: job.clone(),
                        steps,
                        record: None,
                    },
                );
            }
            Event::JobStep { job: id, step } => {
                if let Some(job) = self.running.get_mut(id) {
                    update_step(&mut job.steps, step);
                    if job.summary.name.is_empty() {
                        return Some(*id);
                    }
                }
            }
            Event::JobFinished { job } => {
                self.status.queued.retain(|known| known.id != job.id);
                self.running.remove(&job.id);
                self.samples.remove(&job.id);
                self.recent.retain(|known| known.id != job.id);
                self.recent.insert(0, job.clone());
                self.recent.truncate(RECENT);
            }
            Event::SlotChanged { profile, slot } => {
                if let Some(known) = self
                    .status
                    .profiles
                    .iter_mut()
                    .filter(|known| known.name == *profile)
                    .flat_map(|known| known.slots.iter_mut())
                    .find(|known| known.slot == slot.slot)
                {
                    *known = slot.clone();
                }
            }
            Event::ProfileChanged { profile } => {
                match self
                    .status
                    .profiles
                    .iter_mut()
                    .find(|known| known.name == profile.name)
                {
                    Some(known) => *known = profile.clone(),
                    None => self.status.profiles.push(profile.clone()),
                }
            }
            Event::ProfileRemoved { profile } => {
                self.status.profiles.retain(|known| known.name != *profile);
            }
            Event::BudgetChanged { budget } => self.status.budget = budget.clone(),
            Event::RepositoryChanged { repository } => {
                match self
                    .status
                    .repositories
                    .iter_mut()
                    .find(|known| known.name == repository.name)
                {
                    Some(known) => *known = repository.clone(),
                    None => self.status.repositories.push(repository.clone()),
                }
            }
            Event::RepositoryRemoved { repository } => self
                .status
                .repositories
                .retain(|known| known.name != *repository),
            Event::JobSample { job, sample } => {
                self.samples.insert(*job, sample.clone());
            }
            Event::HostSample { sample } => self.host = Some(sample.clone()),
            Event::CacheReseeded { .. }
            | Event::LogLine { .. }
            | Event::RecordCreated { .. }
            | Event::RecordDeleted { .. }
            | Event::SizingChanged { .. } => {}
        }
        None
    }

    /// Replaces a running job with its record as read again.
    pub(crate) fn refresh(&mut self, job: Job) {
        if let Some(known) = self.running.get_mut(&job.summary.id) {
            *known = job;
        }
    }

    /// The running job in a profile's slot.
    pub(crate) fn job_in(&self, profile: &str, slot: usize) -> Option<&Job> {
        self.running.values().find(|job| {
            job.summary
                .runner
                .as_ref()
                .is_some_and(|runner| runner.profile == profile && runner.slot == slot)
        })
    }
}

/// A queued job as the status lists it, from the job `job.queued` announces.
fn queued_job(job: &JobSummary) -> QueuedJob {
    QueuedJob {
        id: job.id,
        repository: job.repository.clone(),
        run_id: job.run_id,
        run_attempt: job.run_attempt,
        workflow: job.workflow.clone(),
        name: job.name.clone(),
        branch: job.branch.clone(),
        url: job.url.clone(),
        queued_at: job.timeline.queued_at,
        waiting: None,
    }
}

/// Reads the snapshot, then draws the state after every event and every second, until `draw` returns false or the
/// stream fails for good. `events` must be open already.
pub(crate) async fn run(
    client: &Client,
    events: Events,
    mut draw: impl FnMut(&State) -> bool,
) -> Result<(), ClientError> {
    let mut state = State::read(client).await?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // One read stays pending across redraws: dropping it at every tick would restart the stream's idle timeout, and
    // a stalled stream would never be reconnected.
    let mut next = Box::pin(next_event(events));
    loop {
        if !draw(&state) {
            return Ok(());
        }
        tokio::select! {
            (mut events, envelope) = &mut next => {
                // The stream is open again already, so nothing after this read is missed.
                let missed = events.missed_events();
                next = Box::pin(next_event(events));
                if missed {
                    state = State::read(client).await?;
                }
                if let Some(id) = state.apply(&envelope?) {
                    state.refresh(client.get(&format!("/v1/jobs/{id}")).await?);
                }
            }
            _ = tick.tick() => {}
        }
    }
}

/// Replaces the step with the same number, or adds it in order.
pub(crate) fn update_step(steps: &mut Vec<Step>, step: &Step) {
    if let Some(known) = steps.iter_mut().find(|known| known.number == step.number) {
        *known = step.clone();
    } else {
        steps.push(step.clone());
        steps.sort_by_key(|step| step.number);
    }
}

/// The next event, handing the stream back for the read after it.
async fn next_event(mut events: Events) -> (Events, Result<EventEnvelope, ClientError>) {
    let envelope = events.next().await;
    (events, envelope)
}

/// The step a job is in: the one in progress, else the last one completed; and its position, 1-based.
pub(crate) fn current_step(job: &Job) -> Option<(&Step, usize)> {
    let position = job
        .steps
        .iter()
        .position(|step| step.status == "in_progress")
        .or_else(|| {
            job.steps
                .iter()
                .rposition(|step| step.status == "completed")
        })?;
    Some((&job.steps[position], position + 1))
}

/// The screen.
pub(crate) fn frame(state: &State, now: u64, paint: Paint) -> String {
    let status = &state.status;
    let mut out = String::new();
    let budget = &status.budget;
    let _ = write!(
        out,
        "{} {}  {}{} reserved",
        paint.paint(Tone::Bold, "runnerd"),
        status.version,
        bytes(budget.memory_in_use),
        budget
            .memory_max
            .map(|max| format!(" of {}", bytes(max)))
            .unwrap_or_default(),
    );
    if let Some(cpu) = budget.cpu {
        let _ = write!(out, "; {}", cpu.line());
    }
    if let Some(host) = &state.host {
        let _ = write!(
            out,
            "  {} uses {} cores, {}",
            budget.slice,
            cores(host.cpu),
            bytes(host.memory)
        );
    }
    if status.shutting_down {
        let _ = write!(out, "  {}", paint.paint(Tone::Busy, "shutting down"));
    }
    out.push_str("\n\n");
    out.push_str(&slots(state, now, paint));
    for profile in status.profiles.iter().filter(|profile| profile.reseeding) {
        let _ = writeln!(
            out,
            "{}",
            paint.paint(Tone::Busy, &format!("{}: reseeding", profile.name))
        );
    }
    out.push_str(&jobs(state, now, paint));
    let _ = writeln!(
        out,
        "\n{}",
        paint.paint(
            Tone::Dim,
            "CPU and MEM are the last sample, taken every 2 s. Ctrl-C quits."
        )
    );
    out
}

/// Every slot, with the job in it.
fn slots(state: &State, now: u64, paint: Paint) -> String {
    let mut slots = Table::new(&["SLOT", "JOB", "STEP", "RAN", "CPU", "MEM"]).right(&[3, 4, 5]);
    for profile in &state.status.profiles {
        for slot in &profile.slots {
            let name = format!("{} {}", profile.name, slot.slot);
            let Some(runner) = &slot.runner else {
                slots.row(vec![(name, Tone::Plain), ("free".to_owned(), Tone::Dim)]);
                continue;
            };
            let Some(job) = state.job_in(&profile.name, slot.slot) else {
                let what = if runner.busy {
                    format!("busy: {}", runner.repository)
                } else {
                    format!("waiting for a job of {}", runner.repository)
                };
                slots.row(vec![(name, Tone::Plain), (what, Tone::Dim)]);
                continue;
            };
            let summary = &job.summary;
            let step = current_step(job).map_or_else(String::new, |(step, position)| {
                format!("{} ({position}/{})", step.name, job.steps.len())
            });
            let ran = summary
                .timeline
                .started_at
                .map_or_else(String::new, |started| duration(now.saturating_sub(started)));
            let sample = state.samples.get(&summary.id);
            slots.row(vec![
                (name, Tone::Plain),
                (
                    format!("{} {}", summary.repository, human::title(summary)),
                    Tone::Busy,
                ),
                (step, Tone::Plain),
                (ran, Tone::Plain),
                (
                    sample.map_or_else(String::new, |s| cores(s.cpu)),
                    Tone::Plain,
                ),
                (
                    sample.map_or_else(String::new, |s| bytes(s.memory)),
                    Tone::Plain,
                ),
            ]);
        }
    }
    slots.render(paint, "")
}

/// Queued and finished jobs.
fn jobs(state: &State, now: u64, paint: Paint) -> String {
    let mut out = String::new();
    if !state.status.queued.is_empty() {
        let _ = writeln!(out, "\n{}", paint.paint(Tone::Bold, "Queued"));
        let mut queued = Table::default().right(&[2]);
        for job in &state.status.queued {
            queued.row(vec![
                (job.repository.clone(), Tone::Plain),
                (human::queued_title(job), Tone::Plain),
                (human::queued_ago(job, now), Tone::Dim),
            ]);
        }
        out.push_str(&queued.render(paint, "  "));
    }

    if !state.recent.is_empty() {
        let _ = writeln!(out, "\n{}", paint.paint(Tone::Bold, "Finished"));
        let mut recent = Table::default().right(&[3]);
        for job in &state.recent {
            let ran = match (job.timeline.started_at, job.timeline.finished_at) {
                (Some(started), Some(finished)) => duration(finished.saturating_sub(started)),
                _ => String::new(),
            };
            recent.row(vec![
                human::outcome(job),
                (job.repository.clone(), Tone::Plain),
                (human::title(job), Tone::Plain),
                (ran, Tone::Plain),
                (
                    job.timeline
                        .finished_at
                        .map_or_else(String::new, |at| human::ago(at, now)),
                    Tone::Dim,
                ),
            ]);
        }
        out.push_str(&recent.render(paint, "  "));
    }
    out
}

#[cfg(test)]
mod tests {
    use runnerd_core::api::{
        Budget, Event, EventEnvelope, JobMetrics, JobRunner, JobState, JobSummary, LogSummary,
        ProfileStatus, RunnerStatus, SlotStatus, Status, Step, Timeline,
    };

    use std::collections::BTreeMap;

    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State as Shared;
    use axum::routing::get;
    use futures_util::{StreamExt, stream};
    use runnerd_core::api::{EventQuery, JobPage};

    use super::{State, frame, run};
    use crate::human::Paint;
    use crate::testing::Fake;

    fn summary(state: JobState) -> JobSummary {
        JobSummary {
            id: 42,
            repository: "halyard/ledger".to_owned(),
            run_id: 7,
            run_attempt: 1,
            workflow: Some("build".to_owned()),
            name: "test".to_owned(),
            branch: Some("main".to_owned()),
            url: None,
            labels: vec!["self-hosted".to_owned()],
            state,
            conclusion: None,
            runner: Some(JobRunner {
                name: "runnerd-ab12-jvm-1".to_owned(),
                profile: "jvm".to_owned(),
                slot: 1,
                memory_cap: Some(4 << 30),
                cpu_cap: Some(2000),
            }),
            timeline: Timeline {
                queued_at: 1000,
                started_at: Some(1010),
                ..Timeline::default()
            },
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
        }
    }

    fn step(number: u32, name: &str, status: &str) -> Step {
        Step {
            number,
            name: name.to_owned(),
            status: status.to_owned(),
            conclusion: (status == "completed").then(|| "success".to_owned()),
            started_at: Some(1010),
            completed_at: (status == "completed").then_some(1020),
        }
    }

    fn state() -> State {
        State {
            status: Status {
                version: "0.0.1".to_owned(),
                shutting_down: false,
                budget: Budget {
                    admission: runnerd_core::api::AdmissionPolicy::Caps,
                    held: false,
                    slice: "runnerd.slice".to_owned(),
                    memory_max: Some(8 << 30),
                    memory_in_use: 0,
                    mirror_memory: 0,
                    seed_memory: Vec::new(),
                    shell_memory: Vec::new(),
                    cpu: None,
                },
                profiles: vec![ProfileStatus {
                    name: "jvm".to_owned(),
                    image: "runnerd-runner:jvm".to_owned(),
                    cpus: 2,
                    memory: 4 << 30,
                    reseeding: false,
                    slots: vec![SlotStatus {
                        slot: 1,
                        runner: None,
                    }],
                }],
                repositories: Vec::new(),
                queued: Vec::new(),
            },
            running: BTreeMap::default(),
            samples: BTreeMap::default(),
            host: None,
            recent: Vec::new(),
        }
    }

    fn event(event: Event) -> EventEnvelope {
        EventEnvelope {
            id: Some(1),
            at: 1030,
            event,
        }
    }

    fn screen(state: &State) -> String {
        frame(state, 1030, Paint { colour: false })
    }

    #[test]
    fn a_job_moves_from_its_slot_through_its_steps_to_the_finished_list() {
        let mut state = state();
        state.apply(&event(Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 1,
                runner: Some(RunnerStatus {
                    repository: "halyard/ledger".to_owned(),
                    name: "runnerd-ab12-jvm-1".to_owned(),
                    runner_id: 3,
                    container: "c0ffee".to_owned(),
                    busy: true,
                    started_at: 1005,
                    reserved: 9 << 30,
                }),
            },
        }));
        state.apply(&event(Event::JobStarted {
            job: summary(JobState::Running),
        }));
        state.apply(&event(Event::JobStep {
            job: 42,
            step: step(1, "Set up job", "completed"),
        }));
        state.apply(&event(Event::JobStep {
            job: 42,
            step: step(2, "Run tests", "in_progress"),
        }));
        let running = screen(&state);
        assert!(
            running.contains("jvm 1  halyard/ledger build / test  Run tests (2/2)  20s"),
            "{running}"
        );

        let mut finished = summary(JobState::Completed);
        finished.conclusion = Some("success".to_owned());
        finished.timeline.finished_at = Some(1025);
        state.apply(&event(Event::JobFinished { job: finished }));
        state.apply(&event(Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 1,
                runner: None,
            },
        }));
        let done = screen(&state);
        assert!(done.contains("jvm 1  free"), "{done}");
        assert!(
            done.contains("success  halyard/ledger  build / test  15s  5s ago"),
            "{done}"
        );
        assert!(state.running.is_empty());
    }

    #[test]
    fn queued_jobs_show_oldest_first_and_leave_the_queue_when_they_start() {
        let mut state = state();
        let queued = |id, name: &str, at| {
            let mut job = summary(JobState::Queued);
            job.id = id;
            job.name = name.to_owned();
            job.runner = None;
            job.timeline = Timeline {
                queued_at: at,
                ..Timeline::default()
            };
            job
        };
        let newer = queued(8, "lint", 1020);
        let older = queued(7, "test", 1000);
        state.apply(&event(Event::JobQueued { job: newer }));
        state.apply(&event(Event::JobQueued { job: older }));
        let ids: Vec<u64> = state.status.queued.iter().map(|job| job.id).collect();
        assert_eq!(ids, [7, 8], "oldest first");
        let shown = screen(&state);
        let rows: Vec<&str> = shown
            .lines()
            .skip_while(|line| *line != "Queued")
            .skip(1)
            .take(2)
            .collect();
        assert_eq!(
            rows,
            [
                "  halyard/ledger  build / test  30s ago",
                "  halyard/ledger  build / lint  10s ago"
            ],
            "{shown}"
        );

        let mut started = summary(JobState::Running);
        started.id = 7;
        state.apply(&event(Event::JobStarted { job: started }));
        let ids: Vec<u64> = state.status.queued.iter().map(|job| job.id).collect();
        assert_eq!(ids, [8], "the started job left the queue");
    }

    #[test]
    fn a_profile_a_reload_adds_shows_with_its_slots_and_one_it_removes_goes() {
        let mut state = state();
        let mut node = state.status.profiles[0].clone();
        node.name = "node".to_owned();
        state.apply(&event(Event::ProfileChanged {
            profile: node.clone(),
        }));
        state.apply(&event(Event::SlotChanged {
            profile: "node".to_owned(),
            slot: SlotStatus {
                slot: 1,
                runner: None,
            },
        }));
        assert!(screen(&state).contains("node"), "{}", screen(&state));
        state.apply(&event(Event::ProfileRemoved {
            profile: "jvm".to_owned(),
        }));
        assert_eq!(state.status.profiles, [node]);
    }

    #[test]
    fn a_job_started_before_the_daemon_knew_its_name_shows_its_id_and_is_read_again() {
        let mut state = state();
        let mut unnamed = summary(JobState::Running);
        unnamed.workflow = None;
        unnamed.name = String::new();
        state.apply(&event(Event::SlotChanged {
            profile: "jvm".to_owned(),
            slot: SlotStatus {
                slot: 1,
                runner: Some(RunnerStatus {
                    repository: "halyard/ledger".to_owned(),
                    name: "runnerd-ab12-jvm-1".to_owned(),
                    runner_id: 3,
                    container: "c0ffee".to_owned(),
                    busy: true,
                    started_at: 1005,
                    reserved: 9 << 30,
                }),
            },
        }));
        assert_eq!(
            state.apply(&event(Event::JobStarted { job: unnamed })),
            None
        );
        let reread = state.apply(&event(Event::JobStep {
            job: 42,
            step: step(1, "Set up job", "completed"),
        }));
        assert_eq!(reread, Some(42));
        assert!(
            screen(&state).contains("halyard/ledger job 42"),
            "{}",
            screen(&state)
        );

        state.refresh(runnerd_core::api::Job {
            admission: None,
            summary: summary(JobState::Running),
            steps: vec![step(1, "Set up job", "completed")],
            record: None,
        });
        assert!(
            screen(&state).contains("halyard/ledger build / test"),
            "{}",
            screen(&state)
        );
        assert_eq!(
            state.apply(&event(Event::JobStep {
                job: 42,
                step: step(2, "Run tests", "in_progress"),
            })),
            None
        );
    }

    /// The first event stream sends its headers and then nothing; the next sends a budget of 7 bytes in use.
    async fn stalling_then_live(Shared(streams): Shared<Arc<AtomicU32>>) -> Body {
        if streams.fetch_add(1, Ordering::Relaxed) == 0 {
            return Body::from_stream(stream::pending::<Result<Bytes, Infallible>>());
        }
        let envelope = EventEnvelope {
            id: Some(1),
            at: 1030,
            event: Event::BudgetChanged {
                budget: Budget {
                    admission: runnerd_core::api::AdmissionPolicy::Caps,
                    held: false,
                    slice: "runnerd.slice".to_owned(),
                    memory_max: None,
                    memory_in_use: 7,
                    mirror_memory: 0,
                    seed_memory: Vec::new(),
                    shell_memory: Vec::new(),
                    cpu: None,
                },
            },
        };
        let data = format!(
            "event: budget.changed\nid: 1\ndata: {}\n\n",
            serde_json::to_string(&envelope).unwrap()
        );
        Body::from_stream(
            stream::iter([Ok::<_, Infallible>(Bytes::from(data))]).chain(stream::pending()),
        )
    }

    #[tokio::test]
    async fn a_stalled_event_stream_is_reconnected_while_the_screen_keeps_redrawing() {
        let streams = Arc::new(AtomicU32::new(0));
        let fake = Fake::serve(
            Router::new()
                .route("/v1/status", get(|| async { axum::Json(state().status) }))
                .route(
                    "/v1/jobs",
                    get(|| async {
                        axum::Json(JobPage {
                            jobs: Vec::new(),
                            next: None,
                        })
                    }),
                )
                .route("/v1/events", get(stalling_then_live))
                .with_state(streams.clone()),
        );
        // Silent for longer than the one-second redraw, which must not keep restarting the wait.
        let client = fake.client(Duration::from_secs(5), Duration::from_millis(1500));
        let events = client.events(EventQuery::default(), None).await.unwrap();
        let watched = run(&client, events, |state| {
            state.status.budget.memory_in_use != 7
        });
        tokio::time::timeout(Duration::from_secs(8), watched)
            .await
            .expect("watch should reconnect the stalled stream")
            .unwrap();
        assert_eq!(streams.load(Ordering::Relaxed), 2);
    }

    /// The first event stream sends a live sample and ends; the one opened again at once stays open. The budget
    /// changed in between, and no event says so.
    async fn gap_events(Shared(streams): Shared<Arc<AtomicU32>>) -> Body {
        let sample = EventEnvelope {
            id: None,
            at: 1030,
            event: Event::HostSample {
                sample: runnerd_core::api::HostSample {
                    at: 1030,
                    cpu: 1,
                    memory: 1,
                },
            },
        };
        let data = Bytes::from(format!(
            "event: host.sample\ndata: {}\n\n",
            serde_json::to_string(&sample).unwrap()
        ));
        if streams.fetch_add(1, Ordering::Relaxed) == 0 {
            return Body::from(data);
        }
        Body::from_stream(stream::iter([Ok::<_, Infallible>(data)]).chain(stream::pending()))
    }

    #[tokio::test]
    async fn a_stream_reopened_with_no_event_to_resume_after_reads_the_state_again() {
        let streams = Arc::new(AtomicU32::new(0));
        let fake = Fake::serve(
            Router::new()
                .route(
                    "/v1/status",
                    get(|Shared(streams): Shared<Arc<AtomicU32>>| async move {
                        let mut status = state().status;
                        if streams.load(Ordering::Relaxed) > 1 {
                            status.budget.memory_in_use = 7;
                        }
                        axum::Json(status)
                    }),
                )
                .route(
                    "/v1/jobs",
                    get(|| async {
                        axum::Json(JobPage {
                            jobs: Vec::new(),
                            next: None,
                        })
                    }),
                )
                .route("/v1/events", get(gap_events))
                .with_state(streams.clone()),
        );
        let client = fake.client(Duration::from_secs(5), Duration::from_secs(5));
        let events = client.events(EventQuery::default(), None).await.unwrap();
        let watched = run(&client, events, |state| {
            state.status.budget.memory_in_use != 7
        });
        tokio::time::timeout(Duration::from_secs(5), watched)
            .await
            .expect("watch should read the state again after the gap")
            .unwrap();
        assert_eq!(streams.load(Ordering::Relaxed), 2);
    }
}
