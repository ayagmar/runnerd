//! `@actions/cache` against GitHub Actions' cache on this host, end to end (ADR 0082): the runner image's
//! `build-cache.sh` and `NODE_OPTIONS` preload in a container of a Node image, the cache served by `BuildCache` on
//! this host. A save of main's in one container is restored by a pull request's job in the next, fresh one, though
//! both were given GitHub's `ACTIONS_RESULTS_URL` as the runner gives it; the pull request's own save is dropped.
//! Needs Docker with sysbox and the network (it builds `runnerd-test-actions-cache` from the Node image, Debian's
//! socat and iptables, and `@actions/cache` from npm):
//!
//! ```sh
//! cargo test -p runnerd-core --test actions_cache_node -- --ignored --nocapture
//! docker rmi runnerd-test-actions-cache
//! ```
//!
//! `RUNNERD_TEST_ISOLATION=unsafe-dind` runs the containers with Docker's own runtime, with `NET_ADMIN` for the rule,
//! instead of sysbox. BuildKit's `type=gha` and artifacts are not covered here: the report of ADR 0082 says how to
//! check them on a sandbox repository.

#![allow(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "run with --nocapture, this test shows what node printed; a step that fails fails the test"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use runnerd_core::buildcache::{ActionsCache, BuildCache, MOUNT, Scopes, Writes};

const IMAGE: &str = "runnerd-test-actions-cache";

/// Node 24 on Debian bookworm.
const NODE: &str =
    "node:24-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c";

/// The `@actions/cache` the test saves and restores with.
const ACTIONS_CACHE: &str = "@actions/cache@6.3.0";

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../image")
}

/// A fresh directory under the system's temp dir, which Docker can mount.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "runnerd-actions-cache-{name}-{}",
        std::process::id()
    ));
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

/// Builds the test image: the Node image with the runner image's scripts, `@actions/cache` and a `runner` user.
fn build_image() {
    let context = scratch("image");
    for file in ["build-cache.sh", "build-cache.gradle", "actions-cache.cjs"] {
        std::fs::copy(image_dir().join(file), context.join(file)).unwrap();
    }
    std::fs::write(
        context.join("Dockerfile"),
        format!(
            "FROM {NODE}\n\
             RUN apt-get update && apt-get install -y --no-install-recommends socat iptables util-linux \
             && rm -rf /var/lib/apt/lists/* && useradd -m runner\n\
             RUN npm install --prefix /opt/cache {ACTIONS_CACHE}\n\
             COPY build-cache.sh /usr/local/lib/runnerd-build-cache.sh\n\
             COPY build-cache.gradle /usr/local/share/runnerd/build-cache.gradle\n\
             COPY actions-cache.cjs /usr/local/lib/runnerd-actions-cache.cjs\n\
             ENV GRADLE_USER_HOME=/home/runner/.cache/gradle\n"
        ),
    )
    .unwrap();
    let (built, output) = docker(&["build", "-q", "-t", IMAGE, context.to_str().unwrap()]);
    std::fs::remove_dir_all(&context).unwrap();
    assert!(built, "cannot build {IMAGE}:\n{output}");
}

/// Runs `script` with `@actions/cache` as `cache` in a fresh container that mounts `socket_dir` where runnerd mounts
/// a runner's, as the entrypoint starts it, as the runner user, with the environment the runner gives a node action;
/// returns what it printed.
fn node(socket_dir: &Path, script: &str) -> String {
    let runtime = match std::env::var("RUNNERD_TEST_ISOLATION").as_deref() {
        Ok("unsafe-dind") => "runc",
        _ => "sysbox-runc",
    };
    let script = format!(
        "const cache = require('/opt/cache/node_modules/@actions/cache');\n\
         console.log('results service: ' + process.env.ACTIONS_RESULTS_URL);\n{script}"
    );
    let shell = "set -e; . /usr/local/lib/runnerd-build-cache.sh && start_build_cache \
        && install -d -o runner /work /work/tmp && cd /work \
        && setpriv --reuid=runner --regid=runner --init-groups env HOME=/home/runner RUNNER_TEMP=/work/tmp \
           GITHUB_WORKSPACE=/work GITHUB_SERVER_URL=https://github.com ACTIONS_CACHE_SERVICE_V2=True \
           ACTIONS_RUNTIME_TOKEN=runtime-token \
           ACTIONS_RESULTS_URL=https://results-receiver.actions.githubusercontent.com/ \
           node -e \"$SCRIPT\"";
    let (ran, output) = docker(&[
        "run",
        "--rm",
        "--runtime",
        runtime,
        // Sysbox's root has it in the container's own network; Docker's runtime adds it only when asked.
        "--cap-add=NET_ADMIN",
        "-e",
        &format!("SCRIPT={script}"),
        "--mount",
        &format!(
            "type=bind,source={},target={MOUNT},readonly",
            socket_dir.display()
        ),
        IMAGE,
        "bash",
        "-c",
        shell,
    ]);
    println!("{output}");
    assert!(ran, "node failed:\n{output}");
    output
}

/// The scopes of a run of `branch` in a repository whose default branch is `main`.
fn scopes(branch: &str) -> Scopes {
    Scopes {
        own: Some(format!("refs/heads/{branch}")),
        base: None,
        default: Some("refs/heads/main".to_owned()),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker, sysbox and the network"]
async fn mains_save_is_a_pull_requests_restore_in_a_fresh_container() {
    tokio::task::spawn_blocking(build_image).await.unwrap();
    let root = scratch("cache");
    let cache = BuildCache::open(&root.join("entries"), &root.join("sockets"), 1 << 30).unwrap();

    let main = cache
        .attach("main", "halyard/web", ActionsCache::Served)
        .unwrap();
    cache.decide("main", Writes::Allowed, scopes("main"));
    let output = tokio::task::spawn_blocking(move || {
        node(
            &main,
            "require('fs').mkdirSync('deps', {recursive: true});\n\
             require('fs').writeFileSync('deps/lock', 'bowline');\n\
             cache.saveCache(['deps'], 'deps-0a1b').then(id => console.log('saved ' + id));",
        )
    })
    .await
    .unwrap();
    assert!(
        output.contains("results service: http://"),
        "the preload points node at runnerd:\n{output}"
    );
    let stored = cache.detach("main").unwrap();
    assert!(stored.written_bytes > 0, "main stored nothing: {stored:?}");

    let pull = cache
        .attach("pull", "halyard/web", ActionsCache::Served)
        .unwrap();
    cache.decide("pull", Writes::Refused, scopes("feature"));
    let output = tokio::task::spawn_blocking(move || {
        node(
            &pull,
            "cache.restoreCache(['deps'], 'deps-ffff', ['deps-']).then(key => {\n\
               console.log('restored ' + key + ': ' + require('fs').readFileSync('deps/lock'));\n\
               require('fs').writeFileSync('deps/lock', 'poison');\n\
               return cache.saveCache(['deps'], 'deps-ffff');\n\
             });",
        )
    })
    .await
    .unwrap();
    assert!(output.contains("restored deps-0a1b: bowline"), "{output}");
    let read = cache.detach("pull").unwrap();
    assert_eq!((read.hits, read.refused), (1, 1), "{read:?}");
    assert_eq!(
        cache.size().entries,
        1,
        "the pull request's save was dropped"
    );

    std::fs::remove_dir_all(&root).unwrap();
}
