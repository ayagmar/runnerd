//! Runner names, and the engine against fake GitHub and Docker: polling order, restart recovery, a Docker that
//! does not answer, which runners it deletes, the job history it keeps across restarts, and the API's commands:
//! pause of one repository or all, forced or not and across restarts, reseed, cache clean and reload.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::models::ContainerCreateBody;
use futures_util::StreamExt;

use super::{
    Command, Commit, ControlError, Engine, Handle, HostIdentity, RunningJobs, Setup, Shutdown,
    Target,
};
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
    /// The repository a runner is registered on, where a test says; others show under every repository.
    registered_on: HashMap<u64, String>,
    /// How many deletes GitHub refuses next because the runner is busy.
    busy_deletes: u32,
    /// What `GET /actions/jobs/{id}` answers.
    jobs: HashMap<u64, JobDetails>,
    calls: Vec<String>,
    /// While true, the runners and queued jobs listings do not answer, like a GitHub that hangs.
    held: tokio::sync::watch::Sender<bool>,
    /// While true, `GET /actions/jobs/{id}` does not answer.
    held_jobs: tokio::sync::watch::Sender<bool>,
    /// Listings and job reads waiting for an answer.
    waiting: usize,
}

#[derive(Clone, Default)]
struct FakeGithub(Arc<Mutex<GithubState>>);

impl FakeGithub {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().calls.clone()
    }

    /// Holds the runners and queued jobs listings until `release`.
    fn hold(&self) {
        self.0.lock().unwrap().held.send_replace(true);
    }

    fn release(&self) {
        self.0.lock().unwrap().held.send_replace(false);
    }

    /// Holds job reads until `release_jobs`.
    fn hold_jobs(&self) {
        self.0.lock().unwrap().held_jobs.send_replace(true);
    }

    fn release_jobs(&self) {
        self.0.lock().unwrap().held_jobs.send_replace(false);
    }

    /// Waits while the listings are held.
    async fn answer(&self) {
        self.wait(|state| &state.held).await;
    }

    async fn wait(&self, held: fn(&GithubState) -> &tokio::sync::watch::Sender<bool>) {
        let mut held = {
            let mut state = self.0.lock().unwrap();
            state.waiting += 1;
            held(&state).subscribe()
        };
        let _ = held.wait_for(|held| !held).await;
        self.0.lock().unwrap().waiting -= 1;
    }

    /// Waits until `count` listings wait for an answer at once.
    async fn until_waiting(&self, count: usize) {
        for _ in 0..500 {
            if self.0.lock().unwrap().waiting >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("fewer than {count} listings ever waited at once");
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
        self.answer().await;
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("queued {repository}"));
        let queued = state.queued.clone();
        FakeGithub::polled(&mut state);
        Ok(queued)
    }

    async fn runners(&self, repository: &str) -> Result<Vec<Runner>, github::Error> {
        self.answer().await;
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("runners {repository}"));
        let runners = state
            .runners
            .iter()
            .filter(|runner| {
                state
                    .registered_on
                    .get(&runner.id)
                    .is_none_or(|on| on == repository)
            })
            .cloned()
            .collect();
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
        if state.busy_deletes > 0 {
            state.busy_deletes -= 1;
            return Ok(Deleted::Busy);
        }
        state.runners.retain(|runner| runner.id != id);
        Ok(Deleted::Deleted)
    }

    async fn job(&self, _repository: &str, id: u64) -> Result<JobDetails, github::Error> {
        self.wait(|state| &state.held_jobs).await;
        let state = self.0.lock().unwrap();
        state
            .jobs
            .get(&id)
            .cloned()
            .ok_or(github::Error::Token(format!("no job {id}")))
    }

    async fn cancel_run(&self, repository: &str, run: u64) -> Result<(), github::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("cancel {repository} {run}"));
        Ok(())
    }
}

/// What the fake Docker Engine runs and what it was asked.
#[derive(Default)]
struct DockerState {
    containers: Vec<Container>,
    /// Every `list` fails, as when the Engine does not answer.
    fail_lists: bool,
    /// The memory cap of each container that has one, by ID.
    memory: HashMap<String, u64>,
    /// The CPU limit in millicores of each container that has one, by ID.
    cpus: HashMap<String, u64>,
    /// How many `inspect`s of a container fail next, by ID, as when the Engine is busy.
    fail_inspects: HashMap<String, u32>,
    /// How many `remove`s fail next, as when the Engine is busy.
    fail_removes: u32,
    /// What the Docker Hub mirror's log says.
    mirror_log: String,
    /// What the mirror logs while a runner container is removed, as when the runner's last pulls land just then.
    mirror_log_on_remove: String,
    /// What `volume_sizes` measures, by volume name.
    volume_sizes: BTreeMap<String, u64>,
    /// What the log capture prints in a container, by the position it is started at.
    captures: HashMap<(String, String), String>,
    /// What the log capture prints in one container, whatever the position, before `captures`.
    container_captures: HashMap<String, String>,
    /// The capture commands run: container, page, offset.
    captured: Vec<(String, String, String)>,
    /// Images Docker does not have; every other image is present.
    missing_images: BTreeSet<String>,
    /// Every pull fails, as when the registry does not answer.
    fail_pulls: bool,
    /// Pulls do not finish while this is set, as when a large image downloads.
    block_pulls: bool,
    /// The image of each runner container started, in order.
    started_images: Vec<String>,
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
        if state.missing_images.contains(&spec.image) {
            return Err(docker::Error::DockerResponseServerError {
                status_code: 404,
                message: format!("No such image: {}", spec.image),
            });
        }
        state.started_images.push(spec.image.clone());
        state.memory.insert(spec.name.clone(), spec.memory.0);
        state
            .cpus
            .insert(spec.name.clone(), u64::from(spec.cpus) * 1000);
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
        let mut state = self.0.lock().unwrap();
        if let Some(failures) = state.fail_inspects.get_mut(container)
            && *failures > 0
        {
            *failures -= 1;
            return Err(docker::Error::DockerResponseServerError {
                status_code: 500,
                message: "the Engine is busy".to_owned(),
            });
        }
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
            memory: state.memory.get(&found.id).copied(),
            cpus: state.cpus.get(&found.id).copied(),
        })
    }

    async fn remove(&self, container: &str) -> Result<(), docker::Error> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("remove {container}"));
        if state.fail_removes > 0 {
            state.fail_removes -= 1;
            return Err(docker::Error::DockerResponseServerError {
                status_code: 500,
                message: "the Engine is busy".to_owned(),
            });
        }
        if container.starts_with("container-") {
            let late = std::mem::take(&mut state.mirror_log_on_remove);
            state.mirror_log.push_str(&late);
        }
        state
            .containers
            .retain(|candidate| candidate.id != container && candidate.name != container);
        Ok(())
    }

    async fn run(&self, name: &str, body: ContainerCreateBody) -> Result<String, docker::Error> {
        let mut state = self.0.lock().unwrap();
        let memory = body.host_config.and_then(|host| host.memory).unwrap_or(0);
        state
            .memory
            .insert(name.to_owned(), u64::try_from(memory).unwrap_or(0));
        state.containers.push(Container {
            id: name.to_owned(),
            name: name.to_owned(),
            running: true,
            labels: HashMap::from([("runnerd.mirror".to_owned(), String::new())]),
        });
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
        Ok(self.0.lock().unwrap().mirror_log.clone())
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
            .container_captures
            .get(container)
            .or_else(|| state.captures.get(&(page, offset)))
            .cloned()
            .unwrap_or_default();
        Ok(futures_util::stream::iter([Ok(output.into_bytes())]).boxed())
    }

    async fn image_present(&self, image: &str) -> Result<bool, docker::Error> {
        Ok(!self.0.lock().unwrap().missing_images.contains(image))
    }

    async fn pull(&self, image: &str) -> Result<(), docker::Error> {
        self.0.lock().unwrap().calls.push(format!("pull {image}"));
        while self.0.lock().unwrap().block_pulls {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut state = self.0.lock().unwrap();
        if state.fail_pulls {
            return Err(docker::Error::DockerResponseServerError {
                status_code: 500,
                message: "registry unreachable".to_owned(),
            });
        }
        state.missing_images.remove(image);
        Ok(())
    }

    async fn ensure_network(&self, _name: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn ensure_volume(&self, _name: &str) -> Result<(), docker::Error> {
        Ok(())
    }

    async fn remove_volume(&self, name: &str) -> Result<(), docker::Error> {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(format!("remove volume {name}"));
        Ok(())
    }

    async fn volume_sizes(&self) -> Result<BTreeMap<String, u64>, docker::Error> {
        Ok(self.0.lock().unwrap().volume_sizes.clone())
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

/// A pause or resume of one repository.
fn one(repository: &str) -> Target {
    Target::Repository(repository.to_owned())
}

/// An engine on the fakes, with its history in `dir`.
fn engine_in(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    engine_with(github, docker, dir, CONFIG)
}

fn engine_with(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let store = Store::open(&dir.0.join("history.db")).unwrap();
    Engine::new(
        Config::parse(config).unwrap(),
        github.clone(),
        docker.clone(),
        Setup {
            names: Names::new("runnerd"),
            identity: identity("ci", ID_A),
            memory_max: None,
            announced: store.announced_repositories().unwrap(),
            paused: store.paused_repositories().unwrap(),
            store,
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
    handle.control.shutdown(RunningJobs::Keep);
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

/// Waits until the history holds `lines` lines of the job's log.
async fn until_log_lines(handle: &Handle, job: u64, lines: u64) {
    for _ in 0..200 {
        handle.store.flush().await;
        if let Some(found) = handle.store.job(job).await.unwrap()
            && found.summary.log.lines == lines
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job} never stored {lines} log lines");
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
    until_log_lines(&handle, 1, 1).await;
    engine.tick(Shutdown::No).await;
    // A restarted daemon writes nothing more, but a dropped engine's capture and writes go on: let them finish.
    handle.store.flush().await;
    drop((engine, handle));

    // The daemon restarts mid-job: the capture resumes after the stored line, then the job ends.
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    until_log_lines(&handle, 1, 2).await;
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
    tokio::time::advance(super::history::LOST_AFTER + Duration::from_secs(60)).await;
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.state, JobState::Running);
}

#[tokio::test]
async fn mirror_bytes_the_history_failed_to_store_are_counted_on_the_next_tick() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "@@job 1 1\n");
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    docker.0.lock().unwrap().mirror_log = "2026-10-22T19:27:25.869343464Z 172.18.0.3 - - \
        [22/Oct/2026:19:27:25 +0000] \"GET /v2/library/alpine/blobs/sha256:aa HTTP/1.1\" 200 577 \"\" \"docker/29.9.2\"\n"
        .to_owned();
    let dir = TestDir::new("mirror");
    let config = CONFIG.replace("mirror = { enabled = false", "mirror = { enabled = true");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    // Another process holds the history's runner rows: the count fails.
    let fault = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_bytes BEFORE UPDATE OF mirror_bytes ON runners \
             BEGIN SELECT RAISE(ABORT, 'busy'); END;",
        )
        .unwrap();
    engine.tick(Shutdown::No).await;
    fault.execute_batch("DROP TRIGGER no_bytes").unwrap();
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::No).await;
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.metrics.mirror_bytes, 577);
}

/// `CONFIG` with a second repository, `o/s`, on the same profile.
fn two_repositories() -> String {
    format!(
        "{CONFIG}\n[[repositories]]\nname = \"o/s\"\nprofile = \"app\"\nlabels = [\"self-hosted\", \"app\"]\n"
    )
}

/// A running runner container of `repository` in slot `slot`, whose capture reports job `job` of run `job`.
fn running_job(github: &FakeGithub, docker: &FakeDocker, repository: &str, slot: usize, job: u64) {
    let mut container = runner_container(slot, 6 + slot as u64, true);
    container
        .labels
        .insert("runnerd.repository".to_owned(), repository.to_owned());
    let mut state = docker.0.lock().unwrap();
    state
        .container_captures
        .insert(container.id.clone(), format!("@@job {job} {job}\n"));
    state.containers.push(container);
    let mut github = github.0.lock().unwrap();
    github.runners.push(runner(
        6 + slot as u64,
        &identity("ci", ID_A).runner_name("app", slot, 1),
        true,
    ));
    let mut details = details(job, "in_progress", None);
    details.run_id = job;
    github.jobs.insert(job, details);
}

/// The runner containers removed.
fn removed(calls: &[String]) -> Vec<&String> {
    calls
        .iter()
        .filter(|call| call.starts_with("remove container-"))
        .collect()
}

fn runner_in(handle: &Handle, slot: usize) -> Option<String> {
    let status = handle.status.borrow();
    let profile = status
        .profiles
        .iter()
        .find(|profile| profile.name == "app")
        .unwrap();
    profile
        .slots
        .iter()
        .find(|candidate| candidate.slot == slot)
        .and_then(|candidate| candidate.runner.as_ref())
        .map(|runner| runner.repository.clone())
}

#[tokio::test]
async fn a_pause_is_refused_while_the_repository_runs_a_job_and_forced_cancels_only_its_job() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    running_job(&github, &docker, "o/s", 2, 2);
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    until_state(&handle, 2, JobState::Running).await;
    engine.tick(Shutdown::No).await;

    let Err(ControlError::Busy { jobs, .. }) = engine.pause(&one("o/r"), RunningJobs::Keep).await
    else {
        panic!("a pause while o/r runs a job went through");
    };
    assert_eq!(
        jobs.iter().map(|job| job.job).collect::<Vec<_>>(),
        [Some(1)]
    );
    assert!(!handle.control.is_paused("o/r"));

    let paused = engine.pause(&one("o/r"), RunningJobs::Kill).await.unwrap();
    assert_eq!(
        paused.stopped.iter().map(|job| job.job).collect::<Vec<_>>(),
        [Some(1)]
    );
    assert!(handle.control.is_paused("o/r"));
    assert!(!handle.control.is_paused("o/s"));
    let cancelled: Vec<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("cancel "))
        .collect();
    assert_eq!(cancelled, ["cancel o/r 1"]);
    // Its runner reports the job cancelled and exits; then it is removed like any other.
    assert_eq!(removed(&docker.calls()), Vec::<&String>::new());
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
    engine.publish();
    assert_eq!(runner_in(&handle, 1), None);
    assert_eq!(runner_in(&handle, 2).as_deref(), Some("o/s"));
}

#[tokio::test]
async fn a_paused_repository_gets_no_runner_after_a_restart_until_it_is_resumed() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    drop(engine);

    github.0.lock().unwrap().queued = vec![job(1)];
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    assert!(handle.control.is_paused("o/r"), "o/r is still paused");
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());

    handle.control.set_paused(&one("o/r"), false).await.unwrap();
    drop(engine);
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    assert!(!handle.control.is_paused("o/r"), "o/r stays resumed");
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 1);
}

#[tokio::test]
async fn a_resumed_repository_shows_active_in_the_status_once_the_resume_answers() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_in(&github, &docker, &dir);
    tokio::spawn(engine.run());
    until_polled(&github, 1).await;
    handle
        .control
        .pause(Target::All, RunningJobs::Keep)
        .await
        .unwrap();
    assert!(
        handle.status.borrow().repositories[0].paused,
        "o/r shows paused"
    );

    // The tick the resume wakes waits on GitHub; the status must not.
    github.hold();
    handle.control.resume(Target::All).await.unwrap();
    assert!(
        !handle.status.borrow().repositories[0].paused,
        "o/r shows active"
    );
    let mut after = 0;
    let events = repository_events(&handle, &mut after).await;
    assert_eq!(
        events.last(),
        Some(&api::Event::RepositoryChanged {
            repository: repository_status(false, 0)
        })
    );
    github.release();
}

#[tokio::test]
async fn a_pause_answers_and_shows_while_a_tick_waits_for_github() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_in(&github, &docker, &dir);
    github.hold();
    tokio::spawn(engine.run());
    github.until_waiting(1).await;

    let answered = Duration::from_secs(5);
    tokio::time::timeout(
        answered,
        handle.control.pause(one("o/r"), RunningJobs::Keep),
    )
    .await
    .expect("the pause answers")
    .unwrap();
    assert!(
        handle.status.borrow().repositories[0].paused,
        "o/r shows paused"
    );
    tokio::time::timeout(answered, handle.control.resume(one("o/r")))
        .await
        .expect("the resume answers")
        .unwrap();
    assert!(
        !handle.status.borrow().repositories[0].paused,
        "o/r shows active"
    );
    assert_eq!(
        github.calls(),
        Vec::<String>::new(),
        "the tick is still waiting"
    );

    github.release();
    until_polled(&github, 1).await;
}

#[tokio::test]
async fn a_resume_answers_while_a_tick_reads_a_running_jobs_progress() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    github.hold_jobs();
    let control = handle.control.clone();
    tokio::join!(engine.tick(Shutdown::No), async {
        github.until_waiting(1).await;
        let answered =
            tokio::time::timeout(Duration::from_secs(5), control.resume(one("o/r"))).await;
        github.release_jobs();
        answered.expect("the resume answers").unwrap();
    });
}

#[tokio::test]
async fn a_listing_finishes_while_a_long_command_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_in(&github, &docker, &dir);
    github.hold();
    tokio::spawn(engine.run());
    github.until_waiting(1).await;

    // A config whose file takes until `written` to write.
    let (written, writing) = std::sync::mpsc::channel::<()>();
    let (started, writes) = tokio::sync::oneshot::channel::<()>();
    let control = handle.control.clone();
    let replaced = tokio::spawn(async move {
        let commit = Commit::new(move || {
            let _ = started.send(());
            let _ = writing.recv();
            Ok(None)
        });
        control
            .replace_config(other_repository(), None, commit)
            .await
    });
    writes.await.unwrap();
    github.release();
    for _ in 0..500 {
        if github
            .calls()
            .iter()
            .any(|call| call.starts_with("runners "))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listed = github
        .calls()
        .iter()
        .any(|call| call.starts_with("runners "));
    written.send(()).unwrap();
    replaced.await.unwrap().unwrap();
    assert!(
        listed,
        "the runners listing finished while the config was written"
    );
}

#[tokio::test]
async fn a_tick_lists_every_repository_at_once() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let config = format!(
        "{}\n[[repositories]]\nname = \"o/t\"\nprofile = \"app\"\nlabels = [\"self-hosted\", \"app\"]\n",
        two_repositories()
    );
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    github.hold();
    tokio::join!(engine.tick(Shutdown::No), async {
        github.until_waiting(3).await;
        github.release();
    });
    let calls = github.calls();
    assert_eq!(
        calls[..3]
            .iter()
            .filter(|call| call.starts_with("runners "))
            .count(),
        3,
        "{calls:?}"
    );
}

#[tokio::test]
async fn pausing_every_repository_is_refused_while_one_runs_a_job_and_forced_cancels_its_job() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;

    let Err(ControlError::Busy { jobs, message }) =
        engine.pause(&Target::All, RunningJobs::Keep).await
    else {
        panic!("a pause of every repository while o/r runs a job went through");
    };
    assert_eq!(
        jobs.iter().map(|job| job.job).collect::<Vec<_>>(),
        [Some(1)]
    );
    assert!(message.contains("o/r runs job 1"), "{message}");
    assert!(
        !handle.control.is_paused("o/r"),
        "a refused pause pauses none"
    );
    assert!(
        !handle.control.is_paused("o/s"),
        "a refused pause pauses none"
    );

    let paused = engine.pause(&Target::All, RunningJobs::Kill).await.unwrap();
    assert_eq!(
        paused.stopped.iter().map(|job| job.job).collect::<Vec<_>>(),
        [Some(1)]
    );
    assert!(handle.control.is_paused("o/r"), "o/r is paused");
    assert!(handle.control.is_paused("o/s"), "o/s is paused");
    let cancelled: Vec<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("cancel "))
        .collect();
    assert_eq!(cancelled, ["cancel o/r 1"]);
}

/// Makes the history refuse every change to the paused repositories, as a full disk would, until it is dropped.
fn refuse_paused_records(dir: &TestDir) -> rusqlite::Connection {
    let fault = rusqlite::Connection::open(dir.0.join("history.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER no_insert BEFORE INSERT ON paused BEGIN SELECT RAISE(ABORT, 'full'); END; \
             CREATE TRIGGER no_delete BEFORE DELETE ON paused BEGIN SELECT RAISE(ABORT, 'full'); END;",
        )
        .unwrap();
    fault
}

#[tokio::test]
async fn a_pause_the_history_cannot_record_says_so_and_holds_only_until_a_restart() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.recover().await.unwrap();
    let _fault = refuse_paused_records(&dir);
    let Err(ControlError::NotRecorded(message)) =
        engine.pause(&one("o/r"), RunningJobs::Keep).await
    else {
        panic!("a pause the history did not record reported success");
    };
    assert!(message.contains("a restart resumes it"), "{message}");
    assert!(
        handle.control.is_paused("o/r"),
        "o/r is paused until the restart"
    );
    drop(engine);

    let (_engine, handle) = engine_in(&github, &docker, &dir);
    assert!(!handle.control.is_paused("o/r"), "the restart resumed o/r");
}

#[tokio::test]
async fn a_forced_pause_the_history_cannot_record_still_cancels_the_run_and_says_so() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    let _fault = refuse_paused_records(&dir);
    let Err(ControlError::NotRecorded(message)) =
        engine.pause(&one("o/r"), RunningJobs::Kill).await
    else {
        panic!("a forced pause the history did not record reported success");
    };
    assert!(message.contains("stopped job 1"), "{message}");
    assert!(
        handle.control.is_paused("o/r"),
        "o/r is paused until the restart"
    );
    let cancelled: Vec<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("cancel "))
        .collect();
    assert_eq!(cancelled, ["cancel o/r 1"]);
}

#[tokio::test]
async fn a_resume_the_history_cannot_record_says_a_restart_pauses_it_again() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_in(&github, &docker, &dir);
    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    let _fault = refuse_paused_records(&dir);
    let Err(ControlError::NotRecorded(message)) =
        handle.control.set_paused(&Target::All, false).await
    else {
        panic!("a resume the history did not record reported success");
    };
    assert!(message.contains("a restart pauses it again"), "{message}");
    assert!(
        !handle.control.is_paused("o/r"),
        "o/r runs until the restart"
    );
    drop(engine);

    let (_engine, handle) = engine_in(&github, &docker, &dir);
    assert!(
        handle.control.is_paused("o/r"),
        "the restart paused o/r again"
    );
}

#[tokio::test]
async fn resuming_every_repository_resumes_them_across_a_restart() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.pause(&Target::All, RunningJobs::Keep).await.unwrap();
    handle
        .control
        .set_paused(&Target::All, false)
        .await
        .unwrap();
    assert!(!handle.control.is_paused("o/r"), "o/r runs again");
    assert!(!handle.control.is_paused("o/s"), "o/s runs again");
    drop(engine);

    let (_engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    assert!(!handle.control.is_paused("o/r"), "o/r stays resumed");
    assert!(!handle.control.is_paused("o/s"), "o/s stays resumed");
}

#[tokio::test]
async fn a_paused_repository_a_reload_removes_comes_back_active_also_after_a_restart() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    engine.pause(&Target::All, RunningJobs::Keep).await.unwrap();
    engine.reload(Config::parse(CONFIG).unwrap(), None);
    assert!(handle.control.is_paused("o/r"), "o/r stays paused");
    assert_eq!(
        handle.control.set_paused(&one("o/s"), false).await,
        Err(ControlError::NotFound(
            "o/s is not a configured repository".to_owned()
        ))
    );

    engine.reload(Config::parse(&two_repositories()).unwrap(), None);
    assert!(!handle.control.is_paused("o/s"), "o/s comes back active");
    handle.store.flush().await;
    drop(engine);
    let (_engine, handle) = engine_with(&github, &docker, &dir, &two_repositories());
    assert!(handle.control.is_paused("o/r"), "o/r is still paused");
    assert!(!handle.control.is_paused("o/s"), "o/s stays active");
}

#[tokio::test]
async fn the_runner_of_a_cancelled_run_that_does_not_exit_is_removed_after_a_while() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    engine.pause(&one("o/r"), RunningJobs::Kill).await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(removed(&docker.calls()), Vec::<&String>::new());
    tokio::time::pause();
    tokio::time::advance(super::control::CANCEL_GRACE).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
}

#[tokio::test]
async fn the_runner_of_a_cancelled_run_is_removed_on_a_later_tick_when_docker_fails_once() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    engine.pause(&one("o/r"), RunningJobs::Kill).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(super::control::CANCEL_GRACE).await;
    docker.0.lock().unwrap().fail_removes = 1;
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(runner_in(&handle, 1).as_deref(), Some("o/r"));
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(runner_in(&handle, 1), None);
    assert_eq!(docker.0.lock().unwrap().containers.len(), 0);
}

/// Ticks until Docker was asked `call`, as background cache upkeep finishes.
async fn until_called(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    docker: &FakeDocker,
    call: &str,
) {
    for _ in 0..100 {
        if docker.calls().iter().any(|made| made == call) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    panic!("Docker was never asked to {call}: {:?}", docker.calls());
}

#[tokio::test]
async fn killing_the_runner_of_a_cancelled_run_trims_the_shared_caches_and_reseeds_its_profile() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    // The startup check finds the shared caches within their limits, and is collected.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    // A job runs in slot 1 and grows the pnpm cache past its limit.
    docker
        .0
        .lock()
        .unwrap()
        .captures
        .insert(("1".to_owned(), "0".to_owned()), "@@job 1 1\n".to_owned());
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    until_state(&handle, 1, JobState::Running).await;
    github.0.lock().unwrap().queued.clear();
    docker
        .0
        .lock()
        .unwrap()
        .volume_sizes
        .insert("runnerd-pnpm".to_owned(), 6_000_000_000);
    engine.tick(Shutdown::No).await;

    // Its run is cancelled, but the runner does not exit and is killed.
    engine.pause(&one("o/r"), RunningJobs::Kill).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(super::control::CANCEL_GRACE).await;
    engine.tick(Shutdown::No).await;
    assert_eq!(docker.0.lock().unwrap().containers.len(), 0);

    until_called(&mut engine, &docker, "remove volume runnerd-pnpm").await;
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    until_called(&mut engine, &docker, &seed).await;
}

#[tokio::test]
async fn a_forced_shutdown_counts_the_mirror_bytes_its_last_runner_pulled_before_it_was_killed() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    running_runner(&github, &docker, "@@job 1 1\n");
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    docker.0.lock().unwrap().mirror_log_on_remove = "2026-10-22T19:27:25.869343464Z 172.18.0.3 - - \
        [22/Oct/2026:19:27:25 +0000] \"GET /v2/library/alpine/blobs/sha256:aa HTTP/1.1\" 200 577 \"\" \"docker/29.9.2\"\n"
        .to_owned();
    let dir = TestDir::new("mirror");
    let config = CONFIG.replace("mirror = { enabled = false", "mirror = { enabled = true");
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    until_state(&handle, 1, JobState::Running).await;
    engine.tick(Shutdown::No).await;
    engine.tick(Shutdown::Force).await;
    handle.store.flush().await;
    let job = handle.store.job(1).await.unwrap().unwrap();
    assert_eq!(job.summary.metrics.mirror_bytes, 577);
}

#[tokio::test]
async fn a_reload_while_a_job_runs_keeps_the_job() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // The repository moves to a new profile; `app` is gone from the config.
    let config = CONFIG
        .replace("[profiles.app]", "[profiles.big]")
        .replace("profile = \"app\"", "profile = \"big\"");
    engine.reload(Config::parse(&config).unwrap(), None);
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(removed(&docker.calls()), Vec::<&String>::new());
    assert_eq!(runner_in(&handle, 1).as_deref(), Some("o/r"));
    // Its memory still counts against the budget.
    assert_eq!(handle.status.borrow().budget.memory_in_use, 4 << 30);

    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    engine.publish();
    let names: Vec<String> = handle
        .status
        .borrow()
        .profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    assert_eq!(names, ["big"]);
}

#[tokio::test]
async fn fewer_slots_stop_new_starts_but_not_the_running_job() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 2, 1);
    github.0.lock().unwrap().queued = vec![job(3)];
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.reload(
        Config::parse(&CONFIG.replace("slots = 2", "slots = 1")).unwrap(),
        None,
    );
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    assert_eq!(removed(&docker.calls()), Vec::<&String>::new());
    assert_eq!(runner_in(&handle, 2).as_deref(), Some("o/r"));

    // Once it finishes, the one slot left takes the queued job.
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(registered(&github.calls()).len(), 1);
    let slots: Vec<usize> = handle.status.borrow().profiles[0]
        .slots
        .iter()
        .map(|slot| slot.slot)
        .collect();
    assert_eq!(slots, [1]);
}

#[tokio::test]
async fn a_requested_reseed_holds_back_new_runners_until_the_profile_is_free_and_then_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.request_reseed("app").unwrap();
    assert!(matches!(
        engine.request_reseed("app"),
        Err(ControlError::Conflict(_))
    ));
    assert!(matches!(
        engine.request_reseed("nope"),
        Err(ControlError::NotFound(_))
    ));
    github.0.lock().unwrap().queued = vec![job(3)];
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert!(handle.status.borrow().profiles[0].reseeding);
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());

    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    for _ in 0..100 {
        if docker.calls().contains(&seed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(docker.calls().contains(&seed), "{:?}", docker.calls());
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_cache_is_cleaned_only_when_no_runner_uses_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    for used in ["runnerd-app-slot-1", "runnerd-pnpm"] {
        let Err(ControlError::Busy { jobs, .. }) = engine.clean_cache(used).await else {
            panic!("{used} was cleaned while a runner used it");
        };
        assert_eq!(
            jobs.iter().map(|job| job.job).collect::<Vec<_>>(),
            [Some(1)]
        );
    }
    assert!(matches!(
        engine.clean_cache("runnerd-app-base").await,
        Err(ControlError::Invalid(_))
    ));
    assert!(matches!(
        engine.clean_cache("runnerd-other").await,
        Err(ControlError::NotFound(_))
    ));
    engine.clean_cache("runnerd-app-slot-2").await.unwrap();
    let volumes: Vec<String> = docker
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("remove volume "))
        .collect();
    assert_eq!(volumes, ["remove volume runnerd-app-slot-2"]);
}

/// Two profiles and their repositories, with the mirror's memory: `app` for `o/r`, `small` for `o/s`.
fn sized(mirror: &str, app: &str, small: &str) -> String {
    format!(
        r#"
[host]
slice = "ci.slice"
isolation = "sysbox"

[caches]
mirror = {{ enabled = {}, memory = "{mirror}", cpus = 1, ttl = "168h" }}
pnpm = {{ max_size = "5G" }}
maven = {{ max_size = "5G" }}

[profiles.app]
image_stage = "app"
cpus = 2
memory = "{app}"
slots = 1

[profiles.small]
image_stage = "small"
cpus = 2
memory = "{small}"
slots = 1

[[repositories]]
name = "o/r"
profile = "app"
labels = ["self-hosted", "app"]

[[repositories]]
name = "o/s"
profile = "small"
labels = ["self-hosted", "small"]
"#,
        mirror != "0G"
    )
}

fn small_job(id: u64) -> QueuedJob {
    QueuedJob {
        labels: vec!["self-hosted".to_owned(), "small".to_owned()],
        ..job(id)
    }
}

#[tokio::test]
async fn a_running_reseed_keeps_its_memory_when_a_reload_lowers_its_profiles() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &sized("0G", "8G", "1G"));
    engine.memory_max = Some(10 << 30);
    engine.recover().await.unwrap();
    engine.request_reseed("app").unwrap();
    engine.tick(Shutdown::No).await;
    assert!(engine.reseeds.contains_key("app"));
    // The seed container still runs with 8G; 8G more for `small` would not fit in 10G.
    engine.reload(
        Config::parse(&sized("0G", "1G", "8G")).unwrap(),
        Some(10 << 30),
    );
    github.0.lock().unwrap().queued = vec![small_job(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_running_mirror_keeps_its_memory_when_a_reload_lowers_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    let mirror = Container {
        id: Names::new("runnerd").mirror(),
        name: Names::new("runnerd").mirror(),
        running: true,
        labels: HashMap::from([("runnerd.mirror".to_owned(), String::new())]),
    };
    left_running(&docker, mirror, 4 << 30);
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &sized("4G", "6G", "1G"));
    engine.memory_max = Some(10 << 30);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    // The mirror still runs with 4G and the runner with 6G: 3G more would not fit in 10G.
    engine.reload(
        Config::parse(&sized("512M", "6G", "3G")).unwrap(),
        Some(10 << 30),
    );
    github.0.lock().unwrap().queued = vec![small_job(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

/// An idle runner of `o/r` in slot 1 of `app`: registered, not busy, no job.
fn idle_runner(github: &FakeGithub, docker: &FakeDocker) {
    docker.0.lock().unwrap().containers = vec![runner_container(1, 7, true)];
    let mut state = github.0.lock().unwrap();
    state.runners = vec![runner(
        7,
        &identity("ci", ID_A).runner_name("app", 1, 1),
        false,
    )];
    state.registered_on.insert(7, "o/r".to_owned());
}

#[tokio::test]
async fn a_reload_removes_the_idle_runners_of_a_repository_it_removed_or_moved() {
    let other_repository = CONFIG.replace("name = \"o/r\"", "name = \"o/x\"");
    let other_profile = CONFIG
        .replace("[profiles.app]", "[profiles.big]")
        .replace("profile = \"app\"", "profile = \"big\"");
    for config in [other_repository, other_profile] {
        let github = FakeGithub::default();
        let docker = FakeDocker::default();
        idle_runner(&github, &docker);
        let (mut engine, _handle, _dir) = engine(&github, &docker);
        engine.recover().await.unwrap();
        engine.tick(Shutdown::No).await;
        engine.reload(Config::parse(&config).unwrap(), None);
        engine.tick(Shutdown::No).await;
        assert_eq!(removed(&docker.calls()), ["remove container-1"], "{config}");
        assert!(github.0.lock().unwrap().runners.is_empty(), "{config}");
    }
}

#[tokio::test]
async fn the_runner_of_a_removed_repository_github_held_busy_is_deleted_once_it_lets_go() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    idle_runner(&github, &docker);
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    engine.reload(
        Config::parse(&CONFIG.replace("name = \"o/r\"", "name = \"o/x\"")).unwrap(),
        None,
    );
    // GitHub gives it a job just as it is retired, and still holds it when its container exits.
    github.0.lock().unwrap().busy_deletes = 2;
    engine.tick(Shutdown::No).await;
    assert_eq!(removed(&docker.calls()), Vec::<&String>::new());
    docker.0.lock().unwrap().containers[0].running = false;
    engine.tick(Shutdown::No).await;
    // Its container is removed; GitHub still holds the registration, which the same tick's poll deletes.
    assert_eq!(removed(&docker.calls()), ["remove container-1"]);
    assert_eq!(github.0.lock().unwrap().runners.len(), 0);
}

/// A container left running by an earlier daemon, with its memory cap.
fn left_running(docker: &FakeDocker, container: Container, memory: u64) {
    let mut state = docker.0.lock().unwrap();
    state.memory.insert(container.id.clone(), memory);
    state.containers.push(container);
}

/// An engine on `config` with a 10G budget, after recovery.
async fn recovered_with(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> Engine<FakeGithub, FakeDocker> {
    let (mut engine, _handle) = engine_with(github, docker, dir, config);
    engine.memory_max = Some(10 << 30);
    engine.recover().await.unwrap();
    engine
}

#[tokio::test]
async fn after_a_restart_a_seed_container_counts_at_the_cap_it_runs_with() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    left_running(
        &docker,
        Container {
            id: "seed".to_owned(),
            name: Names::new("runnerd").seed("app"),
            running: true,
            labels: HashMap::from([
                ("runnerd.seed".to_owned(), String::new()),
                ("runnerd.profile".to_owned(), "app".to_owned()),
            ]),
        },
        8 << 30,
    );
    let dir = TestDir::new("engine");
    // A reload lowered `app` to 1G while the seed container ran with 8G, then runnerd restarted.
    let mut engine = recovered_with(&github, &docker, &dir, &sized("0G", "1G", "8G")).await;
    github.0.lock().unwrap().queued = vec![small_job(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn after_a_restart_a_runner_and_the_mirror_count_at_the_caps_they_run_with() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    docker
        .0
        .lock()
        .unwrap()
        .memory
        .insert("container-1".to_owned(), 6 << 30);
    let mirror = Container {
        id: Names::new("runnerd").mirror(),
        name: Names::new("runnerd").mirror(),
        running: true,
        labels: HashMap::from([("runnerd.mirror".to_owned(), String::new())]),
    };
    left_running(&docker, mirror, 3 << 30);
    let dir = TestDir::new("engine");
    // A reload lowered the mirror to 512M and `app` to 1G; their containers still run with 3G and 6G.
    let mut engine = recovered_with(&github, &docker, &dir, &sized("512M", "1G", "2G")).await;
    github.0.lock().unwrap().queued = vec![small_job(3)];
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_jobs_record_keeps_the_caps_its_container_ran_with_after_a_reload_lowered_its_profile() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    {
        let mut state = docker.0.lock().unwrap();
        state.memory.insert("container-1".to_owned(), 6 << 30);
        state.cpus.insert("container-1".to_owned(), 4000);
    }
    let dir = TestDir::new("engine");
    // A reload lowered `app` to 1G and 2 CPUs, then runnerd restarted; the container still runs with 6G and 4.
    let (mut engine, handle) = engine_with(&github, &docker, &dir, &sized("0G", "1G", "1G"));
    engine.recover().await.unwrap();
    let job = until_state(&handle, 1, JobState::Running).await;
    let runner = job.summary.runner.unwrap();
    assert_eq!(
        (runner.memory_cap, runner.cpu_cap),
        (Some(6 << 30), Some(4000))
    );
}

/// The caps the job's record says its container ran with, once recovery under `app` at 1G and 2 CPUs saw it running.
async fn recorded_caps(docker: &FakeDocker, github: &FakeGithub) -> (Option<u64>, Option<u64>) {
    let dir = TestDir::new("engine");
    let (mut engine, handle) = engine_with(github, docker, &dir, &sized("0G", "1G", "1G"));
    engine.recover().await.unwrap();
    let runner = until_state(&handle, 1, JobState::Running)
        .await
        .summary
        .runner
        .unwrap();
    (runner.memory_cap, runner.cpu_cap)
}

#[tokio::test]
async fn a_jobs_record_takes_its_containers_cap_when_recovery_could_not_inspect_it() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    {
        let mut state = docker.0.lock().unwrap();
        state.memory.insert("container-1".to_owned(), 6 << 30);
        state.cpus.insert("container-1".to_owned(), 4000);
        // Recovery counts it at the configured 1G; the inspect when it is observed sees 6G.
        state.fail_inspects.insert("container-1".to_owned(), 1);
    }
    assert_eq!(
        recorded_caps(&docker, &github).await,
        (Some(6 << 30), Some(4000))
    );
}

#[tokio::test]
async fn a_container_docker_reports_without_a_memory_cap_is_recorded_without_one() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    running_job(&github, &docker, "o/r", 1, 1);
    docker
        .0
        .lock()
        .unwrap()
        .cpus
        .insert("container-1".to_owned(), 4000);
    assert_eq!(recorded_caps(&docker, &github).await, (None, Some(4000)));
}

/// `CONFIG` with `o/r` replaced by `o/x`, whose runners take jobs labelled `other`.
fn other_repository() -> Config {
    Config::parse(&CONFIG.replace("name = \"o/r\"", "name = \"o/x\"").replace(
        "labels = [\"self-hosted\", \"app\"]",
        "labels = [\"self-hosted\", \"other\"]",
    ))
    .unwrap()
}

fn failing_commit() -> Commit {
    Commit::new(|| Err("cannot write runnerd.toml: read-only file system".to_owned()))
}

#[tokio::test]
async fn a_config_whose_file_cannot_be_written_changes_nothing_and_a_paused_repository_stays_paused()
 {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    let (reply, answer) = tokio::sync::oneshot::channel();
    engine
        .command(Command::ReplaceConfig {
            config: Box::new(other_repository()),
            memory_max: None,
            commit: failing_commit(),
            reply,
        })
        .await;
    assert!(matches!(
        answer.await.unwrap(),
        Err(ControlError::Failed(_))
    ));
    assert!(handle.control.is_paused("o/r"));
    assert_eq!(engine.config.repositories[0].name, "o/r");
}

/// Waits until the engine polled GitHub's queue `count` times.
async fn until_polled(github: &FakeGithub, count: usize) {
    for _ in 0..500 {
        let polls = github
            .calls()
            .iter()
            .filter(|call| call.starts_with("queued "))
            .count();
        if polls >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the engine polled fewer than {count} times");
}

#[tokio::test]
async fn a_new_config_runs_from_its_commit_on_and_a_refused_one_never_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    github.0.lock().unwrap().queued = vec![QueuedJob {
        labels: vec!["self-hosted".to_owned(), "other".to_owned()],
        ..job(3)
    }];
    let (engine, handle, _dir) = engine(&github, &docker);
    let run = tokio::spawn(engine.run());
    until_polled(&github, 1).await;
    let refused = handle
        .control
        .replace_config(other_repository(), None, failing_commit())
        .await;
    assert!(matches!(refused, Err(ControlError::Failed(_))));
    // The command is followed by a tick: it runs under the old config and starts nothing for `o/x`.
    until_polled(&github, 2).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());

    let committed = handle
        .control
        .replace_config(
            other_repository(),
            None,
            Commit::new(|| Ok(Some("syncing the directory failed".to_owned()))),
        )
        .await;
    assert_eq!(
        committed,
        Ok(Some("syncing the directory failed".to_owned()))
    );
    // Answered once it runs with the config: the status shows `o/x` already.
    assert_eq!(handle.status.borrow().repositories[0].name, "o/x");
    handle.control.shutdown(RunningJobs::Kill);
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
}

/// The `repository.*` events stored after `after`, and the last event's ID.
async fn repository_events(handle: &Handle, after: &mut u64) -> Vec<api::Event> {
    handle.store.flush().await;
    let events = handle.store.events_after(*after, 100).await.unwrap();
    if let Some(last) = events.last() {
        *after = last.id.unwrap();
    }
    events
        .into_iter()
        .map(|envelope| envelope.event)
        .filter(|event| event.kind().starts_with("repository."))
        .collect()
}

fn repository_status(paused: bool, queued_jobs: usize) -> api::RepositoryStatus {
    api::RepositoryStatus {
        name: "o/r".to_owned(),
        profile: "app".to_owned(),
        labels: vec!["self-hosted".to_owned(), "app".to_owned()],
        paused,
        queued_jobs,
    }
}

#[tokio::test]
async fn each_change_to_a_repository_is_announced_once_with_its_new_state() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = engine(&github, &docker);
    let mut after = 0;
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryChanged {
            repository: repository_status(false, 0)
        }]
    );
    engine.publish();
    assert_eq!(repository_events(&handle, &mut after).await, []);

    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    engine.publish();
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryChanged {
            repository: repository_status(true, 0)
        }]
    );

    github.0.lock().unwrap().queued = vec![job(1)];
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryChanged {
            repository: repository_status(true, 1)
        }]
    );
    engine.tick(Shutdown::No).await;
    engine.publish();
    assert_eq!(repository_events(&handle, &mut after).await, []);

    handle.control.set_paused(&one("o/r"), false).await.unwrap();
    engine.publish();
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryChanged {
            repository: repository_status(false, 1)
        }]
    );

    let with_s = format!(
        "{CONFIG}\n[[repositories]]\nname = \"o/s\"\nprofile = \"app\"\nlabels = [\"self-hosted\"]\n"
    );
    engine.reload(Config::parse(&with_s).unwrap(), None);
    engine.publish();
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryChanged {
            repository: api::RepositoryStatus {
                name: "o/s".to_owned(),
                profile: "app".to_owned(),
                labels: vec!["self-hosted".to_owned()],
                paused: false,
                queued_jobs: 0,
            }
        }]
    );
    engine.reload(Config::parse(&with_s).unwrap(), None);
    engine.publish();
    assert_eq!(repository_events(&handle, &mut after).await, []);

    engine.reload(Config::parse(CONFIG).unwrap(), None);
    engine.publish();
    assert_eq!(
        repository_events(&handle, &mut after).await,
        [api::Event::RepositoryRemoved {
            repository: "o/s".to_owned()
        }]
    );
}

#[tokio::test]
async fn a_restart_without_a_repository_announces_its_removal_to_clients_resuming_after_the_last_run()
 {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine");
    let (engine, handle) = engine_in(&github, &docker, &dir);
    let mut cursor = 0;
    repository_events(&handle, &mut cursor).await;
    drop((engine, handle));

    // Restarted with o/s in place of o/r, on the same history.
    let config = CONFIG.replace("name = \"o/r\"", "name = \"o/s\"");
    let (_engine, handle) = engine_with(&github, &docker, &dir, &config);
    let mut s = repository_status(false, 0);
    s.name = "o/s".to_owned();
    assert_eq!(
        repository_events(&handle, &mut cursor).await,
        [
            api::Event::RepositoryChanged { repository: s },
            api::Event::RepositoryRemoved {
                repository: "o/r".to_owned()
            }
        ]
    );
}

#[tokio::test]
async fn a_pause_undone_before_it_is_published_leaves_clients_where_the_status_is() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let (mut engine, handle, _dir) = engine(&github, &docker);
    // A client: the status, then the repository events after what it read.
    let mut cursor = 0;
    repository_events(&handle, &mut cursor).await;
    let mut shown = handle.status.borrow().repositories.clone();

    engine.pause(&one("o/r"), RunningJobs::Keep).await.unwrap();
    handle.control.set_paused(&one("o/r"), false).await.unwrap();
    engine.publish();
    for event in repository_events(&handle, &mut cursor).await {
        match event {
            api::Event::RepositoryChanged { repository } => {
                shown.retain(|known| known.name != repository.name);
                shown.push(repository);
            }
            api::Event::RepositoryRemoved { repository } => {
                shown.retain(|known| known.name != repository);
            }
            _ => {}
        }
    }
    assert_eq!(shown, handle.status.borrow().repositories);
    assert!(!shown[0].paused, "o/r shows resumed");
}

const PUBLISHED: &str = "ghcr.io/ayagmar/runnerd-runner:app-0.0.5";

fn pulled(calls: &[String]) -> Vec<&String> {
    calls
        .iter()
        .filter(|call| call.starts_with("pull "))
        .collect()
}

#[tokio::test]
async fn a_profiles_missing_image_is_pulled_before_its_first_runner_starts() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    docker.0.lock().unwrap().missing_images = BTreeSet::from([PUBLISHED.to_owned()]);
    let config = CONFIG.replace("image_stage = \"app\"", &format!("image = \"{PUBLISHED}\""));
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
    for _ in 0..100 {
        if !registered(&github.calls()).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(pulled(&docker.calls()), [&format!("pull {PUBLISHED}")]);
    assert_eq!(docker.0.lock().unwrap().started_images, [PUBLISHED]);
}

#[tokio::test]
async fn a_failed_pull_starts_no_runner_and_is_not_retried_on_the_next_tick() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    {
        let mut state = docker.0.lock().unwrap();
        state.missing_images = BTreeSet::from([PUBLISHED.to_owned()]);
        state.fail_pulls = true;
    }
    let config = CONFIG.replace("image_stage = \"app\"", &format!("image = \"{PUBLISHED}\""));
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    until_called(&mut engine, &docker, &format!("pull {PUBLISHED}")).await;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(pulled(&docker.calls()).len(), 1, "{:?}", docker.calls());
    assert_eq!(registered(&github.calls()), Vec::<&String>::new());
}

#[tokio::test]
async fn a_stage_image_docker_lacks_is_never_pulled_and_its_runner_fails_to_start() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    github.0.lock().unwrap().queued = vec![job(1)];
    docker.0.lock().unwrap().missing_images = BTreeSet::from(["runnerd-runner:app".to_owned()]);
    let (mut engine, _handle, _dir) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    assert_eq!(pulled(&docker.calls()), Vec::<&String>::new());
    assert_eq!(registered(&github.calls()).len(), 1);
    assert_eq!(
        docker.0.lock().unwrap().started_images,
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_requested_reseed_waits_for_the_profiles_image_and_then_runs() {
    let (github, docker) = (FakeGithub::default(), FakeDocker::default());
    {
        let mut state = docker.0.lock().unwrap();
        state.missing_images = BTreeSet::from([PUBLISHED.to_owned()]);
        state.block_pulls = true;
    }
    let config = CONFIG.replace("image_stage = \"app\"", &format!("image = \"{PUBLISHED}\""));
    let dir = TestDir::new("engine");
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, &config);
    engine.recover().await.unwrap();
    engine.request_reseed("app").unwrap();
    until_called(&mut engine, &docker, &format!("pull {PUBLISHED}")).await;
    let seed = format!("start {}", Names::new("runnerd").seed("app"));
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        engine.tick(Shutdown::No).await;
    }
    assert!(!docker.calls().contains(&seed), "{:?}", docker.calls());
    docker.0.lock().unwrap().block_pulls = false;
    until_called(&mut engine, &docker, &seed).await;
    assert_eq!(docker.0.lock().unwrap().started_images, [PUBLISHED]);
}
