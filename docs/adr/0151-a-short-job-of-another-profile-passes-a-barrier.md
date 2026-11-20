# 0151. A short job of another profile passes a barrier

Status: **Accepted** (2026-11-20). Amends [0096](0096-a-job-starved-of-memory-or-cpu-goes-first.md): what a job behind a
barrier says, [0097](0097-a-job-may-start-behind-a-barrier-it-cannot-cost.md): who passes and its first bound,
[0102](0102-the-larger-host-margin-stands-only-while-no-waiter-fits-the-plain-one.md): who the band reads as a waiter,
and [0131](0131-a-job-passes-a-barrier-unless-what-passed-would-keep-its-owner-out.md), whose rule 4 short jobs keep.
No wire, schema or config change.

## Context

On 2026-11-19 at 21:22:41 UTC a `pr-policy` job of fretboard's `light` pool queued. A light runner reserves up to 2G,
and the light jobs queued then had p95s of 16 to 75 s. It started at 21:29:58, after 7m17s, with no host wait and no slot
wait recorded: it waited behind barriers. `check:backend:static` (jvm-heavy, 11G) held from 21:22:30,
`check:backend:it` (jvm) from 21:23:07, the first again from 21:25:26, then the second until 21:29:52. Programs outside
the slice held most of the host: its limit for the slice went from 25.8G to 12.1G and back in that time, and the larger
margin stood all but one reading.

Between 21:23:10 and 21:24:34 the host had 1.4 to 6G to spare by the larger margin, the slice and CPU had plenty, no
runner had passed a barrier since 21:22:30, and no light job passed. Rules 1, 3 and 4 of 0097 hold by those figures, so
rule 2 found no time S before which the owner's room could come, or one shorter than 75 s. A runner GitHub has given no
job yet (the jvm-heavy runner started at 21:23:07 had none for ten seconds), a job past its usual end, a job with no
estimate and the growth count's credit room (it kept 19.7G for growth at 21:23:07) all count as giving their room back
now. While the host is this short one of them nearly always stands, so S is now or close to it and nothing passes. From
21:24:40 to 21:27:15 the host had nothing to spare by either margin, and from 21:27:21 light jobs passed one at a time,
as rule 4 counted each at its 2.75G cap. That evening, 20:55 to 23:35, fretboard's light jobs waited 98.7 minutes past
their first 30 seconds with neither a host wait nor a slot wait recorded, 55 of them more than a minute.

What such a pass costs the owner does not need S. A job that usually runs seconds gives back what it took seconds later;
if the room comes the moment it starts, the owner waits that long. What it must not do is take room the owner, or a job
older than it, could already use, nor keep doing so job after job.

## Decision

Behind a barrier whose owner is of profile O, a younger job of pool P in profile Q starts, besides 0097's rule, if all
of these hold:

1. It has a free slot and fits the budgets left, as under 0097.
2. Q is not O. The owner's own slots stay 0097's, which leaves it one.
3. U(P), the largest usual run time of P's queued jobs, is known and at most `SHORT_RUN`, 2 minutes. If any of them has
   no estimate, P's jobs do not pass, as under 0097.
4. U(P) ends before any job older than the owner that a drain serves could start, by the running runners' usual ends
   as 0097 reads them. If such a job could start now, or no end brings its room, nothing passes as a short job. A short
   job that outlasted an older job's wait would delay it, its end, and the next older job's start after it, and so on
   to the owner.
5. The owner's room has not come even with what passed a barrier given back: those runners' takes added to the budgets
   left, the credit not counted. Once only what passed keeps the owner out, nothing more passes as a short job.
6. 0131's rule 4: the caps of what passed since the barrier matured, this one's included, stay within what a drain gives
   less the owner, and less an older job a drain serves if that needs more. A runner counts at the caps its container
   started with: a reload that lowers its profile's caps does not change them, and one that removes the profile leaves
   it running, so it still counts.
7. The usual run times were read from the history within the last minute. When a read fails the last one stands, and
   0097 goes by it as before, but a pool's runs may have grown longer since: no job passes as short until a read
   succeeds.

Rules 4 and 5, and 0097's own times, read the host's room by the plain margin while the larger one stands. Under the
larger margin the room comes later, so this lets less pass, and a job passes under one margin exactly when it passes
under the other: its reason does not change with the band it moves.

`priority::Backfill` stays the one rule; `plan` and `admission::simulate` both call it, and it keeps nothing between
plans. A reload that changes a profile, its caps or a job's needs is read again by the next plan, and a restart leaves
nothing behind: runners it adopts did not pass (0131), so rule 6 does not count them and rule 5 does not give them
back.

### `SHORT_RUN`

Two minutes is what a served pool's next job already waits before it may hold (`SERVED_PRIORITY_AFTER`), so a short pass
costs an owner no more than its own pool accepts between two of its jobs. Over the job keys that ran at least three times
since 2026-11-13, the p95 of their last 20 successful runs is at most 2 minutes for 38 of fretboard's 40 light keys, all
4 of jvm-small's, 5 of node's 20 and none of jvm's or jvm-heavy's; for trellis's light keys 4 of 4, glowly's 4 of 8.
U(P) is the largest p95 among the queued jobs of the pool, so a pool with one long job queued does not pass as
short. It is its own constant with no config key, as 0096's limits are.

### What a job the barrier lets pass says

A job that rule 0097 or this one lets pass but that does not fit the budgets left says what it lacks
(`Waiting::Memory`, `HostMemory`, `Cpu`), not `BehindMemory`: the barrier no longer holds it. The owner's own younger
jobs, which 0097 lets pass where two slots are free, say so too.

That matters for the host's band. Under 0102 the larger margin stands while jobs wait for the host and none fits the
plain one, and jobs behind a barrier are not waiters. A light job that would pass but fits only the plain margin was
then held by a band that stood because the barrier hid it. Saying `HostMemory`, it is a waiter: the band goes, and the
next plan starts it. A job that waits for the host but lacks CPU too, behind a barrier or not, would not start by the
plain margin either: the band reads it as one that fits no margin (`Plan::host_needs`), or it would go for it, the job
would then say it waits for CPU, and the band would come back, plan after plan. The barrier does not swap with it, as 0102's drained host reads the larger margin whenever a runner
runs, whichever band stands. Its wait counts in `host_waited`, as it is a wait for the host, and the insight on programs
outside the slice hears of it.

No new reason, no new word: a client of the previous release reads every reason it now gets.

### What it promises

0097's first bound, no later than a strict barrier if each run takes what the plan read, still holds for an owner nothing
short passed. A short pass replaces it with this one: **if each run takes what the plan read, an owner that keeps its
barrier starts at most `SHORT_RUN` after it would under a strict one.** Rules 4 and 5 are what bound it: a short job
ends before any older job could start, so none of them is delayed and none delays the owner in turn; once the owner's
room is there but for what passed, nothing more passes, and what passed ends within its run. It is checked over
generated runs, not proved.

0131's second bound is unchanged, and is the promise against starvation: **whatever the run times do, the owner fits once
every runner started before the barrier matured, for a job older than it, or with no barrier standing has ended**, as
rule 5 keeps what passed, at its caps, within what a drain leaves the owner. A steady stream of short jobs cannot keep it
out; a restart turns what passed into runners the next barrier waits out, which end.

### The proof

- Unit tests that fail under each mutant: no short pass while a runner is past its usual end, with no runner's end
  bringing the room, or with a runner that has no job; a short job of the owner's profile passing; a short pass once
  only what passed keeps the owner out; a short pass past rule 6's caps; a job that may pass saying it waits behind the
  barrier.
- `a_short_job_ends_before_an_older_job_could_start`: with exact run times, older jobs of 6G and 7G and a 7G owner in
  10G, two short passes would hold the owner 209 s past its strict start. The first pass waits for the older job's
  start in 1 s, the second for the next one's in 30 s; with that job ten minutes away it passes.
- `a_runner_that_passed_counts_at_the_caps_it_started_with`, and through the engine
  `a_runner_that_passed_counts_at_its_caps_through_a_reload`: a passed runner counts at its launch caps after a reload
  lowers them, and one of a removed profile still counts.
- `a_job_passes_by_the_plain_margin_whichever_stands` and `a_job_short_of_cpu_too_is_no_waiter_the_plain_margin_starts`:
  the two ways a job's reason followed the band it moved.
- `a_replay_reads_no_usual_end_of_a_runner_whose_job_has_no_name_yet`: the replay gives a running job a usual end only
  once the daemon could name it, and none to a runner started at that moment, as the daemon's plan does; before, it let
  a job pass by 0097, and so counted it as a host waiter, where the daemon held it behind the barrier.
- Through the engine, `a_short_job_passes_only_by_run_times_read_within_the_minute`: with the history unreadable and the
  last read two minutes old, a job that passes as short when the read succeeds waits behind the barrier.
- Through the engine: a short job behind a barrier that fits only the plain margin lets the larger margin go and starts at
  the next plan, and stays queued forever if it says `BehindMemory`.
- `a_steady_stream_of_short_jobs_cannot_keep_a_barriers_owner_out`: 64 generated runs, minute ticks, a big job of more
  than half the slice, five to seven three-minute small jobs a minute read as two minutes, mid jobs now and then, run
  times read at half so that only short jobs pass, and restarts at random past a barrier's limit. The owner starts in
  every run, no later than a tick after every runner that did not pass has ended. With rules 5 and 6 both removed it
  never starts in some runs.
- `with_run_times_that_read_short_an_owner_waits_at_most_a_short_run_longer`: 0097's generated runs on minute ticks,
  forked at each barrier with and without passes. No owner that kept its barrier starts past the later of its strict
  start and the end of what passed it, nor more than 2 ticks late. Without rule 5 it is 4; rule 4 does not move it
  there, and the exact schedule above is its test.
- The replay and planner's shared fixture now has a one-minute job beside 0097's three-minute job that waits for the
  older job's slot: it would outlast that wait too, so it passes as a short job once the older job starts, and the
  replay is checked against `plan` for both rules.

The existing generated worlds run in 150 s to 600 s ticks, where no job is short; their counts do not move.

## Replay

`runnerd simulate --causal --count growth` over a copy of the history from 2026-11-12 to 2026-11-20, with the slice at
46G and 32 CPUs, before and after this change. The figure is job-hours waited; "shift" queues every job that much
earlier, packing a busier day into the same host. The replay's own error against what was recorded is in brackets, and
the mean waits are fretboard's.

| Window | Before | After | light | jvm | jvm-heavy |
| --- | --- | --- | --- | --- | --- |
| 12 h (-30%) | 32.0 | 31.6 | 26 s to 24 s | 5m39 to 5m35 | 3m42 to 3m47 |
| 24 h (-33%) | 37.3 | 36.9 | 27 s to 26 s | 4m42 to 4m40 | 3m11 to 3m15 |
| 48 h (-50%) | 49.9 | 49.5 | 23 s to 22 s | 3m28 to 3m26 | 3m10 to 3m12 |
| 60 h (-52%) | 87.2 | 87.7 | 26 s to 28 s | 4m44 to 4m29 | 4m22 to 4m09 |
| 72 h (-51%) | 96.9 | 97.4 | 25 s to 27 s | 4m00 to 3m48 | 3m39 to 3m29 |
| 24 h, 30 min shift (+1%) | 54.8 | 55.2 | 29 s to 26 s | 6m11 to 7m26 | 4m20 to 4m11 |
| 24 h, 1 h shift (+6%) | 59.1 | 58.0 | 40 s to 29 s | 7m03 to 8m19 | 4m15 to 4m42 |
| 24 h, 4 h shift (+169%) | 147.3 | 146.4 | 1m52 to 1m18 | 11m43 to 13m35 | 9m20 to 10m56 |
| 60 h, 30 min shift (-23%) | 126.8 | 141.0 | 38 s to 45 s | 5m40 to 6m26 | 5m46 to 5m39 |
| 60 h, 1 h shift (-13%) | 158.0 | 160.4 | 1m00 to 58 s | 6m45 to 7m28 | 6m02 to 6m34 |
| 60 h, 2 h shift (-43%) | 104.3 | 105.3 | 34 s to 37 s | 5m15 to 5m04 | 4m52 to 4m49 |

Where the replay is close to the 12 to 48 hours that were recorded, every pool waits about the same or a little less.
Packed tighter, light jobs gain most and jvm pays, up to two minutes a job in the 4 h shift, which is the bound met over
and over. Over 60 hours the change is a wash in three windows and costs 14 job-hours in the 60 h, 30 min shift, where
light jobs wait longer too; the replay does not say which passes cost that. Earlier variants, replayed on a copy that
lacked the host's readings after 2026-11-19 11:00, did no better: letting short jobs pass without the reason change
waited longer over 60 hours, as the band held them anyway, and counting the credit in rule 5 or a 60 s `SHORT_RUN` was
mixed.

## Consequences

Short jobs of other profiles go on behind a barrier whenever its owner's room is not there and no older job is about
to start, which under a short host is most of the time it stands. While the history cannot be read, none does. `plan`
reads the runners of profiles a reload removed, and each runner's caps from its container, for every barrier, not only
for short passes; a job that waits for the host but lacks CPU too no longer lets the larger margin go, behind a barrier
or not. An owner can start up to a short run later than under a strict barrier, where 0097 promised
none. A job behind a barrier that the barrier would let pass now says what it lacks, so `host_waited` and the host
insight count more, and less is said to wait behind a barrier.
