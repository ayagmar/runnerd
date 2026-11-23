//! Where a runner keeps pnpm's store, on the fakes (ADR 0167): by default the slot's own, which its environment asks
//! of its image and beside which it mounts no shared store; the shared one, mounted and never asked for, for a
//! profile that keeps records, an image from before the slot's store, and `store = "shared"`.

use super::*;

/// Where the shared store is mounted in a runner.
const SHARED: &str = "/shared/pnpm";

/// What the runner a queued job starts gets under `config`, with Docker's images as `docker` has them: whether it
/// mounts the shared store, and its variables that ask for the slot's own.
async fn first_runner(config: &str, docker: &FakeDocker) -> (bool, Vec<String>) {
    let (github, dir) = (FakeGithub::default(), TestDir::new("pnpm-store"));
    let (mut engine, _handle) = engine_with(&github, docker, &dir, config);
    engine.recover().await.unwrap();
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    let state = docker.0.lock().unwrap();
    let (name, mounts) = state.mounts.iter().next().expect("a runner started");
    let shared = mounts
        .iter()
        .any(|mount| mount.target.as_deref() == Some(SHARED));
    let asked = state.envs[name]
        .iter()
        .filter(|var| var.starts_with(docker::SLOT_PNPM_STORE_ENV))
        .cloned()
        .collect();
    (shared, asked)
}

// Catches: the variable sent while the shared store stays mounted, so a job still writes the store every other
// profile reads, or the mount dropped without the variable, so pnpm fills a store on the container's disk every job.
#[tokio::test]
async fn a_runner_keeps_its_slots_own_pnpm_store_within_the_limit_and_mounts_no_shared_one() {
    let (shared, asked) = first_runner(CONFIG, &FakeDocker::default()).await;
    assert!(!shared, "a slot's own store comes without the shared one");
    let limit = Config::parse(CONFIG).unwrap().caches.pnpm.max_size.0;
    assert_eq!(
        asked,
        [format!("{}={limit}", docker::SLOT_PNPM_STORE_ENV)],
        "the image is asked for the slot's store, with pnpm.max_size as its limit"
    );
}

// Catches: a profile that keeps records getting the slot's store, whose workspace no record holds; an image from
// before the slot's store being asked for one, so its store lands on the container's disk and starts cold each job;
// the opt-out not taken.
#[tokio::test]
async fn records_an_older_image_and_the_opt_out_keep_the_shared_pnpm_store() {
    let older = FakeDocker::default();
    older
        .0
        .lock()
        .unwrap()
        .lacking_slot_pnpm_store
        .insert("runnerd-runner:app".to_owned());
    let cases = [
        (
            "a profile that keeps records",
            CONFIG.replace("slots = 2", "slots = 2\nrecord_failures = true"),
            FakeDocker::default(),
        ),
        ("an image without the label", CONFIG.to_owned(), older),
        (
            "store = \"shared\"",
            CONFIG.replace(
                "pnpm = { max_size = \"5G\" }",
                "pnpm = { max_size = \"5G\", store = \"shared\" }",
            ),
            FakeDocker::default(),
        ),
    ];
    for (case, config, docker) in cases {
        let (shared, asked) = first_runner(&config, &docker).await;
        assert!(shared, "{case}: the runner mounts the shared store");
        assert!(
            asked.is_empty(),
            "{case}: nothing asks for a slot's store: {asked:?}"
        );
    }
}

// Catches: a Docker that could not answer read as an image without the slot's store, so the runner goes to the shared
// store and its entrypoint removes the slot's: one timed-out inspect costs the slot its packages.
#[tokio::test]
async fn a_runner_whose_image_docker_cannot_tell_about_waits_and_the_slots_store_stays() {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("pnpm-store-inspect"),
    );
    docker.0.lock().unwrap().failing_image_inspects = true;
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, CONFIG);
    engine.recover().await.unwrap();
    {
        let mut state = github.0.lock().unwrap();
        state.queued = vec![job(1)];
        state.jobs.insert(1, details(1, "in_progress", None));
    }
    engine.tick(Shutdown::No).await;
    assert!(
        docker.0.lock().unwrap().envs.is_empty(),
        "no runner starts while Docker cannot say what its image does"
    );

    docker.0.lock().unwrap().failing_image_inspects = false;
    engine.tick(Shutdown::No).await;
    let state = docker.0.lock().unwrap();
    let env = state
        .envs
        .values()
        .next()
        .expect("a runner started once Docker answered");
    assert!(
        env.iter()
            .any(|var| var.starts_with(docker::SLOT_PNPM_STORE_ENV)),
        "it keeps the slot's store: {env:?}"
    );
}

// Catches: a failed inspect holding back runners whose pnpm store does not depend on it, as before the slot's store.
#[tokio::test]
async fn a_runner_of_the_shared_store_starts_though_docker_cannot_tell_about_its_image() {
    let docker = FakeDocker::default();
    docker.0.lock().unwrap().failing_image_inspects = true;
    let config = CONFIG.replace(
        "pnpm = { max_size = \"5G\" }",
        "pnpm = { max_size = \"5G\", store = \"shared\" }",
    );
    let (shared, asked) = first_runner(&config, &docker).await;
    assert!(
        shared && asked.is_empty(),
        "it starts on the shared store: {asked:?}"
    );
}
