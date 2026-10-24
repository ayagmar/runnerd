---
title: Upgrading
description: What changes when you upgrade runnerd, and what to do before and after, release by release.
---

Upgrade the daemon, `runnerctl` and the runner images together: the images' scripts and the daemon change in step, and `runnerd doctor` warns when a profile's image is from another version.

## 0.0.10 to 0.0.11

### Before

The first start of 0.0.11 migrates the job history to version 12, and 0.0.10 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-2), into a directory such as `~/runnerd-0.0.10`.

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

The first start of 0.0.10 migrates the job history to version 11, and 0.0.9 refuses a database that new. To keep a way back, copy the database while runnerd is stopped, as in [0.0.7 to 0.0.8](#before-2), into a directory such as `~/runnerd-0.0.9`.

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
