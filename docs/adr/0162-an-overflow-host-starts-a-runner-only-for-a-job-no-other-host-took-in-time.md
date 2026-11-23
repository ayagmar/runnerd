# 0162. An overflow host starts a runner only for a job no other host took in time

Status: **Accepted** (2026-11-23). Adds `[host] role`, `[host] overflow_after`, `config::Role`,
`Config::warm`, `scheduler::OVERFLOW_IDLE_TIMEOUT`, `scheduler::idle_after`, `scheduler::Repository::idle_after`,
`api::Waiting::Overflow`, `api::WaitCause::Overflow`, `sizing::Ran::overflow_waited`, `config::Warning::OverflowKeepsNoWarm`
and `config::Warning::OverflowAutoSizing`; the engine gains `engine/overflow.rs`.

## Context

A second machine, a laptop, may serve the same repositories with the same labels as the desktop, each with its own
daemon and no coordinator. Every daemon starts a runner for each queued job its pools match, so a job queued on a
cold queue gets one runner on each machine. GitHub gives the job to whichever connects first, and the other runner
stays online and idle. GitHub hands the next matching job to any online idle runner at once, so the laptop's spare
runner takes it before the desktop has booted one: for up to `IDLE_TIMEOUT` the laptop is the first choice, with cold
caches, possibly on battery.

The laptop should take only what the desktop cannot: jobs that wait because the desktop is full, asleep or off.
The audit of the multi-machine plan (findings H1, M2, M3 and M4) named what a role has to get right beyond a delay:

- `warm` defaults to 1, and a repository stays active ten minutes after one of its jobs finishes here, so after its
  first job the laptop keeps an idle runner that takes the next job (H1).
- GitHub's `created_at` is 0 when it does not parse, which reads as 1970 and passes any delay (M2).
- `list` reads runners before queued jobs, so the runners listing of the tick that first shows a job began before it;
  a start from it cannot see a runner another machine registered for the job since (M3).
- The deliberate delay lands in the job's wait, where sizing reads it as a wait for a slot (M4).

## Decision

- **`[host] role = "primary" | "overflow"`**, default `primary`: one machine behaves as before. Run one primary per
  set of labels; two primaries on the same labels still both start a runner for a job on a cold queue.
- **`[host] overflow_after`**, default 45 s, read by an overflow host only.
- **An overflow host keeps no warm runner and starts no burst.** `Config::warm` is 0 for every pool there, whatever
  `warm` says; auto sizing's room for warm runners reads the same. A job it lists queued does not make its
  repository active (`note_queued` notes nothing), as it is most likely another machine's: only jobs this host took
  or finished do.
- **An idle runner stops sooner.** The plan takes the idle expiry as input, `Repository::idle_after`:
  `IDLE_TIMEOUT` (300 s) on a primary, `OVERFLOW_IDLE_TIMEOUT` (120 s) on an overflow host. A runner an overflow host
  started for a job the primary then took leaves within two minutes, not five.
- **A job is the overflow host's once both hold:** its age is `overflow_after` or more, counted from GitHub's
  `created_at`, or from when this host first listed it when `created_at` is 0; and this host listed it twice, the
  second time after a runners listing of its repository that began after the first. A runners listing that fails or
  that the rate limit refuses holds every job of the repository until one answers. `engine/overflow.rs` owns what this
  host saw of each job and of the runners listings; it reads no clock.
- **Held jobs are left out before the plan.** `Engine::plan` takes them out of each pool's queued jobs, after the
  jobs this machine's runners may have taken, so they count for no demand, no barrier, no slot wait and no host wait,
  and an idle runner covers only jobs the host may serve. Each says `Waiting::Overflow { until }`: until when its
  time is left to the other machines, in the past while it waits for the second listing. Core words it, `Left to the
  primary machine for 30s more` or `Left to the primary machine until GitHub lists it again`, as `WaitKind::Soon`.
- **The deferral is its own wait cause.** `WaitCause::Overflow` (`overflow` in `jobs.wait_causes`) counts it, as
  ADR 0154 counts every cause. Sizing reads it back as `Ran::overflow_waited` and takes it off each job's wait, as it
  takes off `host_waited`: no slot would have saved it. No schema step: the causes are a JSON object by word.
- **check-config warns** on an overflow host for each pool whose `warm` is above 0, which it does not keep, and for
  `[sizing] auto`, which sizes the profiles from the jobs no other machine took, the tail of the queue.
- **A primary needs no change.** If it starts a runner for a job the overflow host is booting one for, the spare
  runner sits on the primary, where an idle runner is a warm one. Only the overflow host's decision prevents the
  costly duplicate, and a primary on an older runnerd works beside it.
- **A run split across machines is not re-run by either.** ADR 0153 stays: `faulted` needs a runner fault proven by
  this host's own runner, so a run whose failed job ran on the other machine is never re-run here.

What survives: nothing of `engine/overflow.rs` survives a restart, so a job a restarted overflow host finds queued
waits for two listings of its own with a runners listing between them, though its age still counts from GitHub's
time. A reload keeps it, and a reload that changes the role applies from the next plan; the primary keeps the same
record, so a reload to overflow finds the jobs' first sightings known. A sleep of the host forgets it, as nothing
listed before the sleep holds after it (ADR 0159).

## Consequences

- With the desktop up and free, a job is gone from the queue long before 45 s, and the laptop never starts a runner
  for it. With the desktop full, asleep or off, a job waits 45 s plus up to a listing interval more, then runs on the
  laptop.
- An older client reads `Waiting::Overflow` as `Unknown` and says the job waits for a reason it does not know
  (accepted in the wire check), and reads the `overflow` cause as `Other`.
- On an overflow host, insights and the dashboard's waits still count the deferral as part of a job's wait, which is
  what the job saw; the per-cause split names it, and only sizing leaves it out.
- The overflow host still cannot see a runner the primary registered for a job seconds ago: GitHub lists no runner's
  labels, and this change counts none. If duplicates still show on the laptop, a later change counts the other
  machines' fresh runners before an overflow start.
