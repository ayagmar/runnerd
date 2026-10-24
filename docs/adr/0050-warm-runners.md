# 0050. Warm runners: a runner waits for a job before it is queued

Status: **Accepted** (2026-10-24). Amends [0006](0006-ephemeral-jit-runners.md) and [0032](0032-sizing-advice-from-sampled-peaks.md).

## Context

On the owner's host, over 14 days, an uncontended job waited 17 to 25 seconds (p25 to p50): up to a 15-second poll
before runnerd sees it queued, then the JIT registration and the container's start. Only about one job in four
started within 20 seconds. A runner already started and registered takes a job as soon as GitHub hands it out, which
is about a second after the job is queued. The longer waits are contention, which auto sizing
([0049](0049-auto-sizing-without-hand-tuning.md)) addresses; this is the floor under every job.

## Decision

### What a warm runner is

A warm runner is an idle runner of a repository that no queued job of that repository is waiting for: one started
ahead of jobs, or one whose job went to another runner. It is an ordinary ephemeral JIT runner
([0006](0006-ephemeral-jit-runners.md)) with the repository's labels; GitHub hands it any job those labels match, and
it runs that one job and exits like any other. Nothing about it is stored on its container, so a restarted daemon
adopts it like any idle runner.

### How many

`[[repositories]] warm`, default 1, is how many warm runners the repository keeps while it is active; 0 turns warm
runners and bursts off for it. The owner sets nothing: the default pays off on any repository that gets jobs, and a
quiet repository keeps none.

A repository is active while its last job was queued within the last hour, or during an hour of the day (UTC) in
which its jobs were queued on 3 or more of the last 14 days. Outside those, its warm runners stop once they waited
`IDLE_TIMEOUT` (5 minutes), and none starts again until a job comes or a usual hour begins: a warm runner is never
restarted in a loop while no jobs come.

### Bursts

When a job of a workflow run is seen, queued or taken by a runner, the run's remaining jobs get warm runners too: the
median number of jobs per run of that workflow over the last 14 days (3 runs at least), less the run's jobs seen so
far. A run counts for the 90th percentile of the time its workflow's runs took from their first job queued to their
last, plus `IDLE_TIMEOUT`, from its first job queued; after a restart, from its open jobs' queued times. A run is
remembered for a day, so a job of it seen again does not start it over. Runs are told apart by attempt: a re-run
gets its burst. Later `needs:` waves count among the run's jobs, so their runners start with the first wave and
wait. A wrong guess costs an idle runner until the run stops counting.

A repository's target is its `warm` while active, plus its runs' remaining jobs.

### Cost: last in line, first to yield

- A warm runner reserves what a runner of its profile reserves with nothing to predict from: its `memory` cap, and
  under `cpu_overcommit` its `cpus` (at most the CPU budget). GitHub may hand it any job of its repository without
  asking runnerd, so that job must already fit. Once it takes a job, measured admission re-reserves for that job as
  for any runner.
- Warm runners start only after every queued job the plan can start has its runner, and none starts while a queued
  job waits for a slot, memory or CPU, or while measured admission holds the slice at its high-water mark. Each
  active repository gets its first warm runner before any gets a second.
- A queued job that has no free slot, memory or CPU takes them from warm runners of other repositories: the plan
  stops as many as make it fit (those in its profile first) and starts the job's runner in the same tick, in the slot
  freed. If GitHub hands a stopped runner a job first, the stop fails, and the job's start is skipped until the next
  tick. A warm runner never yields to another warm runner.
- A profile due a reseed (slot 1 ran a job; a warm runner leaving slot 1 without one changed nothing) whose runners
  are all idle stops its warm runners and starts none until the reseed ran, so warm runners never starve its
  snapshot.
- The shared caches' check (and the mirror's stop) waits for no runner at all, which warm runners could put off all
  day. An hour after the last check, once a runner ended since, every profile whose runners are all idle stops its warm
  runners and starts none until the check ended: a runner started meanwhile would mount the volumes it may remove.
  The daemon's start counts as a check.
- When a warm runner a start replaces took a job before its stop, every later start of that plan waits for the next
  tick too: they counted on its room.
- A warm runner that ends without a job after waiting `IDLE_TIMEOUT` had started and connected: its end (Docker
  restarted, the runner removed on GitHub) does not hold its repository's next runners back; the second in a row
  does, so a fault that kills idle runners does not restart one every few minutes. Runners that fail while a
  repository is held started before the hold, so they do not lengthen it.

### Measured

A job started on a warm runner when its runner started before GitHub queued it; the history has both times, so no
schema change. The sizing report gains, per profile, how many jobs started on a warm runner and the wait p50 of warm
and of cold starts. Slot use in sizing (and so auto sizing's "slots busy half the time") counts a runner from when
its job was queued, and a runner that took no job not at all: a warm runner's idle time is not demand for slots.

### Shown

A warm runner's slot reads "warm, waiting for a job of `owner/name`", not as a runner waiting for its job. The
status's runners gain `warm`.

## Consequences

- A repository with jobs keeps one runner registered and one container up through its active hours: about the
  memory of an idle runner and its Docker daemon, and a slot. Outside them nothing changes. On a host whose profiles
  are shared by several repositories, warm runners fill free slots, and give them back to the first queued job that
  needs one.
- Under `caps`, a warm runner's cap is reserved while it waits, so the budget left for other profiles' warm runners
  is smaller; real jobs are unaffected, as they take it back.
- The shared caches are emptied, and the mirror stopped, only when no runner runs at all: on an active day, an hour
  after the last check, at the first moment no job runs, warm runners stop for it and start again the next tick.
- `runnerd simulate` replays admission without warm runners.
- A warm runner may wait across a runner release. `--disableupdate` is a `config.sh` flag that `run.sh --jitconfig`
  does not take (the JIT config's settings come from GitHub); a runner that updates exits with code 3 or 4, and
  `run.sh` relaunches it with the same config.
- Rejected: a fixed time to live per warm runner (it would restart one every TTL on a quiet day, or leave none on a
  busy one), a warm runner per profile (runners register per repository), starting a later `needs:` wave's runners
  only when the history says it comes (its runners would start cold whenever the history is off; an idle wait costs
  less), and a column recording warm starts (the times already say it).
