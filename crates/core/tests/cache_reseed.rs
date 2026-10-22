//! A slot's cache changes reach the other slots through a reseed. Needs Docker, sysbox and the runner image
//! (`docker build --target base -t runnerd-runner:base image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test cache_reseed -- --ignored --nocapture
//! ```

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, these tests show what the containers report"
)]

use std::collections::BTreeMap;

use runnerd_core::cache::{self, Names, Reseed, runner_mounts};
use runnerd_core::docker::{Docker, DockerApi, RunnerSpec};
use runnerd_core::isolation::Sysbox;

const PROFILE: &str = "cache";
const IMAGE: &str = "runnerd-runner:base";

/// Runs `script` as root in a runner container of `slot` with its layers mounted; returns its exit code and output.
async fn run_in_slot(
    docker: &Docker,
    names: &Names,
    slot: usize,
    script: &str,
) -> Result<(i64, String), String> {
    let name = format!("{}-slot-{slot}-job", names.network());
    let spec = RunnerSpec {
        name: name.clone(),
        image: IMAGE.to_owned(),
        profile: PROFILE.to_owned(),
        slice: "system.slice".to_owned(),
        cpus: 1,
        memory: "1G".parse().map_err(|e: String| e)?,
        env: Vec::new(),
        mounts: runner_mounts(names, PROFILE, slot),
        network: Some("none".to_owned()),
        labels: BTreeMap::new(),
        entrypoint: Some(vec![
            "bash".to_owned(),
            "-euo".to_owned(),
            "pipefail".to_owned(),
            "-c".to_owned(),
            format!(". /usr/local/lib/runnerd-layers.sh; mount_layers; {script}"),
        ]),
        jit_config: None,
    };
    let id = docker
        .start(&spec, &Sysbox)
        .await
        .map_err(|e| e.to_string())?;
    let result = async {
        let code = docker.wait(&id).await?;
        Ok((code, docker.logs(&id).await?))
    }
    .await
    .map_err(|e: runnerd_core::docker::Error| e.to_string());
    let removed = docker.remove(&id).await.map_err(|e| e.to_string());
    let result = result?;
    removed?;
    Ok(result)
}

async fn scenario(docker: &Docker, names: &Names) -> Result<(), String> {
    for slot in [1, 2] {
        cache::prepare(docker, names, PROFILE, slot)
            .await
            .map_err(|e| e.to_string())?;
    }
    let as_runner = "setpriv --reuid=runner --regid=runner --init-groups --";
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!("{as_runner} sh -c 'echo warm > /home/runner/.cache/runnerd-test'"),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }

    let reseed = |min_changes: &str| Reseed {
        names,
        profile: PROFILE,
        image: IMAGE,
        slots: 2,
        slice: "system.slice",
        cpus: 1,
        memory: "1G"
            .parse()
            .unwrap_or(runnerd_core::config::ByteSize(1 << 30)),
        min_changes: min_changes
            .parse()
            .unwrap_or(runnerd_core::config::ByteSize(0)),
        isolation: &Sysbox,
    };
    let output = cache::reseed(docker, &reseed("1T"))
        .await
        .map_err(|e| e.to_string())?;
    if !output.contains("not seeded") {
        return Err(format!("seeded below the threshold:\n{output}"));
    }
    let output = cache::reseed(docker, &reseed("0K"))
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    if !output.contains("seeded cache") {
        return Err(format!("slot 1's cache layer was not seeded:\n{output}"));
    }

    // Slot 2 sees the file through the snapshot, which it cannot change, and has no copy of its own.
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        "test \"$(cat /home/runner/.cache/runnerd-test)\" = warm \
         && test -f /base/cache/runnerd-test \
         && test ! -e /state/cache/upper/runnerd-test \
         && ! touch /base/cache/runnerd-test 2> /dev/null",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "slot 2 does not see the snapshot (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker, sysbox and the runner image"]
async fn a_reseed_shares_slot_1s_cache_with_slot_2_read_only() {
    let names = Names::new(&format!("runnerd-test-{}", std::process::id()));
    let docker = Docker::connect().unwrap();
    let result = scenario(&docker, &names).await;

    let api = bollard::Docker::connect_with_local_defaults().unwrap();
    let mut cleanup = Vec::new();
    for volume in [
        names.base(PROFILE),
        names.slot(PROFILE, 1),
        names.slot(PROFILE, 2),
        names.pnpm(),
        names.maven(),
    ] {
        cleanup.push(
            docker
                .remove_volume(&volume)
                .await
                .map_err(|e| e.to_string()),
        );
    }
    cleanup.push(
        api.remove_network(&names.network())
            .await
            .map_err(|e| e.to_string()),
    );
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
}
