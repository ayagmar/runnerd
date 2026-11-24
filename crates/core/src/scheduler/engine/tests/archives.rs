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
            draining: store.draining_repositories().unwrap(),
            default_database: store.path().to_owned(),
            store,
            paths: Paths {
                cgroup: dir.0.join("cgroup"),
                proc: dir.0.join("proc"),
                sys: dir.0.join("sys"),
            },
            build_cache: None,
            action_archives: Some(dir.0.join("actions")),
            grype_database: None,
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
    // The first check waits for recovery.
    engine.recover().await.unwrap();
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

/// Waits until `until` holds of the engine's archives, or fails naming `what`.
async fn until(
    engine: &Engine<FakeGithub, FakeDocker>,
    what: &str,
    until: impl Fn(&archives::Link) -> bool,
) {
    for _ in 0..1000 {
        if until(&engine.archives) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{what} never came");
}

/// A runner a previous daemon started with the archives mounted, still running, whose capture prints `output`.
fn reader_left_running(github: &FakeGithub, docker: &FakeDocker, output: &str) -> String {
    let mut container = runner_container(1, 7, true);
    container
        .labels
        .insert(archives::LABEL.to_owned(), "1".to_owned());
    let name = container.name.clone();
    {
        let mut state = docker.0.lock().unwrap();
        state.memory.insert(container.id.clone(), 4 << 30);
        state.containers = vec![container];
        state
            .captures
            .insert(("1".to_owned(), "0".to_owned()), output.to_owned());
    }
    github.0.lock().unwrap().runners = vec![runner(7, &name, true)];
    name
}

#[tokio::test]
async fn a_restart_counts_the_runners_reading_the_archives_unchecked_and_names_them_if_the_first_check_moves_a_file()
 {
    for moved in [true, false] {
        let (github, docker, dir) = (
            FakeGithub::default(),
            FakeDocker::default(),
            TestDir::new("archives"),
        );
        if moved {
            let unchecked = dir.0.join(UNCHECKED);
            std::fs::create_dir_all(unchecked.parent().unwrap()).unwrap();
            std::fs::write(&unchecked, b"not what any check verified").unwrap();
        }
        let reader = reader_left_running(&github, &docker, "");
        boot(&dir, "boot-1");
        let (mut engine, _handle) = engine_with_archives(&github, &docker, &dir, CONFIG);
        engine.recover().await.unwrap();
        let status = engine.archives.status().expect("a status before any check");
        assert_eq!(
            status.unchecked, 1,
            "the adopted runner reads them unchecked"
        );
        assert!(
            status
                .line(0)
                .ends_with("1 adopted runner reads them before a check"),
            "{}",
            status.line(0)
        );
        let _task = engine.archives.start(Arc::new(github.clone())).unwrap();
        until(&engine, "the first check", |link| {
            link.status().is_some_and(|status| status.unchecked == 0)
        })
        .await;
        let status = engine.archives.status().unwrap();
        let exposed: Vec<&str> = status
            .failures
            .iter()
            .filter(|failure| failure.kind == api::ArchiveFailureKind::Exposed)
            .map(|failure| failure.subject.as_str())
            .collect();
        if moved {
            assert_eq!(exposed, vec![reader.as_str()], "named, and left running");
            assert!(
                status.attention_title().is_some_and(
                    |title| title.starts_with("a runner may have read an action archive")
                ),
                "{:?}",
                status.attention_title()
            );
            assert!(
                docker
                    .0
                    .lock()
                    .unwrap()
                    .containers
                    .iter()
                    .any(|c| c.name == reader && c.running),
                "it is not killed"
            );
            engine.archives.released(&reader);
            assert!(
                engine
                    .archives
                    .status()
                    .unwrap()
                    .failures
                    .iter()
                    .all(|failure| failure.kind != api::ArchiveFailureKind::Exposed),
                "gone, it is no longer named"
            );
        } else {
            assert!(exposed.is_empty(), "the check moved nothing: {exposed:?}");
        }
    }
}

#[tokio::test]
async fn every_check_withdraws_the_runners_view_until_it_returns_good() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("archives"),
    );
    boot(&dir, "boot-1");
    // A kept archive: each refresh asks GitHub whether its repository is still public before it publishes the view.
    archives::keep(
        &dir.0.join("actions"),
        "actions/checkout",
        "08eba0b27e820071cde6df949e0beb9ba4906955",
        b"an archive a refresh kept",
    )
    .unwrap();
    let (mut engine, _handle) = engine_with_archives(&github, &docker, &dir, CONFIG);
    let _task = engine.archives.start(Arc::new(github.clone())).unwrap();
    engine.recover().await.unwrap();
    until(&engine, "the first view", |link| link.runner().is_some()).await;
    let mut ready = engine.archives.ready_changes();
    ready.borrow_and_update();
    // The next refresh stays inside its check until the test has seen the view withdrawn.
    github.hold_actions();
    // A reload that changes the settings refreshes: its check runs again.
    let reloaded = format!("{CONFIG}\n[caches.actions]\nkeep = \"7d\"\n");
    engine.archives.reload(&Config::parse(&reloaded).unwrap());
    let withdrawn = tokio::time::timeout(
        Duration::from_secs(5),
        // Mapped, so the borrow of the view is let go before the task publishes again.
        async { ready.wait_for(Option::is_none).await.map(|_| ()) },
    )
    .await;
    assert!(
        matches!(withdrawn, Ok(Ok(()))),
        "the check withdraws the view"
    );
    assert!(
        engine.archives.runner().is_none(),
        "a start during the check gets no view"
    );
    github.release_actions();
    until(&engine, "the view again", |link| link.runner().is_some()).await;
}

#[tokio::test]
async fn a_job_log_that_shows_an_immutable_package_keeps_its_repository_out_of_the_archives() {
    let line = "2026-11-20T10:00:00.0000000Z ##[group]Download immutable action package 'actions/checkout@v6'";
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("archives"),
    );
    reader_left_running(
        &github,
        &docker,
        &format!("@@job 1 1\n@@page 1 0 {}\n{line}\n", line.len() + 1),
    );
    github
        .0
        .lock()
        .unwrap()
        .jobs
        .insert(1, details(1, "in_progress", None));
    boot(&dir, "boot-1");
    let (mut engine, _handle) = engine_with_archives(&github, &docker, &dir, CONFIG);
    let _task = engine.archives.start(Arc::new(github.clone())).unwrap();
    // Recovery adopts the runner and captures its log.
    engine.recover().await.unwrap();
    until(&engine, "the package's report", |link| {
        link.status().is_some_and(|status| {
            status.failures.iter().any(|failure| {
                failure.kind == api::ArchiveFailureKind::Package
                    && failure.subject == "actions/checkout"
            })
        })
    })
    .await;
}
