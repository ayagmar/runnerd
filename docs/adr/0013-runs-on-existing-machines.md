# 0013. Runs on the user's existing machines

Status: **Accepted**.

## Context

People already have laptops, VMs and home servers.

## Decision

runnerd installs onto a machine the user has. It never creates or provisions machines.

## Consequences

No cloud credentials or VM images to manage. Capacity is whatever the machine's slice allows.
