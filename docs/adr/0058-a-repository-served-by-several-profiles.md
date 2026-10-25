# 0058. A repository served by several profiles

Status: **Accepted** (2026-10-25). Amends [0033](0033-measured-admission.md), [0044](0044-queued-jobs-say-why-they-wait.md)
and [0050](0050-warm-runners.md).

## Context

Each `[[repositories]]` entry served a repository in one profile. GitHub may give a runner any queued job its labels
match, so under measured admission a runner reserves the largest prediction among its repository's queued jobs.
Light jobs therefore reserved what the heaviest one needs. On the owner's host, a repository whose lint and
validation jobs peak at 180-770M waited 2-18 minutes (p95) behind 8.5G reservations while its two 7G builds were
queued; another's 600M design jobs waited 6-12 minutes behind its Android gate.

## Decision

- A repository may have several `[[repositories]]` entries with the same `name`. Each is a pool: its runners start
  in its profile and register with its labels, and it has its own `warm` and `build_cache`. A queued job runs in the
  pool whose labels match it. One entry per repository means what it always did.
- Repeated entries rather than a list inside one entry: every key of an entry already describes a pool, an existing
  config needs no change, and TOML has no tidy way to nest a list of tables under an array of tables.
- `check-config` refuses two pools of one repository that share a label, compared without case: a job asking only
  for shared labels would match both and nothing says which one it means. No other preference rule is offered, so
  routing never depends on the order of entries. It also refuses two pools of a repository in one profile (a pool is
  named by its repository and profile) and pools that set `build_cache_writers` differently (whose runs may write
  is the repository's), and, as before, a pool without labels.
- The scheduler plans per pool: a runner's reservation, under measured admission, is the largest prediction among
  the queued jobs its own labels match; a CPU hold, the CPU wait it starts, the warm runners it wants and the warm
  runners a job may stop are its own. A job may stop a warm runner of another pool of its repository.
- Warm runners learn per pool: the jobs a workflow's runs queue are counted per pool, by the labels each job asked
  for, so a light pool does not keep runners for heavy jobs to come. Whether a repository is active stays per
  repository.
- A queued job of a served repository that no pool's labels match waits with `Waiting::NoPool`, an act-on-it reason
  (`No profile serves the label halyard-lite`). Jobs whose every label names one of GitHub's images (`ubuntu-*`,
  `windows-*`, `macos-*`) are GitHub's, not runnerd's, and are not listed.
- Holds after failed runners are per pool: a light pool whose image is broken does not stop the heavy one. Pause
  and resume stay per repository.
- The status lists each repository once with its `pools` (profile, labels, queued jobs), and each queued job with
  its pool's `profile`. `profile` and `labels` stay on the repository, filled from its first pool, so a client
  before pools still shows one; a client reads an older daemon's repository as a single pool
  (`RepositoryStatus::listed_pools`). Clients show where a job runs and a repository's profiles from `QueuedJob::place` and
  `RepositoryStatus::profiles`, so the dashboard, `runnerctl` and the TUI say the same.

## Consequences

- A repository can opt in without risk to its unlabelled jobs: the light pool registers only with its own label
  (leaving out `self-hosted`), so a job still asking for `[self-hosted]` matches only the heavy pool.
- A job whose `runs-on` no pool serves, such as a typo in the new label, says so instead of waiting silently.
- A client older than this daemon shows only a repository's first pool.
