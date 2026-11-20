---
title: Which image do my jobs run on?
description: The published runner images, building one on the host, and adding the tools your jobs need.
---

[`image/Dockerfile`](https://github.com/ayagmar/runnerd/tree/main/image) has four published stages: `base` (runner, Docker Engine, GitHub CLI), `android` (the Android SDK and JDK 17), `jvm` (nothing yet over `android`) and `node` (pipx, Playwright's libraries). Runner images are linux/amd64 only; binaries ship for x86_64 and aarch64, and on aarch64 the base stage is built on the host with `image_stage`.

A profile sets `image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.1.4"`, pulled before its first runner (keep the tag at the daemon's version; `runnerd doctor` warns when they differ), or `image_stage = "jvm"`, which runs `runnerd-runner:jvm` built from a clone with `docker build --target jvm -t runnerd-runner:jvm image` and never pulls.

runnerd removes published images that no profile names ([ADR 0040](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0040-unused-runner-images-removed.md)).

## Your own image

For system packages the stages lack, build on `base` or `android`:

```dockerfile
FROM ghcr.io/ayagmar/runnerd-runner:base-0.1.4
RUN apt-get update \
    && apt-get install -y --no-install-recommends libpq-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
```

Build it, set `image = "quillstack-runner:0.1.0"` on a profile, and keep its entrypoint and root user. runnerd pulls an `image` only when Docker lacks it, so build yours first and rebuild from each new `base`; `runnerd doctor` warns when its inherited `runnerd.version` label is not the daemon's.

## Pinned mise and tools

When a repository pins mise and its tools in `mise.lock`, [`image/mise-tools`](https://github.com/ayagmar/runnerd/tree/main/image/mise-tools) bakes them into an image on a published stage, so a cold slot runs them without a download. From a clone, with the repository's reviewed `mise.toml` and `mise.lock` copied into a directory:

```sh
image/mise-tools/build.sh \
  --base ghcr.io/ayagmar/runnerd-runner:jvm-0.1.4@sha256:<digest> \
  --mise-version 2026.10.18 --mise-sha256 <SHA-256 of the extracted mise binary> \
  --project ~/review/quillstack-a1b2c3d
```

`docker image inspect --format '{{index .RepoDigests 0}}' ghcr.io/ayagmar/runnerd-runner:jvm-0.1.4` prints the digest. The script refuses a lock entry without a `linux-x64` checksum, plugins and a tool's `postinstall`, then builds, then runs the repository's `mise install --locked` as `runner` with no network: it must install nothing, and each tool must print its version from `/usr/local/share/mise/installs`. A tool whose version command is not `<name> --version` gets `--check 'oras=oras version'`. The last line it prints is the tag, such as `runnerd-mise:mise-2026.10.18-6f39e280b924b102`: set it as the profile's `image`. A new lock or a new runnerd release means a new build and a new tag.

mise uses the image's tools only when the job has no install of its own, so the job's mise setup points `MISE_DATA_DIR` at a fresh directory, not the slot's tool cache, and uses `/opt/mise/bin/mise` when its SHA-256 matches the job's pin. When `/opt/mise/libexec/runnerd-mise-inputs match mise.lock` fails, the job's lock pins a tool version to other bytes than the image's: set `MISE_SYSTEM_INSTALLS_DIR` to an empty directory and let mise install everything. [ADR 0148](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0148-pinned-mise-and-tools-are-baked-into-a-derived-image.md) says what the image guarantees and what it does not.
