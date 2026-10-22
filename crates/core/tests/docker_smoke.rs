//! Starts a runner container on the local Docker Engine and runs a container inside it. Needs the isolation
//! backend installed and the runner image built (`docker build --target trellis -t runnerd-runner:trellis image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test docker_smoke -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_ISOLATION=unsafe-dind` tests privileged Docker-in-Docker instead of sysbox, and
//! `RUNNERD_TEST_IMAGE` another image.

use std::time::Duration;

use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{ExecConfig, Mount, MountType, VolumeCreateRequest};
use futures_util::StreamExt;
use runnerd_core::config::Isolation;
use runnerd_core::docker::{Docker, JIT_CONFIG_PATH, JitConfig, RunnerSpec};
use runnerd_core::isolation::backend;

const JIT_CONFIG: &str = "smoke-test-jit-config";

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and the runner image"]
async fn runner_container_runs_containers_of_its_own() {
    with_runner("ok", async |api, name| {
        let (code, output) = exec(api, name, &["stat", "-c", "%a %U", JIT_CONFIG_PATH]).await?;
        if (code, output.trim()) != (0, "400 root") {
            return Err(format!(
                "{JIT_CONFIG_PATH}: expected mode 400 owned by root, got {code}: {output}"
            ));
        }
        let (_, output) = exec(api, name, &["cat", JIT_CONFIG_PATH]).await?;
        if output != JIT_CONFIG {
            return Err(format!("{JIT_CONFIG_PATH} holds {output:?}"));
        }
        let (code, output) = exec(api, name, &["docker", "run", "--rm", "hello-world"]).await?;
        if code != 0 || !output.contains("Hello from Docker!") {
            return Err(format!("docker run hello-world exited {code}:\n{output}"));
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and the runner image"]
async fn a_failed_check_still_removes_the_runner() {
    let name = runner_name("fail");
    let result = with_runner("fail", async |api, name| {
        match exec(api, name, &["false"]).await? {
            (0, _) => Ok(()),
            (code, _) => Err(format!("false exited {code}")),
        }
    })
    .await;
    assert!(result.is_err());
    let api = bollard::Docker::connect_with_local_defaults().unwrap();
    assert!(
        api.inspect_container(&name, None).await.is_err(),
        "{name} was not removed"
    );
    assert!(
        api.inspect_volume(&name).await.is_err(),
        "volume {name} was not removed"
    );
}

fn runner_name(test: &str) -> String {
    format!("runnerd-test-smoke-{}-{test}", std::process::id())
}

/// Starts a runner container with only its Docker Engine, waits for the Engine, and runs `check`. Removes the
/// container and its volume whatever happened, then returns the first error.
async fn with_runner(
    test: &str,
    check: impl AsyncFnOnce(&bollard::Docker, &str) -> Result<(), String>,
) -> Result<(), String> {
    let isolation = match std::env::var("RUNNERD_TEST_ISOLATION").as_deref() {
        Ok("unsafe-dind") => Isolation::UnsafeDind,
        Ok("sysbox") | Err(_) => Isolation::Sysbox,
        Ok(other) => {
            return Err(format!(
                "RUNNERD_TEST_ISOLATION={other:?}: expected sysbox or unsafe-dind"
            ));
        }
    };
    let isolation = backend(isolation);
    let name = runner_name(test);
    let api = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
    let docker = Docker::connect().map_err(|e| e.to_string())?;
    api.create_volume(VolumeCreateRequest {
        name: Some(name.clone()),
        ..Default::default()
    })
    .await
    .map_err(|e| e.to_string())?;
    let spec = RunnerSpec {
        name: name.clone(),
        image: std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:trellis".to_owned()),
        profile: "smoke".to_owned(),
        // Docker's default slice: outside the runners' budget, and it exists already, so the test leaves no unit.
        slice: "system.slice".to_owned(),
        cpus: 2,
        memory: "2G".parse().unwrap(),
        env: Vec::new(),
        mounts: vec![Mount {
            target: Some("/state".to_owned()),
            source: Some(name.clone()),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        }],
        network: None,
        labels: Default::default(),
        // The container's Docker Engine alone, so the JIT config stays where `start` put it.
        entrypoint: Some(vec!["dind".to_owned(), "dockerd".to_owned()]),
        jit_config: Some(JitConfig(JIT_CONFIG.to_owned())),
    };

    let result = async {
        docker
            .start(&spec, isolation)
            .await
            .map_err(|e| e.to_string())?;
        let state = docker.inspect(&name).await.map_err(|e| e.to_string())?;
        println!("{name}: {state:?}");
        if !state.running || state.privileged != isolation.is_unsafe() {
            return Err(format!(
                "unexpected state {state:?} under {}",
                isolation.name()
            ));
        }
        let mut ready = false;
        for _ in 0..60 {
            if exec(&api, &name, &["docker", "info"]).await?.0 == 0 {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !ready {
            return Err("the container's Docker Engine did not start within 60s".to_owned());
        }
        check(&api, &name).await
    }
    .await;

    let removed = docker.remove(&name).await.map_err(|e| e.to_string());
    let volume_removed = api
        .remove_volume(
            &name,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .map_err(|e| e.to_string());
    result.and(removed).and(volume_removed)
}

/// Runs a command in the container; returns its exit code and output.
async fn exec(
    api: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<(i64, String), String> {
    let run = async {
        let exec = api
            .create_exec(
                container,
                ExecConfig {
                    cmd: Some(cmd.iter().map(|arg| (*arg).to_owned()).collect()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await?;
        let mut text = String::new();
        if let StartExecResults::Attached { mut output, .. } =
            api.start_exec(&exec.id, None::<StartExecOptions>).await?
        {
            while let Some(chunk) = output.next().await {
                text.push_str(&chunk?.to_string());
            }
        }
        let code = api.inspect_exec(&exec.id).await?.exit_code.unwrap_or(-1);
        Ok::<_, bollard::errors::Error>((code, text))
    };
    run.await.map_err(|e| format!("{cmd:?}: {e}"))
}
