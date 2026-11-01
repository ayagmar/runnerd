# 0082. GitHub Actions' cache served from the host, on by default

Status: **Accepted** (2026-10-30), amended by [0085](0085-actions-cache-saves-wait-for-the-decision-not-the-job.md). Amends [0052](0052-shared-build-cache.md).

## Context

`actions/cache`, the `cache:` input of `setup-node`, `setup-java`, `setup-python` and `setup-gradle`, and
BuildKit's `type=gha` cache all use GitHub's cache service: each restore downloads an archive from Azure and each
save uploads one, over the host's internet link. The host's own disk is beside the runner.

How a job finds the service (actions/runner at 110142c, actions/toolkit `@actions/cache` 6.3.0):

- The runner sets `ACTIONS_RESULTS_URL` (and `ACTIONS_CACHE_SERVICE_V2`, `ACTIONS_RUNTIME_TOKEN`) for each node
  action from the job's system connection, in `NodeScriptActionHandler`, after the step's own environment, so
  neither the container's environment nor `$GITHUB_ENV` can change it. No runner setting names another URL.
- `@actions/cache` talks to `$ACTIONS_RESULTS_URL/twirp/github.actions.results.api.v1.CacheService/` in JSON:
  `GetCacheEntryDownloadURL`, `CreateCacheEntry` and `FinalizeCacheEntryUpload`. The archive goes to the signed URL
  `CreateCacheEntry` returns, with Azure's block blob protocol (one `PUT` up to 128 MiB, else 64 MiB blocks and a
  block list), and comes back with a `GET` of the signed URL `GetCacheEntryDownloadURL` returns.
- BuildKit (`tonistiigi/go-actions-cache`) gets the URL and token from buildx, which reads them from the
  environment of the action that runs it. It uploads with Azure's Go SDK, which always sends blocks and needs an
  `x-ms-request-id` header back, and reads with ranged `GET`s. With `setup-buildx-action`'s default driver, BuildKit
  runs in a container of the slot's Docker Engine: the runner container's loopback is not its own.
- `@actions/artifact` (`upload-artifact` and `download-artifact` v4) uses the same `ACTIONS_RESULTS_URL` for its
  own Twirp service.

What others do: falcondev-oss's cache server patches the runner's binary (`ACTIONS_RESULTS_URL` renamed to
`ACTIONS_RESULTS_ORL`) and forwards every request it does not serve to GitHub. RunsOn's docs say its agent writes
`ACTIONS_RESULTS_URL` to `$GITHUB_ENV` and forwards artifact requests to GitHub, and scope a job's entries to its
ref with a fallback to the default branch's. Blacksmith's say entries are scoped to their branch or tag, and not how
runners reach its cache.

Ways to point a job at runnerd, other than patching the runner: a TLS-intercepting proxy for
`results-receiver.actions.githubusercontent.com` would carry every job's logs and artifacts and need a CA every
container trusts; an `https_proxy` would carry all of a job's traffic. `NODE_OPTIONS` in the container's
environment reaches every node action, but also every container a job starts that is given the job's
`NODE_OPTIONS` (`docker run -e NODE_OPTIONS`, a compose file's `environment: [NODE_OPTIONS]`), where a `--require`
of a file that container lacks stops node; and a workflow's own `NODE_OPTIONS` replaces it. The runner runs a node
action with its own node, `externals/<node20|node24>[_alpine]/bin/node`, by that path (`NodeScriptActionHandler`),
and the runner image is runnerd's own.

## Decision

- **On by default**, for the repositories the build cache serves: `[caches.build] actions_cache = false` turns it
  off, and so do `build.enabled = false` and a repository's `build_cache = false`. It rides the build cache's socket
  and store. The costs, which the owner accepts for restores and saves that stay on the host's disk:
  - GitHub's cache is neither read nor written for those repositories, so the first runs after an install or an
    upgrade start cold.
  - Its entries share `build.max_size` with Gradle's, sccache's and Bazel's, so a repository with large
    `type=gha` layers trims those sooner.
  - Every node action the runner starts loads the preload, and the image adds an `iptables` rule and a forwarder.
  - While runnerd restarts, a job's artifact requests, which go through it, fail; `@actions/artifact` retries for
    about 25 s.
  - A pull request that may not write keeps nothing across pushes (Consequences).
  `runnerd doctor` and the Caches page say whether it is on.
- **How a job reaches it.** When the runner's socket directory has the `actions` marker the daemon writes, the
  image's `build-cache.sh`, as root before the runner starts, adds an `iptables` rule that drops port 41772 arriving
  on `eth0`, then forwards that port on every address of the container to the runner's socket, and exports
  `RUNNERD_ACTIONS_CACHE_PORT`. The image moves each of the runner's bundled glibc node binaries to `node.real` beside
  a `bash` wrapper (`image/node.sh`; `sh` would drop the action's inputs, `INPUT_FETCH-DEPTH` and the like, as dash
  passes on no variable with a hyphen in its name) that, while that variable is set and the preload is there, runs it with
  `--require=/usr/local/lib/runnerd-actions-cache.cjs`, else as it is. The preload, loaded by each node action the
  runner starts, replaces `ACTIONS_RESULTS_URL` with `http://<the container's address>:41772/` when it is GitHub's
  `https://results-receiver.actions.githubusercontent.com/`, before the action's own code runs. The job's
  environment carries no `NODE_OPTIONS`, so its own containers get nothing, and a workflow's own `NODE_OPTIONS`
  applies besides the `--require`. A `container:` job's container mounts the runner's externals but has neither the
  variable nor the preload, so node runs there as it is. The `_alpine` builds, which only an Alpine `container:` job runs and which
  has no `bash`, stay unwrapped; a glibc job container without `bash` cannot start the wrapped node. A runner that
  updates itself replaces its externals and so the wrapper: its jobs use GitHub's cache. The address is the
  container's own so that BuildKit in a container of the slot's engine, whose traffic arrives on that engine's
  bridge, reaches it too. Other runners and the host arrive on `eth0`, which drops
  them; jobs have no root to remove the rule (Dockerfile). No forwarder, no rule or no marker: the job uses GitHub's
  service as before.
- **Not served**: `container:` jobs and `docker://` actions (their environment is the runner's, not the
  container's), node a job starts itself rather than through the runner, and the cache service v1.
- **Everything else goes to GitHub.** A request to another Twirp service, such as an artifact's, is sent unchanged
  with its headers to `https://results-receiver.actions.githubusercontent.com` and its answer passed back, at most 1
  MiB each way. Its headers and body carry the job's token and are never logged. The artifacts themselves go to the
  URLs GitHub signs, never through runnerd.
- **Scopes, after GitHub's.** An entry is a key and version in a ref's scope of one repository. A job ADR 0052
  and 0053 let write saves into its own scope, after the same checks (commit on the branch, pull request opened by
  a person): a branch run's is its branch, `refs/heads/<branch>`; a `pull_request` run's is its pull request,
  `refs/pull/<number>/merge`, never its head branch, since a pull request may come from `main` into a branch its
  author can push to, and its run then runs that branch's workflow. A run GitHub ties to several pull requests has
  no scope of its own and saves nothing. A writer reads its own scope's entries, then its pull request's base
  branch's, then the default branch's. Every other job, forks' and bots' pull requests included, reads only the
  default branch's and saves nothing. No pull request writes what a `main` job restores.
- **Undecided jobs.** Restores come before GitHub has said what the job is, typically in its first poll interval:
  a lookup then reads the default branch the cache learnt from an earlier decision, and finds nothing on the first
  job of a repository after a start. A save waits up to 60 s for the decision, then is refused.
- **Refused saves answer success.** BuildKit fails a build whose cache reservation fails, so a save a job may not
  make is answered as `actions/cache` and BuildKit expect, its bytes read and dropped, and counted as refused, as
  0052 does for Gradle. So is a key its scope has, or that another job is saving into it, where GitHub answers
  `ok: false`, which BuildKit's client takes as a failure unless its message says "already exists".
- **Turned off under a running job.** A daemon restart that turns it off adopts running jobs whose containers still
  have the forwarder and the port set; runners started after it get no marker, and so neither, as before this.
  Their lookups miss, their saves are answered as kept and dropped, and every other request still goes to GitHub,
  so their artifacts and Docker builds keep working.
- **Lookups as GitHub matches.** In each scope in order: the key, then each restore key, exactly, then as a prefix,
  the newest created first.
- **Store.** Entries are files under `entries/<owner>/<name>/actions/<scope>/~/<version>/<key>/~<created>`, the
  scope's ref and the key hex-encoded in segments of 128 characters, so nothing a job names can reach outside its
  repository's tree and no two scopes or keys share a path. They count against `[caches.build] max_size` with the
  build cache's entries, go least recently used first in the same trim, and are bounded like its uploads: an entry
  at most a quarter of `max_size` (2.5G by default; GitHub allows 10G), whole or the sum of its blocks. A block upload's
  blocks are files until its block list joins them. The cache keeps an index of each scope's entries beside its
  own, so a lookup reads only its scopes'. A download URL names its entry by its creation time, which no other
  entry has, so one from before a restart never reads another entry. No history change: a runner's lookups, hits,
  bytes and refused saves count in its build cache counts.

## Consequences

`npm ci` after a `setup-node` restore, Gradle's dependency cache and `type=gha` layers come from the host's disk.
A default-branch run that restored from GitHub before this was on finds nothing the first time, saves locally, and
hits from then on; GitHub's entries are neither read nor written while it is on. Turning it off brings back GitHub's
for the jobs that start after it.

A pull request that may not write saves nothing, where GitHub would keep its own scope; its next push starts from
the default branch's entries. One that may write restores its own entries, then its base branch's, as on GitHub.
Its `actions/cache` step still says it saved.

The preload runs in every node action the runner starts, and in nothing a job starts itself: it only compares and
sets one variable, which the action's child processes, such as buildx, inherit. A job's nested containers reach the
cache at the container's address; ADR 0052's loopback-only forwarder for Gradle, sccache and Bazel is unchanged.

An image older than this release has no wrapper, no preload and no forwarder on 41772: its jobs use GitHub's
cache.
