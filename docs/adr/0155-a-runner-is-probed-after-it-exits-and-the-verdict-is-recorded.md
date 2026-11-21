# 0155. A runner is probed after it exits, and the verdict is recorded

Status: **Accepted** (2026-11-20). Adds `image/probe.py` (`/usr/local/sbin/runnerd-probe`), the image label
`runnerd.runner-probe`, `docker::PROBE_LABEL`, `docker::probe`, `RunnerState::probe`, `api::RunnerProbe`,
`JobSummary::probe`, `JobSummary::probe_line` and schema 28 (`runners.probe`).

## Context

[ADR 0153](0153-a-run-its-runner-failed-is-rerun-once-per-attempt.md) re-runs a run its runner failed only where
`rerun_runner_faults` asks, because the only evidence of a runner fault is the failed step's own output
(`record::runner_fault`, [ADR 0120](0120-a-job-its-runner-failed-reads-as-a-runner-fault.md)), which a job can print.
`_diag`, Docker's exit state and Sysbox's journal said nothing a job could not fake or said nothing at all.

The 12 recorded faults from 2026-11-05 to 2026-11-20 all failed in a call Sysbox traps through the container's
seccomp listener, made by the in-container Docker Engine:

- `mount` and `umount` returning `function not implemented`: runc's `/dev/shm`, `/dev/pts`, `/dev`, `/sys` and bind
  mounts of `/etc/hostname` and `/etc/resolv.conf`, containerd's overlay mounts while it pulled or unpacked an image
  (10 jobs);
- runc's `unsafe procfs detected` as it opened a file under `/proc`, once
  `/proc/sys/net/ipv4/ip_unprivileged_port_start` with `openat2` (2 jobs).

Every process of a Sysbox container inherits the filter of its init, so any process there that makes these calls
goes through the same listener as the job's did.

## Decision

- **An opt-in for one Sysbox profile.** Set `[host] sysbox_probe_profile` to the profile to sample. Unset, or on
  `unsafe-dind`, the daemon sends no probe request. An older image ignores the request. Start with a healthy job on
  the selected profile and confirm the stored `clean` verdict before treating any findings as evidence.
- **After normal runner exit and scrub.** The entrypoint uses a system-only PATH for root and gives the tool cache
  PATH only to the unprivileged runner. It kills the `runner` user's remaining processes, scrubs, and then probes
  using absolute executable paths. A requested stop skips the probe or interrupts the wait and discards the result. In a
  mount namespace of its own, with `/` made private, it mounts a tmpfs over `/mnt`, then a proc, a sysfs and a bind
  mount on it, unmounting each, and opens and reads `/proc/sys/net/ipv4/ip_unprivileged_port_start` with `openat2`.
  Its namespace goes when it exits, so it changes nothing in the container a record keeps, even when an unmount
  fails. It prints each failed call with its errno to the container's log. A setup failure is not a mount verdict,
  except `ENOSYS`, which is the missing-syscall failure the probe checks for. It needs no package the image lacked:
  Python 3 and `timeout` were there.
- **The verdict is the container's exit code, read beside a label.** 200 clean, 201 a mount or unmount failed, 202 the
  `openat2` failed, 203 both, 204 the probe did not end within 3 s, 205 it could not run. The image carries
  `runnerd.runner-probe="1"`, which Docker gives every container of it. runnerd reads a verdict only from a stopped
  container with that label and one of those codes (`docker::probe`): an image older than the probe, whose runner
  may exit with any code, reads as not probed, never clean. The runner's own exit code goes to the log; runnerd never
  read it.
- **runnerd reads it where it already inspects a stopped container.** `Engine::ended` notes it as it notes a kill for
  memory, for a runner it started and one a restart adopted; recovery reuses the inspect it makes for a stopped container and ensures the runner has a
  history row before recording the verdict. The store keeps it in `runners.probe` (schema 28); the job's
  summary reads it through the join it already makes.
- **Record only.** Nothing decides on it: not the outcome, not `runner_fault`, not the re-runs. It turns into
  evidence once real faults show whether the probe agrees with `record::runner_fault`:

  ```sql
  SELECT j.id, j.repository, j.runner_fault, r.probe FROM jobs j JOIN runners r ON r.name = j.runner
  WHERE j.runner_fault = 1 OR r.probe != 'clean'
  ```

- **Core words it.** `JobSummary::probe_line` gives a failed job, and any job whose probe found a call failing, a line
  under its outcome with its tone: `a probe after the job found the runner's mounts failing` (warn), `the probe's sampled mount
  and /proc calls succeeded` (quiet), `no probe verdict recorded` (quiet) for a failed job without a verdict. The dashboard, the
  TUI and `runnerctl job` show it; the parity fixtures carry a fault (`RUNNER_FAULT`) and a missing verdict
  (`ACTION_ARCHIVE`).

## What a job can and cannot do to it

A job step runs as `runner`, without sudo, and holds the container's Docker socket, which is root in the container.

- **A step that only prints cannot forge it.** The verdict is the exit code of the container's init, which a process
  of `runner` cannot set, and the label is the image's. Printing Sysbox's words, which fools `record::runner_fault`,
  does nothing here.
- **Nothing a step leaves running as `runner` reaches it.** Every such process is killed before the probe; the job's
  containers went in the completed-job hook.
- **A step that sets out to can.** Through the Docker socket a step can run a privileged container and change the
  image's files as root (replace `runnerd-probe` or `timeout`, or the entrypoint), or leave a root process behind
  that outlives the hook. No file or process in the container is beyond that. The probe is evidence against
  accidents, such as a test of mount errors, not against a workflow written to fake a fault; a default that acts on
  it has to accept that a workflow can ask for its own re-run, within ADR 0153's limits.
- **It tells nothing of a fault that passed.** A listener lost for good fails every later trapped call, and the probe
  sees it. An interrupted fd injection, the `EINVAL` of `unsafe procfs`, may hit one call and not the next: the probe
  reads clean after it. Records-only mode is how runnerd learns which kind the faults are.

## Consequences

- A job cancelled through GitHub can still produce a verdict if the runner exits normally. A requested container stop
  does not produce one. Docker killing the entrypoint before it can report a verdict also leaves no verdict.
- On a normal exit the watchdog sends TERM at 3 s and escalates one second later. Both timeout and watchdog KILL
  report a timeout or interruption. This is not a hard deadline for a helper stuck in the kernel.
- `probe` is optional on the wire; the previous release reads it as absent, and a newer verdict reads as `unknown`.
  A 0.1.x daemon refuses a schema 28 history, as after every migration.
- The default of `rerun_runner_faults` stays off until the history shows the probe and the log agree.
