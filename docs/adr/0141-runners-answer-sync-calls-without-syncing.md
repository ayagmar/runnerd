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
- The image says it has the filter: the runner image carries the label `runnerd.skip-syncs=1`
  (`docker::SKIP_SYNCS_LABEL`), the version of the variable it answers. Before each runner start the engine asks
  Docker for its image's labels, and puts `RUNNERD_SKIP_SYNCS=1` (`docker::SKIP_SYNCS`) in the container's
  environment only if the label is there and `[host] skip_syncs` is not `false`. A runner of an image without it,
  such as the 0.1.1 images, is never asked and arms nothing below, whatever the config says. A reload applies the
  setting from the next runner on; a running one keeps what it started with. The setting does not depend on
  `isolation`: the filter works the same under `unsafe-dind`.
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

An orderly shutdown or reboot loses nothing. systemd stops every unit first, and with them every container; then
`systemd-shutdown` logs `Syncing filesystems and block devices.`, calls `sync(2)`, and once it returns logs `Sending
SIGTERM to remaining processes...` before it unmounts. journald writes both lines and syncs its files when that signal
stops it. So a past boot whose journal holds both lines, in that order and with no timeout between, ended with
everything its runners wrote on disk; a crash at any point before leaves the second line out. The owner's host
boots about once a day. Of the 100 past boots its journal kept on 2026-11-15, 97 left both lines. The other 3 left
neither: one ended on a GPU fault, one stopped mid-session, and one died during its shutdown, while systemd unmounted
Docker's overlays. A daemon that took its own stop signal as an orderly shutdown would have kept the caches after that
last one.

This rests on one assumption: no runner process is alive when `systemd-shutdown` starts. It holds where Docker's
cgroup driver is `systemd`, so each container is a systemd scope that systemd stops with the other units, and
`live-restore` cannot keep one past that; the owner's host is set up so. It does not hold with the `cgroupfs` driver
and `live-restore` on, or after `systemctl poweroff --force`, which skips stopping units: runner processes then live
on past the sync the two lines vouch for, until `systemd-shutdown` kills them. systemd 262's `shutdown.c` syncs a
second time after that, once the unmounts are done and just before `reboot()`, so even there the data is lost only if
the host also dies between the two syncs, such as a hung unmount ended with the power button.

So the engine notes, before a runner that skips its syncs starts, the boot it starts in, and forgets it once what
runners wrote is on disk (`scheduler::engine::unsynced`, kept in `unsynced` beside the job history):

- Before the first such runner after the note was forgotten, the engine writes `since <boot ID>` to the file, synced
  and renamed into place. If it cannot write the file or read the kernel's boot ID, the runner starts without
  `RUNNERD_SKIP_SYNCS` and syncs: a crash after it could not be told.
- A stop that leaves no runner running calls `sync(2)` and then removes the file. A stop that leaves busy runners for
  the next start keeps it: they go on writing.
- At start, a file whose boot ID is not the kernel's is a boot since. If the journal shows that boot ended in an
  orderly shutdown (`boots::ended_synced`, `journalctl _BOOT_ID=<id> SYSLOG_IDENTIFIER=systemd-shutdown`), the engine
  removes the file and keeps the caches, busy runners at the shutdown or not. Otherwise the caches are torn: a crash,
  a stop followed by a crash while the runners it left kept writing, a journal that keeps no past boot, or one
  runnerd's user cannot read. The file then says `torn`, and no runner, reseed, measure, trim or record shell starts.
- Each tick while the caches are torn waits for the records being committed, whose containers mount their slots'
  volumes and go once committed. Then, for every slot's volume, layers and Docker store alike, and the pnpm and Maven
  volumes, it asks Docker which containers mount it (`docker ps --all --filter volume=`), removes those of runnerd's
  that no longer run, such as a runner or a seed the crash stopped, and removes the volume (`cache::empty_volumes`).
  Once all are gone, the engine calls `sync(2)` and removes the file.
- A volume Docker does not remove keeps the caches torn, and no runner starts, until a later tick removes it; the log
  names the volume and why. A running container or one that is not runnerd's on it is never removed, and is a refusal
  at once rather than a wait. After 5 ticks that found such a container (`unsynced::REFUSALS`), the engine gives up:
  it calls `sync(2)`, removes the file, logs the volumes left and the `docker volume rm` that empties them, and
  runners start on what the caches hold. A container left on a runnerd volume is the owner's doing, and no runner at
  all fails every job. Docker's own errors, an outage, a 5xx or a request that times out, count nothing: the caches
  stay torn and each tick tries again, as giving up on them would let a reseed copy a torn slot 1 into the snapshot.
  The count is not kept: a restart tries 5 times again.
- The pnpm store and the Maven repository go too. Each is one volume every slot of every profile writes, so a torn
  file there fails every job that uses that package, on every slot, and nothing replaces it: Maven does not check an
  artifact it has. A crash costs one download of each package.
- The snapshots stay. Only a reseed writes them, with its syncs, and no reseed starts before slot 1's layers are
  emptied. The mirror and the build cache keep their syncs. A record is a committed image of a failed job's
  container: its files may be torn, and it is kept, as it shows what the job left; its log and data are the daemon's.
- A daemon that dies and starts again in the same boot finds its boot ID and empties nothing: the kernel lost nothing.
  A boot ID that cannot be read at start cannot be told from another boot, so the journal decides as above.
- The file is the state, so it survives a restart; a reload does not change it. A missing file is nothing owed. A
  file that is empty, cannot be read, or holds what the engine does not write reads as torn. The file a daemon of this
  change's first version wrote, `since <boot ID>` or `torn`, reads the same.

Volumes of a profile the config no longer has are not emptied; a profile added back with the same name finds them as
they were.

## Old and new

An image without `runnerd-skip-syncs` lacks its label, so the engine never asks its runners and arms nothing: no
mark, nothing emptied after a crash. A daemon without the setting sets no variable, and the new image then syncs.
Neither pairing breaks, and each syncs as before.

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

`crates/core/tests/slot_docker_store.rs`, against Docker and sysbox:

- `a_runner_told_to_skip_syncs_skips_them_down_to_its_jobs_containers_and_builds` checks the image's entrypoint and
  label and, on an image built from the tree, each call the filter answers, for the runner as root and as its user,
  and `fsync` in a container and a build step of its engine.
- `a_runner_image_from_before_the_filter_is_never_asked_to_skip_syncs` reads the 0.1.1 image's labels: no filter.
- `torn_volumes_are_emptied_past_runnerds_leftovers_and_a_held_one_is_refused_at_once` leaves an exited
  container of runnerd's on one volume and a running container of someone else's on another. Docker refuses to
  remove the second; `cache::empty_volumes` removes the leftover and then its volume, and returns the second as
  refused, naming the container, without waiting or touching it.

`what_runners_wrote_unsynced_is_emptied_after_a_crash_and_only_then` in
`crates/core/src/scheduler/engine/tests/unsynced.rs` runs the engine on the fakes through generated sequences of runner
starts, jobs taken and ending, stops, daemon deaths, orderly reboots with runners busy, crashes, a crash after a stop
that left runners writing, reloads and restarts of `skip_syncs`, images with and without the filter, a volume another
container holds, with the daemon dying meanwhile, and a leftover container of runnerd's on a slot's volume. Each of
these breaks it: ignoring the journal, trusting it always, asking an image without the filter, and leaving runnerd's
leftovers in place. The cases after it take the file's contents, a missing boot ID, a refusal that never ends and an
outage one at a time. `boots`'s own tests read the journal's lines of an orderly poweroff, a crash before and during
the sync, and a sync that timed out.

## Consequences

- Runners write less: in the measured load, about a fifth fewer bytes and half the write requests of their own. The
  btrfs commit rate did not measurably change in that load.
- A host crash costs every slot and the shared pnpm and Maven stores a cold start. An orderly shutdown or reboot
  costs nothing, on a host whose journal keeps past boots where runnerd's user can read them.
- A program that relies on `fsync` to see a write error, or on its `EBADF`, does not see it. `O_SYNC` and `O_DSYNC`
  writes still sync; io_uring, whose `fsync` the filter cannot see, is blocked in a sysbox runner, whose profile is
  Docker's default with sysbox's own allowlist, neither of which allows it; a privileged `unsafe-dind` runner has no
  profile.
- `runnerd doctor`, and so the dashboard's config view, words what core knows in a check, `runner syncs`
  (`config::Host::syncs_check`): which profiles' images carry the filter and so skip, which lack it and sync, which
  Docker does not have yet, and what a crash empties. It reads the journal too (`boots::past_boots`): a host whose
  journal keeps no past boot, or that runnerd's user cannot read, gets a warning that every boot empties the caches,
  with the fix. It does not say whether a runner's filter is in: the wrapper says that only on the container's output.
