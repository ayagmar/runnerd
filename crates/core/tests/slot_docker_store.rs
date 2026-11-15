//! A slot's Docker Engine keeps its images and build cache between the slot's jobs, each in a new container, within
//! the Docker budget runnerd gives it, BuildKit's cache mounts the longest, and a store its engine does not start on
//! is emptied. Told to, a runner answers its sync calls without syncing, down to its job's containers and builds,
//! and the image's entrypoint is what tells it, as its label says; a runner image from before the filter says
//! nothing. The volumes a crash may have torn are emptied past the stopped containers of runnerd's left on them, and a
//! volume another container holds is refused at once, never waited on.
//! Needs Docker, sysbox and the runner image (`docker build --target base -t runnerd-runner:base image`):
//!
//! ```sh
//! cargo test -p runnerd-core --test slot_docker_store -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_IMAGE` runs another image. `RUNNERD_TEST_OLD_IMAGE` names the runner image from before the filter,
//! `ghcr.io/ayagmar/runnerd-runner:node-0.1.1` unless set, which Docker must have.

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, these tests show what the containers report"
)]

use std::collections::BTreeMap;

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use runnerd_core::cache::{self, Names, runner_mounts};
use runnerd_core::docker::{self, Docker, DockerApi, RunnerSpec};
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

/// Runs `script` as root in a new runner container of slot 1, under `runnerd-skip-syncs` as the image's entrypoint
/// runs the runner, after its layers are mounted and its Docker Engine started as the runner's entrypoint does them; returns its exit code and output. The container is removed after,
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
            "runnerd-skip-syncs".to_owned(),
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
    .map_err(|e: docker::Error| e.to_string());
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
    if code != 0 || output.contains("cache mounts") {
        return Err(format!(
            "the job cleanup kept images over the slot's share, or gave up the cache mount before them \
             (exit {code}):\n{output}"
        ));
    }

    // A package store over the share leaves the store over it once layers are gone. The cleanup gives up the cache
    // mount, and says so in the job's log. On a slow host BuildKit's GC can reach the mount first;
    // then the store is within its share either way, and the log line is required only when the mount was still
    // there for the cleanup.
    let (code, output) = job(
        docker,
        names,
        &[SMALL_SHARE],
        &format!(
            "docker builder prune -af > /dev/null && {SHELL_IMAGE} && build() {{ {MOUNT_BUILD}; }} \
             && build 1300 > /dev/null \
             && {{ docker buildx du --verbose | grep exec.cachemount > /dev/null && echo MOUNT-BEFORE-CLEANUP || true; }} \
             && runnerd-job-cleanup \
             && ! docker buildx du --verbose | grep exec.cachemount > /dev/null \
             && [ \"$(size_gb \"$DOCKER_DATA_ROOT\")\" -le 1 ]"
        ),
    )
    .await?;
    println!("{output}");
    let emptied = output.contains("least recently used cache mounts")
        || output.contains("everything, cache mounts too");
    if code != 0 || (output.contains("MOUNT-BEFORE-CLEANUP") && !emptied) {
        return Err(format!(
            "the job cleanup kept a store over the slot's share, or did not say it gave up its cache mount \
             (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

/// Reports, for the runner as root and as its user, whether each sync call the filter answers synced: on a descriptor
/// the kernel refuses, a real call fails with EBADF and a skipped one returns 0. `sync` cannot fail, so it is not
/// probed; it sits between probed calls in the filter, whose last entry, `syncfs`, is probed.
const CALL_PROBES: &str = r#"cat > /tmp/calls.py <<'PY'
import ctypes, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.sync_file_range.argtypes = [ctypes.c_int, ctypes.c_longlong, ctypes.c_longlong, ctypes.c_uint]
calls = {
    "fsync": lambda: libc.fsync(999),
    "fdatasync": lambda: libc.fdatasync(999),
    "sync_file_range": lambda: libc.sync_file_range(999, 0, 0, 0),
    "syncfs": lambda: libc.syncfs(999),
}
for name, call in calls.items():
    print(f"{sys.argv[1]} {name}:", "SKIPPED" if call() == 0 else "SYNCED")
PY
python3 /tmp/calls.py root && setpriv --reuid=runner --regid=runner --init-groups -- python3 /tmp/calls.py user"#;

/// The calls `CALL_PROBES` reports on.
const CALLS: [&str; 4] = ["fsync", "fdatasync", "sync_file_range", "syncfs"];

/// Reports, for a container of the runner's Docker Engine and a build step, whether `fsync` synced: on `/dev/null`
/// a real one fails with EINVAL, a skipped one returns 0. The build step words its answer only as it runs, so the
/// step's own text, which the build shows, never matches.
const NESTED_PROBES: &str = "probe() { dd if=/dev/zero of=/dev/null count=1 conv=fsync status=none 2> /dev/null \
        && echo \"$1: SKIPPED\" || echo \"$1: SYNCED\"; } \
    && docker run --rm --network none store-shell bash -c \"$(declare -f probe); probe container\" \
    && printf '%s\\n' 'FROM store-shell' 'SHELL [\"bash\", \"-c\"]' \
        'RUN dd if=/dev/zero of=/dev/null count=1 conv=fsync status=none 2> /dev/null && r=SKIPPED || r=SYNCED; echo build: $r' \
        | docker build --no-cache --progress=plain - 2>&1 | grep -o 'build: [A-Z]*$'";

/// The image's own entrypoint, which no test container runs as it needs a just-in-time config: the wrapper must be in
/// it, or no runner skips a sync whatever runnerd asks.
async fn entrypoint() -> Result<Vec<String>, String> {
    let api = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
    let image = api
        .inspect_image(&image())
        .await
        .map_err(|e| e.to_string())?;
    Ok(image
        .config
        .and_then(|config| config.entrypoint)
        .unwrap_or_default())
}

async fn skip_syncs_scenario(docker: &Docker, names: &Names) -> Result<(), String> {
    cache::prepare(docker, names, PROFILE, 1)
        .await
        .map_err(|e| e.to_string())?;
    let entrypoint = entrypoint().await?;
    if entrypoint != ["dind", "runnerd-skip-syncs", "runnerd-runner"] {
        return Err(format!(
            "the image's entrypoint does not run runnerd-skip-syncs before the runner: {entrypoint:?}"
        ));
    }
    if !docker
        .image_skips_syncs(&image())
        .await
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "{} runs runnerd-skip-syncs, and its label does not say so: runnerd would never ask its runners",
            image()
        ));
    }
    for (env, expected) in [(vec![docker::SKIP_SYNCS], "SKIPPED"), (vec![], "SYNCED")] {
        let (code, output) = job(
            docker,
            names,
            &env,
            &format!("{CALL_PROBES}\n{SHELL_IMAGE} && {NESTED_PROBES}"),
        )
        .await?;
        println!("{output}");
        let mut wanted: Vec<String> = ["root", "user"]
            .iter()
            .flat_map(|who| {
                CALLS
                    .iter()
                    .map(move |call| format!("{who} {call}: {expected}"))
            })
            .collect();
        wanted.extend(["container", "build"].map(|place| format!("{place}: {expected}")));
        let missing: Vec<&String> = wanted
            .iter()
            .filter(|line| !output.contains(line.as_str()))
            .collect();
        if code != 0 || !missing.is_empty() {
            return Err(format!(
                "with {env:?}, these do not report {expected}: {missing:?} (exit {code}):\n{output}"
            ));
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker, sysbox and the runner image"]
async fn a_runner_told_to_skip_syncs_skips_them_down_to_its_jobs_containers_and_builds() {
    let names = Names::new(&format!("runnerd-test-syncs-{}", std::process::id()));
    let docker = Docker::connect().unwrap();
    let result = skip_syncs_scenario(&docker, &names).await;
    let cleanup = clean_up(&docker, &names).await;
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
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

#[tokio::test]
#[ignore = "needs Docker and the runner image from before the filter"]
async fn a_runner_image_from_before_the_filter_is_never_asked_to_skip_syncs() {
    let old = std::env::var("RUNNERD_TEST_OLD_IMAGE").unwrap_or(format!(
        "{}:node-0.1.1",
        runnerd_core::config::PUBLISHED_RUNNER
    ));
    let docker = Docker::connect().unwrap();
    assert!(
        docker.image_present(&old).await.unwrap(),
        "Docker has {old}: docker pull {old}"
    );
    assert!(
        !docker.image_skips_syncs(&old).await.unwrap(),
        "{old} lacks runnerd-skip-syncs, so runnerd never asks its runners and arms nothing"
    );
}

/// A container of `image` that mounts `volume` at `/torn`, labelled `labels`, running `command`.
fn holder(image: &str, volume: &str, labels: &[&str], command: &[&str]) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(image.to_owned()),
        entrypoint: Some(command.iter().map(|&part| part.to_owned()).collect()),
        labels: Some(
            labels
                .iter()
                .map(|&label| (label.to_owned(), String::new()))
                .collect(),
        ),
        network_disabled: Some(true),
        host_config: Some(HostConfig {
            mounts: Some(vec![Mount {
                target: Some("/torn".to_owned()),
                source: Some(volume.to_owned()),
                typ: Some(MountType::VOLUME),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn torn_scenario(docker: &Docker, names: &Names) -> Result<(), String> {
    let (left, held) = (names.slot(PROFILE, 1), names.pnpm());
    for volume in [&left, &held] {
        docker
            .ensure_volume(volume)
            .await
            .map_err(|e| e.to_string())?;
    }
    // A runner a crash stopped: its container is left, exited, on its slot's volume.
    let leftover = format!("{}-leftover", names.network());
    docker
        .run(
            &leftover,
            holder(
                &image(),
                &left,
                &["runnerd.job", "runnerd.profile"],
                &["true"],
            ),
        )
        .await
        .map_err(|e| e.to_string())?;
    docker.wait(&leftover).await.map_err(|e| e.to_string())?;
    // Someone's own container, running on the pnpm volume.
    let foreign = format!("{}-foreign", names.network());
    docker
        .run(&foreign, holder(&image(), &held, &[], &["sleep", "600"]))
        .await
        .map_err(|e| e.to_string())?;
    match docker.remove_volume(&held).await {
        Err(error) if !docker::is_unreachable(&error) && !docker::is_not_found(&error) => {
            println!("Docker refuses a held volume: {error}");
        }
        other => {
            return Err(format!(
                "Docker did not refuse to remove a volume a running container mounts: {other:?}"
            ));
        }
    }
    let cache::Cleared { removed, refused } = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        cache::empty_volumes(docker, &[left.clone(), held.clone()]),
    )
    .await
    .map_err(|_| "emptying the volumes waited on Docker for a minute".to_owned())?;
    println!("removed {removed:?}; refused {refused:?}");
    if removed != [left.clone()] {
        return Err(format!(
            "the volume under the leftover was not removed: {removed:?}, {refused:?}"
        ));
    }
    match refused.as_slice() {
        [refusal] if refusal.volume == held && refusal.held && refusal.why.contains(&foreign) => {}
        _ => {
            return Err(format!(
                "the held volume was not refused as held: {refused:?}"
            ));
        }
    }
    match docker.inspect(&leftover).await {
        Err(error) if docker::is_not_found(&error) => {}
        other => return Err(format!("the leftover container was not removed: {other:?}")),
    }
    let api = bollard::Docker::connect_with_local_defaults().map_err(|e| e.to_string())?;
    match api.inspect_volume(&left).await {
        Err(error) if docker::is_not_found(&error) => {}
        other => return Err(format!("{left} is still there: {other:?}")),
    }
    let foreign_state = docker.inspect(&foreign).await.map_err(|e| e.to_string())?;
    println!("the other container is left as it was: {foreign_state:?}");
    api.inspect_volume(&held)
        .await
        .map_err(|e| format!("{held}, which another container holds, is gone: {e}"))?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs Docker and the runner image"]
async fn torn_volumes_are_emptied_past_runnerds_leftovers_and_a_held_one_is_refused_at_once() {
    let names = Names::new(&format!("runnerd-test-torn-{}", std::process::id()));
    let docker = Docker::connect().unwrap();
    let result = torn_scenario(&docker, &names).await;
    let mut cleanup = Vec::new();
    for container in ["leftover", "foreign"] {
        match docker
            .remove(&format!("{}-{container}", names.network()))
            .await
        {
            Err(error) if !docker::is_not_found(&error) => cleanup.push(Err(error.to_string())),
            _ => {}
        }
    }
    for volume in [names.slot(PROFILE, 1), names.pnpm()] {
        cleanup.push(
            docker
                .remove_volume(&volume)
                .await
                .map_err(|e| e.to_string()),
        );
    }
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
}
