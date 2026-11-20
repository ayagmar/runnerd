//! The action archives on the fakes (ADR 0147): a runner started while they are on and checked good reads them,
//! read-only, as the runner's archive cache; one started while they are off, or before their task checked them, gets
//! neither, the mount and the setting together.

use super::*;
use crate::archives;

/// The file under `archives/` no check has seen.
const UNCHECKED: &str =
    "actions/archives/actions_checkout/08eba0b27e820071cde6df949e0beb9ba4906955.tar.gz";

/// An engine on the fakes whose action archives are kept in `dir`'s `actions`.
fn engine_with_archives(
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
            cpus: None,
            announced: store.announced().unwrap(),
            paused: store.paused_repositories().unwrap(),
            store,
            paths: Paths {
                cgroup: dir.0.join("cgroup"),
                proc: dir.0.join("proc"),
                sys: dir.0.join("sys"),
            },
            build_cache: None,
            action_archives: Some(dir.0.join("actions")),
            unsynced: dir.0.join("unsynced"),
            durability: Durability::new(|| {}, |_| false),
        },
    )
}

/// What the one runner a queued job started mounts at `archives::MOUNT`, and its environment's archive setting.
/// `archives/` holds a file no check has seen. With `checked`, the archives' task has started and published its view
/// first; without, it has not started.
async fn first_runner(
    config: &str,
    checked: bool,
) -> (Option<bollard::models::Mount>, Vec<String>) {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("archives"),
    );
    let unchecked = dir.0.join(UNCHECKED);
    std::fs::create_dir_all(unchecked.parent().unwrap()).unwrap();
    std::fs::write(&unchecked, b"not what any check verified").unwrap();
    boot(&dir, "boot-1");
    let (mut engine, _handle) = engine_with_archives(&github, &docker, &dir, config);
    let _task = checked.then(|| engine.archives.start(Arc::new(github.clone())).unwrap());
    if checked && Config::parse(config).unwrap().caches.actions.enabled {
        for _ in 0..1000 {
            if engine.archives.runner().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !unchecked.exists(),
            "the file no check saw went to quarantine before runners got the archives"
        );
    }
    engine.recover().await.unwrap();
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    let state = docker.0.lock().unwrap();
    let (name, mounts) = state.mounts.iter().next().expect("a runner started");
    let mount = mounts
        .iter()
        .find(|mount| mount.target.as_deref() == Some(archives::MOUNT))
        .cloned();
    let env = state.envs[name]
        .iter()
        .filter(|env| env.starts_with(archives::ENV))
        .cloned()
        .collect();
    (mount, env)
}

#[tokio::test]
async fn a_runner_reads_the_action_archives_read_only_as_its_archive_cache() {
    let (mount, env) = first_runner(CONFIG, true).await;
    let mount = mount.expect("the archives are mounted");
    assert_eq!(mount.read_only, Some(true), "a job cannot plant an archive");
    assert!(
        mount
            .source
            .as_deref()
            .is_some_and(|source| source.ends_with("actions/archives")),
        "only archives/ is mounted, never staging/ or quarantine/: {mount:?}"
    );
    assert_eq!(env, vec![format!("{}={}", archives::ENV, archives::MOUNT)]);
}

#[tokio::test]
async fn a_runner_gets_no_archives_while_they_are_off_or_before_their_first_check() {
    let off = format!("{CONFIG}\n[caches.actions]\nenabled = false\n");
    for (config, checked) in [(off.as_str(), true), (CONFIG, false)] {
        let (mount, env) = first_runner(config, checked).await;
        assert_eq!(mount, None, "no mount");
        assert!(env.is_empty(), "no archive setting: {env:?}");
    }
}
