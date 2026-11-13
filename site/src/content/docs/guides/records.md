---
title: How do I look inside a failed job?
description: Keep a failed job's runner as a record and open a shell in it at the failing step.
---

Turn on `record_failures = true` under the job's profile ([`[profiles.<name>]`](../../reference/configuration/#profilesname)). From then on, a failed job's stopped runner is committed to an image, `runnerd-record:<job>`, before it is removed. `runnerctl shell <job>` starts a container from it and opens bash in the job's workspace, after printing the step that failed and what it ran:

```console wrap=false
$ runnerctl shell 120055991489
runnerd: the record of job 120055991489 of halyard/ledger, CI / build
It failed at step 6: Unit tests (exit code 1)
Its log shows it ran:
    ./gradlew test --no-daemon
    shell: /usr/bin/bash -e {0}
You are in /home/runner/_work/ledger/ledger, as the job left it. Not kept: the job's Docker containers and images; what it changed in ~/.cache, the tool cache and the Android SDK, which show the profile's snapshot; and what it added or changed among your home's dot-files and in /tmp (logins, and tools in ~/.local or ~/.nvm), except ~/.rustup and ~/.cargo's bin, registry and git. Exit to remove this container.
runner@4be1c0d2a7e9:~/_work/ledger/ledger$
```

`--step 3` shows step 3 instead. The shell's container counts against the budget with the profile's memory cap and is refused when it does not fit. It mounts the caches read-only, blocks the profile's reseed while open, and ends with the shell, the client or after 8 hours.

A record keeps the workspace with build output and test reports, the runner's `_diag` logs (secrets masked), and what the job wrote outside volumes. It never keeps logins and tools in the runner's home (`.gitconfig`, `.aws`, `~/.local`, `~/.nvm`), `/tmp`, the JIT config, `_work/_temp` or the token `actions/checkout` puts in `.git/config`; runnerd refuses to keep a container that still holds any. runnerd does not read file contents, so a secret saved in the workspace stays.

Anyone who can use Docker on this host can read a record (`docker run runnerd-record:<job>`), so turn `record_failures` on only for profiles whose jobs they may see. A record takes about twice what the job wrote outside volumes. `runnerctl records` lists them; `runnerctl records rm <job>`, `--all` or `--older-than 2d` removes them.

[`[records]`](../../reference/configuration-more/#records) bounds how many records are kept, how much disk they take and for how long.
