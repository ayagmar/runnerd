# Architecture decision records

One short record per decision: its context, the decision, and what follows from it. Add the next number when a
decision changes a boundary, and update [ARCHITECTURE.md](../../ARCHITECTURE.md) with it. A replaced or amended
decision stays, with a line under its title pointing to the record that changes it, and the index says which.

| # | Decision |
|---|---|
| [0001](0001-core-owns-domain-logic-and-api-types.md) | `runnerd-core` owns domain logic and API types |
| [0002](0002-json-and-sse-over-a-unix-socket.md) | JSON and SSE over a Unix socket; the web UI on loopback TCP (refined by 0017, 0020, 0021) |
| [0003](0003-server-rendered-web-ui-with-datastar.md) | Web UI: axum, server-rendered HTML and Datastar |
| [0004](0004-gpui-desktop-app-pinned-to-a-zed-commit.md) | GPUI desktop app pinned to a Zed commit (superseded by 0034) |
| [0005](0005-isolation-backends-sysbox-by-default.md) | Isolation backends: sysbox by default, privileged DinD only when asked |
| [0006](0006-ephemeral-jit-runners.md) | Ephemeral JIT runners, one per job |
| [0007](0007-warm-caches-snapshot-plus-slot-layer.md) | Warm caches: per-profile snapshot plus per-slot layer (amended by 0041, 0052, 0054, 0066 and 0067) |
| [0008](0008-auth-with-the-gh-token.md) | Authenticate with the `gh` CLI's token |
| [0009](0009-newest-stable-and-locked-dependencies.md) | Every dependency newest stable and locked (in part superseded by 0025) |
| [0010](0010-mit-license-public-repository.md) | MIT license, public repository |
| [0011](0011-linux-and-github-only-for-v1.md) | Linux hosts and GitHub only, for v1 |
| [0012](0012-principal-from-one-place.md) | Handlers take a principal from one place |
| [0013](0013-runs-on-existing-machines.md) | Runs on the user's existing machines |
| [0014](0014-host-feature-for-daemon-only-code.md) | Daemon-only code behind core's `host` feature |
| [0015](0015-engine-behind-github-and-docker-traits.md) | The engine calls GitHub and Docker through traits |
| [0016](0016-strict-lints.md) | Strict lints, every exception with a reason |
| [0017](0017-job-history-in-sqlite.md) | Job history in SQLite, written by one thread (amended by 0027, 0028) |
| [0018](0018-job-identity-and-logs-from-inside-the-runner.md) | Job identity and logs from inside the runner container |
| [0019](0019-control-commands-through-the-engine.md) | Control commands go through the engine; a reload keeps running jobs (amended by 0026) |
| [0020](0020-dashboard-is-an-api-client-on-loopback.md) | The dashboard is an API client, on loopback behind a one-time login (amended by 0042, 0043) |
| [0021](0021-clients-speak-http-over-the-socket-with-hyper.md) | Clients speak HTTP over the socket with hyper (in part superseded by 0030) |
| [0022](0022-terminal-ui-is-ratatui-in-runnerctl.md) | The terminal UI is ratatui in runnerctl, behind a feature |
| [0023](0023-runner-image-pinned-to-a-snapshot-date.md) | Runner image pinned to an Ubuntu snapshot date |
| [0024](0024-pauses-recorded-in-the-job-history.md) | Pauses are recorded in the job history and survive restarts |
| [0025](0025-published-runner-images-named-by-reference.md) | Published runner images, named by reference |
| [0026](0026-commands-answered-while-a-tick-waits-for-github.md) | Commands are answered while a tick waits for GitHub |
| [0027](0027-pruning-runs-beside-the-writer.md) | Pruning runs beside the writer, a batch at a time (amended by 0029) |
| [0028](0028-job-records-expire.md) | Job records expire after `history.jobs` |
| [0029](0029-profile-events-and-what-a-restart-announces.md) | Profile events, and what a restart announces |
| [0030](0030-one-socket-client-in-core.md) | One socket client for every client, in core |
| [0031](0031-records-of-failed-jobs.md) | Records of failed jobs, without their secrets |
| [0032](0032-sizing-advice-from-sampled-peaks.md) | Sizing advice from sampled peaks, applied through the config check (amended by 0037, 0039) |
| [0033](0033-measured-admission.md) | Admission by measured peaks, behind a flag (amended by 0036, 0037) |
| [0034](0034-desktop-framework.md) | Failure notifications from the daemon, the installable dashboard and an optional tray, not a GUI toolkit (installable dashboard built in 0042, notifications in 0051) |
| [0035](0035-runner-image-stages.md) | A stack-neutral base image, with Android in its own stage |
| [0036](0036-young-jobs-predicted-from-their-own-runs.md) | A young job predicted from its own runs, one copy at a time |
| [0037](0037-memory-without-page-cache.md) | A job's memory is estimated without page cache; a cap is met when the kernel says so |
| [0038](0038-cpu-admission.md) | Admission by CPU, from each job's average, off by default |
| [0039](0039-auto-sizing.md) | Auto sizing: the daemon applies advice itself, a step at a time, off by default |
| [0040](0040-unused-runner-images-removed.md) | Published runner images no profile runs are removed |
| [0041](0041-snapshot-and-mirror-limits.md) | A profile's snapshot and the mirror's storage have size limits (amended: a snapshot over its limit is due for a reseed; amended by 0065) |
| [0042](0042-dashboard-behind-a-proxy-and-installable.md) | The dashboard behind a proxy the owner runs, and installable as an app (amended by 0043) |
| [0043](0043-phones-paired-by-qr-code-and-sessions-that-last.md) | Phones signed in by QR code, and sessions that outlive a restart |
| [0044](0044-queued-jobs-say-why-they-wait.md) | Queued jobs say why they wait |
| [0045](0045-jobs-filtered-by-workflow-and-name.md) | Jobs filtered by workflow and job name |
| [0046](0046-sizing-says-what-auto-sizing-does-next.md) | Sizing says what auto sizing does next (amended by 0068) |
| [0047](0047-activity-of-the-last-day.md) | The jobs started and ended in a window, by those times |
| [0048](0048-slice-charts-scaled-to-the-slice.md) | The slice's charts reach up to the slice |
| [0049](0049-auto-sizing-without-hand-tuning.md) | Auto sizing changes CPUs and slots too, raises within the hour, and needs no hand-set ceiling (amended by 0068) |
| [0050](0050-warm-runners.md) | Warm runners: a runner waits for a job before it is queued |
| [0051](0051-insights-and-notifications.md) | Insights from the history, and notifications the daemon sends (amended by 0064) |
| [0052](0052-shared-build-cache.md) | A build cache every runner shares, written only by the default branch |
| [0053](0053-build-cache-written-by-own-pull-requests.md) | A repository can let its own branches' pull requests write to the build cache |
| [0054](0054-slot-docker-stores-share-the-disk.md) | The slots' Docker stores share a quarter of the disk |
| [0055](0055-admission-follows-the-hosts-memory.md) | Admission follows the host's memory, and caps keep room for page cache (amended by 0063) |
| [0056](0056-adopted-runners-keep-their-reservations.md) | A runner a restart adopts keeps what it reserved |
| [0057](0057-name-what-holds-the-hosts-memory.md) | Name the programs that hold the host's memory, by name only |
| [0058](0058-a-repository-served-by-several-profiles.md) | A repository served by several profiles, each with its own labels |
| [0059](0059-clients-show-what-core-computes.md) | Clients show what core computes about a job |
| [0060](0060-clients-show-what-core-computes-about-the-host.md) | Clients show what core computes about the host |
| [0061](0061-the-previous-release-reads-the-wire.md) | The previous release reads the wire, checked both ways in CI |
| [0062](0062-client-parity-is-checked.md) | Client parity is checked, not remembered |
| [0063](0063-a-warm-runners-room-pays-the-hosts-deficit-first.md) | A warm runner's room pays the host's deficit first, and a seed container fits the budget |
| [0064](0064-each-notify-kind-is-a-sender.md) | Each notification target kind is a sender of its own, and Discord is one |
| [0065](0065-a-snapshot-starts-over-from-the-image.md) | A snapshot over its limit starts over from the image |
| [0066](0066-a-reseed-that-did-not-finish.md) | A reseed that did not finish starts the profile over from the image |
| [0067](0067-whiteouts-of-the-image-beside-the-snapshot.md) | What a job deleted of the image is hidden by whiteouts beside the snapshot |
| [0068](0068-cpus-follow-run-averages.md) | Auto sizing moves `cpus` on each job's run average, not on CPU peaks |
