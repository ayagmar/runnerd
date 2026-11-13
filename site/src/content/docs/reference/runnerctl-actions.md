---
title: runnerctl actions
description: The `runnerctl` commands that change something, and their options, as `--help` prints them.
---

<!-- Written by site/scripts/reference/cli.py from `runnerctl --help`. Change the help, then run it. -->

Command-line client for the runnerd daemon

The other commands are on [runnerctl](../runnerctl/).

## `runnerctl pause [REPOSITORY]`

Give a repository, or every one, no new runners, also after a restart. Refused while it runs a job, unless --force

- `[REPOSITORY]`: owner/name
- `--all`: Pause every configured repository; refused while any of them runs a job, unless --force
- `--force`: Cancel the running jobs' workflow runs and remove their runners

## `runnerctl cancel <JOB>`

Cancel a job's workflow run on GitHub: every job of the run still queued or running stops. Shows what it would do and the command that sends it, with the confirmation the daemon issued for what it showed

- `<JOB>`: The job's ID, as `runnerctl jobs` lists it
- `--confirm <TOKEN>`: Send what a preview showed: its confirmation, valid for 5 minutes and once
- `--force`: Send it without showing what it does first, for scripts

## `runnerctl rerun <JOB>`

Re-run a job on GitHub, with the jobs that depend on it, in a new attempt of its run. Shows what it would do and the command that sends it, with the confirmation the daemon issued for what it showed

- `<JOB>`: The job's ID, as `runnerctl jobs` lists it
- `--failed`: Re-run the failed jobs of the job's run instead, with the jobs that depend on them
- `--confirm <TOKEN>`: Send what a preview showed: its confirmation, valid for 5 minutes and once
- `--force`: Send it without showing what it does first, for scripts

## `runnerctl resume [REPOSITORY]`

Let a paused repository, or every one, get runners again

- `[REPOSITORY]`: owner/name
- `--all`: Resume every configured repository

## `runnerctl reseed <PROFILE>`

Make a profile's slot 1 layers its new snapshot, once its slots are free; waits until it is done

- `<PROFILE>`
- `--no-wait`: Return once the daemon accepted the request

## `runnerctl shell <JOB>`

Open a shell in a failed job's record: its runner as the job left it, in the job's workspace.

Shows what the failed step ran first. Exiting the shell removes its container.

- `<JOB>`
- `--step <STEP>`: Show what this step ran instead, by its number in `runnerctl job`

## `runnerctl records`

List the records of failed jobs, their sizes and the limits they are kept within

### `runnerctl records rm [JOB]`

Remove records and free the disk they take

- `[JOB]`: The job whose record goes
- `--all`: Remove every record, but those a shell is open in
- `--older-than <AGE>`: Remove the records older than this, such as 2d or 12h

## `runnerctl sizing`

Show what each profile's jobs use: percentiles of their sampled memory and CPU peaks, how long they waited and how busy the slots were, what to set each profile's memory and slots to, and what auto sizing does next

- `--since <SINCE>`: Only jobs queued in the last 30m, 12h, 7d, 2w, … [default: 30d]

### `runnerctl sizing apply <PROFILE>`

Write a profile's advised memory and slots into the daemon's config, after showing the change and asking.

The daemon checks the new config against the slice's MemoryMax before it writes and reloads it, as for any other config.

- `<PROFILE>`
- `--yes`: Write it without asking

### `runnerctl sizing undo <PROFILE>`

Write back a profile's memory and slots from before auto sizing last changed them, after showing the change and asking

- `<PROFILE>`
- `--yes`: Write it without asking

## `runnerctl notify`

Check the notification targets of `[notify]`

### `runnerctl notify test`

Send every target a test message, or only the one named, and say what each did

- `--target <TARGET>`: Only the target with this name

## Every command

- `--socket <SOCKET>`: The daemon's API socket [default: `$XDG_RUNTIME_DIR/runnerd.sock`]
- `--json`: Print the API's JSON instead of text: the response type unchanged, one event per line for `watch`
