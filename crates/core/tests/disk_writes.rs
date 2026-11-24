//! Starts a runner container in `runnerdtest.slice`, waits for its Docker Engine, takes a "job" that writes from the
//! container itself and from a container of its Docker Engine, and follows it through the sampler and the history as
//! the daemon does (ADR 0143): the job's stored figure beside the container's `io.stat` read by hand, and the drive's
//! window beside `/sys/class/block/<disk>/stat` read by hand. Needs the isolation backend and a runner image:
//!
//! ```sh
//! cargo test -p runnerd-core --test disk_writes -- --ignored --nocapture
//! ```
//!
//! `RUNNERD_TEST_ISOLATION=unsafe-dind` runs privileged Docker-in-Docker instead of sysbox, and `RUNNERD_TEST_IMAGE`
//! another image.

#![allow(
    clippy::print_stdout,
    reason = "run with --nocapture, the test shows its figures beside the kernel's"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{ExecConfig, Mount, MountType, VolumeCreateRequest};
use futures_util::StreamExt;
use runnerd_core::config::{ByteSize, Isolation};
use runnerd_core::docker::{Docker, DockerApi, RunnerSpec};
use runnerd_core::github::QueuedJob;
use runnerd_core::isolation::backend;
use runnerd_core::observe::disk::Disks;
use runnerd_core::observe::metrics::{Paths, Sampler, Tracked};
use runnerd_core::store::{RunnerRecord, Store};

/// Milliseconds since the epoch.
fn now_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

const SLICE: &str = "runnerdtest.slice";
const MIB: u64 = 1 << 20;

/// The job: 64 MiB from the container itself, 128 MiB from a container of its Docker Engine, each synced so the
/// bytes reach the disk before the job ends.
const JOB: &str = "dd if=/dev/urandom of=/home/runner/outer bs=1M count=64 conv=fsync status=none && \
                   docker run --rm alpine:3.22 sh -c \
                   'dd if=/dev/urandom of=/inner bs=1M count=128 conv=fsync status=none'";

#[tokio::test]
#[ignore = "needs Docker, the isolation backend and a runner image"]
async fn a_job_s_writes_and_its_inner_containers_are_read_to_its_container_s_end() {
    let isolation = match std::env::var("RUNNERD_TEST_ISOLATION").as_deref() {
        Ok("unsafe-dind") => Isolation::UnsafeDind,
        _ => Isolation::Sysbox,
    };
    let isolation = backend(isolation);
    let name = format!("runnerd-test-disk-{}", std::process::id());
    let api = bollard::Docker::connect_with_local_defaults().unwrap();
    let docker = Docker::connect().unwrap();
    let paths = Paths::default();
    let root = docker.root_dir().await.unwrap();
    let disks = Disks::resolve(&paths, Path::new(&root)).expect("the disks under Docker's root");
    println!("Docker's root {root} is on {}", disks.name());
    api.create_volume(VolumeCreateRequest {
        name: Some(name.clone()),
        ..Default::default()
    })
    .await
    .unwrap();
    let spec = RunnerSpec {
        name: name.clone(),
        image: std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:base".to_owned()),
        profile: "disk".to_owned(),
        slice: SLICE.to_owned(),
        cpus: 2,
        memory: ByteSize(2 << 30),
        env: Vec::new(),
        mounts: vec![Mount {
            target: Some("/state".to_owned()),
            source: Some(name.clone()),
            typ: Some(MountType::VOLUME),
            ..Default::default()
        }],
        tmp_tmpfs: None,
        network: None,
        labels: BTreeMap::new(),
        // Its Docker Engine on the slot's volume, as a runner's keeps it.
        entrypoint: Some(
            ["dind", "dockerd", "--data-root", "/state/docker"]
                .map(str::to_owned)
                .to_vec(),
        ),
        jit_config: None,
    };
    let result = run(&api, &docker, isolation, &spec, &paths, disks).await;
    // The run removes it as a job's end does; this is for one that failed before.
    let removed = match docker.remove(&name).await {
        Err(error) if runnerd_core::docker::is_not_found(&error) => Ok(()),
        other => other,
    };
    let volume = api
        .remove_volume(
            &name,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await;
    result.unwrap();
    removed.unwrap();
    volume.unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "one scenario read top to bottom, in the order its figures print"
)]
async fn run(
    api: &bollard::Docker,
    docker: &Docker,
    isolation: &'static dyn runnerd_core::isolation::Isolation,
    spec: &RunnerSpec,
    paths: &Paths,
    disks: Disks,
) -> Result<(), String> {
    let name = &spec.name;
    let id = docker
        .start(spec, isolation)
        .await
        .map_err(|e| e.to_string())?;
    let state = docker.inspect(name).await.map_err(|e| e.to_string())?;
    let pid = state.pid.ok_or("no pid")?;
    let cgroup = paths.container(SLICE, &id);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let store = Store::open(&dir.join("runnerd.db")).map_err(|e| e.to_string())?;
    store.runner_started(RunnerRecord {
        name: name.clone(),
        repository: "o/r".to_owned(),
        profile: "disk".to_owned(),
        slot: 1,
        container: id.clone(),
        started_at: state.started_at.unwrap_or_else(now_ms),
        memory_cap: None,
        cpu_cap: None,
        admission: None,
        purpose: Some("warm"),
    });
    store.queued(
        "o/r",
        &QueuedJob {
            id: 1,
            run_id: 10,
            name: "disk".to_owned(),
            run_attempt: 1,
            created_at: now_ms(),
            ..QueuedJob::default()
        },
        now_ms(),
    );
    // The sampler, as the engine runs it, every 2 s in the background.
    let mut sampler = Sampler::new(paths.clone(), SLICE);
    sampler.watch_disk(disks.clone());
    let tracked = vec![Tracked {
        runner: name.clone(),
        container: id.clone(),
        pid,
    }];
    let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
    let follow = {
        let store = store.clone();
        let tracked = tracked.clone();
        tokio::spawn(async move {
            let mut watching = tracked;
            loop {
                let pass = sampler.sample(&watching, now_ms());
                store.containers_gone(pass.gone.clone());
                if !pass.ended.is_empty() {
                    store.containers_ended(pass.ended.clone(), now_ms());
                    return sampler.close_window();
                }
                if !pass.samples.is_empty() {
                    store.samples(pass.samples);
                }
                if stopped.try_recv().is_ok() {
                    // As the engine does once it sees the container stop: it stops tracking it.
                    watching.clear();
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    };
    let drive_before = drive(paths, &disks);
    let slice_before = written_by_hand(&paths.slice(SLICE));
    let mut ready = false;
    for _ in 0..60 {
        if exec(api, name, "docker info").await?.0 == 0 {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !ready {
        return Err("the container's Docker Engine did not start within 60s".to_owned());
    }
    // Warm for a few samples, then the job.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let at_claim = written_by_hand(&cgroup);
    store.claimed(1, 10, name, now_ms());
    tokio::time::sleep(Duration::from_secs(3)).await;
    let tree = exec(
        api,
        name,
        // The image's Docker Engine keeps temporary files beside the pnpm store a runner mounts.
        "mkdir -p /shared/pnpm/run && docker run -d --name tree alpine:3.22 sleep 30 >/dev/null && cat /proc/1/cgroup && \
         docker inspect -f '{{.State.Pid}}' tree",
    )
    .await?;
    println!("inside: {}", tree.1.trim());
    let nested: Vec<String> = walk(&cgroup);
    println!("cgroups under the runner's: {nested:?}");
    exec(api, name, "docker rm -f tree >/dev/null").await?;
    let (code, output) = exec(api, name, JOB).await?;
    if code != 0 {
        return Err(format!("the job exited {code}: {output}"));
    }
    let at_end = written_by_hand(&cgroup);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let last_by_hand = written_by_hand(&cgroup);
    docker.remove(name).await.map_err(|e| e.to_string())?;
    let _ = stop.send(());
    let window = tokio::time::timeout(Duration::from_secs(30), follow)
        .await
        .map_err(|_| "the sampler never saw the container end")?
        .map_err(|e| e.to_string())?;
    let drive_after = drive(paths, &disks);
    let slice_after = written_by_hand(&paths.slice(SLICE));
    store.lost(1, now_ms());
    store.flush().await;
    let job = store
        .job(1)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no job")?;
    let figure = job.summary.metrics.disk.ok_or("no figure")?;
    println!(
        "io.stat by hand: {at_claim} at the claim, {at_end} after the job, {last_by_hand} at the last read by hand: \
         {} MiB by hand",
        (last_by_hand - at_claim) / MIB
    );
    println!(
        "stored: {} bytes ({} MiB) in {} writes, {:?}, {} dirty at the last sample: {}",
        figure.bytes,
        figure.bytes / MIB,
        figure.ios,
        figure.measure,
        figure.dirty.unwrap_or(0),
        figure.line()
    );
    let window = window.ok_or("no window of the drive")?;
    println!(
        "drive {} by hand: {} bytes over the test, the slice {}; window {}..{} ms: drive {:?}, slice {:?}, this test's own {:?}, commits {:?}",
        disks.name(),
        drive_after - drive_before,
        slice_after - slice_before,
        window.start,
        window.end,
        window.drive,
        window.slice,
        window.daemon,
        window.commits
    );
    let _ = std::fs::remove_dir_all(&dir);
    if figure.bytes < 192 * MIB {
        return Err(format!(
            "the job wrote 192 MiB synced, inner container included; read {}",
            figure.bytes
        ));
    }
    if figure.measure != runnerd_core::api::Measure::Complete {
        return Err(format!("{figure:?}"));
    }
    Ok(())
}

/// The cgroup's `wbytes` on every device, as `cat io.stat` shows them, summed for the disks' lines by hand.
fn written_by_hand(cgroup: &Path) -> u64 {
    let stat = std::fs::read_to_string(cgroup.join("io.stat")).unwrap_or_default();
    println!(
        "{}/io.stat: {}",
        cgroup.display(),
        stat.trim().replace('\n', " | ")
    );
    stat.lines()
        .filter(|line| !line.starts_with("253:"))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|f| f.strip_prefix("wbytes="))
        })
        .filter_map(|bytes| bytes.parse::<u64>().ok())
        .sum()
}

/// The disks' sectors written, in bytes, read by hand.
fn drive(paths: &Paths, disks: &Disks) -> u64 {
    disks
        .name()
        .split('+')
        .map(|disk| {
            let stat =
                std::fs::read_to_string(paths.sys.join("class/block").join(disk).join("stat"))
                    .unwrap_or_default();
            stat.split_whitespace()
                .nth(6)
                .and_then(|sectors| sectors.parse::<u64>().ok())
                .unwrap_or(0)
                * 512
        })
        .sum()
}

/// The cgroups below `cgroup`, relative to it.
fn walk(cgroup: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![cgroup.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let path = entry.path();
                found.push(
                    path.strip_prefix(cgroup)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
                stack.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Runs a shell command in the container as root; returns its exit code and output.
async fn exec(
    api: &bollard::Docker,
    container: &str,
    command: &str,
) -> Result<(i64, String), String> {
    let run = async {
        let exec = api
            .create_exec(
                container,
                ExecConfig {
                    cmd: Some(vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()]),
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
    run.await.map_err(|e| e.to_string())
}

/// Docker's root can't be moved for a test, so this reads a volume on a loop device the way the sampler reads a
/// loop-backed root: `RUNNERD_TEST_LOOP_MOUNT` is the mount point of a file system on a loop device, as
/// `udisksctl loop-setup` and `udisksctl mount` make without root. A container in `runnerdtest.slice` writes 128 MiB
/// to it; its figure must be the loop device's `io.stat` line alone, and the drive the backing file's disk.
#[test]
#[ignore = "needs Docker and a file system on a loop device in RUNNERD_TEST_LOOP_MOUNT"]
fn a_container_writing_to_a_file_system_in_a_file_is_charged_the_loop_device_s_line() {
    let mount =
        PathBuf::from(std::env::var("RUNNERD_TEST_LOOP_MOUNT").expect("RUNNERD_TEST_LOOP_MOUNT"));
    let paths = Paths::default();
    let disks = Disks::resolve(&paths, &mount).expect("the disks under the mount");
    let store = disks.store().expect("the mount is on a loop device");
    println!(
        "{} is on {store}, a file on {}",
        mount.display(),
        disks.name()
    );
    let name = format!("runnerd-test-disk-loop-{}", std::process::id());
    let run = std::process::Command::new("docker")
        .args(["run", "-d", "--cgroup-parent", SLICE, "--name", &name, "-v"])
        .arg(format!("{}:/data", mount.display()))
        .args([
            "alpine:3.22",
            "sh",
            "-c",
            "sleep 3; dd if=/dev/urandom of=/data/written bs=1M count=128 conv=fsync status=none; \
             rm /data/written; touch /data/done; sleep 600",
        ])
        .output()
        .expect("docker run");
    let id = String::from_utf8_lossy(&run.stdout).trim().to_owned();
    assert!(
        run.status.success(),
        "docker run: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let result = std::panic::catch_unwind(|| {
        let cgroup = paths.container(SLICE, &id);
        let slice = paths.slice(SLICE);
        let mut windows = runnerd_core::observe::disk::Windows::default();
        std::thread::sleep(Duration::from_secs(1));
        let before = disks.written(&cgroup).expect("the container's io.stat");
        let drive_before = drive(&paths, &disks);
        windows.add(disks.read(&paths, &slice, now_ms()));
        for _ in 0..120 {
            if mount.join("done").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(
            mount.join("done").exists(),
            "the container wrote within a minute"
        );
        let after = disks.written(&cgroup).expect("the container's io.stat");
        let window = windows
            .add(disks.read(&paths, &slice, now_ms()))
            .or_else(|| windows.close())
            .expect("a window");
        let drive_after = drive(&paths, &disks);
        let stat = std::fs::read_to_string(cgroup.join("io.stat")).unwrap_or_default();
        println!("io.stat by hand: {}", stat.trim().replace('\n', " | "));
        println!(
            "figure: {} bytes in {} writes (from {} at the start)",
            after.bytes - before.bytes,
            after.ios - before.ios,
            before.bytes
        );
        println!(
            "drive {} by hand: {} bytes; window: drive {:?}, store {:?}, commits {:?}",
            disks.name(),
            drive_after - drive_before,
            window.drive,
            window.store,
            window.commits
        );
        assert!(
            after.bytes - before.bytes >= 128 * MIB,
            "the loop device's line has the 128 MiB"
        );
        assert_eq!(window.store.as_deref(), Some(store.as_str()));
    });
    let _ = std::fs::remove_file(mount.join("done"));
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", &name])
        .output();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
