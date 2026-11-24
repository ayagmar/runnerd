# 0170. A runner container skips work its job does not need

Status: **Accepted** (2026-11-24). Amends [0054](0054-slot-docker-stores-share-the-disk.md),
[0089](0089-a-slots-cache-mounts-go-last.md), [0156](0156-slot-docker-shares-follow-what-each-profiles-stores-hold.md).
Adds the profile key `tmp_tmpfs`, `config::TMP_TMPFS_MIN`, `RunnerSpec::tmp_tmpfs`, `docker::tmp_tmpfs` and the
runner container label `runnerd.tmp-tmpfs`.

## Context

Three things every runner container does cost more than the job needs:

- **The capture forks to wait.** The script that follows a running job's log (`observe/capture.rs`) runs in every
  runner container and passes over `_diag` five times a second. Each pass ended in `sleep 0.2`, and each page file it
  looked at went through a command substitution: at least one process, often three, per container per pass. With 20
  runners that is over 100 processes a second that do nothing.
- **The Docker cleanup does work the cache does not call for.** `job-cleanup.sh` runs from the job-completed hook and
  again when the slot's next container starts. Each run makes three `docker builder prune` calls and walks the slot's
  Docker store with `du` once or twice, whether or not the job built anything.
- **`/tmp` is on disk.** What jobs write to `/tmp` and `/var/tmp` goes to the container's writable layer on the
  host's disk, and is deleted when the container goes. Compilers, test runners and package managers put large,
  short-lived files there.

## Decision

- **The capture waits with `read -t 0.2` on a pipe it holds both ends of** (`exec {tick}<> <(:)`, once): bash's
  builtin, no process, and never an end of file, which would turn the wait into a busy loop. Where the pipe cannot be
  opened, as without `/dev/fd`, the script chooses `sleep 0.2` once, quietly, instead of failing a `read` on every
  pass. Page numbers come from a function that sets a variable instead of a command substitution. The output and the
  resume protocol are unchanged. Not `python3`, which would hold about 10 MB in every container and is not something
  a custom image must have.
- **The cleanup still runs when a container starts.** A marker of a finished cleanup, letting the next container
  skip its own, was weighed and dropped: the hook runs before the entrypoint ends the job's last processes, so a
  process of the job's can change the store after the marker; a container of an older image neither writes nor
  removes it; and a host crash can leave a marker the next container should not trust. Proving that the slot's
  immediately preceding runner wrote it, after its processes ended, and removing it durably before the next job, is
  more than the start-up time it saves.
- **The cleanup prunes only what the build cache holds.** One call, `GET /system/df?type=build-cache` (Engine API
  1.42), given 10 seconds, lists the cache's records. The cleanup prunes build contexts only when some record is of
  that type, git checkouts likewise, and layers to half the budget only when some record is a layer and the records
  no image shares hold at least half the budget. That is what BuildKit weighs against `--max-used-space`, and below
  it the prune removes nothing (`cache/manager.go`, `prune`); records images share still name the types, as
  `prune -af` takes them as candidates. When that layer prune does not run, the store's size the job's report
  measured stands, and the store is not walked a second time. When the Engine does not answer in time, or answers
  without a list of records that each have a type and a size, every prune runs, as before.
- **A store the cleanup cannot measure is reported unmeasured.** A failed `du` leaves no report, which the
  entrypoint prints as `runnerd: docker store unmeasured`, instead of a size of 0 that the Docker shares would read
  as a store holding nothing ([0156](0156-slot-docker-shares-follow-what-each-profiles-stores-hold.md)).
- **A profile may put `/tmp` and `/var/tmp` on `tmpfs`: `tmp_tmpfs = "2G"`, off by default.** The size bounds both
  together: a quarter to `/var/tmp` and the rest to `/tmp`, each down to whole 64 KiB, which every page size Linux
  uses divides, so the kernel's rounding up to a page never takes the two past the bound. Each is mounted
  `rw,exec,nosuid,nodev,mode=1777` through `HostConfig.Tmpfs`, which every Docker API version takes. Docker mounts a
  `tmpfs` `noexec` unless told (`daemon/oci_linux.go`; a later `exec` wins in `MergeTmpfsOptions`), and jobs run what
  they build in `/tmp`, such as `go test`'s binaries. A `Mount` of type `tmpfs` can only say `exec` from API 1.46
  (`TmpfsOptions.Options`).
- **`tmp_tmpfs` is at least `1M` and needs a `min_memory` above it.** Below `1M` the split would not leave whole
  pages; 0 is refused too, as `tmpfs` reads it as no bound. `memory` is never below `min_memory`, and `min_memory` is
  where sizing advice and auto sizing stop lowering it, so neither a hand edit nor an applied advice can leave the
  tmpfs as large as the memory it comes out of, and no advice is one the config check then refuses.
- **A runner keeps the bound it started with**, in its occupant and in its container's label `runnerd.tmp-tmpfs`,
  which a restart reads back. An idle runner whose bound a reload changed, turned on, resized or off, is replaced, as
  one whose memory or labels changed is; a running job keeps its container.
- **Never `_diag` or `_work` on `tmpfs`.** runnerd reads a stopped runner's `Worker_*.log` from `_diag` through
  Docker's archive API, which a `tmpfs` loses when the container stops. `_work` holds the tool cache's layer mount and
  the workspace a failed job's record keeps, and a slot's own pnpm store binds over it
  ([0167](0167-a-slot-keeps-its-own-pnpm-store-beside-the-jobs-workspace.md)).
- **`scrub.sh` is unchanged.** It deletes what changed in `/tmp` and `/var/tmp` since the runner started, never the
  directories: checked on a `tmpfs` mount point in a mount namespace, it empties the job's files and leaves the
  mount and its mode.

## What it does not guarantee

- **`tmp_tmpfs` is memory.** A `tmpfs` page counts against the container's memory limit as shared memory, which the
  kernel cannot reclaim but by swapping, and runners have no swap. A job that fills `/tmp` has that much less memory
  for its processes, and one that needs both is killed for memory instead of filling the disk. This is why the key
  is off by default: the profiles that gain are the ones whose jobs write large temporary files and have the memory
  to spare.
- **A job that fills `/tmp` past the bound fails with "No space left on device"**, where on disk it would have gone
  on. The runner's own temporary directory, `_work/_temp`, stays on disk.
- **The capture still forks for each kept page on each pass**: `stat` reads the page's size, and `tail` and `head`
  copy what grew. Only the wait and the page numbers stopped forking.
- **Only images with this change prune less.** The prune checks live in the image; older images prune everything,
  as before. `tmp_tmpfs` needs no new image.
- **A daemon before this change refuses a config with `tmp_tmpfs`**, as it refuses any key it does not know: remove
  the key before going back to one.

## Not done

- **`cp --reflink=auto` in `seed.sh`.** It is coreutils' default since 9.0 (`cp --help`: "By default or with
  --reflink=auto, cp will try a lightweight copy"), and the image's Ubuntu 24.04 ships 9.4. The reseed copies from
  an overlay view of slot 1's changes over the snapshot to `/base`, which is another file system to the kernel: a
  clone from an overlay file to a file outside it fails with `EXDEV`, and `cp` falls back to a full copy. Cloning the
  snapshot's unchanged files would mean copying `/base/<layer>` directly and replaying the upper's whiteouts by hand,
  which the reseed avoids on purpose.
- **A persistent BuildKit builder per slot.** The slot's Docker Engine already keeps its default builder's cache in
  the slot's store, under the GC policy and the cleanup's budget
  ([0089](0089-a-slots-cache-mounts-go-last.md)). A `docker-container` builder kept between jobs would run as a
  container and keep its state in a volume of that Engine, which the cleanup removes after every job, and its cache
  would sit outside the GC policy written to `daemon.json` and outside `docker builder prune`. Keeping one needs the
  cleanup to spare that container and volume by name, its own prune against the budget, and a way to make
  `docker/setup-buildx-action`, which creates its own builder per job, use it. That is a change of its own.
- **Skipping the cleanup at start**, above.

## Consequences

- The capture makes no process while it waits.
- A cleanup in a slot whose cache holds no build contexts, no git checkouts and less than half the budget makes one
  API call instead of three prunes, and after a job walks the store once.
- A profile with `tmp_tmpfs` writes nothing of `/tmp` and `/var/tmp` to the host's disk.
