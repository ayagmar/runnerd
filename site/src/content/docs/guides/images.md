---
title: Which image do my jobs run on?
description: The published runner images, building one on the host, and adding the tools your jobs need.
---

[`image/Dockerfile`](https://github.com/ayagmar/runnerd/tree/main/image) has four published stages: `base` (runner, Docker Engine, GitHub CLI), `android` (the Android SDK and JDK 17), `jvm` (nothing yet over `android`) and `node` (pipx, Playwright's libraries). Runner images are linux/amd64 only; binaries ship for x86_64 and aarch64, and on aarch64 the base stage is built on the host with `image_stage`.

A profile sets `image = "ghcr.io/ayagmar/runnerd-runner:jvm-0.1.1"`, pulled before its first runner (keep the tag at the daemon's version; `runnerd doctor` warns when they differ), or `image_stage = "jvm"`, which runs `runnerd-runner:jvm` built from a clone with `docker build --target jvm -t runnerd-runner:jvm image` and never pulls.

runnerd removes published images that no profile names ([ADR 0040](https://github.com/ayagmar/runnerd/blob/main/docs/adr/0040-unused-runner-images-removed.md)).

## Your own image

For system packages the stages lack, build on `base` or `android`:

```dockerfile
FROM ghcr.io/ayagmar/runnerd-runner:base-0.1.1
RUN apt-get update \
    && apt-get install -y --no-install-recommends libpq-dev protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
```

Build it, set `image = "quillstack-runner:0.1.0"` on a profile, and keep its entrypoint and root user. runnerd pulls an `image` only when Docker lacks it, so build yours first and rebuild from each new `base`; `runnerd doctor` warns when its inherited `runnerd.version` label is not the daemon's.
