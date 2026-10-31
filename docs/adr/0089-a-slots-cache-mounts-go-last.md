# 0089. A slot's build cache follows its share, and cache mounts go last

Status: **Accepted** (2026-10-31). Amends [0054](0054-slot-docker-stores-share-the-disk.md).

## Context

ADR 0054 meant a slot's build cache to stay within about half of its Docker share. It kept far less. `image/daemon.json`
set only BuildKit's `defaultReservedSpace` (512 MB). With any of its defaults set, Moby skips the disk-based ones
(`DefaultGCPolicy`), so its GC rules have no `maxUsedSpace` or `minFreeSpace`, and BuildKit then keeps the reserved
space and no more (`calculateKeepBytes`). The floor acted as a ceiling: about a minute after each build, the GC took
the slot's unshared build cache down to 512 MB. On a host with an 8 GB share per slot, the private build cache sat
at 274 MB between jobs, and a 1.57 GB cache mount was gone 80 s after the build that wrote it, with no job cleanup
involved.

Cache mounts (`RUN --mount=type=cache`) are where builds keep package stores: pnpm's, apt's, Go's modules. They are
also the largest records, and the first a build touches, so the cleanup's least-recently-used trim to half the share
took them first even when the GC had not. A web app build whose pnpm store is 4.2 GB downloaded all 2,639 packages on
every run, about 2.2 GB and 9 to 10 minutes each, while its slot's images stayed and the store never reached its
share. In a throwaway slot with the same image and an 8 GB share, the policy below took the second build of a new
commit from 657 s, `reused 0, downloaded 2639` and 1,912 MiB received to 343 s, `reused 2639, downloaded 0` and
674 MiB.

Moby passes only `key=value` filters to BuildKit (`toBuildkitPruneInfo`). A `type!=exec.cachemount` filter, in
`docker builder prune` or a GC rule, comes out with no filter at all and matches every record.

## Decision

- **The engine starts with a GC policy from the share.** `start_docker` keeps the share first
  (`keep_docker_budget`, which ran after it), then writes `builder.gc` into the container's
  `/etc/docker/daemon.json` (`docker_gc_policy` in `image/layers.sh`) before dockerd reads it. `image/daemon.json`
  keeps only the mirror. Without a share from runnerd, the image's 20 GB sizes it, as it does the cleanup.
- **The policy, in order.** Build contexts (`type=source.local`) and git checkouts (`type=source.git.checkout`) to
  512 MB; layers (`type=regular`) while the unshared build cache holds over half the share; everything, cache mounts
  included, only while it holds over three quarters. Each BuildKit rule compares the whole unshared build cache with
  its limit and removes only the records its filter matches, least recently used first. Layers therefore go before
  any cache mount, and a package store of up to about three quarters of the share stays. Three quarters, not half:
  the measured pnpm store of a web app build took 4.2 to 4.8 GB of an 8 GB share, which half would drop after each
  build, and the last quarter is left for the slot's images.
- **The cleanup prunes in the same order.** After a job, `job-cleanup.sh` removes build contexts and git checkouts,
  then layers down to half the share, one `docker builder prune --filter type=...` each. A store still over its
  share loses every layer, then its images, and only then everything, cache mounts too. That last step prints, next
  to the job's `received N MiB` line, how much the store held, of what share, and what it held without layers and
  images, so the job's log says why the slot's next job starts cold.
- **Positive type filters only**, in the policy and in the cleanup.
- Nothing changes for `docker/setup-buildx-action`'s default `docker-container` driver, which starts a BuildKit
  container per job and removes it with the job. The docs suggest `driver: docker` on runnerd, which builds on the
  slot's engine and its cache.

## Consequences

- A slot's build cache now holds up to half its share and cache mounts up to three quarters, where it held 512 MB.
  A slot whose images are large reaches its share sooner: its layers and then its images go after a job, where they
  stayed before because the GC had already taken the build cache. Images come back through the mirror; packages a
  cache mount held come back from the network, which is why images go first.
- A cache mount is per slot, not per repository, and its id is a plain string (`pnpm-store`, `apt`), so a job of
  another repository in the same slot reads and writes the same mount. That is the trust the slot's `~/.cache`
  already has ([0007](0007-warm-caches-snapshot-plus-slot-layer.md), the Consequences of
  [0052](0052-shared-build-cache.md)): a pull request's job can leave content that a later default-branch build in
  the slot reads. Like the shared build cache ([0053](0053-build-cache-written-by-own-pull-requests.md),
  Poisoning), it adds no new kind of trust; before, the GC emptied the mounts within a minute of each build, so in
  practice it widens what stays. pnpm checks its store's integrity; an apt or pip cache mount checks less.
- A fork's pull request can reach a slot. runnerd does not tell a fork's job apart when it starts one: GitHub runs
  it on the repository's runners when the repository lets fork pull requests run workflows, which on a public
  repository `runnerd doctor` warns about. 0053 keeps such a job from writing the shared build cache, which runnerd
  serves itself; a cache mount is the slot engine's, and the fork's job writes it as any job does. A repository that
  runs untrusted pull requests belongs on a profile of its own, apart from repositories whose builds must not read
  what those jobs leave.
- A possible follow-up: prune the slot's `exec.cachemount` records when its container starts, if the slot's last job
  came from another repository or a pull request. The engine keeps each slot's last job as repository, workflow and
  job name ([0080](0080-a-job-starts-in-the-slot-that-ran-it-last.md)), so the repository is known; the job about to
  start is known only for a runner registered for it, since a warm one is bound to its slot before GitHub gives it a
  job. Whether a job was a pull request's is not kept: the engine asks GitHub for a run's trigger only to decide
  whether its job may write the shared build cache (0053).
- The GC still runs during a job, at most once a minute, so a build of more than the share's three quarters loses
  its oldest records before the job ends, as Docker's own default would.
- The policy and the cleanup are the runner image's, not the daemon's: a profile keeps the 512 MB GC until it runs
  an image built with this change.
