# 0165. One view over several machines is merged by core

Status: **Accepted** (2026-11-23). Adds `api::Fleet`, `api::MachineReading`, `api::FleetMachine`, `api::Reach`,
`api::FleetJob`, `api::Column::Machine` and `runnerctl fleet`.

## Context

With `--machine` (ADR 0164) a person reads one machine at a time. Two machines that serve the same repositories see
the same jobs: the one that ran a job has it `running` or `completed`, the other `queued` while it waited and then
`elsewhere`. Read side by side, every such job shows twice and the second copy says less.

## Decision

- **A pure merge in core.** `Fleet::merge(readings, limit)` takes what a client read from each machine (its status
  and a page of jobs, or why either could not be read) and returns each machine's facts and one page of jobs. It
  reads nothing; the client asks.
- **Each job once, from the record that knows most.** By job ID: `running` over `completed` over `lost` over
  `elsewhere` over `queued`; at the same state, a record with a runner over one without, then the first machine
  read. A machine's record goes from `queued` to `elsewhere`, never back, so a `queued` beside an `elsewhere` is a
  machine that has not polled since.
  A re-run is a new job ID, and `lost` only exists where the job ran, so no two machines hold two real records of one
  ID.
- **Paging by a shared `before`.** Every machine is asked with the same query. The merge keeps the newest `limit`
  jobs, and the next page starts before the oldest one shown when the page was cut or a machine said it has more.
  A daemon may end a page before `limit` (it caps a page's bytes too), so the merge first drops every job below the
  highest `next` a machine gave: below it, that machine has not been heard. No job is skipped then: each machine's
  unreturned jobs are below its `next`, the page ends at or above the highest one, and the next page asks every
  machine below the oldest job shown.
- **A machine that cannot be read is a fact, not an error.** `Reach::Unreachable` carries the client's reason;
  `Reach::Read` with `jobs_unread` covers a status read without its jobs. `FleetMachine::summary` and `tone` word and
  colour each line, and `Fleet::all_read` decides `runnerctl fleet`'s exit code.
- **Older daemons.** A machine's name is the client's `machines.toml` name, or for this machine
  `MachineReading::this_machine`: its daemon's name, else "this machine". A 0.1.5 status without `machine` needs no
  case; its id shows as absent. `MachineReading::unasked` words a client with no socket to ask.
- **`runnerctl fleet [--before] [--limit]`** reads this machine's daemon and every added machine at once, prints
  each machine's line and the page with a Machine column, or the `Fleet` as JSON. It exits 1 when a machine could not
  be read.
- **Fixtures.** `runnerd_fixtures::fleet` has three machines: this one, an older one that ran a job this one saw
  elsewhere, and one that is down. The CLI's parity test draws them.

## Consequences

- The fleet view costs one SSH forward per machine per call.
- Searches and sorts other than the default order are not merged; `fleet` shows the newest jobs only.
- The TUI's overview (ADR 0166) shows the same `Fleet`.
