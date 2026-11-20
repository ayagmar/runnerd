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
4. U(P) ends before any job older than the owner that a drain by the plain margin serves could start, by the running
   runners' usual ends as 0097 reads them. If such a job could start now, or no end brings its room, nothing passes as a short job. A short
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
under the other: its reason does not change with the band it moves. The plain margin's room is its own count, not the
larger one's plus the margins' difference: the growth count bounds the host's credit by what the host has past the
margin in force, so under the larger margin the credit is smaller too, and the room differs by more than the margins
do. The plan counts the runners as the plain margin would beside the count in force (`Budget::plain`), and counts both
again after each start and stop it makes. The band reads whether a waiter fits the plain margin by the same count. An
older job's start in rule 4 is read by the plain margin, so rule 4 counts every older job a drain by the plain margin
serves; rule 6's allowance keeps room past the owner only for those the drain in force serves, as before.

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
- Through the engine, `a_pass_moves_the_band_only_by_the_plain_margins_own_count`: four settled runners under the growth
  count, a 3.5G runner that passed an 8G owner, and 13G available. The larger margin's count leaves 2.5G, the plain
  margin's 4.5G, as the host's credit is 1G larger by it. Read as the larger count plus the margins' difference, the
  next short job passes as a host waiter, the band drops for it, the plain count then shuts the pass, and the band
  rises again: four plans alternate with nothing started. By the plain count the larger margin stands through all four.
- `a_short_job_waits_for_an_older_job_only_the_plain_margin_serves`: an older 17G job that only the plain margin's 18G
  drain serves, not the larger margin's 16G, starts in a second by the plain margin; a short job waits for it.
- `a_replay_reads_no_usual_end_of_a_runner_whose_job_has_no_name_yet`: the replay gives a running job a usual end only
  once the daemon could name it, and none to a runner started at that moment, as the daemon's plan does; before, it let
  a job pass by 0097, and so counted it as a host waiter, where the daemon held it behind the barrier.
- `the_replay_starts_what_the_daemons_plan_starts_under_the_growth_count`: eight 3.5G jobs under the growth count, through
  `plan` with its recount and through the replay. Both fit a start by the room the count leaves before it, and count a
  runner that starts as one with no job or sample yet. In 13G both start three; the replay's old fit, by the count with
  the start in it, started all eight. In 15G both start eight; reading a start's first sample at once, as the replay did,
  started five.
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

`runnerd simulate --causal --count growth` over a read-only copy of the history from 2026-11-12 to 2026-11-20, with the
slice at 46G and 32 CPUs. "Shift" queues every job that much earlier, packing a busier day into the same host.

This change also makes the replay fit a start as the daemon's plan does, by the room before it, and count a runner
that starts as one with no job or sample yet. That alone moves its figures: the 60 h window waits 87.2 job-hours by the
previous replay and 100.7 by this one with short passes off. So "off" and "on" below are this replay with the short pass
and the band's new waiters switched off, and on. "Run" is the mean time a workflow run takes from its first job's
arrival to its last job's end (`RUN MEAN`); "wait" is job-hours waited (`WAIT H`).

| Window | Run off | Run on | Wait off | Wait on | Passes off | Passes on |
| --- | --- | --- | --- | --- | --- | --- |
| 12 h | 2m34 | 2m28 | 32.8 | 32.1 | 3 | 7 |
| 24 h | 2m22 | 2m18 | 38.0 | 37.3 | 3 | 7 |
| 48 h | 2m22 | 2m20 | 50.8 | 50.0 | 4 | 11 |
| 60 h | 3m42 | 3m35 | 100.7 | 97.5 | 12 | 29 |
| 72 h | 3m37 | 3m31 | 110.2 | 107.4 | 12 | 41 |
| 24 h, 30 min shift | 2m58 | 2m47 | 58.3 | 54.7 | 8 | 109 |
| 24 h, 1 h shift | 2m58 | 2m52 | 59.2 | 56.0 | 6 | 102 |
| 24 h, 4 h shift | 7m11 | 5m35 | 178.5 | 150.2 | 23 | 217 |
| 60 h, 30 min shift | 4m09 | 4m04 | 136.7 | 146.4 | 14 | 177 |
| 60 h, 1 h shift | 4m15 | 4m07 | 139.4 | 138.1 | 34 | 241 |
| 60 h, 2 h shift | 3m51 | 3m54 | 109.7 | 118.6 | 39 | 84 |

The windows as recorded, 12 to 72 hours, all gain a little: runs end 2 to 7 s sooner on average and jobs wait 1.6 to
3.2% less. In the shifted 24 h windows the gain grows with the packing, most in the 4 h shift. Two shifted 60 h windows
wait more job-hours, and the 2 h shift's runs take 3 s longer on average.

The 60 h, 30 min shift was isolated. With only the band's new waiters off it waits 144.9 job-hours; with only short
passes off, 137.7. Short passes cause the difference, not the band. The first divergence is in the window's first
minutes: three light jobs pass and the jvm-heavy owner starts 7 s later than it would. Across the whole window jobs start
0.7 job-hours later in total, and every profile's runs end sooner. The extra wait comes from causal arrivals: a run's
later jobs arrive as its earlier ones end, so a light job that ends sooner brings the next job's arrival forward, and
that job then waits behind the same queue it would have met anyway. The 2 h shift is the same: jobs start 3.9
job-hours sooner in total, yet wait 8.9 job-hours more.

## Consequences

Short jobs of other profiles go on behind a barrier whenever its owner's room is not there and no older job is about
to start, which under a short host is most of the time it stands. While the history cannot be read, none does. `plan`
reads the runners of profiles a reload removed, and each runner's caps from its container, for every barrier, not only
for short passes; a job that waits for the host but lacks CPU too no longer lets the larger margin go, behind a barrier
or not. An owner can start up to a short run later than under a strict barrier, where 0097 promised
none. The replay fits a start by the room before it and counts a start as having no sample yet, as the daemon does, so
its figures for every policy move. A job behind a barrier that the barrier would let pass now says what it lacks, so `host_waited` and the host
insight count more, and less is said to wait behind a barrier.
