---
title: Your first job
description: Send a workflow job to runnerd, watch it start, and run the service day to day.
---

## Send a job here

In a workflow of a repository `runnerd.toml` serves:

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, x64]
```

A job runs here only when every label in its `runs-on` is in its repository's `labels` in `runnerd.toml`. Other jobs, such as `runs-on: ubuntu-latest`, stay on GitHub's runners. To send some jobs of a repository to a smaller runner, see [Split light and heavy jobs](../../guides/sizing/#split-light-and-heavy-jobs).

Push, then watch it start:

```sh
runnerctl status    # slots, queues, and why each queued job waits
runnerctl web       # the dashboard, signed in
runnerctl tui       # the same in the terminal
```

A job that waits says why. [Why is my job waiting?](../../guides/waiting/) lists the reasons.

## The service

`runnerd init` writes a user unit that runs `runnerd serve`:

```ini
[Unit]
Description=runnerd: ephemeral GitHub Actions runners
Documentation=https://ayagmar.github.io/runnerd/

[Service]
Type=exec
ExecStart="/home/dana/.local/bin/runnerd" serve --config "/home/dana/.config/runnerd/runnerd.toml"
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now runnerd   # start now and at login
loginctl enable-linger "$USER"          # keep it running while you are logged out
journalctl --user -u runnerd -f         # its log
systemctl --user restart runnerd        # after editing runnerd.toml by hand
systemctl --user stop runnerd           # exits within 45s; running jobs carry on
```

`systemctl stop`, SIGTERM and SIGINT remove idle and warm runners and exit within 45 seconds, even if Docker or GitHub do not answer. Runners of running jobs and the registry mirror stay up for the next start to adopt; meanwhile those jobs restore and save nothing in the caches runnerd serves. A second signal kills running jobs.

`runnerd stop` waits for running jobs, then removes every runner and the mirror, so nothing is left behind. `runnerd stop --force` kills running jobs instead.

A config saved through the dashboard applies without a restart; [Reloading](../../reference/configuration/#reloading) says what waits for one.
