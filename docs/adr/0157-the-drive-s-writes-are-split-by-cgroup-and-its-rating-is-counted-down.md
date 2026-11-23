# 0157. The drive's writes are split by cgroup, and its rating is counted down

Status: **Accepted** (2026-11-23). Adds `api::DriveOwners`, `DriveShares::owners`, the `ShareOwner` variants
`Docker`, `Containerd`, `System`, `User` and `Kernel`, `observe::disk::Window::owners`, schema 31
(`disk_windows.owners`), `[host.drive]` (`config::Drive`, `config::DriveWritten`, `config::DriveBytes`),
`api::Endurance`, `DiskWrites::endurance`, `insights::DriveRead`, `Store::drive_read_since` and
`InsightKind::DriveWear`.

## Context

[ADR 0143](0143-a-jobs-disk-writes-are-its-containers-io-counters.md) shares the drive's bytes between runner jobs,
the rest of the runner slice, runnerd and "no owner runnerd knows". On the owner's host that last share was 68%. The
unprivileged `io.stat` files already split it (2026-11-21 measure over 34 h): no cgroup 57%, `system.slice` 11%
(`docker.service` 9.7%, `containerd.service` 0.9%, journald 0.4%), `user.slice` apart from runnerd under 1%. The
writes no cgroup sent averaged 16.0 to 16.6 KiB, btrfs's node size; btrfs writes its tree blocks through an inode
of the root cgroup, whoever forced the commit.

The same drive is rated for 600 TB written and had 77.2 TB after 328 power-on hours, so at the measured 4 to 5.7
TB a day its rating lasts months. Nothing in runnerd said so.

## Decision

- **Split by cgroup in each window.** Beside the slice, each reading takes the root cgroup's `io.stat` line for the
  disks (every write to them), each top-level cgroup's, `system.slice/docker.service`'s, `containerd.service`'s and
  runnerd's own (`/proc/self/cgroup`). The root's less its children's is "no cgroup (the kernel)", with its write
  count for the average size. The slice, Docker, containerd and runnerd come off the top-level cgroup they sit in:
  what is left of `user.slice` is "other user programs", of every other top-level cgroup "other system services".
  Known writers that nest in each other are dropped from the later one, so no byte counts twice. Reads go writers,
  then top-level cgroups, then the root, so a counter that rose meanwhile never takes a difference below 0.
- **Each cgroup's rise alone.** A reading keeps the raw counters, and a window adds each cgroup's rise before it
  subtracts. A restarted service's cgroup starts again at 0 while its parent keeps what it wrote; differences of
  lifetime counters moved that lifetime into "other system services" in one tick. A cgroup made anew rises from 0, a
  gone one rises by nothing, its bytes already in the root's.
- **The physical drive's lines, also on a loop.** The owner's Docker root is an ext4 in a file on btrfs, on
  `loop0`. Runner jobs' figures stay the loop's lines, what they wrote to that file system, but the split reads the
  drive's: runnerd, the services and btrfs write the drive, not the file system in the file, and the 16K inference
  sets the drive's no-cgroup writes against the drive's btrfs commits. The loop's writes to its backing file reach the
  drive on the writer's account only some of the time; the rest is in "no cgroup".
- **Missing is absent.** No root `io.stat` or a top-level cgroup without one (cgroup v1, no io controller) leaves the
  reading unsplit; a missing `docker.service` or `containerd.service` adds nothing for that tick, its bytes in the
  other services, and its share is absent only when no window had it. One tick without it, as while it restarts,
  leaves what it wrote before in its share. A window with an unsplit reading is unsplit, like its other counters.
- **runnerd's share is its cgroup's once split.** `/proc/self/io`'s `write_bytes` counts bytes before compression
  and the cgroup's after; subtracting one from the other undercounts the user programs, so a split stretch uses the
  cgroup's and keeps `daemon` for unsplit ones.
- **Stored as JSON** in `disk_windows.owners`, NULL for windows 0.1.5 wrote and for unsplit ones. Text this build
  cannot read is not split, never a failed read.
- **Shares over the split windows.** Insights share the drive's bytes over the windows that read the slice and were
  split, when there are any, else over those that read the slice, else all, and say over which stretch. After an
  upgrade the first windows are split and the older ones are not, so the shares cover the new windows until the old
  ones age out. A stretch that is not split says "not split by cgroup in these windows".
- **Metadata as an inference.** On btrfs, when the writes no cgroup sent average 16K to within a sixteenth, the drive
  section adds that they are likely file-system metadata, "inferred from the size, not measured". A trace could tell;
  runnerd has none unprivileged.
- **Endurance from the config.** Unprivileged runnerd cannot read the SMART log, and sysfs exposes none of it
  (`/sys/block/nvme*/device/` holds the model, serial and firmware; hwmon the temperature). So `[host.drive]` names
  the disk, its rating and, optionally, `written = { bytes, at }` from `smartctl`. runnerd adds what it read the disk
  write in the windows that started after `at` (`Store::drive_read_since`): a floor, since a daemon that did not run
  counted nothing, and the line says over how much of the time since. Rates are over the last 7 days' and 24 hours'
  windows of that disk only, per second runnerd read it, so a move of Docker's root to another disk drops the old
  one's windows instead of charging them to the new one. A blank device and a rating of 0 are refused.
- **Shown whenever rated.** The endurance section shows on a day without jobs or windows too, beside its warning.
- **Decimal units, said.** Drive makers and SMART count in decimal TB. runnerd's `ByteSize` `T` is 2⁴⁰ bytes, 10% more,
  so `[host.drive]` takes `KB` to `PB` (`DriveBytes`) and refuses `600T`. Every endurance line is worded in decimal
  units and the first says so.
- **A warning insight.** Past its rating, or reaching it in fewer than `warn_days` (default 180) at the week's rate,
  `drive-wear:<device>` is a warning insight, which attention and notifications already carry. An older client reads
  the kind as its fallback and draws it by its sentence (accepted in `wire-compat`). Past its rating it claims no
  days left, and its notification says it passed its rating. The digest's look 24 hours back counts neither a SMART
  count given since nor the windows that closed since, so a drive that crossed its rating today is new in it.

## Consequences

- The drive's line names the 68% for what it is. Its parts add up to the drive's bytes; what neither the root nor the
  writers counted, as a loop device's overhead, stays "no owner runnerd knows".
- A release before this one refuses the database once this one migrates it to schema 31, as every schema step does.
- The days left move with the SMART count the owner pastes; runnerd never reads the drive's own counter.
