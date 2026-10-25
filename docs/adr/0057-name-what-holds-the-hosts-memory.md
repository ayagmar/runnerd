# 0057. Name the programs that hold the host's memory, by name only

Status: **Accepted** (2026-10-25). Extends [0055](0055-admission-follows-the-hosts-memory.md).

## Context

A job waiting for the host's memory said "programs outside CI hold 30G". On 2026-10-25 the owner still had to run
`ps` to learn that an Android emulator (6G) and two Gradle daemons (9G) held it, all in their terminal's cgroup.
They can act on it ("stop the emulator") only once they know which programs those are.

## Decision

- Every 30 s, not every 2 s sample, the sampler lists `/proc`, skips each process whose `/proc/<pid>/cgroup` is the
  slice or under it, and sums `RssAnon` and `RssShmem` from `/proc/<pid>/status` by the process's name. Those two
  are what the host can't drop as page cache, the same memory `HostMemory::outside` counts. `RssFile` is left out,
  and so is PSS: `smaps_rollup` walks every mapping under the process's memory lock, which is too costly to do
  for every process on the host.
- The three largest programs that hold 512M or more are `HostMemory::holders`. The wait reason, the host line every
  client shows and `runnerd doctor`'s host-memory warning name them: `programs outside CI hold 30G, most java 9.3G
  and qemu-system-x86 5.8G`.
- Only the name leaves `observe::metrics`: the kernel's `comm`, as `status` escapes it, without control
  characters. A command line, its arguments, the environment or a path never does, since the dashboard can be
  reached from other origins (`[web] origins`).
- A daemon before this sends no `holders`, and a client before this ignores them.

## Consequences

- The owner sees what to close without leaving runnerd.
- Shared memory that several processes map counts for each of them, and the kernel's own memory and tmpfs belong to
  no process, so the figures say what to close; they don't add up to `outside`.
- A process can name itself anything up to 15 bytes, so a name says what a program calls itself, not what it is.
