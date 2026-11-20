//! The action archives on the fakes (ADR 0147): a runner started while they are on reads them, read-only, as the
//! runner's archive cache; one started while they are off, or before their directory exists, gets neither.

use super::*;
use crate::archives;

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
async fn first_runner(
    config: &str,
    archives_dir: bool,
) -> (Option<bollard::models::Mount>, Vec<String>) {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("archives"),
    );
    if archives_dir {
        std::fs::create_dir_all(dir.0.join("actions/archives")).unwrap();
    }
    boot(&dir, "boot-1");
    let (mut engine, _handle) = engine_with_archives(&github, &docker, &dir, config);
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
async fn a_runner_gets_no_archives_while_they_are_off_or_not_there_yet() {
    let off = format!("{CONFIG}\n[caches.actions]\nenabled = false\n");
    for (config, there) in [(off.as_str(), true), (CONFIG, false)] {
        let (mount, env) = first_runner(config, there).await;
        assert_eq!(mount, None, "no mount");
        assert!(env.is_empty(), "no archive setting: {env:?}");
    }
}
