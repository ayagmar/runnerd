# 0006. Ephemeral JIT runners, one per job

Status: **Accepted**, amended by [0050](0050-warm-runners.md).

## Context

Persistent registered runners keep state between jobs and form one pool per repository.

## Decision

For each queued job the daemon registers a just-in-time runner (`POST /repos/{owner}/{repo}/actions/runners/generate-jitconfig`, `GithubApi::jit_config`) and starts one container for it. The runner exits after one job.

## Consequences

No idle registrations and no state carried between jobs. One pool of slots serves every configured repository. Each job costs one extra API call and a container start.
