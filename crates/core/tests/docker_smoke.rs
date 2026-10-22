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
use runnerd_core::docker::{Docker, RunnerSpec};
use runnerd_core::isolation::backend;

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and the runner image"]
async fn runner_container_runs_containers_of_its_own() {
    let isolation = match std::env::var("RUNNERD_TEST_ISOLATION").as_deref() {
        Ok("unsafe-dind") => Isolation::UnsafeDind,
        Ok("sysbox") | Err(_) => Isolation::Sysbox,
        Ok(other) => panic!("RUNNERD_TEST_ISOLATION={other:?}: expected sysbox or unsafe-dind"),
    };
    let isolation = backend(isolation);
    let name = format!("runnerd-test-smoke-{}", std::process::id());
    let api = bollard::Docker::connect_with_local_defaults().unwrap();
    api.create_volume(VolumeCreateRequest {
        name: Some(name.clone()),
        ..Default::default()
    })
    .await
    .unwrap();
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
        // The container's Docker Engine alone: the runner needs a just-in-time config from GitHub.
        entrypoint: Some(vec!["dind".to_owned(), "dockerd".to_owned()]),
    };
    let docker = Docker::connect().unwrap();

    let result = async {
        docker.start(&spec, isolation).await?;
        let state = docker.inspect(&name).await?;
        println!("{name}: {state:?}");
        assert!(state.running);
        assert_eq!(state.privileged, isolation.is_unsafe());

        let mut ready = false;
        for _ in 0..60 {
            if exec(&api, &name, &["docker", "info"]).await?.0 == 0 {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert!(
            ready,
            "the container's Docker Engine did not start within 60s"
        );

        let (code, output) = exec(&api, &name, &["docker", "run", "--rm", "hello-world"]).await?;
        assert_eq!(code, 0, "docker run hello-world failed:\n{output}");
        assert!(output.contains("Hello from Docker!"), "{output}");
        Ok::<_, bollard::errors::Error>(())
    }
    .await;

    let removed = docker.remove(&name).await;
    let volume_removed = api
        .remove_volume(
            &name,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await;
    result.unwrap();
    removed.unwrap();
    volume_removed.unwrap();
}

/// Runs a command in the container; returns its exit code and output.
async fn exec(
    api: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<(i64, String), bollard::errors::Error> {
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
    Ok((code, text))
}
