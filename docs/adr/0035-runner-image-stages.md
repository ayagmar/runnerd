# 0035. A stack-neutral base image, with Android in its own stage

Status: **Accepted** (2026-10-23).

## Context

The stages of `image/Dockerfile` grew around the owner's two repositories. `base` carried a JDK 17 and the Android
SDK, `jvm` was empty and `node` added Playwright's libraries. `runnerd init` writes a profile on `base`, so a
stranger with a Rust, Go or Python repository pulled Android tools they never use. Only Gradle, npm, pnpm, Maven,
pub and pipx kept their caches under `~/.cache`, the slot's warm layer ([0007](0007-warm-caches-snapshot-plus-slot-layer.md)),
so a Rust or Go job downloaded its toolchain and dependencies again in every new container.

## Decision

- `base` promises what any workflow needs and nothing tied to a stack: the GitHub Actions runner, Docker Engine with
  Compose and Buildx, the GitHub CLI, iptables, make, xz, zip, zstd, and runnerd's entrypoint, hooks and scripts.
  Every stage builds on it, and a custom image should too.
- `android` adds the Android SDK command-line tools and platform-tools under `/opt/android-sdk` and a JDK 17. The
  JDK stays because Android builds whose workflow sets up no JDK, such as `flutter build appbundle`, need one, as
  they find one on GitHub's ubuntu-24.04 image; a workflow's `setup-java` still takes precedence.
- `jvm` and `node` build on `android`; `node` adds pipx and Playwright's system libraries. Every stage is published,
  `android` included, so a custom Android image can start from it.
- The toolchain homes whose default is outside `~/.cache` point into it from `base`: `CARGO_HOME`, `RUSTUP_HOME`,
  `GOPATH` (so `GOMODCACHE`), `BUN_INSTALL_CACHE_DIR` and `YARN_GLOBAL_FOLDER`, with `$CARGO_HOME/bin` on the
  `PATH` so a warm rustup is found. pip, uv, Poetry, Yarn 1, Deno, coursier and Go's build cache already default
  there. sbt and Ivy are left out: moving `~/.sbt` and `~/.ivy2` takes `SBT_OPTS`, which a workflow's own
  `SBT_OPTS` replaces, and `~/.sbt` holds credentials.
- The cache layer outlives a job, so a file there that may hold a token or a job's settings never reaches the next
  job, a record shell or a new snapshot: `layers.sh` lists them (`CACHE_SECRETS`: cargo's `credentials`,
  `credentials.toml`, `config` and `config.toml`, Gradle's `gradle.properties`; [0077](0077-cache-credentials-closed-slots-and-build-cache-counts.md)
  adds more). `mount_layers` deletes them from
  the slot's changes before it mounts the layer, then through the mounted layer the ones the snapshot holds, and
  `seed.sh` drops them from a new snapshot. A snapshot seeded by an older image may hold one (Gradle's home was
  already in the cache layer) and is only replaced at the next reseed, which may not come for a while: hiding its
  copies at every mount covers it without a migration. An image's own copy of such a file is put back over
  the hiding whiteout, so a custom image's cargo config still applies. A layer whose path the image lacks, such as the Android
  SDK on `base`, is not mounted.
- `images.yml` publishes the stages the tag's `image/Dockerfile` has, so publishing a release from before
  `android` existed still works.

## Consequences

`base` drops the JDK and the Android SDK: 2.63 GB instead of 3.43 GB on disk, 675 MB instead of 991 MB to pull. A Rust or Go job is warm from its second run
in a slot, and from the first after a reseed. A record of a failed job no longer holds the Rust toolchain the job
installed: it is in the cache layer, which a record shell sees as the profile's snapshot. A profile that used `base`
for Android builds moves to `android`. The scrub's rules for `~/.rustup` and `~/.cargo` stay, for workflows that
set those homes back.
