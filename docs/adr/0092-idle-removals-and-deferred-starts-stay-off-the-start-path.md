# 0092. Idle removals and deferred starts stay off the start path, and a job's wait says where it went

Status: **Accepted** (2026-11-01). Amends [0076](0076-a-slot-is-free-once-docker-removed-its-container.md): a start
that no longer fits makes the tick plan again. Amends [0091](0091-a-signal-leaves-running-jobs-to-the-next-start.md):
every idle removal uses the detach's order, not only a detach's.

## Context

After 0.0.26, a job whose runner started after it was queued waited a median 75 to 79 s from its queueing to its
runner's container start on the owner's host (jvm, light and node alike), with `poll_interval` at 15 s and slots
mostly idle. The container's start to the job's start took 6 s. Two causes show in the journal:

- **Idle removals stalled the starts behind them.** Removing an idle runner deregistered it, then waited for its log
  capture to end before removing its container. A capture ends when its container stops, so each removal waited
  `CAPTURE_DRAIN` (10 s) and only then removed the container: the median gap before a "removed idle" line was 11.5 s,
  one after the other, against 1.1 s before a start. The plan's starts came after its removals. Detaching stops
  (ADR 0091) already removed first and drained after.
- **One deferred start held back every later start of the plan.** A start admitted again just before it starts
  (ADR 0076) that no longer fit set a flag that skipped the plan's remaining starts, other profiles' too, until the
  next poll. The log said so once per tick, not per start skipped.

Of the cold jobs since 0.0.26, the 335 during whose wait the journal logged neither an idle removal nor a deferral
waited a median 15 s to their container's start, the 236 with removals only 39 s, and the 969 with a deferral 93 s.

Nothing recorded when runnerd first listed a job, so the part of a wait before runnerd knew of the job (GitHub's
listing, the poll interval, the tick's own work, downtime) could not be told from the part after.

## Decision

- **Idle runners are removed together, ahead of the starts** (`Engine::remove_idle`). A plan's stops run first, all at
  once: each runner is deregistered, a few GitHub requests at a time, then the containers are removed together, and
  only then do their captures end, which they do with their containers. GitHub's `Busy` still keeps a runner that just
  took a job, as does one whose capture named a job by the time GitHub answered. GitHub also answers "not found" for a
  runner whose job ended, as an ephemeral runner's registration goes with its job: such a runner stays when its log
  shows a job. Either ends as a busy runner does, its log read and a failed job recorded before its container goes;
  one whose log cannot be read stays until it can. A runner whose deregistration or removal fails keeps its slot and
  reservation until a later removal is confirmed (ADR 0076). A forced stop still kills runner by runner. Runners that
  ended on their own are released as before: their logs are read and their failed jobs recorded before the removal.
- **A deferred start makes the tick plan again** from the same listings, after the starts that went ahead, rather
  than skipping the rest of the plan until the next poll. It plans again only after a plan that removed or started a
  runner, at most three plans a tick, so a start that keeps not fitting costs one plan more, not a loop. Admission
  stays one start at a time against what runs by then. A replan lists nothing on GitHub: the runners it started
  count as idle against the jobs they were started for, as at the next poll. Once a stop is asked for, mid-tick
  included, the tick neither plans again nor starts another runner, and a runner GitHub registered while the stop
  came is deleted before its container starts: a detaching stop would leave the job it takes running unwatched.
- **Starts stay one at a time.** A start takes about 1.6 s on the owner's host. Starting them concurrently would need
  a pending-start state that every budget, slot, upkeep and status path counts, cancellation at a stop, and orphan
  sweeps that spare registrations not installed yet. The figures below do not ask for it.
- **GitHub is polled as before**: every `poll_interval` after a tick ends, runners before queued jobs. A shorter tick
  only brings the next poll sooner, back to the configured interval.
- **A job's history keeps when runnerd first listed it** (`jobs.seen_at`, schema 19): the time GitHub answered the
  first listing that showed it queued, kept across later listings and restarts. It is written as each repository's
  listing answers, before the others do, so a warm runner that takes the job meanwhile, or a stop that ends the tick,
  does not lose it; a job a runner claimed first gets it if the listing answered before its runner took it, and keeps
  its state. `Timeline::seen_at` carries it and `Timeline::milestones` shows it as "Seen by runnerd", between "Queued"
  and "Container started", so a job's wait reads in three parts: runnerd had not seen it, runnerd had seen it and not
  started its runner, the runner booted. It is `None` for a job no listing showed queued before its runner took it, as
  one a warm runner took first, and for one an older runnerd recorded. `unseen` (ADR 0081) keeps its meaning: the part
  of the wait no runnerd ran; the time before "Seen by runnerd" includes it and more, so it is not called GitHub's
  lag.
- **A tick logs its pace**: how long it took to reach its last start, by phase (collecting, listing, following,
  removing, starting), with its removals, starts and plans. At info level when that took longer than
  `poll_interval`, at debug level otherwise. A tick whose starts are still deferred after its last plan says how
  many.
- **A runner's record says why it started and why it went.** `runners.purpose` is `job`, `warm` (one of the
  repository's configured warm runners, counting only its idle runners left over once its queued jobs have theirs) or
  `burst` (one for a job its runs are expected to queue, ADR 0050), and the start's log line says which.
  `runners.stopped_for` is why an idle runner was removed (`scheduler::StopReason`): `expired`, `room`, `upkeep`,
  `paused`, `closed`, `changed`, `over_budget`, `clean` or `shutdown`; the "removed idle" line says it in words.

## Consequences

- On a fake workload with the owner's latencies (a GitHub request 0.3 s, a container's start and the removal of a
  running one 1 s each; captures that end only with their containers), three jobs queued every 40 s beside three
  whose runners another machine's runners outpace: queued to container start went from a median 20.9 s and p90
  39.2 s to 12.8 s and 18.9 s (`tests/pacing.rs`, `workload_figures`). Its ticks being shorter, each repository was
  listed 36 times in 12 minutes instead of 27.
- On the owner's host the expected median is that of the jobs whose waits saw neither cause, about 15 s, for jobs
  that find a free slot and room. That is an expectation; the history's `seen_at` and the pace lines measure it.
- The warm rule is unchanged. Of 479 idle removals since 0.0.26, a median 93 s after their start, 25% came after
  `IDLE_TIMEOUT`; of the others, 162 (34%) were followed within a minute by a reseed of their profile (upkeep) and 174
  (36%) by a job's start (likely room for a queued job). Of the 140 reseeds in that time, 135 changed nothing ("not
  seeded"). Each removal used to cost 11.5 s of the starts after it; now it costs about the removal itself. The stop
  reasons recorded now tell those causes apart before the rule changes.
- 0.0.26 refuses a schema-19 history as newer. A client of 0.0.26 reads the new `Timeline` and ignores `seen_at`.
