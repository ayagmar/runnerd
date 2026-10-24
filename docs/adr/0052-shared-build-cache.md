# 0052. A build cache every runner shares, written only by the default branch

Status: **Accepted** (2026-10-24). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md).

## Context

On the owner's host, a Gradle job on `main` takes about 40 s when Gradle's build cache hits ("62 actionable tasks: 23
executed, 35 from cache") and 2 to 3.5 minutes when it misses ("49 executed, 10 from cache"). Which one a job gets is
luck: Gradle's local build cache is in the slot's `~/.cache`, so what a pull request built in slot 2 is a miss for
`main` in slot 3, and a reseed resets it. Hosted runners share no build cache without a paid remote one.

A shared cache has a risk the slot's own does not: a job that may write an entry another job then trusts can put
its own code into that job's build. A fork's pull request must not be able to write what a `main` build loads.

## Decision

- **In the daemon.** runnerd serves the build cache itself (`runnerd_core::buildcache`), not an off-the-shelf
  server in a container: whether a write is kept depends on the job behind it, which only the daemon knows. It
  needs no image to pin and no container lifecycle; hyper, already in the build, serves it. Entries are files in
  `[caches.build] directory` (default `$XDG_CACHE_HOME/runnerd/build-cache`), under `<owner>/<name>/<tool>/<key>`,
  not a Docker volume: the daemon's user writes them.
- **One socket per runner.** Before a runner's container starts, the engine opens a Unix socket for it in a
  directory beside the API socket, and the container mounts that directory read-only at `/run/runnerd-build-cache`.
  Which socket a request comes in on is who sent it, so a job cannot speak as another runner: no token to leak, no
  source address to forge (a job with Docker can change its container's address). The runner image's
  `build-cache.sh` forwards the socket to `127.0.0.1:41771` with socat, started as root before the runner, so no job
  takes the port. No host firewall stands in the way, and nothing outside the runners reaches the cache. The
  socket is mode 0666 in a 0755 directory: under sysbox the container's root is not the daemon's user. The parent
  directory is 0700, so no other user of the host reaches it.
- **Who writes.** A job's uploads are kept only when GitHub says, through `GET /actions/jobs/{id}`, that the job
  runs on that runner, and through `GET /actions/runs/{id}` that its run is a `push`, `schedule` or
  `workflow_dispatch` of the repository itself on its default branch (`GET /repos/{repo}`, asked once per
  repository) or on one of `[caches.build] branches`. The job's own runner log, which its capture reads, names the
  job, but a job can write that log, so it decides nothing. Every other job reads. Uploads that come before GitHub
  has answered, typically the first poll interval of a job, are held and become entries or are dropped once it has.
  A pull request whose head branch is called `main` is still a pull request.
- **Refused uploads answer 200.** Gradle turns its remote cache off, reads included, for the rest of a build after
  any upload it is refused (Gradle 9.8.2, `BaseRemoteBuildCacheServiceHandle`), so the cache answers every upload
  as stored and drops the ones it does not keep. Each runner's count says how many.
- **Repositories do not share.** A runner reads and writes only its repository's entries. Sharing between
  repositories is left until someone needs it.
- **Size.** `[caches.build] max_size` (default `10G`): the shared caches' check, at most hourly when no runner runs
  (ADR 0050), removes the entries used least recently until the rest fit. Between checks an upload that would take
  the cache past twice the limit, or an entry over a quarter of it, is refused. Entries are last used when last
  written or read; a read sets the file's time, so the order outlives a restart. `runnerctl caches clean
  runnerd-build-cache` empties it while no runner runs.
- **Tools**, wired by the image with no change to a repository, each only where the repository configures no
  remote cache of its own:
  - Gradle: an init script in `$GRADLE_USER_HOME/init.d` sets an `HttpBuildCache` remote with `push = true` when
    `settings.buildCache.remote` is unset. It does not turn the build cache on: a build that never asked for one
    keeps building as it did.
  - sccache: `SCCACHE_WEBDAV_ENDPOINT`, used when a workflow sets `RUSTC_WRAPPER=sccache`. WebDAV comes after S3,
    Redis, Memcached, GCS, GitHub Actions' cache and Azure in sccache's choice (0.19.0, `CacheConfigs::into_fallback`),
    so a workflow's own backend wins. `SCCACHE_SKIP_CACHE_CHECK` keeps a cache that does not answer from failing
    sccache's start; `SCCACHE_WEBDAV_DISABLE_CREATE_DIR` leaves it plain `GET` and `PUT`.
  - Bazel: `/etc/bazel.bazelrc`, which a workspace's `.bazelrc` overrides, with `--remote_download_outputs=all` so
    no build depends on an upload the cache dropped.
  - Not Turborepo: it is configured by environment, and our `TURBO_API` would send a repository that sets only
    `TURBO_TOKEN` and `TURBO_TEAM` for Vercel's cache to ours. Not Nx: its self-hosted cache client fails the run
    when the cache does not answer (Nx 24.0.1, `http_remote_cache.rs`).
- **Measured.** The cache counts each runner's lookups that hit and missed, the bytes it served and stored, the
  uploads it dropped, and whether the runner could write; the engine stores them with the runner when it is
  removed (history version 12). The job page, `runnerctl job` and the TUI show them, `GET /v1/caches` the cache's
  size and the last 7 days' hit rate, and the insight "Cold cache" names a job whose writing runs find almost
  nothing.
- **On by default** (`[caches.build] enabled`, `[[repositories]] build_cache`): with these defaults no job but a
  push, schedule or manual run of the default branch changes what another job loads.

## Consequences

A pull request's Gradle build gets what `main` built in any slot, and `main` what the previous `main` run built.

A job that pushes to the default branch is trusted with every later job's build of that repository. It already was
with the repository itself; runnerd adds no new way in. The slot's own `~/.cache`, which jobs of every branch
share, is as before (ADR 0007): a `main` build in a slot a pull request used may still load the pull request's entries
in Gradle's local build cache.

What a `main` job uploads before GitHub answers waits in the daemon's directory, at most a poll interval's worth.
Counts of a runner adopted after a restart start again at 0. Containers a job starts, such as a `container:` job's,
do not reach the cache. A runner image older than this release has no forwarder: its runners run as before.
