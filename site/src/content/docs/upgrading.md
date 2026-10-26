---
title: Upgrading
description: What changes when you upgrade runnerd, and what to do before and after, release by release.
---

Upgrade the daemon, `runnerctl` and the runner images together: the images' scripts and the daemon change in step, and `runnerd doctor` warns when a profile's image is from another version.

## 0.0.17 to 0.0.18

### Before

The first start of 0.0.18 migrates the job history to version 14, and 0.0.17 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-4), into a directory such as `~/runnerd-0.0.17`.

### Upgrade

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.18/install.sh | sh -s -- --version v0.0.18
```

Move each profile's published `image` to its `-0.0.18` tag, then check the file and start runnerd:

```sh
runnerd check-config
systemctl --user start runnerd
runnerctl doctor
```

### What changes on its own

- **A job reserves memory from its last runs, not its last days.** A job that got lighter reserves its new peaks within about 20 runs, and a hold at its cap ends after 3 runs in a row that stayed clear of it, instead of lasting 7 days.
- **A cap is raised only when a container's own limit ran out of memory** (`memory.events.local`). A peak near the cap that the kernel never stopped, as a JVM's heap grows to the room it has, no longer raises it, and neither does a limit an inner container hit.
- **Auto sizing lowers `memory` from a longer window of each job's runs** and takes a lowering back to the cap it came from when a run is killed at the new cap, or when a job that passed before fails 3 runs in a row on two branches. Runs recorded before the upgrade can hold a cap up, but never raise one.

## 0.0.16 to 0.0.17

The job history stays at version 13, so either version reads it. Install 0.0.17, move each profile's published `image` to its `-0.0.17` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.17/install.sh | sh -s -- --version v0.0.17
runnerd check-config
systemctl --user restart runnerd
runnerctl doctor
```

Going back to an older image after 0.0.17's has seeded a profile needs that profile's snapshot and slot volumes removed (`docker volume rm runnerd-<profile>-base runnerd-<profile>-slot-1 …`): older images do not read the files a 0.0.17 snapshot keeps beside it.

### What changes on its own

- **Auto sizing moves `cpus` on each job's average CPU over its runs, not on its CPU peaks.** A parallel build that spikes to any cap no longer raises it. A profile whose jobs all average far under their cap comes down by at most `max_step` a day, toward twice its heaviest job's p95 average, so a profile the old rule raised steps down over the following days. A raise the slice or the CPU budget cannot hold is not written, and `runnerctl sizing`, the TUI and the dashboard say why.
- **A job cancelled before any runner took it reads `cancelled in queue`**, with no start or run time, instead of `elsewhere`. Jobs recorded before the upgrade keep their old wording.
- **The runner's JIT config is no longer in any process's arguments**, so other users of the host cannot read it with `ps`.
- **A reseed that did not finish starts the profile over from the image**, and a reseed keeps the image files a job deleted hidden, instead of bringing them back.

## 0.0.15 to 0.0.16

The job history stays at version 13, so either version reads it. Install 0.0.16, move each profile's published `image` to its `-0.0.16` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.16/install.sh | sh -s -- --version v0.0.16
runnerd check-config
systemctl --user restart runnerd
runnerctl doctor
```

### What changes on its own

- **A snapshot over its `snapshot_max` starts over from the image**, and every slot's changes go with it. Before, it started over from slot 1's changes alone, which miss the files slot 1's jobs only read: a cache's index could name content that was gone, and every Gradle or npm job of the profile failed. Every slot of a profile whose snapshot starts over now runs its next job cold. The limit is checked against the snapshot the reseed would make, so this happens less often.
- A profile whose jobs fail on missing cache files since such a reseed keeps failing after the upgrade. Stop runnerd, remove the profile's snapshot and slot volumes (`docker volume rm runnerd-<profile>-base runnerd-<profile>-slot-1 …`) and start it again.

## 0.0.14 to 0.0.15

The job history stays at version 13, so either version reads it. Install 0.0.15, move each profile's published `image` to its `-0.0.15` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.15/install.sh | sh -s -- --version v0.0.15
runnerd check-config
systemctl --user restart runnerd
runnerctl doctor
```

### What changes on its own

- **The budget's free figure is what admission would start.** When the host leaves the slice less than `MemoryMax`, the free figure and the bar's free block show what the host leaves, and the rest of the bar is marked *held by the host*. While the slice is past 90% under measured admission, or the host stalls, nothing shows as free and the bar says why. The host line in status, the TUI and the dashboard ends with what is free.

### New, off until you set them

- [Discord notifications](../insights/#in-a-discord-channel): a `[[notify.targets]]` entry with `kind = "discord"` and the channel webhook's URL. To go back to 0.0.14, remove those targets first: 0.0.14 refuses a config with them.

## 0.0.13 to 0.0.14

The job history stays at version 13, so either version reads it. Install 0.0.14, move each profile's published `image` to its `-0.0.14` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.14/install.sh | sh -s -- --version v0.0.14
runnerd check-config
systemctl --user restart runnerd
runnerctl doctor
```

Upgrade `runnerctl` with the daemon: a 0.0.13 client cannot read a status that lists a job no profile serves.

### What changes on its own

- **A queued job that no repository entry's labels match is listed**, waiting with *No profile serves the label …*, and so is a job that asks for no labels. Before, it was left out. Jobs asking for one GitHub-hosted label, such as `ubuntu-latest`, are still left out.
- **A job waiting for the host's memory names what holds it**: `programs outside CI hold 30G, most java 9.3G and qemu-system-x86 5.8G`. The host line in status, the TUI and the dashboard names them too, and so does `runnerd doctor`'s warning.
- **The failed figure turns red only when a runner failed its job**: lost with it, or killed for memory at its cap. `GET /v1/activity` gains `runner_failed`.
- **Under `admission = "measured"`, `check-config`, the doctor and the config editor hold the largest runner and the mirror to `MemoryMax`**, as admission does, instead of every slot at its cap.
- **A job's memory and CPU charts draw what admission reserved** beside the cap.
- **A restart keeps a running job whose slot, profile or repository the config no longer has**, and lets it drain, as a reload does.
- **Admission stays inside its budgets when a job takes a warm runner's room.** A job stops another pool's warm runner only where that runner's reservation covers what the job needs, on the slice, its CPUs and the host; otherwise it waits.
- **A reseed's seed container waits for room** instead of starting past `MemoryMax` or the host's limit. After `runnerctl reseed`, the drained profile holds its seed's room so other pools' jobs don't take it, as long as the host could ever fit it.

### New, off until you set them

- [Several profiles for one repository](../configuration/#split-light-and-heavy-jobs): repeat its `[[repositories]]` entry with another `profile` and a label of its own, and point light jobs' `runs-on` at that label. To go back to 0.0.13, remove the extra entries first.

## 0.0.12 to 0.0.13

### Before

The first start of 0.0.13 migrates the job history to version 13, and 0.0.12 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-4), into a directory such as `~/runnerd-0.0.12`.

### Upgrade

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.13/install.sh | sh -s -- --version v0.0.13
```

Move each profile's published `image` to its `-0.0.13` tag, then check the file and start runnerd:

```sh
runnerd check-config
systemctl --user start runnerd
runnerctl doctor
```

### What changes on its own

- **Admission follows the host's memory.** New runners start only while the host has room for them beside everything else it runs, and not while it stalls short of memory. Waiting jobs say "The host is short of memory"; running jobs are never stopped, and these waits don't count as demand for more slots ([The host's memory](../admission/#the-hosts-memory)). `runnerd doctor` has a `host memory` check.
- **Caps keep at least 1G above a job's largest peak** for page cache, so its downloads aren't throttled at its cap. Under `[sizing] auto`, small jobs' profiles grow their `memory` over the next looks.
- **A job's page and `runnerctl job` count the times the kernel throttled its downloads for memory pressure**, and the Memory pressure insight warns when it happens to a repository again and again.
- **After a restart, runners runnerd adopts keep the CPU and memory they reserved and their uptime.** Waiting runners whose labels, limits or image a reload changed are replaced; for an image, once the new one is pulled, so the old runners serve meanwhile.

## 0.0.11 to 0.0.12

The job history stays at version 12, so either version reads it. Install 0.0.12, move each profile's published `image` to its `-0.0.12` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.12/install.sh | sh -s -- --version v0.0.12
runnerd check-config
systemctl --user restart runnerd
runnerctl doctor
```

### What changes on its own

- **Each slot keeps its Docker images and build cache** between jobs, which needs the 0.0.12 images. Each slot's first job after the upgrade still starts cold. The slots' Docker stores together take at most a quarter of the disk not otherwise used, so `runnerctl caches` shows larger slot volumes than before ([Warm caches](../running/#warm-caches)).
- **The volumes of slots past a profile's slot count** are removed at the next moment no job runs, such as `node-slot-9` once `node` has 8 slots.
- **A snapshot over its `snapshot_max`** starts over at its profile's next idle moment.

### New, off until you set them

- `build_cache_writers = "pull-requests"` under a repository lets pull requests from its own branches, opened by people, write to the [build cache](../running/#build-cache); forks and bots still only read.

## 0.0.10 to 0.0.11

### Before

The first start of 0.0.11 migrates the job history to version 12, and 0.0.10 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-4), into a directory such as `~/runnerd-0.0.10`.

### Upgrade

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.11/install.sh | sh -s -- --version v0.0.11
```

Move each profile's published `image` to its `-0.0.11` tag, then check the file and start runnerd:

```sh
runnerd check-config
systemctl --user start runnerd
runnerctl doctor
```

Jobs reach the build cache only from the 0.0.11 images; an older image ignores it.

### What changes on its own

- **The [build cache](../running/#build-cache)** is on, up to 10G in `~/.cache/runnerd/build-cache`. Gradle builds that turn on their build cache (`org.gradle.caching=true`) and set no remote cache of their own share what the default branch built, whichever slot built it; sccache and Bazel get it too. Only pushes, schedules and manual runs of the default branch write. Set `build_cache = false` under a repository to keep it out, or `[caches.build] enabled = false` to turn it off.
- Each job that may write costs up to three GitHub reads, once per job: its run, the branch's head, and a compare of the two.
- The job page, the TUI and `runnerctl job` show each job's build cache hits; the Caches page shows the cache's size and hit rate. A new insight, Cold cache, names jobs whose writing runs find almost nothing in it.
- The "slower" insight no longer reports a job that only flips between build cache hits and misses.

## 0.0.9 to 0.0.10

### Before

The first start of 0.0.10 migrates the job history to version 11, and 0.0.9 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-4), into a directory such as `~/runnerd-0.0.9`.

### Upgrade

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.10/install.sh | sh -s -- --version v0.0.10
```

Move each profile's published `image` to its `-0.0.10` tag, then check the file and start runnerd:

```sh
runnerd check-config
systemctl --user start runnerd
runnerctl doctor
```

### What changes on its own

- **Warm runners** are on: each active repository keeps one idle runner registered, so its next job starts without waiting for one. A queued job takes back the room a warm runner holds. Set `warm = 0` under a repository to turn it off ([Warm runners](../running/#warm-runners)).
- **Auto sizing**, where `[sizing] auto` is on, now raises within the hour (`raise_interval`) and lowers no sooner than `interval`. It changes `cpus` too, and adds slots while they fit `MemoryMax`, so `max_slots` is optional. Keep `max_slots`, or set a long `raise_interval`, to hold the old behaviour ([Auto sizing](../admission/#auto-sizing)).
- **The shared caches and the mirror** are checked at most hourly on a busy day, at the first moment no job runs; idle warm runners stop for it.
- **`runnerctl config show`** and the dashboard's config editor show notification URLs and header values as `<secret>`. Back up `runnerd.toml` itself, not that output.

### New, off until you set them

- [Notifications](../insights/#notifications) to ntfy or a webhook under `[notify]`: failed and lost jobs, runners that cannot start, new insights, a daily digest.
- `runnerd init` writes `[sizing] auto = true` for new configs; existing configs are left as they are.

## 0.0.8 to 0.0.9

The job history stays at version 9, so either version reads it. Install 0.0.9, move each profile's published `image` to its `-0.0.9` tag, and restart runnerd:

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.9/install.sh | sh -s -- --version v0.0.9
systemctl --user restart runnerd
```

Browsers signed in to the dashboard sign in once more; from then on they stay signed in across restarts ([From another device](../clients/#from-another-device)).

## 0.0.7 to 0.0.8

### Before

The first start of 0.0.8 migrates the job history to version 9, and 0.0.7 refuses a database that new: it stops with `the job history database is version 9, newer than the 7 this runnerd reads`. To keep a way back, copy the database while runnerd is stopped.

The database is the file [`history.database`](../configuration/#history) names in the config the service runs, `~/.config/runnerd/runnerd.toml` unless the unit's `--config` names another. Look for it first:

```sh
grep -E '^\s*database\s*=' ~/.config/runnerd/runnerd.toml
```

When that prints nothing, the database is the default, and these copy it with its `-wal` and `-shm` files:

```sh
systemctl --user stop runnerd
mkdir -p ~/runnerd-0.0.7
cp -a "${XDG_STATE_HOME:-$HOME/.local/state}"/runnerd/runnerd.db* ~/runnerd-0.0.7/
ls ~/runnerd-0.0.7
```

When it prints a path, copy that file and the `-wal` and `-shm` files beside it instead. Go on only once the copy lists `runnerd.db`, or the file `database` names. The copy is the history up to the upgrade; whatever 0.0.8 records after it stays in the new one.

### Upgrade

```sh
curl -fsSL https://github.com/ayagmar/runnerd/releases/download/v0.0.8/install.sh | sh -s -- --version v0.0.8
```

That installs 0.0.8 even after a newer release: this page's steps are for 0.0.8 and its images.

Move each profile's published `image` to the new tag, such as `ghcr.io/ayagmar/runnerd-runner:jvm-0.0.8`, then check the file and start runnerd:

```sh
runnerd check-config
systemctl --user start runnerd
runnerctl doctor
```

runnerd pulls the new images before the profiles' first runners, then removes the 0.0.7 ones, since no profile names them ([Runner images](../configuration/#runner-images)). It leaves images built on this host: rebuild a `runnerd-runner:<stage>` from a 0.0.8 clone, and remove the old one yourself.

### Runner images

- `base` no longer has a JDK or the Android SDK. They are in a new stage, `android`, which `jvm` and `node` build on, so those two keep them. A profile on `base` that builds Android apps moves to `android`, and an image of your own built `FROM` `base` for Android builds starts from `android-0.0.8` instead.
- Rust, Go, Bun and Yarn keep their homes and caches in `~/.cache` now, so the slot keeps them warm. Each slot's first such job after the upgrade still downloads them.

### Memory reads lower

A job's memory leaves out page cache now ([What a job's memory is](../admission/#what-a-jobs-memory-is)), so its peaks, the dashboard's and the TUI's memory charts read lower than before, and lower than `docker stats`. The slice's chart reads as before.

- **Sizing advice** still counts peaks 0.0.7 recorded, which read higher, until they leave the 30-day window. Its advice errs toward more memory until then.
- **Measured admission** stops predicting from those peaks. Until a profile has `[sizing] min_jobs` runs recorded by 0.0.8, its jobs reserve their caps, as with `admission = "caps"`, so expect longer waits for the first few days; then a job without new runs of its own reserves its profile's largest peak plus the margin. A lower `min_jobs` shortens that, and makes advice and predictions from fewer runs. A 0.0.7 run killed for memory still puts its job on its cap for 7 days.

### Caches get limits

Two limits now apply without a change to the config ([`[caches]`](../configuration/#caches)):

- `snapshot_max`: a profile's snapshot over it starts over at its next reseed, from slot 1's changes alone, and the profile's jobs run cold once for what slot 1's last job did not touch. A snapshot already over it stays until that reseed.
- `mirror.max_size`: a mirror over it is emptied the next time no job runs, and pulls from Docker Hub again.

Set either higher in `runnerd.toml` to keep a larger snapshot or mirror.

### New, all off until you set them

- [`cpu_overcommit`](../admission/#cpu-admission) under `[host]`: runners also wait for CPU.
- [`[sizing] auto`](../admission/#auto-sizing): runnerd applies the sizing advice itself, within `min_memory`, `max_memory` and `max_slots`.
- [`[web] origins`](../clients/#from-another-device): the dashboard takes writes through a proxy such as `tailscale serve`. runnerd reads it when it starts, so restart it after adding one.
