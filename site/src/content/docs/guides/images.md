---
title: Which image do my jobs run on?
description: The published runner images, building one on the host, and adding the tools your jobs need.
---

[`image/Dockerfile`](https://github.com/ayagmar/runnerd/tree/main/image) has four published stages: `base` (runner, Docker Engine, GitHub CLI), `android` (the Android SDK and JDK 17), `jvm` (nothing yet over `android`) and `node` (pipx, Playwright's libraries). Runner images are linux/amd64 only; binaries ship for x86_64 and aarch64, and on aarch64 the base stage is built on the host with `image_stage`.

A profile sets `image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.1.7"`, pulled before its first runner (keep the tag at the daemon's version; `runnerd doctor` warns when they differ), or `image_stage = "jvm"`, which runs `runnerd-runner:jvm` built from a clone with `docker build --target jvm -t runnerd-runner:jvm image` and never pulls.

runnerd removes published images that no profile names ([ADR 0040](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0040-unused-runner-images-removed.md)).

## Your own image

For system packages the stages lack, build on `base` or `android`:

```dockerfile
FROM ghcr.io/ayagmar/runnerd-runner:base-0.1.7
RUN apt-get update \
    && apt-get install -y --no-install-recommends libpq-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
```

Build it, set `image = "quillstack-runner:0.1.0"` on a profile, and keep its entrypoint and root user. runnerd pulls an `image` only when Docker lacks it, so build yours first and rebuild from each new `base`; `runnerd doctor` warns when its inherited `runnerd.version` label is not the daemon's.

## Pinned mise and tools

When a repository pins mise and its tools in `mise.lock`, [`image/mise-tools`](https://github.com/ayagmar/runnerd/tree/main/image/mise-tools) bakes them into an image on a published stage, so a cold slot runs them without a download. It needs Docker Engine 28.3.0 or newer: older BuildKit looks a base's digest up on the registry even when Docker holds the image. From a clone, with the repository's reviewed `mise.toml` and `mise.lock` copied into a directory:

```sh
image/mise-tools/build.sh \
  --base ghcr.io/ayagmar/runnerd-runner:jvm-0.1.7@sha256:<digest> \
  --mise-version 2026.10.18 --mise-sha256 <SHA-256 of the extracted mise binary> \
  --project ~/review/quillstack-a1b2c3d
```

`docker image inspect --format '{{index .RepoDigests 0}}' ghcr.io/ayagmar/runnerd-runner:jvm-0.1.7` prints the digest. The mise version and digest must be a row of `image/mise-tools/tested-mise.txt`, today only 2026.10.18. The script accepts only lock entries it has tested mise refuses wrong bytes for: aqua and GitHub release assets, Node from nodejs.org, Temurin JDKs and CPython, each with a full digest and its exact URL, and literal versions in `[tools]`. It refuses anything else before downloading, naming the tool. It builds under a candidate tag, then runs `mise install --locked` in that image as `runner` with no network: it must install nothing, and each `[tools]` request must resolve to its locked version's directory under `/usr/local/share/mise/installs`, whose program prints that version. A tool whose version command is not `<name> --version`, such as oras, whose proof fails with `unknown flag: --version`, gets `--check 'oras=oras version'`. Only a proved image gets its tag; the last line printed pins it to the image ID, such as `runnerd-mise:mise-2026.10.18-cb9fb477f5376c92@sha256:fb2f…`: set that as the profile's `image`. A new lock, config, mise or runnerd release means a new build and a new tag.

mise uses the image's tools only when the job has no install of its own, and reads every config file it finds. So the job's mise setup refuses a checkout with a mise config besides `mise.toml`, such as `mise.local.toml`, sets `MISE_OVERRIDE_CONFIG_FILENAMES=mise.toml`, and makes a fresh directory with `mktemp -d` for mise's data, installs, cache, state and config, never the slot's tool cache. It sets `MISE_SYSTEM_INSTALLS_DIR` to `/usr/local/share/mise/installs` only when `/opt/mise/share/SHA256SUMS` names the job's mise pin and `sha256sum --check --strict /opt/mise/share/SHA256SUMS` passes in the checkout. Otherwise it sets it to an empty directory, runs `/opt/mise/libexec/runnerd-mise-inputs check .` (or the recipe's `inputs.py`) and stops if that refuses, then downloads mise and installs every tool itself. [ADR 0149](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0149-pinned-mise-and-tools-are-baked-into-a-derived-image.md) says what the image guarantees and what it does not.
