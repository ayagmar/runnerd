# 0100. An insight for jobs held for the host's memory

Status: **Accepted** (2026-11-06). Extends [0051](0051-insights-and-notifications.md); amends
[0057](0057-name-what-holds-the-hosts-memory.md), whose names it sends to the owner unasked.

## Context

Admission keeps a margin of the host's memory free ([0055](0055-admission-follows-the-hosts-memory.md)), and a
job whose runner does not fit what the host has to spare waits with the reason `HostMemory`. The status names the
programs outside the slice that hold the most memory (0057), but only to someone who opens it. Over one heavy
evening 288 job-hours of waiting were for the host's memory, and what moved the queue was the owner closing programs,
which nothing told them to do. Insights and notifications named jobs that got slower, flaky or throttled, never jobs
held for the host.

## Decision

- **A warning insight**, `host-held`, when a job waits for the host's memory now and a queued job has waited 30
  minutes or more for it (`HOST_HELD_AFTER`, `jobs.host_waited`). One insight for the host, not per job: the cause
  is the host's memory, and the owner's act is closing programs. Its repository is the longest waiter's.
- **Worded once, by core.** `Insight::host_held_sentence` says how many jobs wait, the longest wait and its job, then
  `HostMemory::outside_line` (0057: names only, never a command line): `3 jobs wait for the host's memory, the longest
  for 42m 10s (halyard/api: CI / build); programs outside CI hold 30G, most java 9.3G and claude 5.1G. Closing
  programs frees only part of that.` The last sentence is there because `outside` also counts the kernel, compressed
  swap and the many small programs: the idle desktop reads 9G to 10G. Without holders (cgroup v1) the line has no
  names. `runnerctl insights`, the dashboard, the TUI and notifications show the sentence; the fixtures draw it from
  the same function.
- **Inputs are the daemon's now, not the history's.** `Store::insights` adds `History::host` when the engine's last
  plan had jobs waiting for the host and the store has a reading: the jobs' ids (`Store::host_waiting`, set by
  `note_host` with the plan's time), narrowed to those still queued, with their `host_waited`. A paused repository's
  job waits as `Paused`, so it is not among them: pausing is the owner's own act. A job that starts between the plan
  and the look is left out, and the next longest waiter decides. `Run` is untouched: closed runs would fire the
  insight after the job ended.
- **Fresh or silent.** The reading and the engine's look must each be a minute old at most and not from after the
  look. A daemon that stopped planning, or no reading, says nothing; so does the digest's look at 24 hours ago, whose
  window precedes any reading.
- **Once in each 6-hour window.** The key is `host-held:<now / 6 h>` (`HOST_HELD_EVERY`). The notifier tells a key
  once and again only when it was gone for a day, so a fixed key would stay silent on the next evening; this one
  is new in each window the wait lasts into. Told keys are stored, so a restart inside a window repeats nothing, and
  a wait that ends and returns inside one is not told twice. The windows are aligned to the epoch, not to the last
  message: a wait that lasts across a boundary is told again after the boundary, so two messages can be minutes
  apart. On the week replayed to choose the rule, it gives 2 to 3 messages.
- **A kind with a fallback.** `InsightKind::HostHeld { jobs, waited }`. 0.0.28's `Unknown` reads it by its sentence,
  under the label `Insight`; `crates/wire-compat` accepts that read as a difference.

## Consequences

- With `[notify]` off nothing is sent; the insight is still in `runnerctl insights` and the dashboard while it holds.
- `host_waited` keeps its meaning (the host's wait as labelled by `plan`, which checks the slice's budget first), so
  a job short of both budgets counts as waiting for the host's memory. The insight says what the host holds; it
  does not say closing it would start the job.
- Insights are no longer a pure function of the history alone: `History::host` is a fact of now. Its freshness rule
  is in `insights::host_held`, so the endpoint, the notifier and the digest apply the same one.
- Rejected: a new `Event` variant for each span (`Event` has no fallback, and 252 messages a week), and an
  insight per waiting job.
