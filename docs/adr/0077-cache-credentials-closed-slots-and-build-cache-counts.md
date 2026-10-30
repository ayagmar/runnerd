# 0077. Credentials in the cache layer, closed slots' layers, and what the build cache counts

Status: **Accepted** (2026-10-30). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md),
[0035](0035-runner-image-stages.md) and [0052](0052-shared-build-cache.md).

## Context

Four ways a slot's or the build cache's state outlived what it should:

- `MAVEN_USER_HOME` is in the cache layer, and `CACHE_SECRETS` did not list the Maven wrapper's `settings.xml`
  and `settings-security.xml`. A job's registry password there reached the slot's next job and, through a
  reseed, every slot of the profile. Gradle's init scripts and Hugging Face's tokens (`HF_HOME` defaults to
  `~/.cache/huggingface`) were missing too. Podman, Buildah and Skopeo keep logins under `XDG_RUNTIME_DIR`, which the
  image puts on the pnpm volume that every slot of every profile mounts.
- A reseed mounts and clears only slots `1..=slots`. A slot a lower `slots` closed kept its layers' changes; when
  `slots` grew again before the caches' check removed its volume, or Docker refused to remove it, those changes
  came back over a snapshot they were not made against.
- The build cache was trimmed in the background task of the shared caches' check, after Docker measured every
  volume, which can take minutes. A job could start meanwhile and lose entries mid-build. The trim also unlinked
  its victims after releasing the lock, so it could delete a file an upload had just been renamed to.
- Uploads were bounded by bytes only. An empty upload reserved nothing, yet made a file, an index entry and, while
  its job's writes were undecided, a held upload. The directories entries were made in were never removed.

## Decision

- `CACHE_SECRETS` adds `m2/settings.xml`, `m2/settings-security.xml`, `gradle/init.d`, `gradle/init.gradle`,
  `gradle/init.gradle.kts`, `huggingface/token` and `huggingface/stored_tokens`. An entry may be a directory: it goes
  whole, and `seed.sh` makes no whiteout for anything under one. `REGISTRY_AUTH_FILE` moves container logins to the
  container's own disk. Credential files in `$HOME` and `~/.config` (`.npmrc`, `.netrc`, `.docker/config.json`,
  `~/.config/gh`, pip's config, `.git-credentials`, Yarn's and pnpm's rc files) are not listed: that disk goes with
  the container after its one job. pub's `credentials.json` is not listed either: pub keeps credentials in its
  config directory and no longer reads `PUB_CACHE` for them.
- What a job writes elsewhere in the cache layer, in the tool cache, in the pnpm store or in the Maven repository
  reaches later jobs of other repositories. The configuration docs call cache sharing a trust boundary.
- Each change to a snapshot writes a new value to `/base/.generation`, inside the window `/base/.reseeding` marks.
  `mount_layers` compares it with the slot's `/state/.generation` (a missing file reads as 0), and on a mismatch
  removes the slot's layers, never its Docker store, before writing the new value. No Docker call lists or resets
  closed slots: the slot resets itself the next time a container mounts it. A value is never deleted, so an old
  slot with no value never matches a snapshot that has one.
- The build cache is trimmed in the tick, at the first moment no runner runs, before the shared caches' check
  starts in the background. Runners start only in the tick, so none starts during the trim; commands are answered
  meanwhile and none of them starts a runner. The trim unlinks each victim while holding the lock, and only while
  the index still lacks it, then removes the directories that leaves empty, down to `<owner>/<name>/<tool>`.
- Each entry and upload counts at least 4 KiB against `max_size` (`MIN_CHARGE`): in an upload's reservation,
  taken before its file is made, in `store`'s check against twice the limit, and in the trim. Reported sizes stay
  real bytes. A runner holds at most 10,000 undecided uploads and is served at most 256 connections at once;
  further connections wait to be accepted. An upload keeps the limit it started with, so a lower `max_size`
  holds back only the uploads that start after the reload. None of these is a config key.

## Consequences

- Upgrading needs no migration: a slot and a snapshot without `.generation` match until the next reseed writes
  one, after which every slot's next mount starts its layers over once. The slots a reseed mounts were already
  emptied, so only closed slots lose anything.
- A shell mounts an empty `/state`, so the stamp's mismatch there removes nothing.
- A build cache with `max_size` under 16 KiB can hold no entry: a quarter of it is less than one entry's charge.
- The idle tick that trims the build cache waits for the trim. Removing entries from a local directory takes
  seconds at most, and the API answers meanwhile.
