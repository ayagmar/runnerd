# 0150. A script asks whether a job waits for the host's memory in an exit code

Status: **Accepted** (2026-11-20). Builds on [0055](0055-admission-follows-the-hosts-memory.md) and
[0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md). Adds `runnerctl status --waiting-for host-memory [--repo owner/name]`. No
API or config change.

## Context

Local builds on the CI host take memory the runners' admission counts as the host's other programs (0055). A build
wrapper that holds back its extra parallel builds while CI waits for that memory needs one question answered, often:
does a queued job, of its repository, wait for the host's memory now? Reading `runnerctl status` text breaks on any
change of wording, and `status --json` makes every wrapper re-derive the rule from `Waiting`, including the job that
waits behind an older one (0096).

## Decision

- Core answers it: `QueuedJob::waits_for_host_memory` holds for a job whose reason is `Waiting::HostMemory`, and for
  one waiting `BehindMemory` an older queued job that waits for the host's memory, followed to the end of the chain.
  `Status::host_wait` asks it of every queued job, or of one repository's, named in any case. A repository the
  daemon does not serve is `HostWait::UnknownRepository`, not "nothing waits". The attention list's host item reads
  the same answer.
- `runnerctl status --waiting-for host-memory` prints nothing and exits 0 when a job waits, 1 when none does, and 2
  when it cannot answer: the daemon is not running or refused, the repository is unknown, `--json` was given, or
  clap rejected the arguments. 2 is never "clear", so a script that treats only 1 as room does not take memory on a
  stopped daemon or a typo.
- It reads `GET /v1/status`, as `runnerctl status` does; the daemon computes nothing new. `--waiting-for` takes a
  value so other waits can be asked the same way later.

## Consequences

- Any daemon this client reads answers the question, since `Waiting::HostMemory` and `BehindMemory` are older than
  it; a reason a newer daemon adds reads as `Waiting::Unknown` and counts as not waiting for the host.
- A job behind an older job that waits for the slice's budget, a slot or CPU does not count: memory a local build
  gives back would not start it.
- The answer is as fresh as the last plan the daemon made, as every reason `runnerctl status` shows is.
