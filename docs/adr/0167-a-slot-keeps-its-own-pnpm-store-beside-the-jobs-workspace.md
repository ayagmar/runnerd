# 0167. A slot keeps its own pnpm store beside the job's workspace

Status: **Accepted** (2026-11-23). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md),
[0077](0077-cache-credentials-closed-slots-and-build-cache-counts.md). Adds `[caches] pnpm.store`,
`cache::PnpmStore`, the image label `runnerd.slot-pnpm-store` and the runner variable `RUNNERD_PNPM_SLOT_STORE`.

## Context

Every runner mounts one pnpm store that all slots of all profiles share, the `runnerd-pnpm` volume at
`/shared/pnpm`, and works in `/home/runner/_work` on the container's own disk. pnpm puts a package into
`node_modules` by hard link, by clone or by copy, the first its file system allows. A hard link needs the store and
`node_modules` on one mount, a clone a file system with reflinks. The store and the workspace are two mounts, so a
link fails with `EXDEV`, and since Docker moved to ext4 on 2026-11-23 a clone fails too. Every warm web job now
copies its whole `node_modules` out of the store: 913 MB in 62,818 files for fretboard's web jobs, which pnpm reports
as `reused 1286, downloaded 0`. That is the largest single write of those jobs, about 100 GB a day at the host's usual
load (the after-move disk report, section 2 and lever 4).

The fix is to put the store and the workspace on one mount. The question is whose jobs can then change the files
the next job runs.

What pnpm does, read from the source of 12.15.0, the version fretboard pins (`pnpm/crates`):

- **On Linux pnpm 12 tries a hard link first** (`deps-restorer/src/link_file.rs`, `next_auto_tier`), then a clone,
  then a copy. pnpm 11 tries a clone first. Both settle on the hard link wherever the store and the project share a
  mount, which is what every pnpm install on one Linux disk already does, GitHub's hosted runners included.
- **A package that a build script or a patch is going to change is copied, never linked**
  (`effective_import_method`: `needs_build` gives `clone-or-copy`; `patching/src/apply.rs` writes a patched file
  through a rename that breaks the link). pnpm 11 does the same for `willBeBuilt` packages.
- **What else writes through a link reaches the store.** A job's own step that edits a file in `node_modules` in place
  changes the store's copy, and every other link to it.
- **`verify-store-integrity`, on by default, catches that change by its time stamp.** Before a store's package is
  used again, pnpm compares each file's mtime with the `checked_at` its index recorded and hashes the file again when
  the mtime moved by more than 100 ms (`store-dir/src/check_pkg_files_integrity.rs`). A file whose hash no longer
  matches is fetched again and replaced by a rename. The same file says what this is not: "corruption detection in a
  trusted local store. It is not a tamper boundary for a store writable by untrusted users or jobs." A job that sets
  the mtime back passes the check.
- **Without a configured store, pnpm puts it at the root of the project's mount**, `<mount>/.pnpm-store`, when it
  cannot link from its home store (`config/src/store_path.rs`). The layout below is the one pnpm itself would pick.

What runnerd already trusts: the repositories of a profile share each slot's `~/.cache`, tool cache, Docker images
and cache mounts, and through the snapshot every slot of the profile ([0089](0089-a-slots-cache-mounts-go-last.md)).
The pnpm store and the Maven repository are wider still: every profile shares them, and a job writes the store as
the user that owns it, so a job can change what another profile's job installs today, links or not
([0077](0077-cache-credentials-closed-slots-and-build-cache-counts.md), the security page).

## Options

1. **The job's workspace on the slot's volume, with a store of the slot's own beside it.** The entrypoint binds
   `/state/work` over `/home/runner/_work`, and pnpm's store is `/home/runner/_work/.pnpm-store`: one mount, so pnpm
   links. The runner does not mount the shared store. Who can change a slot's store: the jobs of that slot, the same
   jobs that can already change its `~/.cache` and images. Costs: each slot fills its own store once, from the
   network; the store takes disk once per slot, not once per host; and the workspace is a volume, which `docker
   commit` leaves out of a failed job's record.
2. **The shared store and every slot's workspace on the shared volume.** It links and fills once per host, but a hard
   link would carry one profile's accidental edits into every profile's packages, and every slot's workspace would be
   open to every other running job. Rejected.
3. **A store of the slot's own, but the workspace stays in the container**, with pnpm's virtual store
   (`virtualStoreDir`, or the global virtual store) moved onto the slot's volume and `node_modules` holding symlinks
   to it. Records keep the workspace, but every package resolves to a real path outside the project, which tools
   that look for the project's root from a package's own path do not expect. pnpm keeps the global virtual store off
   by default. Rejected: a build tool's misreading would fail jobs, where option 1 is pnpm's ordinary layout.
4. **A slot's store copied from the shared one when it is empty.** The copy writes what a download writes, once per
   slot either way, and downloads are compressed. Not done.
5. **Clones instead of links.** ext4 has none. Docker back on btrfs is lever 6 of the report, not this change.

## Decision

- **`[caches] pnpm.store`, `"slot"` by default, `"shared"` to opt out.** With `"slot"`, a runner's slot keeps its own
  store when two more things hold. Its profile does not set `record_failures`, whose records need the workspace in
  the container ([0031](0031-records-of-failed-jobs.md)). Its image carries the label `runnerd.slot-pnpm-store=1`,
  so an image from before this change, which would put a store on the container's own disk and fill it again every
  job, keeps the shared one. Core decides this in one place, `cache::PnpmStore::of`, and the runner's mounts and its
  environment both follow that one value.
- **A slot's store runner** mounts no `runnerd-pnpm` volume and gets `RUNNERD_PNPM_SLOT_STORE=<max_size in bytes>`.
  Its entrypoint, before it mounts the layers or starts the Docker Engine:
  - empties `/state/work`, the last job's workspace, but for `.pnpm-store`, and removes that store whole when it
    holds more than `pnpm.max_size` (`clear_slot_work` in `image/layers.sh`), as the shared store is emptied whole
    above the same limit;
  - binds `/state/work` over `/home/runner/_work`, so the slot's Docker Engine, the tool cache's layer at
    `_work/_tool` and a `container:` job's `/__w` all see the slot's workspace;
  - sets `pnpm_config_store_dir=/home/runner/_work/.pnpm-store` for the runner.
  `XDG_RUNTIME_DIR` stays at `/shared/pnpm/run`, which is now the container's own directory: the store has one
  container at a time, so its lock directory needs no sharing.
- **The entrypoint falls back to the container's workspace, said on its output,** when the bind fails or when the
  image holds anything under `_work` but an empty `_tool`, which the bind would hide. pnpm then fills a store on the
  container's disk: slower, never wrong.
- **What the scrub does is unchanged.** It still deletes `_work/_temp` and checkout's `extraheader` lines through the
  bind, before the container exits, so no step script or token waits on the volume for the next job; it skips the
  store when it looks for git configs.
- **A runner that does not get the slot's store removes `/state/work`**, so a slot whose runners go back to the
  shared store, by `"shared"`, by `record_failures` or by an image without the label after one with it, does not
  keep a store nothing reads.
- **Everything else that empties a slot's volume empties its store:** a crash under skipped syncs
  ([0141](0141-runners-answer-sync-calls-without-syncing.md)), `runnerctl caches clean` of the slot's volume, a closed
  slot. A reseed does not: the store is not a layer, as the slot's Docker store is not one.

## What remains

- **A slot's jobs can still change the packages its later jobs run.** A step that edits `node_modules` in place
  changes the slot's store. pnpm finds such an edit at the next install by its mtime and fetches the package again;
  a job that means to hide an edit sets the mtime back and is not found. Those jobs are the repositories of one
  profile, which already share the slot's caches, so this adds no repository to what a job can reach. A job can no
  longer reach another profile's packages through the pnpm store: a slot's store runner does not mount the shared one.
- **The shared store stays for `"shared"`, for profiles that keep records, and for older images.** Those runners
  still write it, and still copy out of it.
- **Each slot fills its own store**, once, from the network, and again after its store went over `max_size`, after a
  crash, or after its volume was cleaned. Each slot that runs pnpm jobs holds a store of up to `max_size`.
- **The old shared store is not removed.** While no runner uses it, it keeps what it held, under its limit;
  `runnerctl caches clean runnerd-pnpm` frees it.
- **A container killed before its scrub**, as `docker stop` past its timeout does, leaves its step scripts and
  checkout's token in the workspace on the slot's volume until the slot's next runner empties it before its job.
  Before, they went with the container, unless it was kept as a record. No job reads them: the next runner empties
  the workspace before its runner starts.
- **Emptying the last workspace takes time at the start of the next runner of the slot**, the unlinking of its tree.
  A warm runner does this before its job is given to it.

## Consequences

- A warm pnpm job of a slot's store runner links its `node_modules` instead of copying it: the 913 MB of fretboard's
  web jobs becomes directory entries, a few MB of metadata. Its checkout and everything else it writes to the
  workspace go to the slot's volume instead of the container's disk, on the same file system, at the same cost.
- The first job of each slot downloads its packages; the shared store's copies are not reused.
- The security page says what a slot's store shares and what the shared store still does.
- Needs the runner images rebuilt: until then every runner keeps the shared store, as the label says.
