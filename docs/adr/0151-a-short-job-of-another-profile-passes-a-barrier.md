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
4. The owner's room has not come, nor that of a job older than it with a free slot, even with what passed a barrier given
   back: their runners' takes added to the budgets left, the credit not counted. Once only what passed keeps the owner
   or an older job out, nothing more passes as a short job.
5. 0131's rule 4: the caps of what passed since the barrier matured, this one's included, stay within what a drain gives
   less the owner, and less an older job a drain serves if that needs more.

`priority::Backfill` stays the one rule; `plan` and `admission::simulate` both call it, and it keeps nothing between
plans. A reload that changes a profile, its caps or a job's needs is read again by the next plan, and a restart leaves
nothing behind: runners it adopts did not pass (0131), so rule 4 does not count them and rule 4 above does not give them
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
next plan starts it. The barrier does not swap with it, as 0102's drained host reads the larger margin whenever a runner
runs, whichever band stands. Its wait counts in `host_waited`, as it is a wait for the host, and the insight on programs
outside the slice hears of it.

No new reason, no new word: a client of the previous release reads every reason it now gets.

### What it promises

0097's first bound, no later than a strict barrier if each run takes what the plan read, still holds for an owner nothing
short passed. A short pass replaces it with this one: **if each run takes what the plan read, an owner that keeps its
barrier starts at most `SHORT_RUN` after it would under a strict one.** Rule 4 above is what bounds it: once the room is
there but for what passed, nothing more passes, and what passed ends within its run. It is measured, not proved: a short
pass that delays an older job delays what that job then gives back.

0131's second bound is unchanged, and is the promise against starvation: **whatever the run times do, the owner fits once
every runner started before the barrier matured, for a job older than it, or with no barrier standing has ended**, as
rule 5 keeps what passed, at its caps, within what a drain leaves the owner. A steady stream of short jobs cannot keep it
out; a restart turns what passed into runners the next barrier waits out, which end.

### The proof

- Unit tests that fail under each mutant: no short pass while a runner is past its usual end, with no runner's end
  bringing the room, or with a runner that has no job; a short job of the owner's profile passing; a short pass once
  only what passed keeps the owner or an older job out; a short pass past rule 4's caps; a job that may pass saying it
  waits behind the barrier.
- Through the engine: a short job behind a barrier that fits only the plain margin lets the larger margin go and starts at
  the next plan, and stays queued forever if it says `BehindMemory`.
- `a_steady_stream_of_short_jobs_cannot_keep_a_barriers_owner_out`: 64 generated runs, minute ticks, a big job of more
  than half the slice, five to seven three-minute small jobs a minute read as two minutes, mid jobs now and then, run
  times read at half so that only short jobs pass, and restarts at random past a barrier's limit. The owner starts in
  every run, no later than a tick after every runner that did not pass has ended. With rules 4 and 5 both removed it
  never starts in some runs.
- `with_run_times_that_read_short_an_owner_waits_at_most_a_short_run_longer`: 0097's generated runs on minute ticks,
  forked at each barrier with and without passes. No owner that kept its barrier starts past the later of its strict
  start and the end of what passed it, nor more than 2 ticks late. Without rule 4 it is 4.
- The replay and planner's shared fixture now has a one-minute job that passes at once beside 0097's three-minute job
  that waits for the older job's slot, so the replay is checked against `plan` for both rules.

The existing generated worlds run in 150 s to 600 s ticks, where no job is short; their counts do not move.

## Replay

`runnerd simulate --causal --count growth` over the history up to 2026-11-20, with the slice at 46G and 32 CPUs, before
and after this change. The figure is job-hours waited; "shift" queues every job that much earlier, packing a busier
day into the same host. The replay's own error against what was recorded is in brackets.

| Window | Before | After | Light jobs, mean wait |
| --- | --- | --- | --- |
| 12 h (-30%) | 32.0 | 31.5 | 26 s to 24 s |
| 24 h (-30%) | 37.3 | 36.8 | |
| 48 h (-30%) | 61.9 | 61.4 | |
| 30 min shift | 54.8 | 48.0 | every pool waits less |
| 1 h shift | 59.1 | 58.7 | 40 s to 29 s; jvm and node wait more |
| 4 h shift (+171%) | 147.3 | 147.1 | 1m52 to 51 s; jvm 11m43 to 14m56 |
| 60 h (+186%) | 523 | 636 | every pool waits more |
| 60 h, 30 min shift | 117.1 | 103.6 | |
| 60 h, 1 h shift (-31%) | 127.2 | 127.6 | |
| 60 h, 2 h shift (-48%) | 96.5 | 106.3 | |
| 72 h (+208%) | 608.6 | 649.2 | 3m17 to 3m58; jvm and jvm-heavy wait less |

Where the replay is close to what happened, every window waits a little less and light jobs most. Where it packs the
host tighter than it ever was, light jobs still gain and the big pools pay for it, up to three minutes for jvm in the
4 h shift, which is rule 4's bound met over and over. The replays that run two to three times over what was recorded
are mixed both ways. Of the variants tried, letting short jobs pass without the reason change waited 727 job-hours over
60 h against this change's 636, as the band then held them anyway; counting the credit in rule 4 was mixed, and a 60 s
`SHORT_RUN` was mixed and worse in the 4 h shift.

## Consequences

Short jobs of other profiles go on behind a barrier whenever its owner's room is not there, which under a short host is
most of the time it stands. An owner can start up to a short run later than under a strict barrier, where 0097 promised
none. A job behind a barrier that the barrier would let pass now says what it lacks, so `host_waited` and the host
insight count more, and less is said to wait behind a barrier.
