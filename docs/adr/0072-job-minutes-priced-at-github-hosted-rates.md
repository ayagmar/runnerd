# 0072. What this machine's job minutes would have cost on GitHub-hosted runners

Status: **Accepted** (2026-10-26).

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
  runners are free in public repositories, for GitHub Pages and for Dependabot. For private repositories each
  account's plan includes minutes a month, shared by everything the account runs on GitHub's runners.
- [Dependabot on Actions runners](https://docs.github.com/en/code-security/dependabot/working-with-dependabot/about-dependabot-on-github-actions-runners):
  Dependabot's jobs run in a dynamic workflow, `dynamic/dependabot/dependabot-updates`; on standard or self-hosted
  runners they "do not count towards your included GitHub Actions minutes", and on larger runners they are billed. A
  repository sends them to self-hosted runners by the label `dependabot` or one of its own.
- The $0.002 a minute platform charge for self-hosted runners announced for 2026-03-01 is
  [postponed](https://github.blog/changelog/2025-12-16-coming-soon-simpler-pricing-and-a-better-experience-for-github-actions/):
  "We're postponing the announced billing change for self-hosted GitHub Actions to take time to re-evaluate our
  approach."

## Decision

runnerd shows one figure: what this machine's job minutes would have cost at GitHub's per-minute rates, before any
minutes the account's plan includes, with its basis beside it wherever it appears. It is an estimate priced from this
machine's run times, not a bill and not money saved.

### Which jobs, how many minutes

- **Priced**: jobs a runner of this machine took that GitHub reported completed, whatever the conclusion (success,
  failure, timed out, or cancelled after a runner took it), with a start and an end. GitHub bills a failed or
  cancelled job for the minutes it ran, and a re-run is a job of its own, so each attempt counts. Jobs of a light pool
  ran here and count.
- **Not priced, and counted**: `lost` jobs, whose end is when runnerd saw their runner gone, which after a crash or a
  day down says nothing of how long they ran; and completed jobs without a start or an end. Each window counts them,
  and core says so beside the figure, so a short figure never reads as complete.
- **Never counted**: `elsewhere` jobs, another machine's or cancelled in the queue (0069).
- **Minutes**: each job bills `max(1, ceil((finished_at - started_at) / 60 s))` from the history's milliseconds: a
  61-second job bills 2 minutes, a 0-second job 1. Rounded per job, then summed, as GitHub does.
- **Its window**: a job falls in a window by its end in UTC, or without one its start, else its queue time; the store
  selects by the same. This is runnerd's convention: GitHub's reports may place a job that crosses midnight
  elsewhere.

### The runner it is priced at

- A profile names one with `priced_as`, a Linux billing SKU of GitHub's pricing page: `actions_linux_slim`,
  `actions_linux`, `actions_linux_arm`, `linux_2_core_advanced`, `linux_4_core` … `linux_96_core`, and
  `linux_2_core_arm` … `linux_64_core_arm`. GitHub's own names, so the figure checks against the pricing page and the
  usage report. Windows, macOS and GPU SKUs are refused: runnerd runs Linux containers without GPUs. `config check`
  refuses an unknown key, naming the ones it takes.
- The default is the 2-core standard runner of the daemon's architecture: `actions_linux` on x86-64, the one
  `ubuntu-latest` gives a private repository; `actions_linux_arm` on aarch64.
- A job is priced at what its profile's `priced_as` says now, and a job whose profile is gone from the config at the
  default. `priced_as` is the owner's judgement of what runner the jobs need, so a change applies to the whole history
  and a reload changes the figure at once. Core says so in the figure's detail.
- The rates are one table in core (`api::Sku::rate`), in integer micro-dollars a minute (`actions_linux` = 6,000).
  What a minute costs here is one constant, `api::SELF_HOSTED_RATE` = 0, subtracted from each rate; if GitHub brings
  the platform charge back, it is that one change.
- The figure is a runtime-based estimate: on GitHub's runner a job runs at another speed, with other caches, longer or
  shorter. A `priced_as` near the profile's `cpus` is the more representative choice; core claims no direction.

### What GitHub would not have billed

- **Visibility.** `GithubApi::default_branch` becomes `repository`, the same `GET /repos/{owner}/{name}` request,
  returning the default branch and `visibility`. No new request kind and no new permission: the token that registers
  runners reads it. The engine asks for each configured repository's visibility on its first tick and once a day
  after, an hour after a failed ask, and stores the answer (schema 15: `repositories`, with `name`, `visibility` and
  `checked_at`). Rows are never removed, so a repository gone from the config keeps its answer, and a failed request
  keeps the last one.
- **Public repositories** run free on standard SKUs: those jobs are counted as free. On a larger SKU they are priced,
  as GitHub bills larger runners in public repositories too and `priced_as` says the jobs need one.
- **Dependabot and Pages** jobs are free on standard SKUs and priced on larger ones, as public jobs are. Core knows
  them by the workflow name GitHub gives them (`Dependabot Updates`, `pages build and deployment`, or a
  `dynamic/dependabot/` or `dynamic/pages/` path) or, for Dependabot, by a job asking for the `dependabot` label. A
  Dependabot job sent to a custom label under another workflow name is not recognised and is priced; this is
  accepted.
- **Unknown visibility.** A repository with no stored answer (deleted, or GitHub not asked or not answering since it
  was added) is not priced and its jobs are counted, so the figure never prices a job that may have been free.
- Jobs are judged by the repository's latest stored visibility: one made public reads as free over its whole history.
  Core says so in the figure's detail.

### Plan minutes

The figure is before any included minutes, and says so. The included minutes are the account's, shared with
everything it runs on GitHub's runners elsewhere, and runnerd sees only this machine's jobs; subtracting the plan's
number from them would claim savings the account may not have had. No setting asks for it.

### Periods

Two windows: the UTC calendar month so far, as GitHub resets usage at the start of each month; and the last 30 days,
so the first days of a month do not read near nothing. When the oldest job the history keeps was queued after a
window's start (`[history] jobs` under a month, or a younger database), the window starts at that job and core words
it "Since Oct 3". The default 90 days hold both.

### The wire and the words

- Money is `api::Usd`, integer micro-dollars, never a float. Core writes it to the cent, rounded down: `$7.40`,
  `$1,204.18`.
- `GET /v1/savings` is `api::Savings`: `until`, `month` and `last_30_days`, each an `api::SavedWindow` of `period`,
  `since`, `cut` (the history starts after the window's start), the priced `jobs`, `minutes` and `cost`, one
  `PricedRunner` per SKU priced at, and the counts of jobs not in the figure: `free`, `unknown`, `lost`, `untimed`.
  Computed on each request from the store and the running config; nothing is kept in memory.
- `api::Digest` gains `savings: Option<SavedWindow>`, the month so far, `#[serde(default)]` and skipped when `None`:
  0.0.18 ignores it, and this version reads a 0.0.18 digest as without one. `Savings` is a new document, which 0.0.18
  never asks for; a client facing a daemon without the route shows no figure. `Sku` is a closed enum: a SKU added
  later is a break for older clients, recorded then.
- Core words it. `SavedWindow::headline`, for compact places, carries the figure with its basis: "This month: 1,234
  job-minutes here, $7.40 at GitHub's 2-core Linux rate, before your plan's free minutes". With several SKUs, "at
  GitHub's rates for each profile's runner"; a window the history cut short starts "Since Oct 3"; jobs not in the
  figure add "(5 jobs not priced)". `SavedWindow::details` gives a line per SKU, one per kind of job not priced and
  why, and the basis: `priced_as` and visibility as they are now, from this machine's run times. One module,
  `api::savings`, computes and words all of it; the daemon only reads the rows.

### Where it shows

- `runnerctl insights` opens with both windows' headlines and details. Insights is the command for what the history
  says; there is no `runnerctl stats`. `--json` prints the `Insights` document as before; `GET /v1/savings` is the
  JSON. `runnerctl insights --digest` ends with the month's headline.
- The TUI's overview shows the month's headline under its health line, while the slots and lists keep their room;
  the dashboard's overview under its health figures.
- The daily digest's message ends with the month's headline.
- `crates/fixtures` gains a savings set every client's parity test draws.

## Consequences

- A repository made public, or a profile given another `priced_as`, changes the figure for the whole history.
- Lost jobs, often the longest-looking ones after a crash, never inflate the figure; a window with many of them says
  how many it left out.
- The figure can be above or below what GitHub would have charged: run times differ, and the plan's minutes may have
  covered some of it.
- Schema 15 adds the `repositories` table. A 0.0.18 daemon refuses the newer database, as with every schema change.
- The engine asks GitHub one more request per repository a day, beside its polls.

## Tests

Through `api::savings`, the store, the engine and the daemon's API, named in domain terms:

1. A 61-second job bills 2 minutes and a 0-second job 1; two attempts bill twice.
2. An `elsewhere` job, with or without a start, is not counted.
3. A job of a public repository is free at `actions_linux` and priced at `linux_8_core`; Dependabot and Pages jobs
   likewise; a repository with no stored visibility is not priced and is counted; a profile gone from the config is
   priced at the default.
4. A lost job and a job without an end are not priced and are counted; the store reads them by their end, start or
   queue time, and the daemon's route says how many it left out.
5. The month starts at 00:00 UTC on the 1st: a job that ended a second before is in the last 30 days, not the month.
   A history whose oldest job is newer than the window's start says the window starts there.
6. The store reads a repository's latest visibility back after a reopen; the engine asks once, not on every poll.
7. `config check` refuses an unknown `priced_as`, a Windows and a GPU SKU, naming the keys it takes.
8. `Usd` writes 7,409,999 micro-dollars as `$7.40`.
9. Each client's parity test over the fixtures' savings set shows core's headline.

Wire compat: the `Digest` sample has `savings`, which 0.0.18 reads; `Savings` is sampled under `documents!`'s `new:`,
read by this version only until 0.0.19 becomes the previous release.
