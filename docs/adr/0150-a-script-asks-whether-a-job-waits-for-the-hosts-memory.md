# 0150. A script asks whether a job waits for the host's memory in an exit code

Status: **Accepted** (2026-11-20). Builds on [0055](0055-admission-follows-the-hosts-memory.md),
[0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md) and
[0061](0061-the-previous-release-reads-the-wire.md). Adds `runnerctl status --waiting-for host-memory [--repo
owner/name]` and `RepositoryStatus::queue` to the API. No config change.

## Context

Local builds on the CI host take memory the runners' admission counts as the host's other programs (0055). A build
wrapper that holds back its extra parallel builds while CI waits for that memory needs one question answered, often:
does a queued job, of its repository, wait for the host's memory now? Reading `runnerctl status` text breaks on any
change of wording, and `status --json` makes every wrapper re-derive the rule from `Waiting`, including the job that
waits behind an older one (0096).

An empty queue in the status is not proof that nothing waits. The engine leaves a repository's jobs out of
`Status::queued` until its first listing answers (the daemon publishes its repositories before Docker's recovery and
the first poll, and retries a failed recovery for as long as it takes), while its listings fail, once the rate limit
stops one, and while the daemon stops. `RepositoryStatus::listing` names only a failure, so nothing told an empty
queue that was listed from one that was not.

## Decision

- Each `RepositoryStatus` says whether its jobs in `Status::queued` are those its last listing answered with:
  `queue` is `listed` or `unlisted`. A status without it, from a daemon that predates it, reads as `unknown`, and so
  does a value from a newer daemon: unknown never means listed.
- Core answers the question: `QueuedJob::waits_for_host_memory` holds for a job whose reason is
  `Waiting::HostMemory`, and for one waiting `BehindMemory` an older queued job that waits for the host's memory,
  followed to the end of the chain. `Status::host_wait` gives `Waits` when a job asked about does, however the rest
  stands; `Clear` only when every repository asked about is `listed`, the daemon is not stopping, and every job asked
  about has a reason this client reads that leads nowhere near the host (not none yet, not `Unknown`, not behind a
  job the status does not show); `Unavailable` otherwise. A repository the daemon does not serve is
  `UnknownRepository`. `HostWait::trouble` words the error for the last two. The attention list's host item reads
  the same answer.
- `runnerctl status --waiting-for host-memory` prints nothing and exits 0 for `Waits`, 1 for `Clear`, and 2 for
  everything else: `Unavailable`, `UnknownRepository`, a daemon not running or refusing, `--json`, or arguments clap
  rejects. 2 is never "clear", so a script that treats only 1 as room does not take memory it cannot see is free.
- It reads `GET /v1/status`, as `runnerctl status` does. `--waiting-for` takes a value so other waits can be asked
  the same way later.

## Consequences

- Against a daemon that predates `queue`, the command answers 0 when it sees a job wait and 2 otherwise, never 1. The
  previous release ignores the new field (0061; `check-wire-compat.sh` reads both ways).
- Right after a start, and through a failed recovery, a failing or rate-limited listing and a stop, a wrapper gets 2
  and decides for itself; it gets 1 again once a listing answers and the plan gave every job a reason.
- A job behind an older job that waits for the slice's budget, a slot or CPU does not count: memory a local build
  gives back would not start it.
- The answer is as fresh as the last plan the daemon made, as every reason `runnerctl status` shows is.
