//! Starts a runner container on the local Docker Engine and runs a container inside it, and checks where its runner's
//! JIT config shows. Needs the isolation backend installed and the runner image built
//! (`docker build --target jvm -t runnerd-runner:jvm image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test docker_smoke -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_ISOLATION=unsafe-dind` tests privileged Docker-in-Docker instead of sysbox, and
//! `RUNNERD_TEST_IMAGE` another image.

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, these tests show what the containers report"
)]

use std::collections::BTreeMap;
use std::time::Duration;

use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{ExecConfig, Mount, MountType, VolumeCreateRequest};
use futures_util::StreamExt;
use runnerd_core::config::{ByteSize, Isolation};
use runnerd_core::docker::{Docker, DockerApi, JIT_CONFIG_PATH, JitConfig, RunnerSpec};
use runnerd_core::isolation::backend;

const JIT_CONFIG: &str = "smoke-test-jit-config";

/// A JIT config the runner's listener takes, encoded as GitHub encodes one: a `.runner` for a server at
/// `127.0.0.1:9` and OAuth `.credentials` without their key, so the listener fails to start and is started again
/// every 5 s, for as long as the test looks.
const LISTENER_JIT_CONFIG: &str = "eyIucnVubmVyIjoiZXlKaFoyVnVkRWxrSWpveExDSmhaMlZ1ZEU1aGJXVWlPaUp5ZFc1dVpYSmtMWFJsYzNRdGMyMXZhMlVpTENKd2IyOXNTV1FpT2pFc0luTmxjblpsY2xWeWJDSTZJbWgwZEhBNkx5OHhNamN1TUM0d0xqRTZPUzhpTENKbmFYUklkV0pWY213aU9pSm9kSFJ3T2k4dk1USTNMakF1TUM0eE9qa3ZieTl5SWl3aWQyOXlhMFp2YkdSbGNpSTZJbDkzYjNKcklpd2laWEJvWlcxbGNtRnNJanAwY25WbGZRPT0iLCIuY3JlZGVudGlhbHMiOiJleUp6WTJobGJXVWlPaUpQUVhWMGFDSXNJbVJoZEdFaU9uc2lZMnhwWlc1MFNXUWlPaUl3TURBd01EQXdNQzB3TURBd0xUQXdNREF0TURBd01DMHdNREF3TURBd01EQXdNREFpTENKaGRYUm9iM0pwZW1GMGFXOXVWWEpzSWpvaWFIUjBjRG92THpFeU55NHdMakF1TVRvNUwzUnZhMlZ1SW4xOSJ9";

/// The container's Docker Engine alone, so the JIT config stays where `start` put it.
fn engine_only() -> Vec<String> {
    vec!["dind".to_owned(), "dockerd".to_owned()]
}

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and the runner image"]
async fn runner_container_runs_containers_of_its_own() {
    with_runner("ok", Some(engine_only()), JIT_CONFIG, async |api, name| {
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
    let result = with_runner(
        "fail",
        Some(engine_only()),
        JIT_CONFIG,
        async |api, name| match exec(api, name, &["false"]).await? {
            (0, _) => Ok(()),
            (code, _) => Err(format!("false exited {code}")),
        },
    )
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

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and the runner image"]
async fn no_process_shows_the_jit_config_in_its_arguments_and_jobs_do_not_inherit_it() {
    with_runner("jit", None, LISTENER_JIT_CONFIG, async |api, name| {
        until_listener_started(api, name).await?;
        // The config is the search's environment, which only root reads, not its arguments.
        let needle = format!("NEEDLE={LISTENER_JIT_CONFIG}");
        let search = r#"for f in /proc/[0-9]*/cmdline; do
            c=$(tr '\0' ' ' < "$f" 2> /dev/null) || continue
            [[ $c == *"$NEEDLE"* ]] && echo "${f%/cmdline}: ${c:0:40}"
        done; true"#;
        let (_, inside) = exec_as(api, name, "root", &[&needle], &["bash", "-c", search]).await?;
        if !inside.trim().is_empty() {
            return Err(format!("the JIT config is in the arguments of {inside}"));
        }
        // Every user of the host reads every process's arguments.
        if let Some(process) = host_process_with(LISTENER_JIT_CONFIG.as_bytes())? {
            return Err(format!("the JIT config is in the arguments of host process {process}"));
        }
        let taken = ["grep", "-qrF", "Removing env var: 'ACTIONS_RUNNER_INPUT_JITCONFIG'", "/home/runner/_diag"];
        if exec(api, name, &taken).await?.0 != 0 {
            return Err("the listener did not take the JIT config from its environment".to_owned());
        }
        // A process the listener starts, as its job's steps are, inherits its environment but not the config.
        let started = r#"bin=$(mktemp -d)
            printf '#!/bin/sh\nenv > %s/env\n' "$bin" > "$bin/git"
            chmod +x "$bin/git"
            PATH="$bin:$PATH" ./bin/Runner.Listener --check --url http://127.0.0.1:9/o/r --pat none > /dev/null
            cat "$bin/env""#;
        let env = [
            "ACTIONS_RUNNER_INPUT_JITCONFIG=a-jit-config",
            "RUNNERD_INHERITED=yes",
        ];
        let (_, inherited) = exec_as(api, name, "runner", &env, &["bash", "-c", started]).await?;
        if !inherited.contains("RUNNERD_INHERITED=yes")
            || inherited.contains("ACTIONS_RUNNER_INPUT_JITCONFIG")
        {
            return Err(format!("the listener's child has the environment:\n{inherited}"));
        }
        Ok(())
    })
    .await
    .unwrap();
}

/// Waits until the runner's listener started, once its log is there.
async fn until_listener_started(api: &bollard::Docker, name: &str) -> Result<(), String> {
    for _ in 0..60 {
        let log = ["sh", "-c", "ls /home/runner/_diag/Runner_*.log"];
        if exec(api, name, &log).await?.0 == 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err("the runner's listener did not start within 60s".to_owned())
}

/// The ID of a process of the host whose arguments hold `needle`.
fn host_process_with(needle: &[u8]) -> Result<Option<String>, String> {
    for entry in std::fs::read_dir("/proc")
        .map_err(|e| e.to_string())?
        .flatten()
    {
        // Not a process, or one that ended.
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline.windows(needle.len()).any(|window| window == needle) {
            return Ok(Some(entry.file_name().to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

fn runner_name(test: &str) -> String {
    format!("runnerd-test-smoke-{}-{test}", std::process::id())
}

/// Starts a runner container with `entrypoint`, the image's when `None`, and `jit_config`; waits for its Docker Engine,
/// and runs `check`. Removes the container and its volume whatever happened, then returns the first error.
async fn with_runner(
    test: &str,
    entrypoint: Option<Vec<String>>,
    jit_config: &str,
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
        image: std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:jvm".to_owned()),
        profile: "smoke".to_owned(),
        // Docker's default slice: outside the runners' budget, and it exists already, so the test leaves no unit.
        slice: "system.slice".to_owned(),
        cpus: 2,
        memory: ByteSize(2 << 30),
        env: Vec::new(),
        mounts: vec![Mount {
            target: Some("/state".to_owned()),
            source: Some(name.clone()),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        }],
        network: None,
        labels: BTreeMap::new(),
        entrypoint,
        jit_config: Some(JitConfig(jit_config.to_owned())),
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

/// Runs a command in the container as root; returns its exit code and output.
async fn exec(
    api: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<(i64, String), String> {
    exec_as(api, container, "root", &[], cmd).await
}

/// Runs a command in the container as `user` in its home, with `env` (`NAME=value`) added to the container's
/// environment; returns its exit code and output.
async fn exec_as(
    api: &bollard::Docker,
    container: &str,
    user: &str,
    env: &[&str],
    cmd: &[&str],
) -> Result<(i64, String), String> {
    let run = async {
        let exec = api
            .create_exec(
                container,
                ExecConfig {
                    cmd: Some(cmd.iter().map(|arg| (*arg).to_owned()).collect()),
                    env: Some(env.iter().map(|var| (*var).to_owned()).collect()),
                    user: Some(user.to_owned()),
                    working_dir: (user == "runner").then(|| "/home/runner".to_owned()),
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
