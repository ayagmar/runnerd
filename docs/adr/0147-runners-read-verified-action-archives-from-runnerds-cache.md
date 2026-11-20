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
  reads them, `staging/` for downloads, `quarantine/` for what failed a check, `lock`, and `index.json` with each
  archive's SHA-256, size, when it was fetched and last pinned and what its action files pin, and each served
  repository's last whole inventory; it is written to a temporary file and renamed.
- **One owner per directory.** The store opens by making the directory and its parents and taking an exclusive
  `flock` on `lock`, held until the task stops; a second runnerd on the same directory is refused, says so and shows
  no archives. The engine awaits the task when it stops. A store that cannot open or check is a `store` failure on
  the status, and the next refresh tries again.
- **What to fetch comes from the served repositories alone.** At most every 10 minutes, a task of its own asks GitHub
  for each served repository's default branch and that branch's head, two requests while it is unchanged; when the
  head moved, it reads the repository's git tree, its workflows and every `action.yml` or `action.yaml`, parsed as
  YAML: each `jobs.<job>.steps[].uses` of a workflow and `runs.steps[].uses` of an action, never a script's text. Only
  a `uses` pinned to a 40-digit commit counts; a local action, `docker://`, a tag, a branch and a reusable workflow do
  not. Each action's repository is asked of GitHub for its canonical name and visibility, and only a public one is
  fetched. The action file inside a fetched archive, `action.yml` before `action.yaml` as the runner loads it, pins
  further actions, followed from level 0 to level 9, as deep as the runner nests composite actions. Job logs never
  name what is fetched.
- **An archive goes only on a whole inventory.** A repository's inventory is whole when its tree was read in full
  (not cut short, at most 300 workflows and action files, each at most 1 MiB and each parsed) and every archive its
  pins reach is held, with its action file read, and every repository they name answered. Each whole inventory,
  nested pins included, is kept in the index and survives restarts; while a repository cannot be read, what its last
  whole inventory reached stays pinned. A refresh in which any served repository's inventory is not whole removes no
  archive; quarantined files still go after `keep`.
- **Archives are downloaded anonymously.** `GET https://codeload.github.com/{o}/{r}/tar.gz/{sha}` without the token
  or any credential, never retried with one, and no redirect followed: only a public repository's archive can come
  back. An archive whose repository GitHub now says is not public, or not there, is removed at the next refresh,
  whether or not it is still pinned; a runner that already copied it keeps its copy. Each download has a timeout,
  three tries, a 256 MiB cap, at most three run at once, and a refresh downloads at most 2 GiB, counting failed and
  refused downloads, and asks GitHub's API at most 1000 times, never once fewer than 1000 requests of the rate limit
  are left: what does not fit waits for the next refresh.
- **Nothing reaches `archives/` unverified.** The verifier reads the tar headers itself, in the narrow shape
  `git archive` writes, so what it checks is what GNU `tar -xzf` unpacks: the whole gzip stream with nothing after
  it; POSIX ustar headers with valid checksums, octal fields and permission-only modes; first, one PAX global header
  whose only record is the `comment` naming the commit asked for; extended headers of one `path` record each, for
  the entry after them; directories and regular files only, each under one top directory and inside a directory
  listed before it, with no absolute path, no `.` or `..` and no name twice; then two zero blocks and nothing but
  zeros. Links, devices, GNU long names and any other metadata are refused, and the runner downloads that action
  itself. All within 1 GiB expanded and 200 000 entries. A rejected download is removed from `staging/`, logged and
  reported. One that passes is synced, renamed into `archives/` without replacing a file there, its directory synced,
  and only then named in the index, all 0644 under 0755 directories.
- **What is kept is checked again.** At start and at each refresh every archive is hashed against the index; one that
  differs, anything at an archive's place that is not a regular file, and any file the index does not name moves to
  `quarantine/` and is fetched again. Links are never followed. `staging/` is emptied at start. An index entry whose
  repository or commit is not one GitHub could name is dropped.
- **Runners get the archives only once a check found them good.** The task publishes one ready view, the mount and
  the environment, after a check that read and settled every file, and withdraws it while the archives are off or a
  check failed. A runner's start reads that view once: it mounts `archives/` only, read-only, at
  `/opt/runnerd/actions`, with `ACTIONS_RUNNER_ACTION_ARCHIVE_CACHE=/opt/runnerd/actions`, or neither.
  `ACTIONS_RUNNER_SYMLINK_CACHED_ACTIONS` is never set: a job's copy is its own.
- **The owner sees it.** `Status` and `/v1/caches` carry `ActionArchives` (entries, size, last refresh, failures);
  core words the lines, and `runnerctl caches`, the TUI and the dashboard's Caches page show them. A failure is an
  attention item. A previous client reads the attention kind as `unknown` (accepted in the wire check).

## What it does not guarantee

- The runner never verifies an archive. runnerd's checks bound what it accepts; a valid archive written over a kept
  one by the daemon's own user between two checks is served until the next check finds it. A runner already running
  keeps the mount it started with when a later check fails.
- The PAX `comment` is what the archive declares, not a proof. What makes an archive the commit's is codeload's
  answer for that exact commit of that public repository.
- The runner looks up an immutable action package (`PackageDetails`) in the same directory, under the name and commit
  it resolved. runnerd keeps only source archives, from codeload; a package that resolves to the same name and commit
  reads the source archive of that commit, the same files the package was built from.
- An archive anyone could download, once kept and read, cannot be recalled from a runner that copied it.
- A private action, one pinned by tag, and one whose archive holds a link are downloaded by the runner as before.
  The runner still asks GitHub's API to resolve every action, so an API outage still fails `Set up job`.

## Consequences

- An archive the runner finds is never downloaded by a job. ADR 0148's `action-archive` cause tells the owner when one
  still failed.
- The index, the inventories, the archives and the quarantine survive restarts; the failures are in memory.
- The tests build archives as `git archive` does and corrupt them each way above, compare the verifier with real
  `git archive` output and `tar -tzf`, and check that a generated sequence of refreshes, restarts, failed downloads,
  pin changes, damage and killed downloads leaves under `archives/` only files the index names and verifies. A cache
  hit inside a real runner on the shipped image is proved in the end-to-end run, not in these tests.
