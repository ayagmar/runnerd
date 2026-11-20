# 0147. Runners read verified action archives from runnerd's cache

Status: **Accepted** (2026-11-20). Adds `[caches.actions]`, the `archives` host module, `ActionArchives` on `Status`
and `Caches`, and the attention kind `action-archives`.

## Context

Before a job's first step, the runner downloads every action the job uses: it asks GitHub's API where each one's
archive is, then downloads it from codeload.github.com, three tries each. On 2026-11-17, 21 fretboard jobs failed in
`Set up job` within 90 minutes because those downloads failed; every runner downloaded the same few archives again for
every job. The runner (2.338.1, `ActionManager.cs`) can read archives from a directory instead: with
`ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE` set, it looks for `<dir>/<owner>_<repo>/<sha>.tar.gz`, with the owner and
repository as GitHub resolved them, copies the file and runs `tar -xzf` on it. It checks nothing: whatever is at that
path is the action the job runs.

## Decision

- **runnerd keeps the archives, on by default.** `[caches.actions] enabled` (default `true`) turns them off,
  `directory` (default `$XDG_CACHE_HOME/runnerd/actions`, read at start) moves them, and `keep` (default `14d`) is
  how long one stays once nothing pins it. Under the directory: `archives/<owner>_<repo>/<sha>.tar.gz` as the runner
  reads them, `staging/` for downloads, `quarantine/` for what failed a check, and `index.json` with each archive's
  SHA-256, size, when it was fetched and when it was last pinned, written to a temporary file and renamed.
- **What to fetch comes from the served repositories alone.** At most every 10 minutes, a task of its own reads each
  served repository's default-branch head; when it moved, its git tree, its workflows and every `action.yml` or
  `action.yaml`. Only `uses:` pinned to a 40-digit commit counts; a local action, `docker://`, a tag, a branch and a
  reusable workflow do not. Each action's repository is asked of GitHub for its canonical name and visibility, and
  only a public one is fetched. The `action.yml` files inside a fetched archive pin further actions, followed 9 levels
  deep. A tree GitHub cut short, or a failed read, keeps the pins last read. Job logs never name what is fetched.
- **The download goes through `github::Github`.** `GET /repos/{o}/{r}/tarball/{sha}` with the token, to the API only;
  GitHub's redirect is followed without the token, and only to https codeload.github.com or the API's own origin. The
  signed address is never logged nor put in an error. Each download has a timeout, three tries, a 256 MiB cap, at
  most three run at once, and a refresh downloads at most 2 GiB.
- **Nothing reaches `archives/` unverified.** The whole gzip stream, with nothing after it, and the whole tar must
  read, within 1 GiB expanded and 200 000 entries; every entry sits under one top directory, with no absolute path, no
  `..`, no hard link or device, and no symlink leading out; the first entry is the PAX global header whose `comment`
  is the commit asked for, as `git archive` writes it. A rejected download is removed from `staging/`, logged and
  reported. One that passes is synced, renamed into `archives/` without replacing a file there, its directory synced,
  and only then named in the index, all 0644 under 0755 directories.
- **What is kept is checked again.** At start and at each refresh every archive is hashed against the index: one that
  differs, or a file the index does not name, moves to `quarantine/` and is fetched again; `staging/` is emptied at
  start. An archive goes `keep` after the last refresh that found it pinned; a pinned one never goes.
- **Runners mount `archives/` only, read-only, at `/opt/runnerd/actions`**, with
  `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE=/opt/runnerd/actions`, while the archives are on and the directory exists.
  `ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS` is never set: a job's copy is its own.
- **The owner sees it.** `Status` and `/v1/caches` carry `ActionArchives` (entries, size, last refresh, failures);
  core words the lines, and `runnerctl caches`, the TUI and the dashboard's Caches page show them. A failure is an
  attention item. A previous client reads the attention kind as `unknown` (accepted in the wire check).

## What it does not guarantee

- The runner never verifies an archive. runnerd's checks bound what it accepts; a valid archive written over a kept
  one by the daemon's own user between two checks is served until the next check finds it.
- The PAX `comment` is what the archive declares, not a proof. What makes an archive the commit's is GitHub's
  authenticated answer for that exact commit of that repository.
- A private action, or one pinned by tag, is downloaded by the runner as before. The runner still asks GitHub's API to
  resolve every action, so an API outage still fails `Set up job`.

## Consequences

- An archive the runner finds is never downloaded by a job. ADR 0148's `action-archive` cause tells the owner when one
  still failed.
- The index, the archives and the quarantine survive restarts; the pins and failures are in memory and read again.
- The tests build archives as `git archive` does and corrupt them each way above; a generated sequence of refreshes,
  restarts, failed downloads, pin changes, damage and killed downloads checks that every file under `archives/` is one
  the index names and verifies.
