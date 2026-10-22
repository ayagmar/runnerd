# 0005. Isolation backends: sysbox by default, privileged DinD only when asked

## Context

Each job runs its own Docker Engine inside its container. Privileged containers give a job root on the host.

## Decision

An `Isolation` trait (`crates/core/src/isolation.rs`) sets how runner containers are created. `sysbox` is the default. `unsafe-dind` (privileged) needs `allow_unsafe = true` in the config. Kata may come later.

## Consequences

Hosts need sysbox installed; `runnerd doctor` checks it. Kata needs `/dev/kvm` and is slower for cache-heavy I/O, so it is not the default. Rejected: gVisor (weak Docker-in-sandbox support), Firecracker directly (own VM images and networking), rootless Podman (owner's call).
