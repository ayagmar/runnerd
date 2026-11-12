# 0134. A job action is previewed, then sent to GitHub once

Status: **Accepted** (2026-11-12). Adds `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun`, the types in
`api::actions`, `[github] job_actions`, `Status::job_actions`, the doctor's `job actions` check, `runnerctl cancel` and
`runnerctl rerun`. No schema change.

## Context

An owner who sees a stuck or failed job in a client has to open GitHub to cancel its run or run it again. runnerd
already holds a token that can do both, and the forced pause already cancels runs (ADR 0019). Four things make it more
than a button:

- GitHub cancels runs, not jobs. A cancel from one job stops every job of its run, on this machine and on others. A
  re-run of a job also re-runs the jobs that depend on it.
- runnerd knows only the jobs it saw. The run may have jobs on other runners, or jobs GitHub has not queued yet.
- A POST that times out, or that a gateway answers, may or may not have been done. Sending it again can cancel or
  re-run a second time.
- Two clients, or two tabs, can ask at once.

## Decision

**Preview first.** Both routes answer 409 with an `ActionPreview` and send nothing, unless the request has
`force=true` and a `confirm` token this daemon issued. Each preview reads the run from GitHub once, within the
deadline, for its current attempt and whether it completed; the read's answer leaves `Github` as a class and status
only. The preview names that attempt (`Cancel run 4182 (attempt 3) of halyard/ledger?`) and holds what GitHub does
(the whole run, or this job and the jobs that depend on it), notes for a paused repository, a completed run, a job of
an earlier attempt or a confirmation that was not taken, the other jobs of the run the history has (at most 50, newest
attempt first, every name GitHub gave cleaned of control characters and escapes while the history keeps the original),
and a sentence that always says GitHub may have more. Its `message` makes it an `ErrorResponse` too. If the run cannot
be read, the preview says so and has no `confirm`: nothing can be sent until a later preview reads it.

**A confirmation is issued, bound and used once.** `confirm` is 128 random bits, in hex, that
`actions::flights::Flights` issues with the preview and keeps for 5 minutes, at most 256 at once. It is bound to the
action, repository, run, attempt and job the preview showed and to the config revision it was shown under. A token
this runnerd does not hold (made up, run out, from before a restart or from another runnerd), one for another action
or job, one whose revision is not the current one, or one already sent gets a new 409 preview with a note saying why,
never a send. The same token while its request is in flight joins that request; after it landed, it is refused.

**Checked again right before the send.** `Github::exchange` asks the action's permit once the token is in hand and
before each POST, the one after a 401 included. The permit reads the run again with that token: another attempt than
the confirmation's, or a run it cannot read, ends the request with a new preview. Then it compares the config
revision with the confirmation's: a reload since then withdraws the request, refused as `withdrawn`. Nothing but the
POST follows that check, and no lock is held across GitHub.

**Sent once, by one owner.** A confirmed request goes through `runnerd_core::actions::JobActions`, a host module the
engine builds on its own `Github` client and hands out in `Handle::actions`. Requests for one repository and run meet
in `Flights`, the sealed owner of the confirmations and of what is in flight: the same action of the same job joins
the one in flight and gets its answer; another action is refused 409 while it runs. The POST runs in its own task, so
a client that hangs up neither stops it nor leaves the entry behind. It is sent once, with no retry, within 20 s (the
token read, the permit's read of the run and the client's own 401 retry included), under the 30 s a client waits. A
stop of any mode ends it.

**Status and headers only.** The action's POST reads the answer's status and headers and drops its body unread: a 202
whose body never ends is accepted, and a refusal's body never reaches an error, a `Debug` output or a log line. The
rate-limit accounting and the 401 refresh are the client's own. The token is read by one thread at a time, bounded by
gh's 15 s deadline; gh's process group is killed at that deadline and at a stop, and a request that stops waiting
leaves the read for the next request to join, so a token source that hangs piles nothing up and holds no shutdown.

**Four outcomes.** `ActionResult::outcome` is `accepted` (GitHub's 2xx), `already-finished` (a cancel GitHub answers
409 and a read of the run says it completed), `refused` (no token, unreachable, withdrawn, 401, 403, 404 or 410, any
other 409, 422, the rate limit, any other status) or `unknown` (a timeout, a broken connection, any 5xx, or a stop
while in flight). A refusal is its class and HTTP status: a rate limit GitHub answered keeps its 403 or 429, and one
runnerd held back locally has none. GitHub's text never reaches the answer, a log line or an error, and the token
never leaves `Github`. Core words every result and its tone; a newer daemon's outcome reads as `unknown`, never as
accepted. An unknown outcome keeps its status and tells the owner to check the run on GitHub before asking again; it
is never retried.

**Nothing local changes.** The job's stored lifecycle is not written. No container is killed and no deadline is kept:
GitHub tells the runner, and the listings bring the cancelled job and any new attempt. The forced pause keeps its own
cancel path, with job actions on or off.

**The config of the moment.** `[github] job_actions` is on by default. The setting and the configured repositories
are one snapshot, `actions::policy::Policy`, replaced whole by each reload under a revision that moves only when
either changes. A request is authorized against one snapshot: with actions off, or for a repository the snapshot does
not have, both routes answer 403 before any write. A paused repository's job can be cancelled. It can be re-run too,
and the preview says runnerd starts none of the new attempt's jobs until the repository is resumed. A cancel from a
job of an earlier attempt cancels the run at its current attempt, and the preview says so. A job with no run recorded
is 422.

**Evidence, not a trial.** The doctor's `job actions` check reads what gh says about the token: a classic or OAuth
token with `repo` can, one without it cannot, and a fine-grained token cannot be verified without a write, so the
check says that and makes none. gh reads the token's kind inside its own process; only the kind comes out. The
attention list leaves the check out whatever its status.

## Consequences

- A preview costs one GitHub read, and a confirmed request a second one before its POST. Both count against the rate
  limit; a preview made while it is used up has no confirmation.
- A restart forgets every confirmation: a client that held one gets a new preview and asks again. Nothing is stored.
- `runnerctl cancel` and `rerun` print the preview and the command that sends it, `--confirm <token>`; `--force` sends
  without showing the preview, for scripts.
- The sibling list is what this runnerd saw, labelled so. A client that wants GitHub's list of the run's jobs needs a
  read this decision does not add.
- `[github]` is one-way: 0.1.0 refuses a config with the section. A daemon without `Status::job_actions` reads as
  `unsupported`, and a client offers neither action; one without the routes answers 404 `no such endpoint`, which
  `ClientError::is_unsupported` reads.
- A fine-grained token's doctor check stays "not verifiable". The first real request is the proof.
