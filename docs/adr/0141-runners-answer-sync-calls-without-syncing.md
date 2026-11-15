# 0141. Runners answer sync calls without syncing

Status: **Accepted** (2026-11-15). Amends [0054](0054-slot-docker-stores-share-the-disk.md).

## Context

The owner's system disk, one SSD holding `/`, `/home` and `/var/lib/docker` on btrfs, wrote 2,512 GiB in 14 hours of
about 1,100 jobs. In a 60 s trace of the host, 66% of the bytes written were synchronous, and all 106 btrfs
transaction commits were forced, by dockerd, buildkitd, containerd and dpkg, most of them inside runner containers,
each of which runs its own Docker Engine on its slot's volume. A forced commit rewrites btrfs's metadata, which is
kept twice, so commits every 1.7 s instead of every 30 s multiply what the disk writes.

Everything a runner writes is a cache or goes with its container: the slot's layers and Docker store, the shared pnpm
store and Maven repository, the job's workspace and containers. The syncs that keep those consistent across a power
loss are worth less than the wear they cost, and ADR 0054 already empties a slot's Docker store that does not start.

The obvious mechanism, a seccomp profile runnerd passes when it creates a runner container (Docker's default with
the sync calls answered `SCMP_ACT_ERRNO`, `errnoRet: 0`), does not work under sysbox: sysbox-runc 0.7.1's
`cfgSeccomp` (`libsysbox/syscont/spec.go`) removes from a profile every error rule for a call in its own allowlist
(`syscontSyscallWhitelist`), which holds every sync call. Probed on the owner's host (Docker 29.9.3, sysbox 0.7.1),
`fsync(999)` in a sysbox container with that profile still fails with `EBADF`; under runc it returns 0.

## Decision

- The runner image installs the filter itself. Its entrypoint is `dind runnerd-skip-syncs runnerd-runner`:
  `runnerd-skip-syncs` (`image/skip-syncs.py`), with `RUNNERD_SKIP_SYNCS=1` in its environment, installs a seccomp
  filter in which `fsync`, `fdatasync`, `sync`, `syncfs`, `sync_file_range` and `msync` return 0 at once, then execs
  the entrypoint. Root in the container may install it without `no_new_privs`, which would stop the runner's sudo,
  because it has `CAP_SYS_ADMIN` in the container's user namespace. A filter cannot be removed and every process
  started after it inherits it: the runner, its job, the slot's Docker Engine, its containers and its BuildKit
  builders. The kernel runs their own filters beside it, and an error wins over an allow. The filter knows x86_64 and
  aarch64, the two platforms the image is built for; a call of another ABI syncs.
- runnerd puts `RUNNERD_SKIP_SYNCS=1` (`docker::SKIP_SYNCS`) in a runner container's environment unless
  `[host] skip_syncs = false`. A reload applies it from the next runner on; a running one keeps what it started with.
  The setting does not depend on `isolation`: the filter works the same under `unsafe-dind`.
- Only runners. A seed container keeps its syncs: `seed.sh` syncs the snapshot's and slots' volumes on purpose around
  `/base/.reseeding` (ADR 0066), and its `sync -f` also writes what slot 1's runners left in memory. The mirror is
  Docker's registry image, which runnerd does not change, and keeps its syncs. The containers that measure layers and
  open a record's shell bypass the entrypoint, so they keep theirs. The build cache is the daemon's, outside any
  container.
- `runnerd doctor`, and so the dashboard's config view, words the setting in an information check, `runner syncs`
  (`config::Host::syncs_check`).
- Docker's default profile is not copied into runnerd, so there is no copy to keep current. If sysbox stops removing
  such rules, a profile passed at create would need no image change; until then this is the mechanism.

## What a crash can leave

A container that stops, is killed or removed is not a crash: what it wrote is in the page cache and reaches the disk.
Only a host crash or power loss loses what was not written, up to about a minute of it here (btrfs commits every 30 s,
and dirty pages are written once 30 s old). btrfs then mounts at its last commit, where a file whose data was not
written yet is cut short, never filled with another file's data.

- A slot's Docker store: a store its engine does not start on is emptied and started again (ADR 0054), seen in the
  container's output. One that starts with an image whose layer files were cut short fails that image's jobs in that
  slot until the store is emptied, by its budget or by `runnerctl caches --clean runnerd-<profile>-slot-<n>`. Nothing
  detects it.
- A slot's layers, the pnpm store and the Maven repository: a file cut short fails the jobs that use it until the
  volume is cleaned.
- The profile's snapshot: a reseed writes it with its syncs, from slot 1's layers as they are.
- A record's container: its files may be cut short; its log and data are the daemon's.
- The mirror and the build cache keep their syncs.

No guard was added for these: none of them could be shown here, and each is recovered by cleaning one volume.

## Old and new

An image without `runnerd-skip-syncs` ignores the variable and syncs; a daemon without the setting sets no variable,
and the new image then syncs. Neither pairing breaks, and each syncs as before.

## Evidence

Probed on the owner's host with sysbox, `--cgroup-parent ci.slice` and `ghcr.io/ayagmar/runnerd-runner:node-0.1.1`,
`runnerd-skip-syncs` mounted in: a script that starts the container's Docker Engine on a volume, pulls an image,
builds with `docker build` and with a buildx `docker-container` builder, runs postgres (initdb, 300 commits), writes
512 MiB and 1,000 4 KiB files with `dd conv=fsync`, installs two packages with apt and commits 500 files with git.
Three runs each, alternated:

| | without | with |
|---|---|---|
| bytes the container wrote to the disk (`io.stat` `wbytes`) | 1.61, 1.58, 1.61 GB | 1.44, 1.25, 1.22 GB |
| write requests (`wios`) | 59,566, 59,014, 64,113 | 39,447, 31,435, 29,528 |
| wall time | 87, 57, 48 s | 62, 57, 42 s |

`io.stat` counts the container's own writes, not the btrfs commits it forces, which the transaction thread writes.
The disk as a whole wrote 4.0 to 4.7 GiB during the runs without, 2.5 and 2.8 GiB during two with; the third with ran
beside a burst of live jobs (13.3 GiB), so the whole-disk figure is noisy. The filter reached the runner as root and as
its user, a container of its Docker Engine, a `docker build` step and a buildx `docker-container` builder's step.
dockerd, containerd, the builds, postgres, apt and dpkg (`dpkg --audit` clean), git (`git fsck` clean) and the
runner's sudo all worked. `a_runner_told_to_skip_syncs_skips_them_down_to_its_jobs_containers_and_builds` in
`crates/core/tests/slot_docker_store.rs` keeps the first part.

## Consequences

- Runners write less, and fewer of their writes force a btrfs commit; jobs that sync a lot run faster.
- A host crash can cost a slot its next jobs on a torn image, until its volume is cleaned.
- A program that relies on `fsync` to see a write error does not see it. `O_SYNC` and `O_DSYNC` writes still sync;
  io_uring, whose `fsync` the filter cannot see, is blocked in a sysbox runner, whose profile is Docker's default
  with sysbox's own allowlist, neither of which allows it; a privileged `unsafe-dind` runner has no profile.
- The image needs `python3`, which the Dockerfile installs.
