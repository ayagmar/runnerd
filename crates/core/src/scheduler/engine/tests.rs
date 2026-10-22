//! Runner names, and the engine against fake GitHub and Docker: polling order, restart recovery, a Docker that
//! does not answer, which runners it deletes, and the job history it keeps across restarts.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::models::ContainerCreateBody;
use futures_util::StreamExt;

use super::{Engine, Handle, HostIdentity, Setup, Shutdown};
use crate::api::{self, JobState};
use crate::cache::Names;
use crate::config::Config;
use crate::docker::{self, Container, DockerApi, JitConfig, RunnerSpec, RunnerState};
use crate::github::{self, Deleted, GithubApi, JobDetails, JobStep, QueuedJob, Runner};
use crate::isolation::Isolation;
use crate::observe::metrics::Paths;
use crate::store::Store;
use crate::testdir::TestDir;

const ID_A: &str = "0123abcdef0123456789abcdef012345\n";
const ID_B: &str = "fedc9876543210fedcba9876543210fe\n";

fn identity(hostname: &str, machine_id: &str) -> HostIdentity {
    HostIdentity::new(hostname, machine_id).unwrap()
}

#[test]
fn no_machines_runner_prefix_starts_another_machines_runner_names() {
    // Host names where one is a prefix of the other, and two identical ones.
    for (a, b) in [("ci", "ci-2"), ("archlinux", "archlinux")] {
        let prefix = identity(a, ID_A).prefix();
        let name = identity(b, ID_B).runner_name("app", 1, 1_792_692_722);
        assert!(!name.starts_with(&prefix), "{prefix} matches {name}");
    }
}

#[test]
fn a_new_host_name_still_matches_the_runners_named_before() {
    let before = identity("laptop", ID_A).runner_name("app", 1, 1_792_692_722);
    let after = identity("laptop-dhcp-42", ID_A);
    assert!(before.starts_with(&after.prefix()), "{before}");
}

#[test]
fn runner_names_are_short_safe_and_hide_the_machine_id() {
    let name = identity("My.Very_Long-Hostname.example.org\n", ID_A).runner_name("app", 2, 7);
    let id = &name["runnerd-".len().."runnerd-".len() + 8];
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    assert!(!ID_A.contains(id), "the machine ID shows in {name}");
    assert_eq!(name, format!("runnerd-{id}-my-very-long-hostnam-app-2-7"));
    assert_eq!(
        identity("", ID_A).runner_name("app", 2, 7),
        format!("runnerd-{id}-host-app-2-7")
    );
    assert_eq!(HostIdentity::new("ci", "not-an-id"), None);
}

/// What the fake GitHub serves and what it was asked.
#[derive(Default)]
struct GithubState {
    queued: Vec<QueuedJob>,
    runners: Vec<Runner>,
    /// GitHub gives this queued job to this runner right after the first poll that sees them.
    assign: Option<(u64, u64)>,
    /// What `GET /actions/jobs/{id}` answers.
    jobs: HashMap<u64, JobDetails>,
    calls: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeGithub(Arc<Mutex<GithubState>>);

impl FakeGithub {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    fn polled(state: &mut GithubState) {
        if let Some((job, runner)) = state.assign.take() {
            state.queued.retain(|queued| queued.id != job);
            for held in &mut state.runners {
                if held.id == runner {
                    held.busy = true;
                }
            }
        }
    }
}

impl GithubApi for FakeGithub {
    async fn queued_jobs(&self, repository: &str) -> Result<Vec<QueuedJob>, github::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("queued {repository}"));
        let queued = state.queued.clone();
        FakeGithub::polled(&mut state);
        Ok(queued)
    }

    async fn runners(&self, repository: &str) -> Result<Vec<Runner>, github::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("runners {repository}"));
        let runners = state.runners.clone();
        FakeGithub::polled(&mut state);
        Ok(runners)
    }

    async fn jit_config(
        &self,
        _repository: &str,
        name: &str,
        _labels: &[String],
    ) -> Result<(Runner, JitConfig), github::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("register {name}"));
        let runner = Runner {
            id: 100 + state.runners.len() as u64,
            name: name.to_owned(),
            status: "offline".to_owned(),
            busy: false,
        };
        state.runners.push(runner.clone());
        Ok((runner, JitConfig("jit".to_owned())))
    }

    async fn delete_runner(&self, _repository: &str, id: u64) -> Result<Deleted, github::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("delete {id}"));
        state.runners.retain(|runner| runner.id != id);
        Ok(Deleted::Deleted)
    }

    async fn job(&self, _repository: &str, id: u64) -> Result<JobDetails, github::Error> {
        let state = self.0.lock().unwrap();
        state
            .jobs
            .get(&id)
            .cloned()
            .ok_or(github::Error::Token(format!("no job {id}")))
    }
}

/// What the fake Docker Engine runs and what it was asked.
#[derive(Default)]
struct DockerState {
    containers: Vec<Container>,
    /// Every `list` fails, as when the Engine does not answer.
    fail_lists: bool,
    /// What the log capture prints in a container, by the position it is started at.
    captures: HashMap<(String, String), String>,
    /// The capture commands run: container, page, offset.
    captured: Vec<(String, String, String)>,
    calls: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeDocker(Arc<Mutex<DockerState>>);

impl FakeDocker {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }
}

fn not_found() -> docker::Error {
    docker::Error::DockerResponseServerError {
        status_code: 404,
        message: "no such container".to_owned(),
    }
}

impl DockerApi for FakeDocker {
    async fn start(
        &self,
        spec: &RunnerSpec,
        _isolation: &dyn Isolation,
    ) -> Result<String, docker::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("start {}", spec.name));
        let mut labels: HashMap<String, String> = spec.labels.clone().into_iter().collect();
        labels.insert("runnerd.profile".to_owned(), spec.profile.clone());
        state.containers.push(Container {
            id: spec.name.clone(),
            name: spec.name.clone(),
            running: true,
            labels,
        });
        Ok(spec.name.clone())
    }

    async fn inspect(&self, container: &str) -> Result<RunnerState, docker::Error> {
        let state = self.0.lock().unwrap();
        let found = state
            .containers
            .iter()
            .find(|candidate| candidate.id == container || candidate.name == container)
            .ok_or_else(not_found)?;
        Ok(RunnerState {
            running: found.running,
            exit_code: None,
            privileged: false,
            runtime: None,
            pid: Some(4242),
            address: Some("172.18.0.3".to_owned()),
            started_at: Some(1_000),
        })
    }

    async fn remove(&self, container: &str) -> Result<(), docker::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("remove {container}"));
        state
            .containers
            .retain(|candidate| candidate.id != container && candidate.name != container);
        Ok(())
    }

    async fn run(&self, name: &str, _body: ContainerCreateBody) -> Result<String, docker::Error> {
        Ok(name.to_owned())
    }

    async fn list(&self, label: &str) -> Result<Vec<Container>, docker::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("list {label}"));
        if state.fail_lists {
            return Err(docker::Error::DockerResponseServerError {
                status_code: 500,
                message: "the Engine is starting".to_owned(),
            });
        }
        Ok(state
            .containers
            .iter()
            .filter(|container| container.labels.contains_key(label))
            .cloned()
            .collect())
    }

    async fn wait(&self, _container: &str) -> Result<i64, docker::Error> {
        // Seed containers run for the whole test.
        std::future::pending().await
    }

    async fn logs(&self, _container: &str) -> Result<String, docker::Error> {
        Ok("Running job: build".to_owned())
    }

    async fn logs_since(&self, _container: &str, _since: u64) -> Result<String, docker::Error> {
        Ok(String::new())
    }

    async fn exec(
        &self,
        container: &str,
        command: Vec<String>,
    ) -> Result<docker::Output, docker::Error> {
        let mut state = self.0.lock().unwrap();
        let (page, offset) = (command[4].clone(), command[5].clone());
        state
            .captured
            .push((container.to_owned(), page.clone(), offset.clone()));
        let output = state
            .captures
            .get(&(page, offset))
            .cloned()
            .unwrap_or_default();
        Ok(futures_util::stream::iter([Ok(output.into_bytes())]).boxed())
    }

    async fn ensure_image(&self, _image: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn ensure_network(&self, _name: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn ensure_volume(&self, _name: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn remove_volume(&self, _name: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn volume_sizes(&self) -> Result<BTreeMap<String, u64>, docker::Error> {
        Ok(BTreeMap::new())
    }
}

const CONFIG: &str = r#"
[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = { enabled = false, memory = "512M", cpus = 1, ttl = "168h" }
pnpm = { max_size = "5G" }
maven = { max_size = "5G" }

[profiles.app]
image_stage = "app"
cpus = 2
memory = "4G"
slots = 2

[[repositories]]
name = "o/r"
profile = "app"
labels = ["self-hosted", "app"]
"#;

/// An engine on the fakes, with its history in `dir`.
fn engine_in(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    Engine::new(
        Config::parse(CONFIG).unwrap(),
        github.clone(),
        docker.clone(),
        Setup {
            names: Names::new("runnerd"),
            identity: identity("ci", ID_A),
            memory_max: None,
            store: Store::open(&dir.0.join("history.db")).unwrap(),
            paths: Paths {
                cgroup: dir.0.join("cgroup"),
                proc: dir.0.join("proc"),
            },
        },
    )
}

fn engine(
    github: &FakeGithub,
    docker: &FakeDocker,
) -> (Engine<FakeGithub, FakeDocker>, Handle, TestDir) {
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_in(github, docker, &dir);
    (engine, handle, dir)
}

fn job(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: 1,
        name: "build".to_owned(),
        labels: vec!["self-hosted".to_owned(), "app".to_owned()],
        ..QueuedJob::default()
    }
}

fn runner(id: u64, name: &str, busy: bool) -> Runner {
    Runner {
        id,
        name: name.to_owned(),
        status: "online".to_owned(),
        busy,
    }
}

/// A runner container of this machine in slot `slot` of `app`, as `start` labels it.
fn runner_container(slot: usize, runner_id: u64, running: bool) -> Container {
    let name = identity("ci", ID_A).runner_name("app", slot, 1);
    Container {
        id: format!("container-{slot}"),
        name: name.clone(),
        running,
        labels: HashMap::from([
            ("runnerd.job".to_owned(), name),
            ("runnerd.profile".to_owned(), "app".to_owned()),
            ("runnerd.repository".to_owned(), "o/r".to_owned()),
            ("runnerd.slot".to_owned(), slot.to_string()),
            ("runnerd.runner-id".to_owned(), runner_id.to_string()),
        ]),
    }
}

fn registered(calls: &[String]) -> Vec<&String> {
    calls
        .iter()
        .filter(|call| call.starts_with("register "))
        .collect()
}

#[tokio::test]
async fn a_runner_that_takes_its_job_between_polls_gets_no_twin() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().containers = vec![runner_container(1, 7, true)];
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.runners = vec![runner(
            7,
            &identity("ci", ID_A).runner_name("app", 1, 1),
            false,
        )];
        state.assign = Some((1, 7));
    }
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_restart_adopts_running_runners_and_reseeds_and_removes_stopped_runners() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let seed = Container {
        id: "seed".to_owned(),
        name: Names::new("runnerd").seed("app"),
        running: true,
        labels: HashMap::from([
            ("runnerd.seed".to_owned(), String::new()),
            ("runnerd.profile".to_owned(), "app".to_owned()),
        ]),
    };
    docker.0.lock().unwrap().containers = vec![
        runner_container(1, 7, true),
        runner_container(2, 8, false),
        seed,
    ];
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.runners = vec![runner(
            7,
            &identity("ci", ID_A).runner_name("app", 1, 1),
            true,
        )];
    }
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.publish();
    {
        let status = handle.status.borrow();
        let profile = &status.profiles[0];
        assert!(profile.reseeding);
        let adopted = profile.slots[0].runner.as_ref().unwrap();
        assert_eq!((adopted.runner_id, adopted.busy), (7, true));
        assert_eq!(profile.slots[1].runner, None);
    }
    assert!(docker.calls().contains(&"remove container-2".to_owned()));
    // The reseed still runs, so slot 2 starts nothing for the queued job.
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn nothing_is_scheduled_while_docker_cannot_list_containers() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().fail_lists = true;
    github.0.lock().unwrap().queued = vec![job(1)];
    let (engine, handle, _dir) = engine(&github, &docker);
    let run = tokio::spawn(engine.run());
    for _ in 0..100 {
        if !docker.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(docker.calls(), ["list runnerd.seed"]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(github.calls(), Vec::<String>::new());
    handle.control.shutdown(false);
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(github.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn only_this_machines_runners_without_a_container_are_deleted() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().containers = vec![runner_container(1, 1, true)];
    github.0.lock().unwrap().runners = vec![
        runner(1, &identity("ci", ID_A).runner_name("app", 1, 1), true),
        runner(2, &identity("ci", ID_A).runner_name("app", 2, 1), false),
        runner(3, &identity("ci", ID_B).runner_name("app", 1, 1), false),
        runner(4, "my-laptop", false),
    ];
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let deleted: Vec<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("delete "))
        .collect();
    assert_eq!(deleted, ["delete 2"]);
}

fn details(id: u64, status: &str, conclusion: Option<&str>) -> JobDetails {
    let step = |number: u32, name: &str| JobStep {
        number,
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("success".to_owned()),
        started_at: Some("2026-10-22T19:27:25Z".to_owned()),
        completed_at: Some("2026-10-22T19:27:27Z".to_owned()),
    };
    JobDetails {
        id,
        run_id: 1,
        run_attempt: 1,
        name: "build".to_owned(),
        workflow_name: Some("ci".to_owned()),
        head_branch: Some("main".to_owned()),
        html_url: None,
        labels: vec!["self-hosted".to_owned(), "app".to_owned()],
        created_at: Some("2026-10-22T19:27:05Z".to_owned()),
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        started_at: Some("2026-10-22T19:27:24Z".to_owned()),
        completed_at: (status == "completed").then(|| "2026-10-22T19:28:30Z".to_owned()),
        runner_name: Some(identity("ci", ID_A).runner_name("app", 1, 1)),
        steps: vec![step(1, "Set up job"), step(2, "Run make")],
    }
}

/// A running runner container in slot 1 whose log capture, started at page 1 offset 0, prints `output`.
fn running_runner(github: &FakeGithub, docker: &FakeDocker, output: &str) {
    let mut state = docker.0.lock().unwrap();
    state.containers = vec![runner_container(1, 7, true)];
    state
        .captures
        .insert(("1".to_owned(), "0".to_owned()), output.to_owned());
    github.0.lock().unwrap().runners = vec![runner(
        7,
        &identity("ci", ID_A).runner_name("app", 1, 1),
        true,
    )];
}

/// Waits until the history shows the job in `state`.
async fn until_state(handle: &Handle, job: u64, state: JobState) -> api::Job {
    for _ in 0..200 {
        handle.store.flush().await;
        if let Some(found) = handle.store.job(job).await.unwrap()
            && found.summary.state == state
        {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job} never got to {state:?}");
}

#[tokio::test]
async fn a_job_github_still_lists_queued_after_its_runner_took_it_gets_no_twin() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    // GitHub reports the runner busy, but its listing of queued jobs still has the job it took.
    running_runner(&github, &docker, "@@job 1 1\n");
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_job_whose_capture_never_reported_it_leaves_the_queue_once_github_completes_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    // The capture prints nothing, as when the runner's `_diag` layout changed.
    running_runner(&github, &docker, "");
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    github.0.lock().unwrap().queued.clear();
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    assert_eq!(
        handle.store.job(1).await.unwrap().unwrap().summary.state,
        JobState::Queued
    );
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("success")));
    engine.tick(Shutdown::No).await;
    until_state(&handle, 1, JobState::Elsewhere).await;
}

#[tokio::test]
async fn a_finished_job_keeps_its_timeline_steps_and_log_across_a_restart() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_runner(&github, &docker, "@@job 1 1\n@@page 1 0 6\nhello\n");
    docker.0.lock().unwrap().captures.insert(
        ("1".to_owned(), "6".to_owned()),
        "@@job 1 1\n@@page 1 6 6\nworld\n@@end 1\n".to_owned(),
    );
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let dir = TestDir::new("restart");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    let mut events = handle.store.subscribe();
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    drop((engine, handle));

    // The daemon restarts mid-job: the capture resumes after the stored line, then the job ends.
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    for _ in 0..100 {
        handle.store.flush().await;
        if handle
            .store
            .job(1)
            .await
            .unwrap()
            .unwrap()
            .summary
            .log
            .lines
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("success")));
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 1, JobState::Completed).await;
    drop((engine, handle));

    let store = Store::open(&dir.0.join("history.db")).unwrap();
    assert_eq!(store.job(1).await.unwrap().unwrap(), job);
    assert_eq!(job.summary.conclusion.as_deref(), Some("success"));
    let timeline = &job.summary.timeline;
    assert_eq!(timeline.started_at, Some(1_792_697_244));
    assert_eq!(timeline.finished_at, Some(1_792_697_310));
    assert!(timeline.cleaned_at.is_some());
    let steps: Vec<(&str, Option<u64>)> = job
        .steps
        .iter()
        .map(|step| (step.name.as_str(), step.completed_at))
        .collect();
    assert_eq!(
        steps,
        [
            ("Set up job", Some(1_792_697_247)),
            ("Run make", Some(1_792_697_247))
        ]
    );
    let log: Vec<String> = store
        .log_lines(1, 0, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(log, ["hello", "world"]);
    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event.id.is_some() && event.event.job() == Some(1) {
            kinds.push(event.event.kind());
        }
    }
    assert_eq!(kinds, ["job.started", "job.step", "job.step"]);
    let stored: Vec<&str> = store
        .events_after(0, 100)
        .await
        .unwrap()
        .iter()
        .filter(|event| event.event.job() == Some(1))
        .map(|event| event.event.kind())
        .collect();
    assert_eq!(
        stored,
        ["job.started", "job.step", "job.step", "job.finished"]
    );
}

#[tokio::test]
async fn a_job_whose_container_vanished_closes_once_github_reports_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_runner(&github, &docker, "@@job 1 1\n");
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    // Someone removes the container mid-job.
    docker.0.lock().unwrap().containers.clear();
    engine.tick(Shutdown::No).await;
    engine.publish();
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Running);
    assert!(job.summary.timeline.cleaned_at.is_some());
    assert_eq!(handle.status.borrow().profiles[0].slots[0].runner, None);
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("failure")));
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 1, JobState::Completed).await;
    assert_eq!(job.summary.conclusion.as_deref(), Some("failure"));
}

/// Runs job 1 on a runner in slot 1 until the history has it running, then stops that daemon.
async fn run_until_restart(github: &FakeGithub, docker: &FakeDocker, dir: &TestDir) {
    running_runner(github, docker, "@@job 1 1\n");
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    let (mut engine, handle) = engine_in(github, docker, dir);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
}

#[tokio::test]
async fn a_job_whose_container_vanished_while_the_daemon_was_down_closes_after_the_restart() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("restart");
    run_until_restart(&github, &docker, &dir).await;
    docker.0.lock().unwrap().containers.clear();
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "completed", Some("success")));
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let job = until_state(&handle, 1, JobState::Completed).await;
    assert!(job.summary.timeline.cleaned_at.is_some());
}

#[tokio::test]
async fn an_adopted_job_that_runs_long_after_a_restart_is_not_lost() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let dir = TestDir::new("restart");
    run_until_restart(&github, &docker, &dir).await;
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    tokio::time::pause();
    tokio::time::advance(super::LOST_AFTER + Duration::from_secs(60)).await;
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Running);
}
