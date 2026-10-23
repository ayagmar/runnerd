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
| [0007](0007-warm-caches-snapshot-plus-slot-layer.md) | Warm caches: per-profile snapshot plus per-slot layer |
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
| [0020](0020-dashboard-is-an-api-client-on-loopback.md) | The dashboard is an API client, on loopback behind a one-time login |
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
| [0032](0032-sizing-advice-from-sampled-peaks.md) | Sizing advice from sampled peaks, applied through the config check |
| [0033](0033-measured-admission.md) | Admission by measured peaks, behind a flag |
| [0034](0034-desktop-framework.md) | Failure notifications from the daemon, the installable dashboard and an optional tray, not a GUI toolkit |
| [0035](0035-runner-image-stages.md) | A stack-neutral base image, with Android in its own stage |
| [0037](0037-memory-the-kernel-cannot-reclaim.md) | Memory is what the kernel cannot reclaim; a cap is met when the kernel says so |
