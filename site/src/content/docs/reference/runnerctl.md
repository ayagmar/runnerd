---
title: runnerctl
description: The `runnerctl` commands that show something, and their options, as `--help` prints them.
---

<!-- Written by site/scripts/reference/cli.py from `runnerctl --help`. Change the help, then run it. -->

Command-line client for the runnerd daemon

The other commands are on [runnerctl actions](../runnerctl-actions/).

## `runnerctl status`

Show the memory and CPU budgets, every slot and what runs in it, the repositories, and why each queued job waits

## `runnerctl jobs`

List jobs, newest first

- `--repo <REPO>`: Only this repository (owner/name)
- `--failed`: Only jobs that concluded failure; same as --conclusion failure
- `--conclusion <CONCLUSION>`: Only jobs with this conclusion, such as success, failure or cancelled
- `--state <STATE>`: Only jobs in this state: queued, running, completed, lost or elsewhere
- `--since <SINCE>`: Only jobs queued in the last 30m, 12h, 7d, 2w, …
- `--before <BEFORE>`: Only jobs older than this job ID: the next page
- `--limit <LIMIT>`: At most this many jobs [default: 50, at most 500]

## `runnerctl job <ID>`

Show one job: its timeline and wait, steps with their durations, and its CPU and what held it back, memory, network and build cache

- `<ID>`

## `runnerctl logs <ID>`

Print a job's log

- `<ID>`
- `-f`, `--follow`: Keep printing lines as they arrive, until the job's record is final
- `--from <FROM>`: Start at this line, 0-based

## `runnerctl watch`

Live view of slots, running jobs and their steps, queued jobs with why each waits, and finished jobs.

Redraws in place on a terminal; otherwise prints one line per event.

## `runnerctl tui`

Full-screen dashboard: what needs attention, CI's health, the budget and each slot's job, the queue with why each job waits, the jobs with search, filters and a timeline, insights, sizing and caches, the doctor's checks, and each job's steps, failure and folded log, with cancel and re-run.

Keys: 1-5 the views, tab and j/k to move, enter to open, / to search, ? for all. The mouse clicks and scrolls unless --no-mouse or RUNNERCTL_NO_MOUSE is set.

- `--no-mouse`: Leave the mouse to the terminal: no clicks or wheel in the TUI, and the terminal's own selection works. RUNNERCTL_NO_MOUSE set to anything but empty does the same

## `runnerctl insights`

Show what the history says: what the job minutes would have cost on GitHub's runners, jobs that got slower and stayed so, fail and pass on a re-run, run near their memory cap or miss the build cache, downloads slowed for want of memory, and how long each repository's jobs wait this week against the last. One line per insight

- `--days <DAYS>`: Days of history to read [default: 14, at most 90]
- `--digest`: Show the last 24 hours instead: jobs finished and failed, new insights and the longest waits

## `runnerctl caches`

Show the cache volumes, their sizes and limits, the build cache's hits, and whether it serves GitHub Actions' cache

- `--clean <VOLUME>`: Empty this volume (a slot layer or a shared cache) instead; refused while a runner uses it

## `runnerctl config`

Show or check the daemon's config

### `runnerctl config show`

Print the config the daemon runs with, and its path

### `runnerctl config check [FILE]`

Check a config file against the daemon's budget, as `runnerd check-config` does; without a file, the daemon's own config

- `[FILE]`

## `runnerctl doctor`

Check that the host can run runners. Runs `runnerd doctor` here when the daemon is not running

## `runnerctl web`

Open the web dashboard with a one-time login link, and print one for each of `[web] origins`

- `--no-open`: Print the links instead of opening one. Over SSH they are printed anyway: a browser opened there would be the remote machine's, and spend the link where nobody sits
- `--base <ORIGIN>`: The address of a proxy in front of the dashboard to log in through instead, such as `https://host.example.ts.net:8443`, over `[web] open`. `[web] origins` need not be named here
- `--local`: Open the dashboard on its loopback address, over `[web] open`
- `--qr`: Print a QR code of a fresh link on each of `[web] origins`, or on `--base`, that signs a phone in within 5 minutes
- `--sign-out-all`: Sign every browser out of the dashboard, with a new session secret

## Every command

- `--socket <SOCKET>`: The daemon's API socket [default: `$XDG_RUNTIME_DIR/runnerd.sock`]
- `--json`: Print the API's JSON instead of text: the response type unchanged, one event per line for `watch`
