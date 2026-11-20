# 0154. A job keeps how long each cause held it in the queue

Status: **Accepted** (2026-11-20).

## Context

The history kept two waits per job: `host_waited`, the time a plan held it for the host's memory (ADR 0103), and
`slot_waited`, the time one more slot would have saved it (ADR 0090). Both exist for sizing, and neither says why
the rest of a wait passed. A day's audit of one repository found its jvm jobs queued 1074 s on average, 558 s of it
explained by neither; splitting that into the host's memory, the budget and a barrier took a reconstruction from the
budget's events. Yet each plan already says why it holds each queued job (`api::Waiting`), and the status shows it.

## Decision

- `api::WaitCause` groups the `Waiting` reasons into what a wait is counted under: a barrier (`BehindMemory`,
  `BehindCpu`), an older job's start, no free slot, the memory budget, the host's memory, the CPU budget, a runner
  starting, a snapshot, image or cleanup, a pause, a backoff, no pool, and another reason. Each is stored by a word,
  and a word a later runnerd adds reads as another reason, so a history read across versions never fails on it.
- The waits are accumulated from the plans the engine already commits, with no sampling of its own. Where a tick or a
  pass commits its plan's slot waits (ADR 0110), it commits the plan's causes: each job the plan holds is held for its
  cause from then on, and every cause open since the last commit adds the time between to its job. A job the plan
  starts a runner for keeps the cause it had until that runner is in its slot, and from then on waits for its runner
  starting until a runner takes it. A job the plan no longer has, cancelled or taken by a runner elsewhere, waits no
  more. The open causes live with the waits sizing reads, in one map by job (`OpenWaits`), each job's two clocks on
  their own.
- The history keeps them in `jobs.wait_causes`, schema 26: a JSON object of milliseconds by cause's word. A row from
  before schema 26 is NULL and takes no cause, so it reads as not counted, never as a wait of 0; a new row starts at
  `{}`. Asking the history why its jobs queued is one query over `json_each`.
- Insights split each profile's queue time over the window by cause (`api::ProfileQueue`), and name the two parts no
  cause covers: the time before runnerd first listed the job (GitHub's listing lag and runnerd's listing pace), and
  the time no plan held it once listed. The wait is the one sizing and insights already count, with the time no
  runnerd ran left out; jobs from before schema 26 are counted apart. Core words the section; `runnerctl insights`,
  the TUI and the dashboard lay it out.

## Consequences

- Nothing of an open cause survives a restart: what passed between the last commit before it and the first plan
  after it is in "No plan covered", next to the time `unseen` already leaves out. A reload keeps the open causes; a
  job of a repository the reload removed ends at the next commit.
- A cause counts up to the commit or start that ends it, so a job's causes can pass its start by up to a tick when a
  runner took it and runnerd had not seen that yet; the split scales such a job's causes down to its wait, keeping
  their shares.
- The previous release refuses a schema-26 history as newer, as for every schema change; the API's new `queue` field
  is skipped when empty and ignored by older clients.
- `host_waited` and `slot_waited` stay as sizing reads them. The cause "Host memory" counts the same plans, but on
  its own clock, so the two can differ by a pass's unmade starts.
