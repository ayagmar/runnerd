//! A slot's Docker Engine keeps its images and build cache between the slot's jobs, each in a new container, within
//! the Docker budget runnerd gives it, BuildKit's cache mounts the longest, and a store its engine does not start on
//! is emptied. Needs Docker, sysbox and the runner image (`docker build --target base -t runnerd-runner:base image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test slot_docker_store -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_IMAGE` runs another image.

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, these tests show what the containers report"
)]

use std::collections::BTreeMap;

use runnerd_core::cache::{self, Names, runner_mounts};
use runnerd_core::docker::{Docker, DockerApi, RunnerSpec};
use runnerd_core::isolation::Sysbox;

const PROFILE: &str = "store";

/// The runner image the containers run.
fn image() -> String {
    std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:base".to_owned())
}

/// A job's image build, which needs no network: a layer the build cache keeps, in an image the job tags.
const BUILD: &str = "mkdir -p /root/app && cd /root/app && echo app > app.txt \
    && printf 'FROM scratch\\nCOPY app.txt /app.txt\\n' > Dockerfile \
    && docker build --progress=plain -t store-app . 2>&1";

/// Runs `script` as root in a new runner container of slot 1, after its layers are mounted and its Docker Engine
/// started as the runner's entrypoint does them; returns its exit code and output. The container is removed after,
/// as a runner's is.
async fn job(
    docker: &Docker,
    names: &Names,
    env: &[&str],
    script: &str,
) -> Result<(i64, String), String> {
    let spec = RunnerSpec {
        name: format!("{}-job", names.network()),
        image: image(),
        profile: PROFILE.to_owned(),
        slice: "system.slice".to_owned(),
        cpus: 1,
        memory: "1G".parse().map_err(|e: String| e)?,
        env: env.iter().map(|&var| var.to_owned()).collect(),
        mounts: runner_mounts(names, PROFILE, 1),
        network: Some("none".to_owned()),
        labels: BTreeMap::new(),
        entrypoint: Some(vec![
            "dind".to_owned(),
            "bash".to_owned(),
            "-euo".to_owned(),
            "pipefail".to_owned(),
            "-c".to_owned(),
            format!(". /usr/local/lib/runnerd-layers.sh; mount_layers; start_docker; {script}"),
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
    cache::prepare(docker, names, PROFILE, 1)
        .await
        .map_err(|e| e.to_string())?;

    // The first job builds an image and leaves a container and a volume behind, as a job does when its runner is
    // stopped before the job-completed cleanup: its Docker Engine is killed with the container.
    let (code, output) = job(
        docker,
        names,
        &[],
        &format!(
            "{BUILD} && docker volume create store-leftover && docker create --name store-leftover \
             -v store-leftover:/data store-app /app.txt"
        ),
    )
    .await?;
    println!("{output}");
    if code != 0 {
        return Err(format!("the first job exited {code}:\n{output}"));
    }

    // The next job's Docker Engine starts on what the killed one left, the entrypoint's cleanup removes the
    // container and volume, and the image and its build cache remain.
    let (code, output) = job(
        docker,
        names,
        &[],
        &format!(
            "runnerd-job-cleanup \
             && test -z \"$(docker ps -aq)\" && test -z \"$(docker volume ls -q)\" \
             && docker image inspect store-app > /dev/null && {BUILD}"
        ),
    )
    .await?;
    println!("{output}");
    if code != 0 {
        return Err(format!(
            "the slot's next job lost its Docker images, or still sees the last job's containers \
             (exit {code}):\n{output}"
        ));
    }
    if !output.contains("CACHED") {
        return Err(format!(
            "the slot's next job built its image without the build cache:\n{output}"
        ));
    }

    // A job damages the store's containerd database with its engine stopped, as a crash mid-write does. The next
    // job's engine does not start on it, so the store is emptied and the engine starts on an empty one.
    let (code, output) = job(
        docker,
        names,
        &[],
        "pkill -x dockerd; while pgrep -x 'dockerd|containerd' > /dev/null; do sleep 0.2; done; \
         dd if=/dev/urandom bs=4096 count=2 conv=notrunc \
         of=\"$DOCKER_DATA_ROOT/containerd/daemon/io.containerd.metadata.v1.bolt/meta.db\"",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "the job that damages the store exited {code}:\n{output}"
        ));
    }
    let (code, output) = job(
        docker,
        names,
        &[],
        &format!("! docker image inspect store-app > /dev/null 2>&1 && {BUILD}"),
    )
    .await?;
    if code != 0 || !output.contains("emptied it") {
        return Err(format!(
            "the slot's Docker Engine did not start again on an emptied store (exit {code}):\n{output}"
        ));
    }

    // With no Docker budget from runnerd, the job-completed hook the runner runs, as the runner and without the
    // container's environment, empties the slot's Docker Engine.
    let (code, output) = job(
        docker,
        names,
        &["RUNNERD_DOCKER_BUDGET_GB=0"],
        "setpriv --reuid=runner --regid=runner --init-groups -- env -i PATH=\"$PATH\" \
         /usr/local/bin/runnerd-job-completed.sh \
         && ! docker image inspect store-app > /dev/null 2>&1",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "the job cleanup kept the slot's images over its Docker budget (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

/// Removes the volumes and the network a scenario made; returns every removal's result.
async fn clean_up(docker: &Docker, names: &Names) -> Vec<Result<(), String>> {
    let mut cleanup = Vec::new();
    for volume in [
        names.base(PROFILE),
        names.slot(PROFILE, 1),
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
    let network = match bollard::Docker::connect_with_local_defaults() {
        Ok(api) => api
            .remove_network(&names.network())
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    cleanup.push(network);
    cleanup
}

#[tokio::test]
#[ignore = "needs Docker, sysbox and the runner image"]
async fn a_slots_docker_images_and_build_cache_outlive_its_jobs() {
    let names = Names::new(&format!("runnerd-test-store-{}", std::process::id()));
    let docker = Docker::connect().unwrap();
    let result = scenario(&docker, &names).await;
    let cleanup = clean_up(&docker, &names).await;
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
}

/// The slot's Docker share in these jobs, in GB: BuildKit's GC and the cleanup keep the build cache's layers to
/// 512 MiB and all of it to 768 MiB.
const SMALL_SHARE: &str = "RUNNERD_DOCKER_BUDGET_GB=1";

/// An image with bash and dd only, made from the runner's own files, so a build needs no network.
const SHELL_IMAGE: &str = "docker image inspect store-shell > /dev/null 2>&1 \
    || tar -ch /usr/bin/bash /usr/bin/dd \
        $(ldd /usr/bin/bash /usr/bin/dd | grep -o '/[^ :]*' | grep -v ^/usr/bin/ | sort -u) 2> /dev/null \
    | docker import -c 'ENV PATH=/usr/bin' - store-shell > /dev/null";

/// A build of a new commit whose step fills a package store in a cache mount of `$1` MiB the first time, and
/// reports `MOUNT-HIT` when it finds the store a build left there. The step's own text, which the build shows, has
/// neither word whole.
const MOUNT_BUILD: &str = "mkdir -p /root/mount && cd /root/mount && date +%s%N > commit \
    && printf '%s\\n' 'FROM store-shell' 'SHELL [\"/usr/bin/bash\", \"-c\"]' 'COPY commit /commit' \
        'ARG MIB' 'RUN --mount=type=cache,id=store-packages,target=/packages [ -f /packages/store ] && echo MOUNT-\"\"HIT \
        || { echo MOUNT-\"\"MISS; dd if=/dev/zero of=/packages/store bs=1M count=$MIB status=none; }' > Dockerfile \
    && docker build --progress=plain --build-arg MIB=\"$1\" -t store-mount . 2>&1";

async fn cache_mount_scenario(docker: &Docker, names: &Names) -> Result<(), String> {
    cache::prepare(docker, names, PROFILE, 1)
        .await
        .map_err(|e| e.to_string())?;

    // A job fills a 600 MiB package store in a cache mount, over the 512 MiB the old GC floor kept, and its build
    // cache's layers go past the share's half. BuildKit's GC runs at most once a minute, so the job waits past one
    // run before its cleanup, as a longer job's later steps would.
    let (code, output) = job(
        docker,
        names,
        &[SMALL_SHARE],
        &format!(
            "{SHELL_IMAGE} && build() {{ {MOUNT_BUILD}; }} && build 600 && sleep 70 && runnerd-job-cleanup \
             && docker buildx du"
        ),
    )
    .await?;
    println!("{output}");
    if code != 0 || !output.contains("MOUNT-MISS") {
        return Err(format!(
            "the job that fills a cache mount failed (exit {code}):\n{output}"
        ));
    }

    // The slot's next job, on a new container: its engine runs BuildKit's GC at start, its entrypoint runs the
    // cleanup, and its build of a new commit finds the package store.
    let (code, output) = job(
        docker,
        names,
        &[SMALL_SHARE],
        &format!(
            "runnerd-job-cleanup && sleep 5 && docker buildx inspect && docker buildx du \
             && build() {{ {MOUNT_BUILD}; }} && build 600"
        ),
    )
    .await?;
    println!("{output}");
    if code != 0 {
        return Err(format!(
            "the slot's next job failed its build (exit {code}):\n{output}"
        ));
    }
    if !output.contains("Max Used Space: 768MiB") {
        return Err(format!(
            "the slot's BuildKit GC policy does not follow its Docker share:\n{output}"
        ));
    }
    if !output.contains("MOUNT-HIT") {
        return Err(format!(
            "the slot's next job built without the cache mount its last job filled:\n{output}"
        ));
    }

    // Images put the store over its share: they go, and the package store stays.
    let (code, output) = job(
        docker,
        names,
        &[SMALL_SHARE],
        "dd if=/dev/urandom of=/root/big bs=1M count=700 status=none \
         && tar -C /root -c big | docker import - store-big > /dev/null && runnerd-job-cleanup \
         && ! docker image inspect store-big > /dev/null 2>&1 \
         && docker buildx du --verbose | grep exec.cachemount > /dev/null",
    )
    .await?;
    println!("{output}");
    if code != 0 || output.contains("emptied it") {
        return Err(format!(
            "the job cleanup kept images over the slot's share, or gave up the cache mount before them \
             (exit {code}):\n{output}"
        ));
    }

    // A package store over the share leaves the store over it once layers and images are gone. The cleanup, which
    // here runs before BuildKit's next GC, empties it, cache mounts too, and says so in the job's log.
    let (code, output) = job(
        docker,
        names,
        &[SMALL_SHARE],
        &format!(
            "docker builder prune -af > /dev/null && {SHELL_IMAGE} && build() {{ {MOUNT_BUILD}; }} \
             && build 1300 > /dev/null && runnerd-job-cleanup \
             && ! docker buildx du --verbose | grep exec.cachemount > /dev/null"
        ),
    )
    .await?;
    println!("{output}");
    if code != 0 || !output.contains("emptied it, cache mounts too") {
        return Err(format!(
            "the job cleanup kept a store over the slot's share, or did not say it emptied it \
             (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker, sysbox and the runner image"]
async fn a_slots_cache_mounts_outlive_its_jobs_within_its_share() {
    let names = Names::new(&format!("runnerd-test-mount-{}", std::process::id()));
    let docker = Docker::connect().unwrap();
    let result = cache_mount_scenario(&docker, &names).await;
    let cleanup = clean_up(&docker, &names).await;
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
}
