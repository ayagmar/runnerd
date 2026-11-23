# 0160. Runners read one Grype database runnerd keeps current

Status: **Accepted** (2026-11-23). Adds `[caches.grype]`, the `grype` host module, `GrypeDatabase` on `Status` and
`Caches`, the attention kind `grype-database` and the container label `runnerd.grype-db`.

## Context

fretboard's image checks scan each image with Grype (0.121.0, run from mise on the runner). Grype keeps its
vulnerability database in `~/.cache/grype/db/6/`, inside the job's container, and downloads a new one when it finds
none, one older than its last check allows, or one older than five days: an archive of 207 MB (2026-11-23) that
Grype unpacks and indexes ("hydrates") in place into 3.4 GB of SQLite. One `check:backend:image` went from
1.23 GB to 4.08 GB written during its 67-second scan (job 137620468773). Restoring the database from a cache in
every job would write its 3.4 GB each time, so the saving needs one database on the host that every runner reads
without writing (the disk-wear report of 2026-11-23, fix 6; fretboard's T-1112 waits on it).

Checked on 2026-11-23 with Anchore's database of that day: `grype db import` of v0.122.1 took 45 to 60 seconds and
succeeded under a 1.5 GiB memory cap; `grype db status -o json` then printed the schema, the build time and
`"valid": true`; and v0.122.1 and fretboard's v0.121.0 both scanned from the imported directory, made read-only, with
the settings below, writing nothing to it.

What Grype does, as its source (anchore/grype at v0.122.1, `grype/db/v6`) shows:

- `distribution`: reads `https://grype.anchore.io/databases/v6/latest.json` (status, `schemaVersion`, `built`,
  `path` beside the listing, `checksum` as `sha256:<hex>`) and downloads the archive with that checksum checked.
- `installation`: imports into a temporary directory under the cache directory, hydrates it (creates the indexes
  the distributed file lacks), writes `import.json` with the database's xxh64 digest and the importing client's
  schema version, and renames it to `<cache-dir>/6`.
- On each scan with `db.auto-update` off: reads `6/vulnerability.db` with `immutable=1&mode=ro`, checks its age
  (`db.validate-age`, at most `db.max-allowed-built-age`, 120h by default) and its digest against `import.json`
  (`db.validate-by-hash-on-start`), and writes nothing. When the scanning Grype's schema is newer than the one that
  hydrated the file it tries to hydrate again; on a read-only directory that fails, is logged as a warning, and the
  scan goes on.
- Settings come from the environment as `GRYPE_DB_CACHE_DIR`, `GRYPE_DB_AUTO_UPDATE`, `GRYPE_DB_VALIDATE_AGE`.

Only Grype's own code can hydrate a database: the indexes are its models' (`gorm` auto-migration), not something
runnerd can reproduce.

## Decision

- **runnerd keeps the database, on by default.** `[caches.grype] enabled` (default `true`) turns it off and
  `directory` (default `$XDG_CACHE_HOME/runnerd/grype`, read at start) moves it. Under it: `lock`, `staging/` for
  downloads and imports, `dbs/<built>-<sha>/6/` for each database published, and `state.json` naming the one runners
  mount, written to a temporary file and renamed. The store holds an exclusive `flock` on `lock` while its task runs;
  a second runnerd on the same directory is a `store` failure and mounts nothing.
- **runnerd checks what Grype checks, then has Grype import it.** A task of its own reads the listing at start, every
  6 hours, and 30 minutes after a failed listing or download. It takes only a listing whose schema is `v6.<n>.<n>`, whose path is one
  file name ending `.tar.zst`, `.tar.xz` or `.tar.gz`, and whose checksum is a SHA-256; `eol` is refused. When the
  listing's database was built after the one held, it downloads the archive anonymously (15-minute timeout, at most
  1 GiB) into a new work directory, hashing it as it arrives, and refuses one whose SHA-256 differs from the
  listing's. Then a container of `anchore/grype:v0.122.1`, pinned by digest, runs `grype db import` on it, and a
  second `grype db status -o json`, which checks the schema, the age and the digest as a scan does. Both run as the
  daemon's user, without a network, capabilities or new privileges, on a read-only root, with 3 GiB of memory and
  the work directory as their only mount (the import needs under 1.5 GiB); each is removed after it ends or after its deadline (20 and 5 minutes),
  and one an earlier daemon left is removed before the store opens and again when the task stops. The status must be
  valid and name the listing's build time and schema.
- **A failure that cost a download is not repeated for a day.** The image is pulled before the archive is
  downloaded, so a missing image or a stopped Docker costs nothing and is tried again in 30 minutes. An archive whose
  checksum, import, check or publish failed is held back by its SHA-256 for 24 hours, while the listing still names
  it: a failure that repeats writes at most one archive and one import a day, not 48.
- **A database is published once and never written again.** Its directories are made 0755 and its files 0644, so a
  runner's user reads them, links or anything but files and directories refuse it, the archive is removed, every file
  is synced, and the directory is renamed to `dbs/<built>-<first 12 hex of the archive's SHA-256>` before
  `state.json` names it with the SHA-256 of its `vulnerability.db`; the store holds it only once that write succeeded,
  and removes the directory if it failed. Any failure before that leaves the database held
  as it was and removes the work directory.
- **A runner mounts the current generation, read-only, once it is checked.** At start, the task hashes the held
  database against `state.json` before any runner mounts it; one that differs or is missing is removed and downloaded
  again. A runner's start reads the ready view once: it mounts that generation at `/opt/runnerd/grype` read-only and
  sets `GRYPE_DB_CACHE_DIR=/opt/runnerd/grype`, `GRYPE_DB_AUTO_UPDATE=false` and `GRYPE_DB_VALIDATE_AGE=true`, or none
  of them. `db.max-allowed-built-age` is left to the workflow's own Grype config. The view is withdrawn while the
  database is off, and from an hour before the held database passes 5 days (Grype's default limit), so a job that
  starts with it does not find it past the limit when it scans; a scan without it downloads its own. A start checks
  that against the clock itself, as the task's timers do not count a time the host slept.
- **A generation stays while a container names it.** A runner's container carries `runnerd.grype-db=<generation>`.
  At the start of each refresh, before the listing is read, the task removes every generation but the held one that
  no container (running or stopped) names; while Docker cannot list containers, none is removed. A replaced
  generation so stays until the refresh after the one that replaced it, by when every start that read it has made its
  container.
- **The owner sees it.** `Status` and `/v1/caches` carry `GrypeDatabase` (the database held, its schema, build time
  and size, whether runners mount it, when the listing was last read, and what failed); core words the line and the
  failure, and `runnerctl caches`, the TUI and the dashboard's Caches page show them. A failure, a stale database
  among them, is an attention item. A previous client reads the attention kind as `unknown` (accepted in the wire
  check).

## What it does not guarantee

- That a scan uses it. A step that sets its own `GRYPE_DB_CACHE_DIR` or `GRYPE_DB_AUTO_UPDATE`, or a Grype that runs
  in a container of its own, reads its own database as before.
- That every Grype step still passes. `grype db update` fails on the read-only directory, and a Grype older than 0.88
  looks for a schema v5 database (`5/`) that is not there. Such a step sets its own `GRYPE_DB_CACHE_DIR`, or the host
  sets `grype.enabled = false`. The environment overrides a Grype config file's
  `db.cache-dir`, `db.auto-update` and `db.validate-age`.
- That the database is newer than 6 hours behind Anchore's. A job that needs the latest one sets its own cache
  directory and updates.
- That a scan with a Grype newer than v0.122.1 gets the newest indexes. Its attempt to hydrate again fails on the
  read-only mount; Grype logs it and scans with the indexes the pinned Grype made. The image is moved forward with
  Grype's releases.
- What a runner started before a database changed on disk reads. Grype checks the digest on each scan, so a changed
  file fails the scan rather than passing it; runnerd notices at its next start.
- The archive's origin beyond TLS to `grype.anchore.io` and the listing's checksum: runnerd does not check a
  signature, as Grype does not.

## Consequences

- A scan writes nothing for its database. The download, import and indexing are written once per database on the
  host, about once a day, instead of in each job that refreshes it.
- runnerd pulls one more image, `anchore/grype`, when it first imports.
- The store's directory, `state.json` and the generations survive restarts; the failure and the last read of the
  listing are in memory.
- The tests run the task on fakes of Anchore and Grype through each failed step, a restart, a database changed on
  disk, a second daemon, a stale database and a generation a container still mounts, and a generated sequence of all
  of them; the HTTP client against a local server; and a runner's start on the engine's fakes. A scan inside a real
  runner on the shipped image is proved in the end-to-end run, not in these tests.
