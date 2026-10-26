# 0072. What runnerd saved against GitHub-hosted runners

Status: **Proposed** (2026-10-26).

## Context

The owner runs the CI of private repositories on runnerd instead of GitHub-hosted runners and wants to see what
that saved: "this month, N job minutes here would have cost $X on GitHub's runners". The history has every job a
runner of this machine took, with its start, end, repository and profile. It does not know a repository's
visibility, nothing prices a minute, and no client shows money.

GitHub's terms, read 2026-10-26:

- [Actions runner pricing](https://docs.github.com/en/billing/reference/actions-runner-pricing) prices a minute by
  billing SKU, such as `actions_linux` ($0.006, Linux 2-core x64) or `linux_8_core` ($0.022). "GitHub rounds the
  minutes and partial minutes each job uses up to the nearest whole minute." "Included minutes cannot be used for
  larger runners." "The larger runners are not free for public repositories."
- [GitHub Actions billing](https://docs.github.com/en/billing/concepts/product-billing/github-actions): standard
  runners are free in public repositories. For private ones each account's plan includes minutes a month (Free and
  Free for organizations 2,000, Pro and Team 3,000, Enterprise Cloud 50,000), and "at the start of each month, the
  minutes used by the account are reset to zero". Self-hosted runners are free.
- The $0.002 a minute platform charge for self-hosted runners announced for 2026-03-01 is
  [postponed](https://github.blog/changelog/2025-12-16-coming-soon-simpler-pricing-and-a-better-experience-for-github-actions/):
  "We're postponing the announced billing change for self-hosted GitHub Actions to take time to re-evaluate our
  approach."

## Decision

### What a job would have cost

- **Which jobs.** The jobs a runner of this machine took: state `completed` (succeeded, failed, timed out, or
  cancelled after a runner took it) or `lost`, with both a start and an end. GitHub bills a failed or cancelled job
  for the minutes it ran, and a re-run is a job of its own, so each attempt counts. Jobs of a light pool ran here
  and count. `elsewhere` jobs, another machine's or cancelled in the queue (0069), never count. A job with no start
  or no end counts no minutes and is counted apart as unmeasured. A lost job runs to the time runnerd closed it,
  which can be later than its real end; lost jobs are rare and this is accepted.
- **Minutes.** Each job bills `max(1, ceil((finished_at - started_at) / 60 s))`: a 61-second job bills 2 minutes, a
  0-second job 1. Rounded per job, then summed, as GitHub does.
- **The runner it is priced at.** A profile names one with `priced_as`, a Linux billing SKU of GitHub's pricing
  page: `actions_linux_slim`, `actions_linux`, `actions_linux_arm`, `linux_2_core_advanced`, `linux_4_core` …
  `linux_96_core`, and `linux_2_core_arm` … `linux_64_core_arm`. GitHub's own names, so the figure checks against
  the pricing page and the usage report without a vocabulary of runnerd's. Windows, macOS and GPU SKUs are refused:
  runnerd runs Linux containers without GPUs. An unknown or refused key fails `config check`, naming the keys it
  takes. The default is the 2-core standard runner of the daemon's architecture: `actions_linux` on x86-64, the one
  `ubuntu-latest` gives a private repository; `actions_linux_arm` on aarch64.
- **At today's config.** A job is priced at what its profile's `priced_as` says now, not when it ran, and a job
  whose profile is gone from the config at the default. `priced_as` is the owner's judgement of what runner the jobs
  need, not a fact of the day they ran, so a change applies to the whole history; a reload changes the figure at
  once. Recording it per runner would need a column for a value only the owner changes.
- **The rates** are one table in core, in integer micro-dollars a minute (`actions_linux` = 6,000). What a minute
  costs here is one constant, `SELF_HOSTED_RATE` = 0; a job saved its hosted cost minus that. If GitHub brings the
  platform charge back, it is that one change, and the ADR that makes it decides whether those minutes also use up
  the included ones, as the announcement said they would.
- **The figure is a lower bound** when a profile has more CPUs than its priced runner has cores: on 2 cores the job
  would have run longer. Core words the runner it priced at, "at GitHub's 2-core Linux rate", and says that a
  `priced_as` closer to the profile's `cpus` is the closer estimate.

### Public and private repositories

- `GithubApi::default_branch` becomes `repository`, the same `GET /repos/{owner}/{name}` request, returning the
  default branch and `visibility`. No new request, no new permission: the token that registers runners reads it.
  The build cache reads the default branch from it as before.
- `visibility` `public` saved nothing at any SKU: GitHub gives a public repository a free standard runner, and
  pricing its jobs at a larger runner it need not use would claim money no one would have paid. `private` and
  `internal` are billed.
- The daemon asks for each configured repository's visibility at start, when a reload adds a repository, and once
  a day, and stores the answer (schema 15: a `repositories` table of `name`, `visibility` and `checked_at`). A row
  stays while the history has jobs of the repository, so one gone from the config keeps its visibility. A failed
  request leaves the last answer.
- Jobs are priced at the repository's latest stored visibility. A repository made public mid-month reads as having
  saved nothing that month, before the change too; this is accepted, as GitHub would have billed those runs.
- A repository with no stored answer (deleted, or GitHub never answered since it was added) is assumed to have
  saved nothing, and its jobs are counted apart, so the figure never claims money it cannot back. No config key
  overrides it: every repository runnerd serves is one its token reads. If the unknown count shows up in practice, a
  `[[repositories]] visibility` key is the follow-up.

### Included minutes and the period

- `[savings] included_minutes`, default 0, is the plan's included minutes a month. Each repository owner on GitHub
  is an account with its own quota, so the number applies to each owner separately; one number serves the common
  case of one plan. Within a month, each owner's jobs on standard SKUs (`actions_linux_slim`, `actions_linux`,
  `actions_linux_arm`) use them up in the order they finished. Larger SKUs never use them.
- Each figure has two amounts: what the jobs would have cost, and what of that is above the included minutes. With
  `included_minutes` 0 they are the same and core words one, "before any minutes your plan includes"; otherwise
  both, naming the included minutes.
- **Two windows**: the UTC calendar month so far, GitHub's reset; and the last 30 days, so the first days of a month
  do not read near nothing. Included minutes apply to the month only: the last 30 days span two months' quotas, so
  that window shows the cost without them. A job falls in a window by when it finished.
- **The history's limits.** When the oldest job the history keeps finished after a window's start (`[history]
  jobs` under a month, or a younger database), the window starts at that job and core says so: "since 3 Oct, the
  oldest job the history keeps". The default 90 days hold both windows.

### The wire

- Money is `api::Usd`, integer micro-dollars, never a float. Core formats it to the cent, rounded down, such as
  `$7.40` or `$1,204.18`, so a figure never says more than the jobs cost.
- `GET /v1/savings`: `{ until, month, last_30_days }`, each a `SavedWindow` of `since`, `jobs`, `minutes`, `cost`,
  `above_included` and `included_minutes`, with the jobs left out counted by why (`public`, `unknown`,
  `unmeasured`) and a line per SKU priced at (`sku`, `jobs`, `minutes`, `cost`). Computed on each request from the
  store and the running config, nothing kept in memory, so a restart or reload mid-month gives what it would have.
- `api::Digest` gains `savings: Option<SavedWindow>`, the month so far, `#[serde(default)]` and skipped when
  `None`: a 0.0.18 reader ignores it, a new reader reads a 0.0.18 digest as without one.
- Core words every fact: a window's headline, "This month: 1,234 job minutes here would have cost $7.40 at GitHub's
  2-core Linux rate", its line above the included minutes, the ones left out ("12 jobs of public repositories saved
  nothing"), the history's limit, and the lower bound. One module, `api::savings`, computes and words them; the
  daemon only reads the rows.

### Where it shows

- `runnerctl insights` prints both windows above the insights. Insights is the command for what the history says;
  `runnerctl stats` does not exist, and `/v1/stats` is the history page's buckets. `--json` keeps printing the
  `Insights` document unchanged; `GET /v1/savings` is the JSON.
- The TUI's overview health line and the web dashboard's overview show the month's headline, reading
  `/v1/savings` on the interval they read the history. A client facing a daemon without the endpoint shows none.
- The daily digest adds the month's headline as one line of its message.
- `crates/fixtures` gains a savings set every client's parity test draws: a 61-second job, a 0-second job, an
  `elsewhere` job, a cancelled-in-queue job, a job of a public and of an unknown repository, a job without an end,
  a profile at `linux_8_core`, a profile gone from the config, a month crossing the included minutes, and a history
  shorter than the month.

## Consequences

- The figure is an estimate of a counterfactual: GitHub's runners would have run the jobs at another speed, with
  other caches. Core words it as a price of this machine's minutes, not as a bill.
- A config whose profiles all keep the default prices every job at the 2-core rate, what `ubuntu-latest` costs a
  private repository.
- A repository made public reads as having saved nothing over the whole history, not from the change.
- A repository owner with a quota other than `included_minutes` (an organisation on Enterprise Cloud beside a Pro
  account) reads the wrong amount above the included minutes until the setting is per owner; the cost before them
  is right.
- Schema 15 adds the `repositories` table. A 0.0.18 daemon refuses the newer database, as with every schema
  change.
- ARCHITECTURE.md names `api::savings` and the visibility the store keeps.

## Tests

Through `api::savings`, the store and the daemon's API, named in domain terms:

1. A 61-second job bills 2 minutes and a 0-second job 1. Catches rounding the sum instead of each job, or not at
   all.
2. An `elsewhere` job, with or without a start, saves nothing, and neither does a cancelled-in-queue one. Catches
   counting jobs this machine did not run.
3. A job of a public repository saves nothing, also at `linux_8_core`; one of a repository with no stored
   visibility saves nothing and is counted as unknown; a repository gone from the config keeps its stored
   visibility. Catches claiming free minutes, and losing a repository's answer on reload.
4. A month whose standard-SKU minutes cross `included_minutes` puts the jobs after the crossing above them, per
   owner, and never a `linux_8_core` job's minutes under them. Catches included minutes taken by larger runners or
   shared across owners.
5. A job with no end or no start counts no minutes and is counted unmeasured. Zero jobs give zero minutes, $0.00
   and no division by zero.
6. A history whose oldest job finished after the month's start says the window starts there.
7. The figure read before and after a store reopen, and before and after a reload that changes `priced_as`, is the
   one computed from the rows with the config then running. Catches state kept in memory.
8. `config check` refuses an unknown `priced_as` and a Windows SKU, naming the keys it takes.
9. `Usd` formats micro-dollars to the cent rounded down: 7,409,999 is `$7.40`.
10. Each client's parity test over the fixtures' savings set shows core's lines.

Wire compat: samples for `Savings` and for a `Digest` with `savings`; a 0.0.18 reader reads the new `Digest`, and
the new reader reads 0.0.18's.
