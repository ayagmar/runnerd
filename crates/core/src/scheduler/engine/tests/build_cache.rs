//! The shared build cache on the fakes (ADR 0052): a push to main's entry is a hit for a pull request's job in
//! another slot, the pull request's uploads stay invisible, each job's record keeps what the cache did for it, a tag
//! named like the default branch only reads, and the cache is trimmed only once no runner runs.

use super::*;
use crate::buildcache::{self, BuildCache};

/// An engine on the fakes with a build cache in `dir`, for `config`.
fn engine_with_cache(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle, BuildCache) {
    let config = Config::parse(config).unwrap();
    let cache = BuildCache::open(
        &dir.0.join("build-cache"),
        &dir.0.join("sockets"),
        config.caches.build.max_size.0,
    )
    .unwrap();
    let store = Store::open(&dir.0.join("history.db")).unwrap();
    let (engine, handle) = Engine::new(
        config,
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
            },
            build_cache: Some(cache.clone()),
        },
    );
    (engine, handle, cache)
}

fn trigger(event: &str, branch: &str) -> github::RunTrigger {
    github::RunTrigger {
        event: event.to_owned(),
        head_branch: Some(branch.to_owned()),
        head_repository: Some("o/r".to_owned()),
        head_sha: "c0ffee".to_owned(),
    }
}

/// The runner started in `slot` of `app`, and the directory its container mounts at `buildcache::MOUNT`.
fn runner_in(docker: &FakeDocker, slot: usize) -> (String, std::path::PathBuf) {
    let state = docker.0.lock().unwrap();
    let (name, mounts) = state
        .mounts
        .iter()
        .find(|(name, _)| name.contains(&format!("-app-{slot}-")))
        .unwrap();
    let mount = mounts
        .iter()
        .find(|mount| mount.target.as_deref() == Some(buildcache::MOUNT))
        .unwrap();
    assert_eq!(
        mount.read_only,
        Some(true),
        "a job cannot change its socket"
    );
    (name.clone(), mount.source.clone().unwrap().into())
}

/// Starts a runner per queued job: job 1, run 1, a push to main, in slot 1; job 2, run 2, a pull request, in slot 2.
/// GitHub places each on its runner.
async fn main_and_pull_request(
    github: &FakeGithub,
    docker: &FakeDocker,
    engine: &mut Engine<FakeGithub, FakeDocker>,
) -> [(String, std::path::PathBuf); 2] {
    two_jobs(
        github,
        docker,
        engine,
        [trigger("push", "main"), trigger("pull_request", "main")],
    )
    .await
}

/// Starts a runner per queued job, job 1 of run 1 in slot 1 and job 2 of run 2 in slot 2, their runs started as
/// `triggers` say. GitHub places each on its runner.
async fn two_jobs(
    github: &FakeGithub,
    docker: &FakeDocker,
    engine: &mut Engine<FakeGithub, FakeDocker>,
    triggers: [github::RunTrigger; 2],
) -> [(String, std::path::PathBuf); 2] {
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![
            job(1),
            QueuedJob {
                run_id: 2,
                ..job(2)
            },
        ];
        let [first, second] = triggers;
        state.triggers.insert(1, first);
        state.triggers.insert(2, second);
    }
    {
        let mut state = docker.0.lock().unwrap();
        state.slot_captures.insert(1, "@@job 1 1\n".to_owned());
        state.slot_captures.insert(2, "@@job 2 2\n".to_owned());
    }
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let runners = [runner_in(docker, 1), runner_in(docker, 2)];
    let mut state = github.0.lock().unwrap();
    for (id, (name, _)) in [1, 2].into_iter().zip(&runners) {
        state.jobs.insert(
            id,
            JobDetails {
                run_id: id,
                runner_name: Some(name.clone()),
                ..details(id, "in_progress", None)
            },
        );
    }
    state.queued.clear();
    runners
}

/// Ticks until neither runner's writes are undecided.
async fn until_decided(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    cache: &BuildCache,
    runners: &[&str],
) {
    for _ in 0..50 {
        if runners.iter().all(|runner| !cache.undecided(runner)) {
            return;
        }
        engine.tick(Shutdown::No).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the writes of {runners:?} were never decided");
}

/// Stops every runner container, as when their jobs end, and lets the engine remove them.
async fn jobs_end(docker: &FakeDocker, engine: &mut Engine<FakeGithub, FakeDocker>) {
    for container in &mut docker.0.lock().unwrap().containers {
        container.running = false;
    }
    engine.tick(Shutdown::No).await;
}

#[tokio::test]
async fn a_main_builds_entry_is_a_hit_for_a_pull_request_in_another_slot() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache");
    let (mut engine, handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    let [(main, main_socket), (pull, pull_socket)] =
        main_and_pull_request(&github, &docker, &mut engine).await;
    // Gradle stores its first outputs before GitHub has said anything of the job.
    assert_eq!(
        buildcache::request(&main_socket, "PUT", "/gradle/0a1b", b"classes")
            .await
            .0,
        200
    );
    until_decided(&mut engine, &cache, &[&main, &pull]).await;
    assert_eq!(
        buildcache::request(&pull_socket, "GET", "/gradle/0a1b", b"").await,
        (200, b"classes".to_vec())
    );
    // The pull request's upload is accepted, so Gradle keeps the cache on, and main never sees it.
    assert_eq!(
        buildcache::request(&pull_socket, "PUT", "/gradle/ffff", b"poison")
            .await
            .0,
        200
    );
    assert_eq!(
        buildcache::request(&main_socket, "GET", "/gradle/ffff", b"")
            .await
            .0,
        404
    );
    jobs_end(&docker, &mut engine).await;
    handle.store.flush().await;
    let counts = |job: u64| {
        let store = handle.store.clone();
        async move {
            store
                .job(job)
                .await
                .unwrap()
                .unwrap()
                .summary
                .metrics
                .build_cache
        }
    };
    let main_counts = counts(1).await;
    assert_eq!((main_counts.written_bytes, main_counts.misses), (7, 1));
    assert!(main_counts.writer, "the push to main wrote");
    let pull_counts = counts(2).await;
    assert_eq!(
        (
            pull_counts.hits,
            pull_counts.read_bytes,
            pull_counts.refused
        ),
        (1, 7, 1)
    );
    assert!(!pull_counts.writer, "the pull request only read");
    assert!(!main_socket.exists(), "a removed runner's socket goes");
}

#[tokio::test]
async fn a_runner_whose_log_claims_another_runners_job_only_reads() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-claim");
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    // The pull request's job writes main's job ID into its runner's log before the capture reads it.
    docker
        .0
        .lock()
        .unwrap()
        .slot_captures
        .insert(2, "@@job 1 1\n".to_owned());
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![
            job(1),
            QueuedJob {
                run_id: 2,
                ..job(2)
            },
        ];
        state.triggers.insert(1, trigger("push", "main"));
    }
    docker
        .0
        .lock()
        .unwrap()
        .slot_captures
        .insert(1, "@@job 1 1\n".to_owned());
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let (main, _) = runner_in(&docker, 1);
    let (liar, liar_socket) = runner_in(&docker, 2);
    {
        let mut state = github.0.lock().unwrap();
        state.jobs.insert(
            1,
            JobDetails {
                runner_name: Some(main.clone()),
                ..details(1, "in_progress", None)
            },
        );
        state.queued.clear();
    }
    until_decided(&mut engine, &cache, &[&main, &liar]).await;
    assert_eq!(
        buildcache::request(&liar_socket, "PUT", "/gradle/0a1b", b"poison")
            .await
            .0,
        200
    );
    assert_eq!(cache.size().entries, 0, "{liar} stored nothing");
}

#[tokio::test]
async fn the_build_cache_is_trimmed_only_once_no_runner_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-trim");
    let config = CONFIG.replace(
        "maven = { max_size = \"5G\" }\n",
        "maven = { max_size = \"5G\" }\nbuild = { max_size = \"1K\" }\n",
    );
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, &config);
    let [(main, main_socket), (pull, _)] =
        main_and_pull_request(&github, &docker, &mut engine).await;
    until_decided(&mut engine, &cache, &[&main, &pull]).await;
    // Each at most a quarter of the limit, as an entry may be.
    for key in ["aa", "bb", "cc", "dd", "ee"] {
        let path = format!("/gradle/{key}");
        assert_eq!(
            buildcache::request(&main_socket, "PUT", &path, &[0; 250])
                .await
                .0,
            200
        );
    }
    assert_eq!(cache.size().bytes, 1250);
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(cache.size().bytes, 1250, "trimmed while jobs run");
    jobs_end(&docker, &mut engine).await;
    for _ in 0..50 {
        if cache.size().bytes <= 1024 {
            return;
        }
        engine.tick(Shutdown::No).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never trimmed: {:?}", cache.size());
}

#[tokio::test]
async fn a_repository_that_opts_out_gets_no_build_cache() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-off");
    let config = CONFIG.replace("warm = 0\n", "warm = 0\nbuild_cache = false\n");
    let (mut engine, _handle, _cache) = engine_with_cache(&github, &docker, &dir, &config);
    github.0.lock().unwrap().queued = vec![job(1)];
    engine.recover().await.unwrap();
    engine.tick(Shutdown::No).await;
    let state = docker.0.lock().unwrap();
    let mounts = state.mounts.values().next().unwrap();
    assert!(
        mounts
            .iter()
            .all(|mount| mount.target.as_deref() != Some(buildcache::MOUNT)),
        "o/r opted out"
    );
}

#[tokio::test]
async fn branches_the_config_names_write_besides_the_default_branch() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-branches");
    let config = CONFIG.replace(
        "maven = { max_size = \"5G\" }\n",
        "maven = { max_size = \"5G\" }\nbuild = { branches = [\"release\"] }\n",
    );
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, &config);
    let [(main, main_socket), (release, release_socket)] = two_jobs(
        &github,
        &docker,
        &mut engine,
        [trigger("push", "main"), trigger("push", "release")],
    )
    .await;
    until_decided(&mut engine, &cache, &[&main, &release]).await;
    for (socket, key) in [(&main_socket, "aa"), (&release_socket, "bb")] {
        let path = format!("/gradle/{key}");
        assert_eq!(
            buildcache::request(socket, "PUT", &path, b"classes")
                .await
                .0,
            200
        );
    }
    assert_eq!(cache.size().entries, 2, "main and release both wrote");
}

#[tokio::test]
async fn a_tag_named_like_the_default_branch_only_reads() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-tag");
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    // A tag called main on a commit main never had: GitHub names the tag as the run's head branch.
    github.0.lock().unwrap().off_branch.push("7a9".to_owned());
    let tag = github::RunTrigger {
        head_sha: "7a9".to_owned(),
        ..trigger("push", "main")
    };
    let [(tagged, tagged_socket), (pull, _)] = two_jobs(
        &github,
        &docker,
        &mut engine,
        [tag, trigger("pull_request", "main")],
    )
    .await;
    until_decided(&mut engine, &cache, &[&tagged, &pull]).await;
    assert!(
        github.calls().contains(&"compare main...7a9".to_owned()),
        "{:?}",
        github.calls()
    );
    assert_eq!(
        buildcache::request(&tagged_socket, "PUT", "/gradle/0a1b", b"poison")
            .await
            .0,
        200
    );
    assert_eq!(cache.size().entries, 0, "the tag's run stored nothing");
}

#[tokio::test]
async fn a_push_whose_commit_github_cannot_compare_never_writes() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-compare");
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    github.0.lock().unwrap().compare_fails = true;
    let [(main, main_socket), (pull, pull_socket)] =
        main_and_pull_request(&github, &docker, &mut engine).await;
    assert_eq!(
        buildcache::request(&main_socket, "PUT", "/gradle/0a1b", b"classes")
            .await
            .0,
        200
    );
    for _ in 0..5 {
        engine.tick(Shutdown::No).await;
    }
    assert!(!cache.undecided(&pull), "the pull request was decided");
    assert!(cache.undecided(&main), "main waits for GitHub to compare");
    assert_eq!(
        buildcache::request(&pull_socket, "GET", "/gradle/0a1b", b"")
            .await
            .0,
        404
    );
}
