//! Gradle against the shared build cache, end to end: the runner image's `build-cache.sh` and Gradle init script in a
//! container of a Gradle image, the cache served by `BuildCache` on this host. What a push to main built in one
//! container is `FROM-CACHE` in the next, fresh one; a cache that does not answer fails no build; turbo is pointed at
//! the cache only where the runner has a socket. Needs Docker with
//! sysbox and the network (it builds `runnerd-test-build-cache` from the Gradle image and Ubuntu's socat):
//!
//! ```sh
//! cargo test -p runnerd-core --test build_cache_gradle -- --ignored --nocapture
//! docker rmi runnerd-test-build-cache
//! ```
//!
//! `RUNNERD_TEST_ISOLATION=unsafe-dind` runs the containers with Docker's own runtime instead of sysbox.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "run with --nocapture, this test shows what Gradle printed; a step that fails fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use runnerd_core::buildcache::{ActionsCache, BuildCache, MOUNT, SOCKET, Scopes, Writes};

const IMAGE: &str = "runnerd-test-build-cache";

/// Gradle 9.8.2 on JDK 21 and Ubuntu 24.04, the runner image's base.
const GRADLE: &str = "gradle:9.8.2-jdk21-noble@sha256:12e887c27c1daaca19cab288bbe90f8fcd8d93a2a3bfb10da500a776a9d6c2c1";

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../image")
}

/// A fresh directory under the system's temp dir, which Docker can mount.
fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("runnerd-build-cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn docker(args: &[&str]) -> (bool, String) {
    let output = Command::new("docker").args(args).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// Builds the test image: the Gradle image with the runner image's build cache scripts and a `runner` user.
fn build_image() {
    let context = scratch("image");
    for file in ["build-cache.sh", "build-cache.gradle"] {
        std::fs::copy(image_dir().join(file), context.join(file)).unwrap();
    }
    std::fs::write(
        context.join("Dockerfile"),
        format!(
            "FROM {GRADLE}\n\
             RUN apt-get update && apt-get install -y --no-install-recommends socat && rm -rf /var/lib/apt/lists/* \
             && useradd -m runner\n\
             COPY build-cache.sh /usr/local/lib/runnerd-build-cache.sh\n\
             COPY build-cache.gradle /usr/local/share/runnerd/build-cache.gradle\n\
             ENV GRADLE_USER_HOME=/home/runner/.cache/gradle\n"
        ),
    )
    .unwrap();
    let (built, output) = docker(&["build", "-q", "-t", IMAGE, context.to_str().unwrap()]);
    std::fs::remove_dir_all(&context).unwrap();
    assert!(built, "cannot build {IMAGE}:\n{output}");
}

/// A Java project with the build cache on, as a repository turns it on.
fn project() -> PathBuf {
    let dir = scratch("project");
    std::fs::write(
        dir.join("settings.gradle"),
        "rootProject.name = 'halyard'\n",
    )
    .unwrap();
    std::fs::write(dir.join("gradle.properties"), "org.gradle.caching=true\n").unwrap();
    std::fs::write(dir.join("build.gradle"), "plugins { id 'java' }\n").unwrap();
    let source = dir.join("src/main/java/halyard");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("Rigging.java"),
        "package halyard;\n\npublic class Rigging {\n    public static String knot() { return \"bowline\"; }\n}\n",
    )
    .unwrap();
    dir
}

/// Builds the project in a fresh container that mounts `socket_dir` where runnerd mounts a runner's, as the
/// entrypoint starts it; returns Gradle's output.
fn gradle_build(project: &Path, socket_dir: &Path) -> String {
    let runtime = match std::env::var("RUNNERD_TEST_ISOLATION").as_deref() {
        Ok("unsafe-dind") => "runc",
        _ => "sysbox-runc",
    };
    let script = ". /usr/local/lib/runnerd-build-cache.sh && start_build_cache \
        && echo \"TURBO_API=${TURBO_API-unset}\" \
        && cp -r /project /work && chown -R runner /work \
        && setpriv --reuid=runner --regid=runner --init-groups env HOME=/home/runner \
           gradle --no-daemon --console=plain -p /work compileJava";
    let (built, output) = docker(&[
        "run",
        "--rm",
        "--runtime",
        runtime,
        "-v",
        &format!("{}:/project:ro", project.display()),
        "--mount",
        &format!(
            "type=bind,source={},target={MOUNT},readonly",
            socket_dir.display()
        ),
        IMAGE,
        "bash",
        "-c",
        script,
    ]);
    println!("{output}");
    assert!(built, "the build failed:\n{output}");
    output
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sysbox and the network"]
async fn a_main_builds_outputs_are_from_cache_in_a_fresh_container() {
    tokio::task::spawn_blocking(build_image).await.unwrap();
    let project = project();
    let root = scratch("cache");
    let cache = BuildCache::open(&root.join("entries"), &root.join("sockets"), 1 << 30).unwrap();

    let main = cache
        .attach("main", "halyard/api", ActionsCache::Off)
        .unwrap();
    cache.decide("main", Writes::Allowed, Scopes::default());
    let output = tokio::task::spawn_blocking({
        let (project, main) = (project.clone(), main.clone());
        move || gradle_build(&project, &main)
    })
    .await
    .unwrap();
    assert!(
        !output.contains("FROM-CACHE"),
        "the first build has nothing to take"
    );
    assert!(
        output.contains("TURBO_API=http://127.0.0.1:41771/turbo"),
        "turbo is pointed at the cache:\n{output}"
    );
    let stored = cache.detach("main").unwrap();
    assert!(stored.written_bytes > 0, "main stored nothing: {stored:?}");

    // A pull request in another slot: it reads what main stored.
    let pull = cache
        .attach("pull", "halyard/api", ActionsCache::Off)
        .unwrap();
    cache.decide("pull", Writes::Refused, Scopes::default());
    let output = tokio::task::spawn_blocking({
        let (project, pull) = (project.clone(), pull.clone());
        move || gradle_build(&project, &pull)
    })
    .await
    .unwrap();
    assert!(
        output.contains("Task :compileJava FROM-CACHE"),
        "compileJava ran again:\n{output}"
    );
    let read = cache.detach("pull").unwrap();
    assert!(read.hits >= 1, "{read:?}");

    // A socket nothing answers on, as while the daemon restarts: Gradle turns the cache off and builds.
    let down = root.join("down");
    std::fs::create_dir_all(&down).unwrap();
    drop(std::os::unix::net::UnixListener::bind(down.join(SOCKET)).unwrap());
    std::fs::set_permissions(
        down.join(SOCKET),
        std::os::unix::fs::PermissionsExt::from_mode(0o666),
    )
    .unwrap();
    let output = tokio::task::spawn_blocking({
        let project = project.clone();
        move || gradle_build(&project, &down)
    })
    .await
    .unwrap();
    assert!(output.contains("BUILD SUCCESSFUL"), "{output}");
    assert!(!output.contains("FROM-CACHE"), "{output}");

    // No socket, as with the cache off for the repository: no tool is pointed at it.
    let off = root.join("off");
    std::fs::create_dir_all(&off).unwrap();
    let output = tokio::task::spawn_blocking({
        let project = project.clone();
        move || gradle_build(&project, &off)
    })
    .await
    .unwrap();
    assert!(output.contains("TURBO_API=unset"), "{output}");

    std::fs::remove_dir_all(&project).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
