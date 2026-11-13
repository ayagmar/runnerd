---
title: Security model
description: How runner containers are kept from the host and from each other, where the GitHub token goes, the dashboard's sessions, and what job actions can do.
---

runnerd runs code from your repositories' workflows on your machine. Everything below follows from that: serve private repositories only. On a public one, anyone who opens a pull request can run code on this host, and their jobs share runnerd's caches with your trusted jobs. `runnerd doctor` warns for each public repository the config serves.

## Isolation

Each job runs in a new container that is removed after it. `isolation = "sysbox"`, which `runnerd init` writes, runs it under [Sysbox](https://github.com/nestybox/sysbox): the container gets its own unprivileged Docker Engine, so a job can build and run containers without a privileged one on the host. `unsafe-dind` runs privileged containers, so a job can take over the host, and the check refuses it unless `[host]` sets `allow_unsafe = true`.

A runner registers with GitHub as a just-in-time runner for one job. Its registration reaches the container only as a root-only file, and the runner's credentials are scrubbed when it exits.

## What jobs share

A slot's `~/.cache`, tool cache, Docker images and cache mounts serve the next job in the slot whichever repository it belongs to, and reach the profile's other slots through its snapshot. The pnpm store and Maven repository are shared by all profiles. A job that writes there can leave a package or tool that another repository's later job runs. runnerd removes only the credential files it knows of. Keep repositories that must not trust each other on different profiles, and on different machines if they must not share the pnpm store or Maven repository.

The build cache and GitHub Actions' cache take writes only from [jobs GitHub says may write](../guides/caches/#build-cache): pushes, schedules and manual runs of the default branch and `build.branches`, and with `build_cache_writers = "pull-requests"` people's pull requests from the repository's own branches. Fork and bot pull requests only read.

## Records

A [record](../guides/records/) of a failed job keeps its workspace, never logins, `/tmp`, the runner's registration or the token `actions/checkout` writes; runnerd refuses to keep a container that still holds one. It does not read file contents, so a secret the job saved in its workspace stays. Anyone who can use Docker on this host can read a record (`docker run runnerd-record:<job>`), so turn `record_failures` on only for profiles whose jobs they may see.

## The GitHub token

runnerd reads the GitHub CLI's token at start, and again when GitHub rejects it. It needs the `repo` scope, or a fine-grained token with Administration: write to register runners. The token stays inside the daemon: no log line, error or client shows it, and runner containers never get it.

## Job actions

With [`[github] job_actions`](../reference/configuration-more/#github) on, the default, a client can ask the daemon to cancel a job's workflow run or re-run jobs on GitHub with that token. Cancel stops the whole run, on every runner, not only here; a re-run takes the jobs that depend on it along. Each is previewed and sent only with a confirmation the daemon issued for that preview, once, within 5 minutes. A request whose outcome is unknown, as after a timeout, is never sent again by itself. `job_actions = false` turns both off; a [forced pause](../reference/api/#routes) still cancels runs.

## The API socket

Clients talk to the daemon over `$XDG_RUNTIME_DIR/runnerd.sock`, mode 0600: only the user runnerd runs as may connect.

## The dashboard

The dashboard listens on loopback only, `127.0.0.1:7477`. A browser signs in through a one-time link the daemon writes to `$XDG_RUNTIME_DIR/runnerd.web`, in the user's own runtime directory, and stays signed in for 30 days after its last visit. `runnerctl web --sign-out-all` makes a new session secret (`dashboard-session` beside the job history), which signs every browser out and ends every earlier link and QR code.

Writes such as a pause also need the dashboard's own `Origin`, so another site open in the browser cannot send them. A proxy's address may write only when it is listed in [`[web] origins`](../reference/configuration-more/#web). Behind a proxy that sends `X-Forwarded-Proto` the session cookie is `Secure`. runnerd ignores Tailscale's identity headers, since any local user could send them to the port.

Notification URLs and headers are secrets: logs, errors and `runnerctl notify test` never show them, and the config editor, `runnerctl config show` and `GET /v1/config` show `"<secret>"`. The config file is mode 0600; `runnerd doctor` warns otherwise.

## Releases

Every release archive, `install.sh`, `sha256.sum` and runner image has a GitHub build provenance attestation; [Install](../start/install/#verify-a-release) shows how to check one.
