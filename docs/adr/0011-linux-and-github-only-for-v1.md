# 0011. Linux hosts and GitHub only, for v1

Status: **Accepted**.

## Context

Supporting other operating systems or forges multiplies the isolation and API work.

## Decision

v1 runs on Linux hosts and serves GitHub repositories only.

## Consequences

The isolation backends, systemd slices and the runner image assume Linux. Other forges are not planned yet.
