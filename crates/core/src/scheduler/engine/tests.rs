use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::models::ContainerCreateBody;

use super::{Engine, Handle, HostIdentity, Shutdown};
use crate::cache::Names;
use crate::config::Config;
use crate::docker::{self, Container, DockerApi, JitConfig, RunnerSpec, RunnerState};
use crate::github::{self, Deleted, GithubApi, QueuedJob, Runner};
use crate::isolation::Isolation;

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
}

/// What the fake Docker Engine runs and what it was asked.
#[derive(Default)]
struct DockerState {
    containers: Vec<Container>,
    /// Every `list` fails, as when the Engine does not answer.
    fail_lists: bool,
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
        Ok(String::new())
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

fn engine(github: &FakeGithub, docker: &FakeDocker) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    Engine::new(
        Config::parse(CONFIG).unwrap(),
        github.clone(),
        docker.clone(),
        Names::new("runnerd"),
        identity("ci", ID_A),
        None,
    )
}

fn job(id: u64) -> QueuedJob {
    QueuedJob {
        id,
        run_id: 1,
        name: "build".to_owned(),
        labels: vec!["self-hosted".to_owned(), "app".to_owned()],
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
    let (mut engine, _handle) = engine(&github, &docker);
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
    let (mut engine, handle) = engine(&github, &docker);
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
    let (engine, handle) = engine(&github, &docker);
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
    let (mut engine, _handle) = engine(&github, &docker);
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let deleted: Vec<String> = github
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("delete "))
        .collect();
    assert_eq!(deleted, ["delete 2"]);
}
