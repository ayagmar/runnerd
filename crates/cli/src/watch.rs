//! `runnerctl watch`: the daemon's state kept current from `/v1/events`. A snapshot (status, queued and running
//! jobs, the last finished ones) read after the stream is open, then each event applied to it. Draws nothing
//! itself: `run` hands the state to a drawing function, and `frame` turns it into text for `output::redraw`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use runnerd_core::api::{
    Event, EventEnvelope, HostSample, Job, JobQuery, JobState, JobSummary, Sample, Status, Step,
};

use crate::client::{Client, ClientError, Events};
use crate::human::{self, Paint, Table, Tone, bytes, cores, duration};

/// Finished jobs kept on screen.
const RECENT: usize = 5;

/// What the screen shows.
#[derive(Debug)]
pub(crate) struct State {
    status: Status,
    queued: BTreeMap<u64, JobSummary>,
    running: BTreeMap<u64, Job>,
    samples: BTreeMap<u64, Sample>,
    host: Option<HostSample>,
    recent: Vec<JobSummary>,
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
        let queued: runnerd_core::api::JobPage = client
            .get_query("/v1/jobs", &page(Some(JobState::Queued)))
            .await?;
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
            queued: queued.jobs.into_iter().map(|job| (job.id, job)).collect(),
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
                self.queued.insert(job.id, job.clone());
            }
            Event::JobStarted { job } => {
                self.queued.remove(&job.id);
                let steps = self
                    .running
                    .remove(&job.id)
                    .map(|job| job.steps)
                    .unwrap_or_default();
                self.running.insert(
                    job.id,
                    Job {
                        summary: job.clone(),
                        steps,
                    },
                );
            }
            Event::JobStep { job: id, step } => {
                if let Some(job) = self.running.get_mut(id) {
                    if let Some(known) = job
                        .steps
                        .iter_mut()
                        .find(|known| known.number == step.number)
                    {
                        *known = step.clone();
                    } else {
                        job.steps.push(step.clone());
                        job.steps.sort_by_key(|step| step.number);
                    }
                    if job.summary.name.is_empty() {
                        return Some(*id);
                    }
                }
            }
            Event::JobFinished { job } => {
                self.queued.remove(&job.id);
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
            Event::BudgetChanged { budget } => self.status.budget = budget.clone(),
            Event::JobSample { job, sample } => {
                self.samples.insert(*job, sample.clone());
            }
            Event::HostSample { sample } => self.host = Some(sample.clone()),
            Event::CacheReseeded { .. } | Event::LogLine { .. } => {}
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
    fn job_in(&self, profile: &str, slot: usize) -> Option<&Job> {
        self.running.values().find(|job| {
            job.summary
                .runner
                .as_ref()
                .is_some_and(|runner| runner.profile == profile && runner.slot == slot)
        })
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
            (events, envelope) = &mut next => {
                next = Box::pin(next_event(events));
                if let Some(id) = state.apply(&envelope?) {
                    state.refresh(client.get(&format!("/v1/jobs/{id}")).await?);
                }
            }
            _ = tick.tick() => {}
        }
    }
}

/// The next event, handing the stream back for the read after it.
async fn next_event(mut events: Events) -> (Events, Result<EventEnvelope, ClientError>) {
    let envelope = events.next().await;
    (events, envelope)
}

/// The step a job is in: the one in progress, else the last one completed.
fn current_step(job: &Job) -> Option<(&Step, usize)> {
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
            "CPU and MEM are the last sample, taken every few seconds. Ctrl-C quits."
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
    if !state.queued.is_empty() {
        let _ = writeln!(out, "\n{}", paint.paint(Tone::Bold, "Queued"));
        let mut queued = Table::default().right(&[2]);
        for job in state.queued.values() {
            queued.row(vec![
                (job.repository.clone(), Tone::Plain),
                (human::title(job), Tone::Plain),
                (human::ago(job.timeline.queued_at, now), Tone::Dim),
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
                    slice: "runnerd.slice".to_owned(),
                    memory_max: Some(8 << 30),
                    memory_in_use: 0,
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
            },
            queued: BTreeMap::default(),
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
            summary: summary(JobState::Running),
            steps: vec![step(1, "Set up job", "completed")],
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
                    slice: "runnerd.slice".to_owned(),
                    memory_max: None,
                    memory_in_use: 7,
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
}
