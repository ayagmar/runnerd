---
title: Uninstall
description: Stop runnerd, then remove its service, its Docker volumes and images, its files and the slice.
---

## Stop it

```sh
runnerd stop                              # waits for running jobs; --force kills them
systemctl --user disable --now runnerd
rm ~/.config/systemd/user/runnerd.service
systemctl --user daemon-reload
```

`runnerd stop` removes every runner, deletes its registration on GitHub, and removes the registry mirror's container. `systemctl stop` alone would leave running jobs' runners for the next start to adopt.

## Docker

Everything runnerd makes in Docker is named `runnerd`:

| What | Name |
|---|---|
| a slot's layers, a profile's snapshot | volumes `runnerd-<profile>-slot-<n>`, `runnerd-<profile>-base` |
| the pnpm store, the Maven repository, the mirror's storage | volumes `runnerd-pnpm`, `runnerd-maven`, `runnerd-mirror` |
| the network runners and the mirror share | `runnerd` |
| records of failed jobs | images `runnerd-record:<job>` |
| runner images | `ghcr.io/ayagmar/runnerd-runner:<stage>-<version>`, or `runnerd-runner:<stage>` built on the host |

```sh
docker volume rm $(docker volume ls -q --filter name=^runnerd-)
docker network rm runnerd
docker rmi $(docker images -q runnerd-record) $(docker images -q ghcr.io/ayagmar/runnerd-runner)
```

The mirror runs the `registry` image, which other programs may use too; remove it with `docker rmi` if nothing else does.

## Files

| What | Where |
|---|---|
| `runnerd`, `runnerctl` | `~/.local/bin`, or the installer's `--prefix` |
| the config | `~/.config/runnerd/runnerd.toml` |
| the job history and the dashboard's session secret | `~/.local/state/runnerd/` (`runnerd.db` and its `-wal` and `-shm` files, `dashboard-session`), or `[history] database`'s directory |
| the build cache | `~/.cache/runnerd/build-cache`, or `[caches.build] directory` |

```sh
rm ~/.local/bin/runnerd ~/.local/bin/runnerctl
rm -r ~/.config/runnerd ~/.local/state/runnerd ~/.cache/runnerd
```

The socket and the login file in `$XDG_RUNTIME_DIR` go when the daemon stops.

## The slice

You wrote the slice's unit as root, so remove it the same way:

```sh
sudo rm /etc/systemd/system/runnerd.slice
sudo systemctl daemon-reload
```

`runnerd init` may instead have printed a drop-in, `/etc/systemd/system/runnerd.slice.d/runnerd.conf`, for a slice that already existed; remove that file.

Sysbox, Docker and the GitHub CLI were installed for runnerd but belong to the host; remove them as your distribution does.
