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
use std::sync::OnceLock;

use runnerd_core::cache::{self, Measure, Measured, Names, Reseed, runner_mounts};
use runnerd_core::docker::{Docker, DockerApi, RunnerSpec};
use runnerd_core::isolation::Sysbox;

const PROFILE: &str = "cache";
/// The runner image the fixture image is made from.
fn runner_image() -> String {
    std::env::var("RUNNERD_TEST_IMAGE").unwrap_or("runnerd-runner:base".to_owned())
}

/// The image every container runs, the seed container's too: the runner image with `IMAGE_FILES`.
static FIXTURE_IMAGE: OnceLock<String> = OnceLock::new();

fn image() -> String {
    FIXTURE_IMAGE.get().cloned().unwrap_or_else(runner_image)
}

/// What the fixture image holds in the cache layer's path: a file a job deletes, a directory it replaces, a file it
/// leaves, and cargo's settings, of which the image's own copy stays.
const IMAGE_FILES: &str = "mkdir -p /home/runner/.cache/fixture/replaced /home/runner/.cache/cargo \
    && echo image > /home/runner/.cache/fixture/deleted && echo image > /home/runner/.cache/fixture/replaced/old \
    && echo image > /home/runner/.cache/fixture/kept && echo image > /home/runner/.cache/cargo/config.toml \
    && chown -R runner:runner /home/runner/.cache";

/// Makes `tag` the runner image with `IMAGE_FILES`.
async fn fixture_image(docker: &Docker, api: &bollard::Docker, tag: &str) -> Result<(), String> {
    let spec = RunnerSpec {
        name: format!("{tag}-build"),
        image: runner_image(),
        profile: PROFILE.to_owned(),
        slice: "system.slice".to_owned(),
        cpus: 1,
        memory: "1G".parse().map_err(|e: String| e)?,
        env: Vec::new(),
        mounts: Vec::new(),
        network: Some("none".to_owned()),
        labels: BTreeMap::new(),
        entrypoint: Some(vec![
            "bash".to_owned(),
            "-c".to_owned(),
            IMAGE_FILES.to_owned(),
        ]),
        jit_config: None,
    };
    let id = docker
        .start(&spec, &Sysbox)
        .await
        .map_err(|e| e.to_string())?;
    let code = docker.wait(&id).await.map_err(|e| e.to_string());
    let committed = api
        .commit_container(
            bollard::query_parameters::CommitContainerOptions {
                container: Some(id.clone()),
                repo: Some(tag.to_owned()),
                ..Default::default()
            },
            bollard::models::ContainerConfig::default(),
        )
        .await
        .map_err(|e| e.to_string());
    docker.remove(&id).await.map_err(|e| e.to_string())?;
    match code? {
        0 => committed.map(drop),
        code => Err(format!("making the fixture image exited {code}")),
    }
}

/// Runs the rest of a command as the runner user, as a job does.
const AS_RUNNER: &str = "setpriv --reuid=runner --regid=runner --init-groups --";

/// Registry tokens and settings a job leaves in the cache layer: cargo's, the Maven wrapper's, a Gradle init script
/// and Hugging Face's.
const CREDENTIALS: [&str; 4] = [
    "/home/runner/.cache/cargo/credentials.toml",
    "/home/runner/.cache/m2/settings.xml",
    "/home/runner/.cache/gradle/init.d/publish.gradle",
    "/home/runner/.cache/huggingface/token",
];

/// A job's script that leaves every one of `CREDENTIALS`.
fn leave_credentials() -> String {
    let files = CREDENTIALS.join(" ");
    format!(
        "{AS_RUNNER} sh -c 'for f in {files}; do mkdir -p \"${{f%/*}}\" && echo token > \"$f\"; done'"
    )
}

/// A script that fails if a container sees any of `CREDENTIALS`.
fn no_credentials() -> String {
    CREDENTIALS
        .iter()
        .map(|file| format!("test ! -e {file}"))
        .collect::<Vec<_>>()
        .join(" && ")
}

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
    let leave_credentials = leave_credentials();
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!(
            "{AS_RUNNER} sh -c 'echo warm > /home/runner/.cache/runnerd-test' && {leave_credentials}"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }

    let image = image();
    let reseed = |min_changes: &str, max_size: &str| Reseed {
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
        max_size: max_size
            .parse()
            .unwrap_or(runnerd_core::config::ByteSize(0)),
        isolation: &Sysbox,
    };
    let output = cache::reseed(docker, &reseed("1T", "1T"))
        .await
        .map_err(|e| e.to_string())?;
    if !output.contains("not seeded") {
        return Err(format!("seeded below the threshold:\n{output}"));
    }
    let output = cache::reseed(docker, &reseed("0K", "1T"))
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
             && {}",
            no_credentials()
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
    let (code, output) = run_in_slot(docker, names, 1, &no_credentials()).await?;
    if code != 0 {
        return Err(format!(
            "slot 1's next job sees its last job's credentials (exit {code}):\n{output}"
        ));
    }

    let measure = Measure {
        names,
        profile: PROFILE,
        image: &image,
        slice: "system.slice",
        isolation: &Sysbox,
    };
    measured_changes(docker, names, &reseed("0K", "1T"), &measure).await?;
    old_snapshot(docker, names, &reseed("1T", "1T")).await?;
    let capped = reseed("0K", "3M");
    fitting_snapshot(docker, names, &capped).await?;
    capped_snapshot(docker, names, &capped).await?;
    interrupted_reseed(docker, names, &reseed("1T", "1T"), &measure).await?;
    closed_slot(docker, names, &reseed("0K", "1T")).await?;
    image_entries_hidden(docker, names, &reseed("0K", "1T")).await
}

/// What slot 1's job deleted of the image, or replaced, stays hidden from every slot after a reseed, and after the
/// next one, which copies a view that leaves the image out; what it left stays, and so does the image's own copy of
/// cargo's settings when the job deleted cargo's home.
async fn image_entries_hidden(
    docker: &Docker,
    names: &Names,
    reseed: &Reseed<'_>,
) -> Result<(), String> {
    let cache = "/home/runner/.cache";
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!(
            "{AS_RUNNER} sh -c 'rm {cache}/fixture/deleted && rm -r {cache}/fixture/replaced \
             && mkdir {cache}/fixture/replaced && echo job > {cache}/fixture/replaced/new && rm -r {cache}/cargo'"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }
    let hidden = format!(
        "test ! -e {cache}/fixture/deleted && test ! -e {cache}/fixture/replaced/old \
         && test -f {cache}/fixture/replaced/new && test -f {cache}/fixture/kept \
         && test \"$(cat {cache}/{IMAGE_CONFIG})\" = image"
    );
    for round in ["the reseed", "the next reseed"] {
        let output = cache::reseed(docker, reseed)
            .await
            .map_err(|e| e.to_string())?;
        println!("{output}");
        let (code, output) = run_in_slot(docker, names, 2, &hidden).await?;
        if code != 0 {
            return Err(format!(
                "after {round}, slot 2 sees what slot 1 deleted of the image, or not what it left (exit {code}):\n{output}"
            ));
        }
        let (code, output) = run_in_slot(
            docker,
            names,
            1,
            &format!("{AS_RUNNER} sh -c 'echo {round:?} > {cache}/unrelated'"),
        )
        .await?;
        if code != 0 {
            return Err(format!("the job in slot 1 exited {code}:\n{output}"));
        }
    }
    Ok(())
}

/// A slot past the profile's `slots`, as a lower `slots` leaves until the caches' check removes its volume, is not
/// mounted by a reseed, so its changes were made against the snapshot before. When `slots` grows again, it starts over
/// from the new snapshot instead of hiding it.
async fn closed_slot(docker: &Docker, names: &Names, reseed: &Reseed<'_>) -> Result<(), String> {
    cache::prepare(docker, names, PROFILE, 3)
        .await
        .map_err(|e| e.to_string())?;
    for (slot, text) in [(3, "slot-3"), (1, "snapshot")] {
        let (code, output) = run_in_slot(
            docker,
            names,
            slot,
            &format!("{AS_RUNNER} sh -c 'echo {text} > /home/runner/.cache/shadow'"),
        )
        .await?;
        if code != 0 {
            return Err(format!("the job in slot {slot} exited {code}:\n{output}"));
        }
    }
    let output = cache::reseed(docker, reseed)
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    for _ in 0..2 {
        let (code, output) = run_in_slot(
            docker,
            names,
            3,
            "test \"$(cat /home/runner/.cache/shadow)\" = snapshot",
        )
        .await?;
        if code != 0 {
            return Err(format!(
                "a slot closed during the reseed kept its changes over the new snapshot (exit {code}):\n{output}"
            ));
        }
    }
    Ok(())
}

/// What a measure of slot 1 says it changed; an error if it says anything else.
async fn changed(docker: &Docker, measure: &Measure<'_>) -> Result<u64, String> {
    match cache::measure(docker, measure).await {
        Ok(Measured::Changed(bytes)) => Ok(bytes),
        other => Err(format!("measuring slot 1 gave {other:?}")),
    }
}

/// A measure counts what slot 1's job changed, and nothing once a reseed made that the snapshot.
async fn measured_changes(
    docker: &Docker,
    names: &Names,
    reseed: &Reseed<'_>,
    measure: &Measure<'_>,
) -> Result<(), String> {
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!("{AS_RUNNER} sh -c 'echo measured > /home/runner/.cache/measured'"),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }
    let before = changed(docker, measure).await?;
    println!("slot 1 changed {before} bytes");
    if before == 0 {
        return Err("the measure missed slot 1's job's changes".to_owned());
    }
    cache::reseed(docker, reseed)
        .await
        .map_err(|e| e.to_string())?;
    match changed(docker, measure).await? {
        0 => Ok(()),
        after => Err(format!(
            "slot 1 still changed {after} bytes after its reseed"
        )),
    }
}

/// A reseed killed between changing the snapshot and dropping the slots' changes leaves its marker: a slot's changes
/// may name what the snapshot no longer holds. Until the next reseed starts both over from the image, whatever its
/// threshold, a container sees neither, and a measure says so.
async fn interrupted_reseed(
    docker: &Docker,
    names: &Names,
    below_threshold: &Reseed<'_>,
    measure: &Measure<'_>,
) -> Result<(), String> {
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        &format!("{AS_RUNNER} sh -c 'echo slot-2 > /home/runner/.cache/index'"),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 2 exited {code}:\n{output}"));
    }
    let (code, output) = run(
        docker,
        names,
        1,
        Snapshot::Writable,
        "",
        "mkdir -p /base/cache && echo half > /base/cache/content && touch /base/.reseeding",
    )
    .await?;
    if code != 0 {
        return Err(format!("planting a killed reseed exited {code}:\n{output}"));
    }
    let unseen = "test ! -e /home/runner/.cache/content && test ! -e /home/runner/.cache/index";
    let (code, output) = run_in_slot(docker, names, 2, unseen).await?;
    if code != 0 {
        return Err(format!(
            "slot 2 used a snapshot or changes a killed reseed left (exit {code}):\n{output}"
        ));
    }
    match cache::measure(docker, measure).await {
        Ok(Measured::Unfinished) => {}
        other => return Err(format!("measuring after a killed reseed gave {other:?}")),
    }
    let output = cache::reseed(docker, below_threshold)
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    if !output.contains("did not finish") {
        return Err(format!(
            "the reseed after a killed one did not start over:\n{output}"
        ));
    }
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        &format!("test ! -e /base/.reseeding && test ! -e /base/cache && {unseen}"),
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "the reseed after a killed one left its marker or snapshot (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

/// A reseed measures the snapshot it would make, not the old snapshot plus slot 1's changes: one within its limit
/// keeps what slot 1's job only read beside what it changed, as a cache's updated index names files it only read.
async fn fitting_snapshot(
    docker: &Docker,
    names: &Names,
    capped: &Reseed<'_>,
) -> Result<(), String> {
    let (code, output) = run(
        docker,
        names,
        1,
        Snapshot::Writable,
        "",
        "mkdir -p /base/cache/npm && head -c 1M /dev/zero > /base/cache/npm/content \
         && head -c 1536K /dev/zero > /base/cache/npm/index && chown -R runner:runner /base/cache/npm",
    )
    .await?;
    if code != 0 {
        return Err(format!("planting a cache exited {code}:\n{output}"));
    }
    // 2.5M of snapshot and 1.5M of changes reach the 3M limit; the 2.5M snapshot they make does not.
    let (code, output) = run_in_slot(
        docker,
        names,
        1,
        &format!(
            "{AS_RUNNER} sh -c 'cat /home/runner/.cache/npm/content > /dev/null \
             && echo entry >> /home/runner/.cache/npm/index'"
        ),
    )
    .await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }
    let output = cache::reseed(docker, capped)
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        "grep -qa entry /home/runner/.cache/npm/index && test -s /home/runner/.cache/npm/content",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "a snapshot within its limit lost what slot 1 only read (exit {code}):\n{output}"
        ));
    }
    Ok(())
}

/// A snapshot over its limit starts over from the image, as slot 1's changes alone miss what its jobs only read.
async fn capped_snapshot(
    docker: &Docker,
    names: &Names,
    capped: &Reseed<'_>,
) -> Result<(), String> {
    let write = |file: &str, megabytes: u32| {
        format!("{AS_RUNNER} head -c {megabytes}M /dev/zero > /home/runner/.cache/{file}")
    };
    let (code, output) = run_in_slot(docker, names, 1, &write("current", 1)).await?;
    if code != 0 {
        return Err(format!("the job in slot 1 exited {code}:\n{output}"));
    }
    let output = cache::reseed(docker, capped)
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        "test ! -e /base/cache && test ! -e /home/runner/.cache/current \
         && test ! -e /home/runner/.cache/npm",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "a snapshot over its limit did not start over from the image (exit {code}):\n{output}"
        ));
    }

    // Slot 1 has no changes to a layer, slot 2 has some to the snapshot's: starting over drops both.
    let (code, output) = run(
        docker,
        names,
        2,
        Snapshot::Writable,
        "",
        "mkdir -p /base/cache && head -c 4M /dev/zero > /base/cache/superseded",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "planting a large snapshot exited {code}:\n{output}"
        ));
    }
    let (code, output) = run_in_slot(docker, names, 2, &write("slot-2", 1)).await?;
    if code != 0 {
        return Err(format!("the job in slot 2 exited {code}:\n{output}"));
    }
    let output = cache::reseed(docker, capped)
        .await
        .map_err(|e| e.to_string())?;
    println!("{output}");
    let (code, output) = run_in_slot(
        docker,
        names,
        2,
        "test ! -e /base/cache && test ! -e /home/runner/.cache/slot-2",
    )
    .await?;
    if code != 0 {
        return Err(format!(
            "slot 2 kept changes to a snapshot layer that started over (exit {code}):\n{output}"
        ));
    }
    Ok(())
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
    let api = bollard::Docker::connect_with_local_defaults().unwrap();
    let fixture = format!("{}-image", names.network());
    let made = fixture_image(&docker, &api, &fixture).await;
    let made_image = made.is_ok();
    FIXTURE_IMAGE.set(fixture.clone()).unwrap();
    let result = match made {
        Ok(()) => scenario(&docker, &names).await,
        Err(error) => Err(error),
    };

    let mut cleanup = Vec::new();
    for volume in [
        names.base(PROFILE),
        names.slot(PROFILE, 1),
        names.slot(PROFILE, 2),
        names.slot(PROFILE, 3),
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
    if made_image {
        cleanup.push(
            api.remove_image(
                &fixture,
                None::<bollard::query_parameters::RemoveImageOptions>,
                None,
            )
            .await
            .map(drop)
            .map_err(|e| e.to_string()),
        );
    }
    result.unwrap();
    for removed in cleanup {
        removed.unwrap();
    }
}
