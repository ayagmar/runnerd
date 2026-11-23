//! The Grype database on the fakes (ADR 0160): a runner started while it is served mounts the generation read-only,
//! with Grype's settings pointing at it, and its container names that generation, so it is not removed under it; one
//! started while it is off gets none of them.

use super::*;
use crate::grype;

/// A generation as the store names one.
const GENERATION: &str = "1795415312-3d24f57cd358";

/// The one runner a queued job started, with the database served from `dir` unless `config` turns it off: its mount
/// at `grype::MOUNT`, its Grype settings and its container's `grype::LABEL`.
async fn first_runner(
    config: &str,
) -> (Option<bollard::models::Mount>, Vec<String>, Option<String>) {
    let (github, docker, dir) = (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("grype"),
    );
    let (mut engine, _handle) = engine_with(&github, &docker, &dir, config);
    engine
        .grype
        .serve(&dir.0.join("grype/dbs").join(GENERATION), GENERATION);
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
        .find(|mount| mount.target.as_deref() == Some(grype::MOUNT))
        .cloned();
    let env = state.envs[name]
        .iter()
        .filter(|env| env.starts_with("GRYPE_"))
        .cloned()
        .collect();
    let label = state
        .containers
        .iter()
        .find(|container| &container.name == name)
        .and_then(|container| container.labels.get(grype::LABEL).cloned());
    (mount, env, label)
}

#[tokio::test]
async fn a_runner_reads_the_grype_database_read_only_and_names_its_generation() {
    let (mount, env, label) = first_runner(CONFIG).await;
    let mount = mount.expect("the database is mounted");
    assert_eq!(
        mount.read_only,
        Some(true),
        "a job cannot change the database"
    );
    assert!(
        mount
            .source
            .as_deref()
            .is_some_and(|source| source.ends_with(&format!("grype/dbs/{GENERATION}"))),
        "one generation, never staging/: {mount:?}"
    );
    assert_eq!(
        env,
        [
            "GRYPE_DB_CACHE_DIR=/opt/runnerd/grype",
            "GRYPE_DB_AUTO_UPDATE=false",
            "GRYPE_DB_VALIDATE_AGE=true",
        ]
    );
    assert_eq!(label.as_deref(), Some(GENERATION), "kept while it runs");
}

#[tokio::test]
async fn a_runner_gets_no_grype_database_while_it_is_off() {
    let off = format!("{CONFIG}\n[caches.grype]\nenabled = false\n");
    let (mount, env, label) = first_runner(&off).await;
    assert_eq!(mount, None, "no mount");
    assert!(env.is_empty(), "no Grype settings: {env:?}");
    assert_eq!(label, None);
}
