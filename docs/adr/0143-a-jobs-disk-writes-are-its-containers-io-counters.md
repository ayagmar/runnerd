# 0143. A job's disk writes are its runner container's io counters, read to its end

Status: **Accepted** (2026-11-15).

## Context

The owner's system SSD writes several terabytes a day under CI. An outside sampler that read cgroup counters every 10
seconds put numbers on it, and its gaps showed what a figure per job must not do: a container removed between two
reads lost its last seconds, a short job vanished between reads, and the bytes written after a container ended were
nobody's. Summed, the jobs it caught came to 79% of the runner slice's writes, and the slice to a third of the
drive's.

runnerd samples every runner's cgroup every 2 seconds already (`observe::metrics`) and keeps the samples in the job
history. A runner container's cgroup holds everything the job runs: under sysbox and under `unsafe-dind` alike, the
Docker Engine inside it puts its containers and BuildKit's in cgroups below the runner's
(`docker-<id>.scope/docker/<inner id>`, read on the owner's host), and v2's `io.stat` counts a cgroup's descendants.

## Decision

- Each sample of a runner's container also reads `io.stat`'s `wbytes` and `wios` and `memory.stat`'s `file_dirty` and
  `file_writeback` (`samples.written`, `write_ios`, `dirty`).
- Only the disks under Docker's root count (`observe::disk::Disks`): the engine asks Docker for its root once it
  answers and the sampler finds the mount that holds it in `/proc/self/mountinfo`; a partition counts as its whole
  disk, as `io.stat` keys it, and a btrfs file system as each of its devices (`/sys/fs/btrfs/<uuid>/devices`). Runners
  write their layers, slot volumes and shared caches there. Other lines of `io.stat`, such as zram's swap, never reach
  the drive. A device-mapper volume counts as itself: its disks' lines would count the same bytes again.
- A job's figure is its container's counters from its last sample before the job's start to its container's last
  sample. The counters are the cgroup's own, so a daemon that stops and starts again mid-job loses nothing and counts
  nothing twice; a warm runner's writes before its job are not the job's. Without a sample before the start, the
  figure counts from the container's start.
- It is `complete` when that baseline was read, the counters never fell, from the baseline on, and the sampler first
  found the cgroup gone after the last sample and within two samples (4 seconds) of it (`runners.cgroup_gone_at`,
  which keeps the first time): the container ran at most that long unread. The sampler finds that whether or not the
  engine still tracks the runner, since the engine lets go of it as soon as Docker says it stopped. A job closed
  before that pass gets its figure again when it comes. Anything else is `partial`, a floor: an end while no daemon
  ran, a pass that stalled or a container no sample read for a while, a counter that fell because the container was
  started again (each run of the counter is added; a restart whose new counters passed the old ones by the next read
  shows as a sample after the cgroup was first found gone), or a baseline sample taken before the disks were known.
  A host without the io controller, cgroup v1 or a sysfs runnerd cannot read has no figure at all, never 0.
- The pages still dirty at the last sample are kept beside the figure: written after the container ends, unless
  deleted first, to the runner slice's account.
- The sampler also reads, each pass, the disks' sectors written (`/sys/class/block/<disk>/stat`), the runner slice's
  `io.stat` (which keeps what its ended containers wrote), the daemon's own `write_bytes` in `/proc/self/io`, and
  btrfs's commit count, and adds them up into windows of up to 10 minutes (`disk_windows`, pruned with job records).
  A daemon's stop stores the window it is in: the engine lets go of the sampler and waits for it before the daemon
  closes its history. The time no daemon ran is in none. A figure a reading lacks leaves the window's absent, and
  insights share the drive's bytes out over the windows that read the slice, so one missed read costs only its
  window.
- The daemon's own writes are a line of the drive's shares, so "no owner runnerd knows" leaves runnerd out. They are
  what it sent to any disk, its history, logs and build cache among them; on a host whose history is on another
  disk than Docker's root they are shown against a drive they did not reach, capped at what the drive's other shares
  leave.
- `runnerctl insights`, the TUI and the dashboard show what the window's jobs wrote (median, p90, total), the
  workflows and jobs that wrote most, and the drive: what it wrote, a day at that rate, runner jobs' share, the rest
  of the runner slice's, the share with no owner runnerd knows, and commits a second. Core words all of it
  (`api::DiskWrites`); a job's figure is `api::DiskWritten`, in the job tables as `Written`.

## Consequences

- The figure is block I/O the kernel charged to the job's cgroup: what reached the disk, not what the job asked to
  write. A file written and deleted before write-back costs nothing; btrfs's copies of its metadata and its commits
  are the file system's, outside any job.
- What a container writes after its last sample, its shutdown and its pages written back late, is not the job's
  figure. It shows as the runner slice's share beside the jobs', so it is neither lost nor spread over jobs.
- `samples` gains three columns and `runners` one; at one sample every 2 seconds per runner this is a few bytes per
  sample. The drive's windows are 144 rows a day.
- Schema 24. A runnerd before it refuses the database as newer, as every schema change does (ADR 0073).
