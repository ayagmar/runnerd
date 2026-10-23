//! A slot's cache changes reach the other slots through a reseed. Needs Docker, sysbox and the runner image
//! (`docker build --target base -t runnerd-runner:base image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test cache_reseed -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_IMAGE` runs another image.

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, these tests show what the containers report"
)]

use std::collections::BTreeMap;

use runnerd_core::cache::{self, Names, Reseed, runner_mounts};
use runnerd_core::docker::{Docker, DockerApi, RunnerSpec};
use runnerd_core::isolation::Sysbox;

const PROFILE: &str = "cache";
/// The runner image the containers run.
fn image() -> String {
    std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:base".to_owned())
}

/// A registry token a job leaves in cargo's home in the cache layer.
const CREDENTIALS: &str = "/home/runner/.cache/cargo/credentials.toml";

/// A registry token an older image's reseed left in the profile's snapshot.
const OLD_SNAPSHOT_CREDENTIALS: &str = "gradle/gradle.properties";

/// Settings both a custom image and an older image's snapshot hold.
const IMAGE_CONFIG: &str = "cargo/config.toml";

/// How a container mounts the profile's snapshot.
#[derive(Clone, Copy)]
enum Snapshot {
    /// As a runner does.
    ReadOnly,
    /// To plant what an older image's reseed left there.
    Writable,
}

/// Runs `script` as root in a runner container of `slot` with its layers mounted; returns its exit code and output.
async fn run_in_slot(
    docker: &Docker,
    names: &Names,
    slot: usize,
    script: &str,
) -> Result<(i64, String), String> {
    run(docker, names, slot, Snapshot::ReadOnly, "", script).await
}

/// Runs `script` as `run_in_slot` does, after `image_files` puts in the container what a custom image would hold
/// before the layers are mounted over it.
async fn run(
    docker: &Docker,
    names: &Names,
    slot: usize,
    snapshot: Snapshot,
    image_files: &str,
    script: &str,
) -> Result<(i64, String), String> {
    let name = format!("{}-slot-{slot}-job", names.network());
    let mut mounts = runner_mounts(names, PROFILE, slot);
    if let Snapshot::Writable = snapshot {
        for mount in &mut mounts {
            if mount.target.as_deref() == Some("/base") {
                mount.read_only = Some(false);
            }
        }
    }
    let spec = RunnerSpec {
        name: name.clone(),
        image: image(),
        profile: PROFILE.to_owned(),
        slice: "system.slice".to_owned(),
        cpus: 1,
        memory: "1G".parse().map_err(|e: String| e)?,
        env: Vec::new(),
        mounts,
        network: Some("none".to_owned()),
        labels: BTreeMap::new(),
        entrypoint: Some(vec![
            "bash".to_owned(),
            "-euo".to_owned(),
            "pipefail".to_owned(),
            "-c".to_owned(),
            format!("{image_files} . /usr/local/lib/runnerd-layers.sh; mount_layers; {script}"),
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
    let leave_credentials = format!(
        "{as_runner} sh -c 'mkdir -p /home/runner/.cache/cargo && echo token > {CREDENTIALS}'"
    );
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!(
            "{as_runner} sh -c 'echo warm > /home/runner/.cache/runnerd-test' && {leave_credentials}"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }

    let image = image();
    let reseed = |min_changes: &str| Reseed {
        names,
        profile: PROFILE,
        image: &image,
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

    // Slot 2 sees the file through the snapshot, which it cannot change, and has no copy of its own; the snapshot
    // holds no credentials.
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        &format!(
            "test \"$(cat /home/runner/.cache/runnerd-test)\" = warm \
             && test -f /base/cache/runnerd-test \
             && test ! -e /state/cache/upper/runnerd-test \
             && ! touch /base/cache/runnerd-test 2> /dev/null \
             && test ! -e {CREDENTIALS}"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "slot 2 does not see the snapshot, or sees slot 1's credentials (exit {code}):\n{output}"
        ));
    }

    // The next job in a slot does not see the credentials its last job left in the slot's layer.
    let (code, output) = run_in_slot(docker, names, 1, &leave_credentials).await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }
    let (code, output) = run_in_slot(docker, names, 1, &format!("test ! -e {CREDENTIALS}")).await?;
    if code != 0 {
        return Err(format!(
            "slot 1's next job sees its last job's credentials (exit {code}):\n{output}"
        ));
    }

    old_snapshot(docker, names, &reseed("1T")).await
}

/// After an upgrade: a snapshot seeded before the image dropped credentials still holds one, and stays, as the
/// profile does not reseed below the threshold. No slot's job sees it, a slot's second job neither, and the rest of
/// the snapshot stays warm. An image's own copy of such a file stays visible.
async fn old_snapshot(
    docker: &Docker,
    names: &Names,
    below_threshold: &Reseed<'_>,
) -> Result<(), String> {
    let (code, output) = run(
        docker,
        names,
        1,
        Snapshot::Writable,
        "",
        &format!(
            "mkdir -p /base/cache/gradle /base/cache/cargo \
             && echo signing.password=hunter2 > /base/cache/{OLD_SNAPSHOT_CREDENTIALS} \
             && echo snapshot > /base/cache/{IMAGE_CONFIG}"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "planting the old snapshot's credentials exited {code}:\n{output}"
        ));
    }
    let output = cache::reseed(docker, below_threshold)
        .await
        .map_err(|e| e.to_string())?;
    if !output.contains("not seeded") {
        return Err(format!("seeded below the threshold:\n{output}"));
    }
    let unseen = format!(
        "test ! -e /home/runner/.cache/{OLD_SNAPSHOT_CREDENTIALS} \
         && test \"$(cat /home/runner/.cache/runnerd-test)\" = warm"
    );
    for slot in [1, 2, 2] {
        let (code, output) = run_in_slot(docker, names, slot, &unseen).await?;
        if code != 0 {
            return Err(format!(
                "slot {slot} sees the old snapshot's credentials, or not its cache (exit {code}):\n{output}"
            ));
        }
    }

    // An image's own copy of a file the old snapshot also holds stays what a job sees, mount after mount.
    let image_files = format!(
        "mkdir -p /home/runner/.cache/cargo && echo image > /home/runner/.cache/{IMAGE_CONFIG};"
    );
    for _ in 0..2 {
        let (code, output) = run(
            docker,
            names,
            1,
            Snapshot::ReadOnly,
            &image_files,
            &format!("test \"$(cat /home/runner/.cache/{IMAGE_CONFIG})\" = image"),
        )
        .await?;
        if code != 0 {
            return Err(format!(
                "slot 1 does not see the image's {IMAGE_CONFIG} over the old snapshot's (exit {code}):\n{output}"
            ));
        }
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
