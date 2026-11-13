---
title: Troubleshooting
description: Every check runnerd doctor makes, what each failure or warning means, and its fix.
---

`runnerd doctor` checks the host and the config, prints each check as `ok`, `info`, `WARN`, `FAIL` or `skip`, and gives a fix under each warning and failure. `runnerctl doctor` shows the running daemon's last run, or runs `runnerd doctor` when no daemon runs; the dashboard's Settings page shows the same, and its warnings and failures are listed as needing attention. A check that cannot run because an earlier one failed, such as everything Docker answers while Docker is down, is skipped.

## Docker and isolation

| Check | Fails or warns when | Fix it prints |
|---|---|---|
| Docker | Docker does not answer | start Docker; unset `DOCKER_HOST` or point it at a `unix://` socket; `sudo systemctl enable --now docker`, `sudo usermod -aG docker "$USER"`, then log in again |
| cgroups | Docker does not use cgroup v2 with the systemd driver | boot with cgroup v2, and remove any `native.cgroupdriver=cgroupfs` from `/etc/docker/daemon.json` |
| isolation | `isolation = "unsafe-dind"`: runners run privileged and a job can take over the host | none: use `sysbox` ([Isolation](../security/#isolation)) |
| sysbox | Docker has no `sysbox-runc` runtime, or Sysbox's `sysbox-mgr` or `sysbox-fs` is stopped | the install for your distribution; `sudo systemctl enable --now sysbox` |
| kernel modules | `/lib/modules/<release>` is missing, as after a kernel update without a reboot | reboot to run the installed kernel |
| image | a profile's image is missing and cannot be pulled, was never built, is published only for another platform, or carries another runnerd version than the daemon | `docker pull`, `docker build --target <stage>`, or the image tag of this version |

## Budget and host

| Check | Fails or warns when | Fix it prints |
|---|---|---|
| slice | the slice has no unit file with a `MemoryMax`, so nothing holds runners to the budget | the unit to write under `/etc/systemd/system`, then `sudo systemctl daemon-reload` |
| config | the config does not fit the slice's `MemoryMax` ([the budget check](../concepts/terms/#slice-and-budget)) | change `runnerd.toml` or the slice's limits |
| config file | other users can read the config, which holds notification secrets | `chmod 600` on it |
| host memory | the host stalls for memory, or has less to spare than the smallest profile's `memory`; or `memory_ceiling` is below the memory the host has | close what uses the host's memory beside the runners (runnerd waits by itself); set `memory_ceiling` to the most the host can grow to |
| records | records near `[records] max_total` | `runnerctl records rm --older-than 2d`, or raise `max_total` |
| sizing | a profile's jobs need more memory than its cap, or than the slice allows | `runnerctl sizing apply <profile>`, after raising `memory_ceiling`, `max_memory` or the slice's `MemoryMax` where needed |

## GitHub

| Check | Fails or warns when | Fix it prints |
|---|---|---|
| gh | the GitHub CLI is not logged in, or its token lacks the `repo` scope | `gh auth login --hostname github.com --scopes repo`, or `gh auth refresh --hostname github.com --scopes repo` |
| job actions | the token cannot [cancel or re-run](../guides/cancel-rerun/) jobs; info when `[github] job_actions = false` or a fine-grained token cannot be checked without trying | `gh auth refresh` with the `repo` scope |
| repository | a repository is public: anyone who opens a pull request can run code on this host | serve private repositories only, or have GitHub require approval before it runs workflows from fork pull requests |

Two checks only inform: **GitHub Actions' cache** says whether jobs' `actions/cache` is served here or by GitHub ([Caches](../guides/caches/#github-actions-cache)), and **/dev/kvm** whether the device is there; nothing runnerd runs today needs it.

## When the doctor is clean

- A job never starts: [Why is my job waiting?](../guides/waiting/)
- The dashboard asks to sign in again: `runnerctl web` signs this browser in with a new link. A link works once.
- The log: `journalctl --user -u runnerd -f`.
