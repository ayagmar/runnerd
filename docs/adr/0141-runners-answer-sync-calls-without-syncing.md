# 0141. Runners answer sync calls without syncing, and a host crash empties what they wrote

Status: **Accepted** (2026-11-15). Amends [0054](0054-slot-docker-stores-share-the-disk.md).

## Context

The owner's system disk, one SSD holding `/`, `/home` and `/var/lib/docker` on btrfs, wrote 2,512 GiB in 14 hours of
about 1,100 jobs. In a 60 s trace of the host, 66% of the bytes written were synchronous, and all 106 btrfs
transaction commits were forced, by dockerd, buildkitd, containerd and dpkg, most of them inside runner containers,
each of which runs its own Docker Engine on its slot's volume. A forced commit rewrites btrfs's metadata, which is
kept twice, so commits every 1.7 s instead of every 30 s multiply what the disk writes.

Everything a runner writes is a cache or goes with its container: the slot's layers and Docker store, the shared pnpm
store and Maven repository, the job's workspace and containers. Their syncs keep those caches whole across a power
loss; nothing else needs them.

The obvious mechanism, a seccomp profile runnerd passes when it creates a runner container (Docker's default with
the sync calls answered `SCMP_ACT_ERRNO`, `errnoRet: 0`), does not work under sysbox: sysbox-runc 0.7.1's
`cfgSeccomp` (`libsysbox/syscont/spec.go`) removes from a profile every error rule for a call in its own allowlist
(`syscontSyscallWhitelist`), which holds every sync call. Probed on the owner's host (Docker 29.9.3, sysbox 0.7.1),
`fsync(999)` in a sysbox container with that profile still fails with `EBADF`; under runc it returns 0.

## Decision

- The runner image installs the filter itself. Its entrypoint is `dind runnerd-skip-syncs runnerd-runner`:
  `runnerd-skip-syncs` (`image/skip-syncs.py`), with `RUNNERD_SKIP_SYNCS=1` in its environment, installs a seccomp
  filter in which `fsync`, `fdatasync`, `sync`, `sync_file_range` and `syncfs` return 0 at once, then execs the
  entrypoint. Root in the container may install it without `no_new_privs`, which would stop the runner's sudo,
  because it has `CAP_SYS_ADMIN` in the container's user namespace. A filter cannot be removed and every process
  started after it inherits it: the runner, its job, the slot's Docker Engine, its containers and its BuildKit
  builders. The kernel runs their own filters beside it, and an error wins over an allow. Whatever stops the filter
  from going in, the wrapper says so on the container's output and execs the entrypoint, which then syncs. An image
  built on runnerd's must keep `python3`, which runs the wrapper whether or not runnerd asks.
- `msync` is left alone. Answered 0, `msync(addr, len, MS_ASYNC)`, which some runtimes use to ask whether an address
  is mapped, would let them read memory that is not. Narrowing it to `MS_SYNC` would keep the same answer for an
  unmapped `MS_SYNC` call.
- The filter sees a call's number and arguments, not the descriptor behind them, so a call on one the kernel would
  refuse returns 0 too: `fsync(999)` and `syncfs(-1)` succeed. A test suite that asserts `EBADF` from them, as a
  language's or a file system library's own tests may, fails in a runner; such a host sets `skip_syncs = false`.
- The filter knows x86_64, where it was run, and aarch64, from the kernel's generic syscall table; runnerd publishes
  amd64 images, so the aarch64 half has not run. A call of another ABI syncs.
- runnerd puts `RUNNERD_SKIP_SYNCS=1` (`docker::SKIP_SYNCS`) in a runner container's environment unless
  `[host] skip_syncs = false`. A reload applies it from the next runner on; a running one keeps what it started with.
  The setting does not depend on `isolation`: the filter works the same under `unsafe-dind`.
- Only runners. A seed container keeps its syncs: `seed.sh` syncs the snapshot's and slots' volumes on purpose around
  `/base/.reseeding` (ADR 0066), and its `sync -f` also writes what slot 1's runners left in memory. The mirror is
  Docker's registry image, which runnerd does not change, and keeps its syncs. The containers that measure layers and
  open a record's shell bypass the entrypoint, so they keep theirs. The build cache is the daemon's, outside any
  container.
- Docker's default profile is not copied into runnerd, so there is no copy to keep current. If sysbox stops removing
  such rules, a profile passed at create would need no image change; until then this is the mechanism.

## A host crash empties what runners wrote

A container that stops, is killed or removed is not a crash: what it wrote is in the page cache and reaches the disk.
A host crash or power loss loses what was not written yet, about a minute of it with the kernel's defaults (btrfs
commits every 30 s, and dirty pages are written once 30 s old; a power profile that writes back later lengthens it).
btrfs then mounts at its last commit and never shows another file's data, but a file is torn: short, empty, zero-filled
in places, or part old and part new at its full size. A file overwritten in place, such as the bbolt databases of
dockerd, containerd and BuildKit, comes back part old and part new; bbolt checks only its meta pages on open, so the
engine starts and fails on the first pull, build or prune that reads a torn page. A file written and renamed over
another can come back empty under its new name. Neither shows to ADR 0054's check, which empties a store only when
its engine does not start, and a reseed copies slot 1's layers as they are into the snapshot every slot reads, where
cleaning a slot no longer reaches it.

So the engine notes, before a runner that skips its syncs starts, the boot it starts in, and forgets it once what
runners wrote is on disk (`scheduler::engine::unsynced`, kept in `unsynced` beside the job history):

- Before the first such runner after the note was forgotten, the engine writes `since <boot ID>` to the file, synced
  and renamed into place. If it cannot write the file or read the kernel's boot ID, the runner starts without
  `RUNNERD_SKIP_SYNCS` and syncs: a crash after it could not be told.
- A stop that leaves no runner running calls `sync(2)` and then removes the file. A stop that leaves busy runners for
  the next start keeps it.
- At start, a file whose boot ID is not the kernel's, a boot since, with no such stop between, makes the caches torn:
  the file says `torn`, no runner starts, and the first tick removes every slot's volume, layers and Docker store
  alike, and the pnpm and Maven volumes. Once Docker removed them all, the engine calls `sync(2)` and removes the
  file. A volume Docker does not remove keeps the caches torn, and no runner starts, until a later tick removes it;
  the log names it.
- The pnpm store and the Maven repository go too. Each is one volume every slot of every profile writes, so a torn
  file there fails every job that uses that package, on every slot, and nothing replaces it: Maven does not check an
  artifact it has. A crash costs one download of each package.
- The snapshots stay. Only a reseed writes them, with its syncs, and slot 1's layers are emptied before any reseed can
  copy them. The mirror and the build cache keep their syncs. A record is a committed image of a failed job's
  container: its files may be torn, and it is kept, as it shows what the job left; its log and data are the daemon's.
- A daemon that dies and starts again in the same boot finds its boot ID and empties nothing: the kernel lost nothing.
  A clean reboot after a stop that left runners running, or after the daemon died, empties the caches, as the engine
  cannot tell it from a crash: one cold start per slot, which ADR 0054 already accepts for a store that does not start.
- The file is the state, so it survives a restart; a reload does not change it. A file that is not one the engine
  writes reads as torn.

Volumes of a profile the config no longer has are not emptied; a profile added back with the same name finds them as
they were.

## Old and new

An image without `runnerd-skip-syncs` ignores the variable and syncs; a daemon without the setting sets no variable,
and the new image then syncs. Neither pairing breaks, and each syncs as before. A daemon with the setting and an old
image still notes the boot and empties the caches after a crash, which that image did not need.

## Evidence

Probed on the owner's host with sysbox, `--cgroup-parent ci.slice` and `ghcr.io/ayagmar/runnerd-runner:node-0.1.1`,
`runnerd-skip-syncs` mounted in: a script that starts the container's Docker Engine on a volume, pulls an image,
builds with `docker build` and with a buildx `docker-container` builder, runs postgres (initdb, 300 commits), writes
512 MiB and 1,000 4 KiB files with `dd conv=fsync`, installs two packages with apt and commits 500 files with git.
The load was made to sync a lot; real jobs were not measured with the filter. Three runs each, alternated, with
`msync` still in the filter:

| | without | with |
|---|---|---|
| bytes the container wrote to the disk (`io.stat` `wbytes`) | 1.61, 1.58, 1.61 GB | 1.44, 1.25, 1.22 GB |
| write requests (`wios`) | 59,566, 59,014, 64,113 | 39,447, 31,435, 29,528 |

Three more runs each, with `msync` out of the filter, counted the btrfs transaction commits of the whole file system
(`/sys/fs/btrfs/<uuid>/commit_stats`) from the container's start to 45 s after its load ended, beside the live runners,
with 60 s of the host alone counted before each:

| run | commits on the host alone, 60 s before | commits during the run | container `wbytes` | container `wios` |
|---|---|---|---|---|
| without 1 | 18 | 162 in 96 s | 1.69 GB | 67,218 |
| with 1 | 112 | 54 in 86 s | 1.29 GB | 30,887 |
| without 2 | 4 | 48 in 101 s | 1.59 GB | 62,863 |
| with 2 | 2 | 44 in 76 s | 1.31 GB | 35,085 |
| without 3 | 56 | 205 in 106 s | 1.84 GB | 73,367 |
| with 3 | 5 | 47 in 81 s | 1.37 GB | 37,172 |

Runs 1 and 3 without the filter ran beside bursts of live jobs, so their commits are the host's more than the load's.
The quiet runs, 2 without against 2 and 3 with, show about as many commits with the filter as without: btrfs answers
an `fsync` from its log tree without a transaction commit, and what forces the commits these runs do lies outside the
filter (the host's Docker Engine creating and removing the container and its volume, overlayfs syncing on unmount and
copy-up). The filter's measured effect is on the container's own writes, not on the commit rate. Whether the forced
commits of the owner's trace drop with real jobs is not measured.

`io.stat` counts the container's own writes, not the btrfs commits it forces, which the transaction thread writes.
Wall time (87, 57, 48 s without, 62, 57, 42 s with) is within the noise. The filter reached the runner as root and as
its user, a container of its Docker Engine, a `docker build` step and a buildx `docker-container` builder's step.
dockerd, containerd, the builds, postgres, apt and dpkg (`dpkg --audit` clean), git (`git fsck` clean) and the
runner's sudo all worked. Kernel-side syncs stay: overlayfs syncs each copy-up, in the slot's layers and in the
engine's containers.

`a_runner_told_to_skip_syncs_skips_them_down_to_its_jobs_containers_and_builds` in
`crates/core/tests/slot_docker_store.rs` checks the image's entrypoint and, on an image built from the tree, each call
the filter answers, for the runner as root and as its user, and `fsync` in a container and a build step of its
engine. `what_runners_wrote_unsynced_is_emptied_after_a_crash_and_only_then` in
`crates/core/src/scheduler/engine/tests/unsynced.rs` runs the engine on the fakes through generated sequences of runner
starts, jobs ending, stops, daemon deaths, clean reboots, crashes, reloads of `skip_syncs` and volumes Docker refuses
to remove.

## Consequences

- Runners write less: in the measured load, about a fifth fewer bytes and half the write requests of their own. The
  btrfs commit rate did not measurably change in that load.
- A host crash costs every slot and the shared pnpm and Maven stores a cold start.
- A program that relies on `fsync` to see a write error, or on its `EBADF`, does not see it. `O_SYNC` and `O_DSYNC`
  writes still sync; io_uring, whose `fsync` the filter cannot see, is blocked in a sysbox runner, whose profile is
  Docker's default with sysbox's own allowlist, neither of which allows it; a privileged `unsafe-dind` runner has no
  profile.
- `runnerd doctor`, and so the dashboard's config view, words what core knows in an information check, `runner syncs`
  (`config::Host::syncs_check`): that each runner is asked, that an image without the wrapper ignores the ask, and
  what a crash empties. It does not say whether a runner's filter is in: the wrapper says that only on the container's
  output.
