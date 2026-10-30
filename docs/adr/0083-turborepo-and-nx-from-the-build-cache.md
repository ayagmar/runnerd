# 0083. Turborepo's remote cache served from the build cache, Nx's on request

Status: **Accepted** (2026-10-30). Amends [0052](0052-shared-build-cache.md).

## Context

0052 left Turborepo and Nx out of the build cache. Turborepo is configured by its environment, so the runner's
`TURBO_API` would send a repository that sets only `TURBO_TOKEN` and `TURBO_TEAM` for Vercel's cache to runnerd's.
Nx's self-hosted cache client fails the run when the cache does not answer. Since then a repository on runnerd uses
turbo with no remote cache of its own, and every turbo task runs again in each job.

Turborepo's remote cache (turborepo at 1364056, turbo 2.11.10):

- `GET`, `HEAD` and `PUT` of `<api>/v8/artifacts/<hash>`, with `?slug=<team>` or `?teamId=<id>` and a bearer token.
  A 404 is a miss. The upload has a `Content-Length` and the task's duration in `x-artifact-duration`, which a hit
  sends back for the summary's time saved; a repository that signs its artifacts (`remoteCache.signature`) also
  sends `x-artifact-tag` and treats a hit without one as an error.
- `GET <api>/v8/artifacts/status` at the start of a run, `{"status":"enabled"}` and an optional `readOnly`, which
  turbo waits for at most 250 ms; `POST <api>/v8/artifacts/events` for its analytics; and `POST <api>/v8/artifacts`
  with `{"hashes":[...]}`, which a dry run sends and which may fail, turbo then asking each hash with a `HEAD`.
- Turbo reads its settings, field by field, from its flags, then its environment (`TURBO_API`, `TURBO_TOKEN` with
  `TURBO_TEAM` or `TURBO_TEAMID`), then `.turbo/config.json`, its user's config, and last `turbo.json`'s
  `remoteCache`. The remote cache is on once it has a token and a team. A cache that does not answer is a warning
  and the run goes on without it.

Nx's self-hosted remote cache (nx at 753a73b, Nx 24.0.2): `GET` and `PUT` of `<server>/v1/cache/<hash>`, a gzipped
tarball, with `NX_SELF_HOSTED_REMOTE_CACHE_SERVER` and an optional `NX_SELF_HOSTED_REMOTE_CACHE_ACCESS_TOKEN`. Nx
Cloud and Nx's S3, GCS, Azure and shared file system caches take precedence over it. A request that fails, or any
answer but 200 and 404, fails the run: `nx run` against a port with nothing on it exits 1 with "Failed to send
request".

## Decision

- **Turborepo, on with the build cache.** The image exports `TURBO_API=http://127.0.0.1:41771/turbo`,
  `TURBO_TOKEN=runnerd` and `TURBO_TEAM=runnerd` beside sccache's variables. The socket says who the job is, as for
  every tool; the token and team only turn turbo's remote cache on, and the server reads the token only to tell
  runnerd's from a workflow's own. Its artifacts are
  entries under `<owner>/<name>/turbo/<hash>`, with the same size limit, trim, write rules and counts as Gradle's.
  `buildcache::turbo` answers the status (`enabled`, never `readOnly`: an upload a job may not make is answered as
  stored, as every tool's is), the events (accepted, unread), and the lookup of many hashes (a hit or `null` each,
  read from the index, counted neither as a hit nor a miss, as a `HEAD` is not). No duration or tag is kept: a hit
  saves no time in turbo's summary, and a repository that signs its artifacts finds none.
- **What a repository's own configuration keeps.** A workflow's `TURBO_API`, `TURBO_TOKEN` and `TURBO_TEAM` each
  take precedence over the runner's, and `TURBO_CACHE: local:rw` keeps turbo off any remote cache. A workflow that
  sets only `TURBO_TOKEN` and `TURBO_TEAM` for Vercel's cache, as Vercel's guide for GitHub Actions does, reaches
  runnerd's socket with its own token: every request under `/turbo` whose bearer token is not `runnerd` goes to
  `https://vercel.com/api`, with the path after `/turbo`, the query, every header but the connection's (the token
  among them) and the body as it arrives, and Vercel's answer comes back the same way, redirects included. The
  origin is fixed, and the path is appended under `/api`, so no request sends the token anywhere else. Nothing of it
  is logged, counted or kept, and runnerd's write rules do not apply: the repository's own cache decides, as on
  GitHub's runners. Signed artifacts pass with their tags. reqwest streams both ways without its `stream` feature
  (`Body::wrap` of the incoming body, `Response::chunk`); it connects within 10 s and waits at most 60 s for each
  read, with no limit on the whole. A request with runnerd's token, or with none, is answered locally.
  A `turbo.json`'s `remoteCache` does not keep its API: turbo reads its environment first, so such a repository
  sets `TURBO_API` in its workflow; the docs say so.
- **Nx, on request.** The socket serves `/nx/v1/cache/<hash>` the same way, entries under `<owner>/<name>/nx/<hash>`,
  but the image sets no `NX_SELF_HOSTED_REMOTE_CACHE_SERVER`: with it set by default, every Nx run that looks up a
  task while runnerd restarts would fail. A workflow opts in by writing
  `NX_SELF_HOSTED_REMOTE_CACHE_SERVER=$RUNNERD_BUILD_CACHE_URL/nx` to `$GITHUB_ENV`.

## Consequences

A pull request's turbo tasks get what `main` built in any slot, with no workflow change, and a writing run of
`main` what the previous one built.

A repository on Vercel's remote cache keeps it with no change; its requests go through the daemon, which carries
its token to Vercel and never logs it. While runnerd restarts, or when Vercel does not answer (a 502), those
requests fail and turbo builds without the cache. The daemon talks to Vercel's API for those jobs.

A repository whose Nx workflow opts in fails its run when runnerd restarts while Nx looks a task up.
