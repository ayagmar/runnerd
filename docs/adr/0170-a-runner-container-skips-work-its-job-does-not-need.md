# 0169. A runner container skips work its job does not need

Status: **Accepted** (2026-11-24). Amends [0054](0054-slot-docker-stores-share-the-disk.md),
[0089](0089-a-slots-cache-mounts-go-last.md), [0156](0156-slot-docker-shares-follow-what-each-profiles-stores-hold.md).
Adds the profile key `tmp_tmpfs`, `RunnerSpec::tmp_tmpfs`, `docker::tmp_tmpfs` and the slot file
`/state/.runnerd-cleaned`.

## Context

Three things every runner container does cost more than the job needs:

- **The capture forks to wait.** The script that follows a running job's log (`observe/capture.rs`) runs in every
  runner container and passes over `_diag` five times a second. Each pass ended in `sleep 0.2`, and each page file it
  looked at went through a command substitution: at least one process, often three, per container per pass. With 20
  runners that is over 100 processes a second that do nothing.
- **The Docker cleanup runs twice per job.** `job-cleanup.sh` runs from the job-completed hook and again when the
  slot's next container starts, for a container stopped before its hook. Each run makes three `docker builder prune`
  calls, prunes volumes and networks, and walks the slot's Docker store with `du` once or twice, whether or not the
  job used Docker. The run at start delays the runner's registration, so every job waits for it.
- **`/tmp` is on disk.** What jobs write to `/tmp` and `/var/tmp` goes to the container's writable layer on the
  host's disk, and is deleted when the container goes. Compilers, test runners and package managers put large,
  short-lived files there.

## Decision

- **The capture waits with `read -t 0.2` on a pipe it holds both ends of** (`exec {tick}<> <(:)`, once): bash's
  builtin, no process, and never an end of file, which would turn the wait into a busy loop. A `read` that fails
  rather than times out, as without `/dev/fd`, falls back to `sleep 0.2`. Page numbers come from a function that sets
  a variable instead of a command substitution. The output and the resume protocol are unchanged. Not `python3`, which
  would hold about 10 MB in every container and is not something a custom image must have.
- **The cleanup leaves a marker when it ran to its end.** `job-cleanup.sh` writes the Docker budget it kept the store
  within to `/state/.runnerd-cleaned` once every Docker step succeeded. The entrypoint removes the marker before the
  runner starts. When a container starts, it skips the cleanup only if all of these hold:
  - the marker is there, so the last container that removed it reached the end of a cleanup after its job;
  - the marker's budget is no larger than this container's, so a store kept within it is within this one;
  - the Engine answers and holds no container and no volume. A process of the job's that outlived the
    job-completed hook can start one before the entrypoint kills it, and a container of an image from before this
    change never removes the marker, so a marker alone does not say what that container's job left.
- **The cleanup prunes only what the build cache holds.** One call, `GET /system/df?type=build-cache` (Engine API
  1.42), lists the cache's records. The cleanup prunes build contexts only when there are any, git checkouts likewise,
  and layers to half the budget only when the records no image shares hold at least half the budget: BuildKit counts
  the same records against `--max-used-space` and removes nothing below it (`cache/manager.go`, `prune`). When that
  prune does not run, the store's size the job's report measured stands, and the store is not walked a second time.
  When the Engine does not list its cache, every prune runs, as before.
- **A profile may put `/tmp` and `/var/tmp` on `tmpfs`: `tmp_tmpfs = "2G"`, off by default.** The size bounds both
  together: three quarters to `/tmp`, a quarter to `/var/tmp`. Each is mounted `rw,exec,nosuid,nodev,mode=1777`
  through `HostConfig.Tmpfs`, which every Docker API version takes. Docker mounts a `tmpfs` `noexec` unless told, and
  jobs run what they build in `/tmp`, such as `go test`'s binaries. A `Mount` of type `tmpfs` can only say `exec`
  from API 1.46 (`TmpfsOptions.Options`). The config refuses a `tmp_tmpfs` of 0, which `tmpfs` reads as no bound, and
  one not below the profile's `memory` and `min_memory`.
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
  for its processes, and one that needs both is killed for memory instead of filling the disk. Auto sizing may lower
  a profile's `memory` below `tmp_tmpfs` when `min_memory` is unset; set `min_memory` to keep the bound below it. This
  is why the key is off by default: the profiles that gain are the ones whose jobs write large temporary files and
  have the memory to spare.
- **A job that fills `/tmp` past the bound fails with "No space left on device"**, where on disk it would have gone
  on. The runner's own temporary directory, `_work/_temp`, stays on disk.
- **A skipped cleanup at start can leave a build cache over its budget** when the last container was one of an
  older image and was stopped mid-job, or when a job ran `sudo runnerd-job-cleanup` itself and then built more and
  was stopped. BuildKit's GC policy, which follows the same budget, and the next job's cleanup remove it. Containers
  and volumes are never left: the start checks for them.
- **Only images with this change skip anything.** The marker and the prune checks live in the image; older images
  clean up twice and prune everything, as before. `tmp_tmpfs` needs no new image.

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

## Consequences

- The capture makes no process while it waits; a running container forks for its log only when a page grew.
- A container whose slot's last job ended cleanly starts its runner after two `docker` calls instead of a full
  cleanup. A job-end cleanup of a job that built nothing makes one API call instead of three prunes, and walks the
  store once.
- A profile with `tmp_tmpfs` writes nothing of `/tmp` and `/var/tmp` to the host's disk.
