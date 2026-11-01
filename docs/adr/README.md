# Architecture decision records

An architecture decision record (ADR) is one short record of a decision: its context, the decision, and what follows
from it. Add the next number when a decision changes a boundary, and update [ARCHITECTURE.md](../../ARCHITECTURE.md)
with it. A replaced or amended decision stays, and its status names the record that changes it.

[ARCHITECTURE.md](../../ARCHITECTURE.md) is the current design. The ADRs are the history of why.

| # | Decision | Status |
|---|---|---|
| [0001](0001-core-owns-domain-logic-and-api-types.md) | `runnerd-core` owns domain logic and API types | Accepted |
| [0002](0002-json-and-sse-over-a-unix-socket.md) | JSON and SSE over a Unix socket; the web UI on loopback TCP | Accepted |
| [0003](0003-server-rendered-web-ui-with-datastar.md) | Web UI: axum, server-rendered HTML and Datastar | Accepted |
| [0004](0004-gpui-desktop-app-pinned-to-a-zed-commit.md) | GPUI desktop app pinned to a Zed commit | Superseded by 0034 |
| [0005](0005-isolation-backends-sysbox-by-default.md) | Isolation backends: sysbox by default, privileged DinD only when asked | Accepted |
| [0006](0006-ephemeral-jit-runners.md) | Ephemeral JIT runners, one per job | Amended by 0050 |
| [0007](0007-warm-caches-snapshot-plus-slot-layer.md) | Warm caches: per-profile snapshot plus per-slot layer | Amended by 0041, 0052, 0054, 0066, 0067, 0077, 0080 |
| [0008](0008-auth-with-the-gh-token.md) | Authenticate with the `gh` CLI's token | Accepted |
| [0009](0009-newest-stable-and-locked-dependencies.md) | Every dependency newest stable and locked | In part superseded by 0025 |
| [0010](0010-mit-license-public-repository.md) | MIT license, public repository | Accepted |
| [0011](0011-linux-and-github-only-for-v1.md) | Linux hosts and GitHub only, for v1 | Accepted |
| [0012](0012-principal-from-one-place.md) | Handlers take a principal from one place | Accepted |
| [0013](0013-runs-on-existing-machines.md) | Runs on the user's existing machines | Accepted |
| [0014](0014-host-feature-for-daemon-only-code.md) | Daemon-only code behind core's `host` feature | Accepted |
| [0015](0015-engine-behind-github-and-docker-traits.md) | The engine calls GitHub and Docker through traits | Accepted |
| [0016](0016-strict-lints.md) | Strict lints, every exception with a reason | Accepted |
| [0017](0017-job-history-in-sqlite.md) | Job history in SQLite, written by one thread | Amended by 0027, 0028, 0073 |
| [0018](0018-job-identity-and-logs-from-inside-the-runner.md) | Job identity and logs from inside the runner container | Accepted |
| [0019](0019-control-commands-through-the-engine.md) | Control commands go through the engine; a reload keeps running jobs | Amended by 0026 |
| [0020](0020-dashboard-is-an-api-client-on-loopback.md) | The dashboard is an API client, on loopback behind a one-time login | Amended by 0042, 0043 |
| [0021](0021-clients-speak-http-over-the-socket-with-hyper.md) | Clients speak HTTP over the socket with hyper | In part superseded by 0030 |
| [0022](0022-terminal-ui-is-ratatui-in-runnerctl.md) | The terminal UI is ratatui in runnerctl, behind a feature | Accepted |
| [0023](0023-runner-image-pinned-to-a-snapshot-date.md) | Runner image pinned to an Ubuntu snapshot date | Accepted |
| [0024](0024-pauses-recorded-in-the-job-history.md) | Pauses are recorded in the job history and survive restarts | Accepted |
| [0025](0025-published-runner-images-named-by-reference.md) | Published runner images, named by reference | Accepted |
| [0026](0026-commands-answered-while-a-tick-waits-for-github.md) | Commands are answered while a tick waits for GitHub | Accepted |
| [0027](0027-pruning-runs-beside-the-writer.md) | Pruning runs beside the writer, a batch at a time | Amended by 0029 |
| [0028](0028-job-records-expire.md) | Job records expire after `history.jobs` | Accepted |
| [0029](0029-profile-events-and-what-a-restart-announces.md) | Profile events, and what a restart announces | Accepted |
| [0030](0030-one-socket-client-in-core.md) | One socket client for every client, in core | Accepted |
| [0031](0031-records-of-failed-jobs.md) | Records of failed jobs, without their secrets | Amended by 0091 |
| [0032](0032-sizing-advice-from-sampled-peaks.md) | Sizing advice from sampled peaks, applied through the config check | Amended by 0037, 0039, 0050, 0055, 0070 |
| [0033](0033-measured-admission.md) | Admission by measured peaks, behind a flag | Amended by 0036, 0037, 0055, 0056, 0058, 0070 |
| [0034](0034-desktop-framework.md) | Failure notifications from the daemon, the installable dashboard and an optional tray, not a GUI toolkit | Accepted |
| [0035](0035-runner-image-stages.md) | A stack-neutral base image, with Android in its own stage | Amended by 0077 |
| [0036](0036-young-jobs-predicted-from-their-own-runs.md) | A young job predicted from its own runs, one copy at a time | Amended by 0070 |
| [0037](0037-memory-without-page-cache.md) | A job's memory is estimated without page cache; a cap is met when the kernel says so | Amended by 0070 |
| [0038](0038-cpu-admission.md) | Admission by CPU, from each job's average, off by default | Accepted |
| [0039](0039-auto-sizing.md) | Auto sizing: the daemon applies advice itself, a step at a time, off by default | Amended by 0049, 0055, 0070, 0071, 0088, 0090 |
| [0040](0040-unused-runner-images-removed.md) | Published runner images no profile runs are removed | Accepted |
| [0041](0041-snapshot-and-mirror-limits.md) | A profile's snapshot and the mirror's storage have size limits | Amended by 0054, 0065 |
| [0042](0042-dashboard-behind-a-proxy-and-installable.md) | The dashboard behind a proxy the owner runs, and installable as an app | Amended by 0043 |
| [0043](0043-phones-paired-by-qr-code-and-sessions-that-last.md) | Phones signed in by QR code, and sessions that outlive a restart | Amended by 0078 |
| [0044](0044-queued-jobs-say-why-they-wait.md) | Queued jobs say why they wait | Amended by 0058 |
| [0045](0045-jobs-filtered-by-workflow-and-name.md) | Jobs filtered by workflow and job name | Accepted |
| [0046](0046-sizing-says-what-auto-sizing-does-next.md) | Sizing says what auto sizing does next | Amended by 0049, 0068 |
| [0047](0047-activity-of-the-last-day.md) | The jobs started and ended in a window, by those times | Accepted |
| [0048](0048-slice-charts-scaled-to-the-slice.md) | The slice's charts reach up to the slice | Accepted |
| [0049](0049-auto-sizing-without-hand-tuning.md) | Auto sizing changes CPUs and slots too, raises within the hour, and needs no hand-set ceiling | Amended by 0068, 0071, 0090 |
| [0050](0050-warm-runners.md) | Warm runners: a runner waits for a job before it is queued | Amended by 0058 |
| [0051](0051-insights-and-notifications.md) | Insights from the history, and notifications the daemon sends | Amended by 0064 |
| [0052](0052-shared-build-cache.md) | A build cache every runner shares, written only by the default branch | Amended by 0053, 0077, 0082, 0083 |
| [0053](0053-build-cache-written-by-own-pull-requests.md) | A repository can let its own branches' pull requests write to the build cache | Accepted |
| [0054](0054-slot-docker-stores-share-the-disk.md) | The slots' Docker stores share a quarter of the disk | Amended by 0089, 0090 |
| [0055](0055-admission-follows-the-hosts-memory.md) | Admission follows the host's memory, and caps keep room for page cache | Amended by 0063, 0070, 0078, 0084 |
| [0056](0056-adopted-runners-keep-their-reservations.md) | A runner a restart adopts keeps what it reserved | Accepted |
| [0057](0057-name-what-holds-the-hosts-memory.md) | Name the programs that hold the host's memory, by name only | Accepted |
| [0058](0058-a-repository-served-by-several-profiles.md) | A repository served by several profiles, each with its own labels | Accepted |
| [0059](0059-clients-show-what-core-computes.md) | Clients show what core computes about a job | Amended by 0069 |
| [0060](0060-clients-show-what-core-computes-about-the-host.md) | Clients show what core computes about the host | Accepted |
| [0061](0061-the-previous-release-reads-the-wire.md) | The previous release reads the wire, checked both ways in CI | Accepted |
| [0062](0062-client-parity-is-checked.md) | Client parity is checked, not remembered | Amended by 0093 |
| [0063](0063-a-warm-runners-room-pays-the-hosts-deficit-first.md) | A warm runner's room pays the host's deficit first, and a seed container fits the budget | Accepted |
| [0064](0064-each-notify-kind-is-a-sender.md) | Each notification target kind is a sender of its own, and Discord is one | Accepted |
| [0065](0065-a-snapshot-starts-over-from-the-image.md) | A snapshot over its limit starts over from the image | Accepted |
| [0066](0066-a-reseed-that-did-not-finish.md) | A reseed that did not finish starts the profile over from the image | Accepted |
| [0067](0067-whiteouts-of-the-image-beside-the-snapshot.md) | What a job deleted of the image is hidden by whiteouts beside the snapshot | Accepted |
| [0068](0068-cpus-follow-run-averages.md) | Auto sizing moves `cpus` on each job's run average, not on CPU peaks | Amended by 0075 |
| [0069](0069-a-job-no-runner-took-did-not-run-elsewhere.md) | A job no runner took did not run elsewhere | Accepted |
| [0070](0070-predictions-follow-recent-runs.md) | Predictions and memory sizing follow each job's recent runs | Accepted |
| [0071](0071-lowerings-every-hour.md) | Auto sizing lowers every hour, by up to half | Accepted |
| [0072](0072-job-minutes-priced-at-github-hosted-rates.md) | What this machine's job minutes would have cost on GitHub-hosted runners | Amended by 0074 |
| [0073](0073-no-compatibility-before-0-0-19.md) | No compatibility before 0.0.19: the history starts at schema 15 | Amended by 0081 |
| [0074](0074-the-cost-line-names-only-what-it-is-before.md) | The cost line names only what it is before | Accepted |
| [0075](0075-cpus-come-down-only-under-a-cap-no-burst-met.md) | `cpus` come down only under `caps` admission and a cap no run's bursts met; under `measured` the bursts raise them | Amended by 0079 |
| [0076](0076-a-slot-is-free-once-docker-removed-its-container.md) | A slot is free once Docker removed its container, each start is admitted again as it starts, and a broken log capture starts again | Accepted |
| [0077](0077-cache-credentials-closed-slots-and-build-cache-counts.md) | Credentials in the cache layer, closed slots' layers, and what the build cache counts | Accepted |
| [0078](0078-a-first-run-that-can-start-and-keeps-its-secrets.md) | A first run that can start its first job and keeps its secrets | Accepted |
| [0079](0079-cpu-weights-and-throttling-as-evidence.md) | Runners share the slice's CPUs by their `cpus`, and throttling, not peaks, says the cap held a run back | Accepted |
| [0080](0080-a-job-starts-in-the-slot-that-ran-it-last.md) | A job starts in the free slot that ran it last, else one that ran its repository | Accepted |
| [0081](0081-waits-leave-out-the-time-no-runnerd-ran.md) | Waits leave out the time no runnerd ran, and the history's schema changes in steps from 15 | Accepted |
| [0082](0082-github-actions-cache-served-from-the-host.md) | GitHub Actions' cache served from the host, on by default | Amended by 0085 |
| [0083](0083-turborepo-and-nx-from-the-build-cache.md) | Turborepo's remote cache served from the build cache, Nx's on request | Accepted |
| [0084](0084-a-host-whose-memory-grows-to-a-ceiling.md) | A host whose memory grows to a ceiling counts what it can still grow by | Amended by 0086 |
| [0085](0085-actions-cache-saves-wait-for-the-decision-not-the-job.md) | GitHub Actions cache saves wait for the decision, not the job | Accepted |
| [0086](0086-sizing-and-init-know-the-memory-ceiling.md) | Auto sizing and `runnerd init` know the memory ceiling | Amended by 0088 |
| [0087](0087-an-insight-for-jobs-their-cpu-cap-holds-back.md) | An insight for jobs their CPU cap holds back | Accepted |
| [0088](0088-sizing-advice-keeps-to-a-profiles-memory-bounds.md) | Sizing advice keeps to a profile's memory bounds | Accepted |
| [0089](0089-a-slots-cache-mounts-go-last.md) | A slot's build cache follows its share, and cache mounts go last | Accepted |
| [0090](0090-slots-follow-waits-for-a-slot-alone-and-dockers-disk.md) | Under measured admission, slots follow waits for a slot alone and Docker's disk | Accepted |
| [0091](0091-a-signal-leaves-running-jobs-to-the-next-start.md) | A signal leaves running jobs to the next start, and the daemon exits within 45 s | Accepted |
| [0092](0092-idle-removals-and-deferred-starts-stay-off-the-start-path.md) | Idle removals and deferred starts stay off the start path, and a job's wait says where it went | Accepted |
| [0093](0093-clients-share-views-columns-and-health-figures.md) | Clients share their views, table columns and health figures | Accepted |
