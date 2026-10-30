//! The shared build cache on the fakes (ADR 0052): a push to main's entry is a hit for a pull request's job in
//! another slot, the pull request's uploads stay invisible, each job's record keeps what the cache did for it, a tag
//! named like the default branch only reads, the cache is trimmed only once no runner runs, and where a repository
//! lets them (ADR 0053), its own branches' pull requests that people opened write while forks' and bots' read.

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
        by_user: true,
        pull_requests: Vec::new(),
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
    engine.recover().await.unwrap();
    two_more_jobs(github, docker, engine, 1, triggers).await
}

/// As `two_jobs` on an engine that runs already, for jobs `first` and `first + 1`, each of its own run.
async fn two_more_jobs(
    github: &FakeGithub,
    docker: &FakeDocker,
    engine: &mut Engine<FakeGithub, FakeDocker>,
    first: u64,
    triggers: [github::RunTrigger; 2],
) -> [(String, std::path::PathBuf); 2] {
    let ids = [first, first + 1];
    {
        let mut state = github.0.lock().unwrap();
        state.queued = ids
            .iter()
            .map(|&id| QueuedJob {
                run_id: id,
                ..job(id)
            })
            .collect();
        for (id, trigger) in ids.into_iter().zip(triggers) {
            state.triggers.insert(id, trigger);
        }
    }
    {
        let mut state = docker.0.lock().unwrap();
        for (slot, id) in [1, 2].into_iter().zip(ids) {
            state
                .slot_captures
                .insert(slot, format!("@@job {id} {id}\n"));
        }
        state.mounts.clear();
    }
    // Once jobs ended, the caches are checked first, and runners wait for it.
    for _ in 0..50 {
        engine.tick(Shutdown::No).await;
        if docker.0.lock().unwrap().mounts.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let runners = [runner_in(docker, 1), runner_in(docker, 2)];
    let mut state = github.0.lock().unwrap();
    for (id, (name, _)) in ids.into_iter().zip(&runners) {
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

/// Uploads one entry of `bytes` to each of `keys` through `socket`.
async fn upload(socket: &std::path::Path, keys: &[&str], bytes: usize) {
    for key in keys {
        let path = format!("/gradle/{key}");
        assert_eq!(
            buildcache::request(socket, "PUT", &path, &vec![0; bytes])
                .await
                .0,
            200
        );
    }
}

#[tokio::test]
async fn the_build_cache_is_trimmed_only_once_no_runner_runs() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-trim");
    let config = CONFIG.replace(
        "maven = { max_size = \"5G\" }\n",
        "maven = { max_size = \"5G\" }\nbuild = { max_size = \"16K\" }\n",
    );
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, &config);
    let [(main, main_socket), (pull, _)] =
        main_and_pull_request(&github, &docker, &mut engine).await;
    until_decided(&mut engine, &cache, &[&main, &pull]).await;
    // Each a quarter of the limit, as an entry may be at most.
    upload(&main_socket, &["aa", "bb", "cc", "dd", "ee"], 4096).await;
    assert_eq!(cache.size().bytes, 20 << 10);
    for _ in 0..3 {
        engine.tick(Shutdown::No).await;
    }
    assert_eq!(cache.size().bytes, 20 << 10, "trimmed while jobs run");

    // The jobs end, and the shared caches' check that starts then takes long to measure Docker's volumes; the reseed
    // slot 1's job makes due finishes.
    {
        let mut state = docker.0.lock().unwrap();
        state.volume_sizes_held.send_replace(true);
        state.seed_exit = Some(0);
    }
    jobs_end(&docker, &mut engine).await;
    assert_eq!(
        cache.size().bytes,
        16 << 10,
        "trimmed at the first idle tick"
    );
    // Jobs start before the check ends, and use the build cache as it is.
    let [(main, main_socket), (pull, _)] = two_more_jobs(
        &github,
        &docker,
        &mut engine,
        3,
        [trigger("push", "main"), trigger("pull_request", "main")],
    )
    .await;
    let started = cache.size();
    docker
        .0
        .lock()
        .unwrap()
        .volume_sizes_held
        .send_replace(false);
    for _ in 0..50 {
        if engine.trim.is_none() {
            break;
        }
        engine.tick(Shutdown::No).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(engine.trim.is_none(), "the check ended");
    assert_eq!(cache.size(), started, "the check trimmed while jobs run");

    // What they store is trimmed once they end.
    until_decided(&mut engine, &cache, &[&main, &pull]).await;
    upload(&main_socket, &["ff"], 4096).await;
    assert_eq!(cache.size().bytes, 20 << 10);
    jobs_end(&docker, &mut engine).await;
    assert_eq!(
        cache.size().bytes,
        16 << 10,
        "trimmed at the next idle tick"
    );
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
async fn a_writing_branch_keeps_its_own_actions_cache_entries_and_reads_mains() {
    use crate::buildcache::actions::{restore, save};

    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-actions-cache");
    let config = CONFIG.replace(
        "maven = { max_size = \"5G\" }\n",
        "maven = { max_size = \"5G\" }\nbuild = { branches = [\"release\"], actions_cache = true }\n",
    );
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, &config);
    let [(main, main_socket), (release, release_socket)] = two_jobs(
        &github,
        &docker,
        &mut engine,
        [trigger("push", "main"), trigger("push", "release")],
    )
    .await;
    assert!(
        main_socket.join(buildcache::ACTIONS_MARKER).is_file(),
        "the config turned it on"
    );
    until_decided(&mut engine, &cache, &[&main, &release]).await;
    assert!(save(&main_socket, "deps", b"main's").await);
    assert!(save(&release_socket, "deps", b"release's").await);
    assert!(save(&main_socket, "tools", b"main's tools").await);
    assert_eq!(
        restore(&main_socket, "deps", &[]).await,
        Some(("deps".to_owned(), b"main's".to_vec())),
        "release's save is in release's scope"
    );
    assert_eq!(
        restore(&release_socket, "deps", &[]).await,
        Some(("deps".to_owned(), b"release's".to_vec()))
    );
    assert_eq!(
        restore(&release_socket, "tools", &[]).await,
        Some(("tools".to_owned(), b"main's tools".to_vec())),
        "a branch falls back to the default branch's entries"
    );
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

/// `CONFIG` with o/r letting its own branches' pull requests write.
fn pull_requests_write() -> String {
    CONFIG.replace(
        "warm = 0\n",
        "warm = 0\nbuild_cache_writers = \"pull-requests\"\n",
    )
}

/// A `pull_request` run of o/r's branch `feature`, for pull request 7, that a person started.
fn pull_request_run() -> github::RunTrigger {
    github::RunTrigger {
        pull_requests: vec![7],
        ..trigger("pull_request", "feature")
    }
}

/// Pull request 7 as GitHub describes it: a person opened it from o/r's branch `feature`.
fn persons_pull_request() -> github::PullRequest {
    github::PullRequest {
        by_user: true,
        head_repository: Some("o/r".to_owned()),
        head_branch: "feature".to_owned(),
    }
}

#[tokio::test]
async fn a_persons_pull_request_writes_where_the_repository_lets_it_and_main_hits() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-pull-writes");
    let (mut engine, _handle, cache) =
        engine_with_cache(&github, &docker, &dir, &pull_requests_write());
    github
        .0
        .lock()
        .unwrap()
        .pulls
        .insert(7, persons_pull_request());
    let [(pull, pull_socket), (main, main_socket)] = two_jobs(
        &github,
        &docker,
        &mut engine,
        [pull_request_run(), trigger("push", "main")],
    )
    .await;
    // The pull request's last run stores its outputs before GitHub has said anything of it.
    assert_eq!(
        buildcache::request(&pull_socket, "PUT", "/gradle/0a1b", b"classes")
            .await
            .0,
        200
    );
    until_decided(&mut engine, &cache, &[&pull, &main]).await;
    let calls = github.calls();
    assert!(
        calls.contains(&"pull request 7".to_owned())
            && calls.contains(&"compare feature...c0ffee".to_owned()),
        "{calls:?}"
    );
    assert_eq!(
        buildcache::request(&main_socket, "GET", "/gradle/0a1b", b"").await,
        (200, b"classes".to_vec())
    );
}

#[tokio::test]
async fn forks_bots_and_repositories_that_did_not_opt_in_only_read() {
    let bots_pull_request = github::PullRequest {
        by_user: false,
        ..persons_pull_request()
    };
    let cases = [
        (
            "a fork's pull request, which GitHub ties to no pull request of o/r",
            pull_requests_write(),
            github::RunTrigger {
                head_repository: Some("fork/r".to_owned()),
                pull_requests: Vec::new(),
                ..pull_request_run()
            },
            persons_pull_request(),
        ),
        (
            "Renovate's pull request",
            pull_requests_write(),
            pull_request_run(),
            bots_pull_request,
        ),
        (
            "a run a bot pushed to a person's pull request",
            pull_requests_write(),
            github::RunTrigger {
                by_user: false,
                ..pull_request_run()
            },
            persons_pull_request(),
        ),
        (
            "a person's pull request where o/r did not opt in",
            CONFIG.to_owned(),
            pull_request_run(),
            persons_pull_request(),
        ),
        (
            "a commit the pull request's branch does not have",
            pull_requests_write(),
            github::RunTrigger {
                head_sha: "7a9".to_owned(),
                ..pull_request_run()
            },
            persons_pull_request(),
        ),
    ];
    for (case, config, run, pull_request) in cases {
        let github = FakeGithub::default();
        let docker = FakeDocker::default();
        let dir = TestDir::new("engine-build-cache-pull-reads");
        let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, &config);
        {
            let mut state = github.0.lock().unwrap();
            state.pulls.insert(7, pull_request);
            state.off_branch.push("7a9".to_owned());
        }
        let [(pull, pull_socket), (main, main_socket)] = two_jobs(
            &github,
            &docker,
            &mut engine,
            [run, trigger("push", "main")],
        )
        .await;
        assert_eq!(
            buildcache::request(&pull_socket, "PUT", "/gradle/ffff", b"poison")
                .await
                .0,
            200,
            "{case}"
        );
        until_decided(&mut engine, &cache, &[&pull, &main]).await;
        assert_eq!(
            buildcache::request(&main_socket, "GET", "/gradle/ffff", b"")
                .await
                .0,
            404,
            "{case} stored nothing"
        );
    }
}

#[tokio::test]
async fn a_pull_request_github_cannot_describe_never_writes() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-pull-lookup");
    let (mut engine, _handle, cache) =
        engine_with_cache(&github, &docker, &dir, &pull_requests_write());
    // GitHub does not answer for pull request 7.
    let [(pull, pull_socket), (main, main_socket)] = two_jobs(
        &github,
        &docker,
        &mut engine,
        [pull_request_run(), trigger("push", "main")],
    )
    .await;
    assert_eq!(
        buildcache::request(&pull_socket, "PUT", "/gradle/ffff", b"poison")
            .await
            .0,
        200
    );
    for _ in 0..5 {
        engine.tick(Shutdown::No).await;
    }
    assert!(!cache.undecided(&main), "main was decided");
    assert!(
        cache.undecided(&pull),
        "the pull request waits for GitHub to describe it"
    );
    assert_eq!(
        buildcache::request(&main_socket, "GET", "/gradle/ffff", b"")
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn a_main_job_that_ends_before_its_writes_were_decided_keeps_what_it_stored() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache");
    let (mut engine, handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    let [(main, main_socket), _] = main_and_pull_request(&github, &docker, &mut engine).await;
    assert!(cache.undecided(&main), "GitHub has not been asked yet");
    // A quick job: it stores its outputs and ends before the next poll asks GitHub about it.
    assert_eq!(
        buildcache::request(&main_socket, "PUT", "/gradle/0a1b", b"classes")
            .await
            .0,
        200
    );
    jobs_end(&docker, &mut engine).await;
    handle.store.flush().await;
    let counts = handle
        .store
        .job(1)
        .await
        .unwrap()
        .unwrap()
        .summary
        .metrics
        .build_cache;
    assert!(counts.writer, "the push to main wrote");
    assert_eq!((counts.written_bytes, counts.refused), (7, 0));
}

#[tokio::test]
async fn jobs_that_end_having_stored_nothing_are_not_asked_about_on_github() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-nothing");
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    let [(main, _), (pull, _)] = main_and_pull_request(&github, &docker, &mut engine).await;
    assert!(
        cache.undecided(&main) && cache.undecided(&pull),
        "GitHub has not been asked yet"
    );
    // The first tick's visibility ask runs in a task of its own; let it finish so its call is not counted below.
    if let Some(asking) = engine.visibility_asking.take() {
        asking.await.unwrap();
    }
    let before = github.calls().len();
    jobs_end(&docker, &mut engine).await;
    let asked: Vec<String> = github.calls()[before..]
        .iter()
        // Only the polls and the registrations' removal.
        .filter(|call| {
            !["delete ", "runners ", "queued "]
                .iter()
                .any(|polls| call.starts_with(polls))
        })
        .cloned()
        .collect();
    assert_eq!(
        asked,
        Vec::<String>::new(),
        "nothing to keep, nothing to ask"
    );
}

#[tokio::test]
async fn a_stalled_github_does_not_hold_commands_while_an_ended_jobs_uploads_wait() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-stalled");
    let (mut engine, handle, _cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    let [(_, main_socket), _] = main_and_pull_request(&github, &docker, &mut engine).await;
    assert_eq!(
        buildcache::request(&main_socket, "PUT", "/gradle/0a1b", b"classes")
            .await
            .0,
        200
    );
    github.hold_jobs();
    let control = handle.control.clone();
    tokio::join!(jobs_end(&docker, &mut engine), async {
        github.until_waiting(1).await;
        let answered =
            tokio::time::timeout(Duration::from_secs(5), control.resume(one("o/r"))).await;
        github.release_jobs();
        answered.expect("the resume answers").unwrap();
    });
    handle.store.flush().await;
    let counts = handle
        .store
        .job(1)
        .await
        .unwrap()
        .unwrap()
        .summary
        .metrics
        .build_cache;
    assert!(counts.writer, "the push to main wrote once GitHub answered");
    assert_eq!((counts.written_bytes, counts.refused), (7, 0));
}

#[tokio::test]
async fn a_default_branch_renamed_while_runnerd_runs_writes_within_the_hour() {
    let github = FakeGithub::default();
    let docker = FakeDocker::default();
    let dir = TestDir::new("engine-build-cache-renamed");
    let (mut engine, _handle, cache) = engine_with_cache(&github, &docker, &dir, CONFIG);
    github.0.lock().unwrap().default_branch = Some("master".to_owned());
    let pushes = || [trigger("push", "master"), trigger("push", "main")];
    let [(master, _), (main, _)] = two_jobs(&github, &docker, &mut engine, pushes()).await;
    until_decided(&mut engine, &cache, &[&master, &main]).await;
    // The reseed slot 1's job makes due finishes.
    docker.0.lock().unwrap().seed_exit = Some(0);
    jobs_end(&docker, &mut engine).await;

    // The repository renames master to main; an hour on, runnerd asks GitHub again.
    github.0.lock().unwrap().default_branch = Some("main".to_owned());
    for known in engine.default_branches.values_mut() {
        known.read_at -= Duration::from_secs(3600);
    }
    let [(master, master_socket), (main, main_socket)] =
        two_more_jobs(&github, &docker, &mut engine, 3, pushes()).await;
    until_decided(&mut engine, &cache, &[&master, &main]).await;
    for (socket, key) in [(&master_socket, "aa"), (&main_socket, "bb")] {
        let path = format!("/gradle/{key}");
        assert_eq!(
            buildcache::request(socket, "PUT", &path, b"classes")
                .await
                .0,
            200
        );
    }
    let found = async |key: &str| {
        buildcache::request(&main_socket, "GET", &format!("/gradle/{key}"), b"")
            .await
            .0
    };
    assert_eq!(
        (found("bb").await, found("aa").await),
        (200, 404),
        "main writes, master no longer does"
    );
}
