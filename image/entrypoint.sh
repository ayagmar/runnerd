#!/bin/bash
# Runs as root under `dind` (cgroup nesting): lays out this slot's state, starts the container's Docker Engine, then
# runs the GitHub Actions runner as the unprivileged `runner` user from a just-in-time config. The runner takes one
# job and exits; scrub.sh deletes its credentials, and runnerd removes the container, or keeps it as the record of a
# failed job.
set -euo pipefail

# runnerd puts the runner's just-in-time config here before it starts the container: never in its environment, which
# Docker stores and clients may log.
jitconfig_file=/run/runnerd/jitconfig
[ -f "$jitconfig_file" ] || { echo "$jitconfig_file is missing: runnerd starts runner containers" >&2; exit 1; }
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

mount_layers
# pnpm processes sharing the store must share their lock directory (Dockerfile).
install -d -o runner -g runner -m 700 "$XDG_RUNTIME_DIR"

dockerd --group docker > /var/log/dockerd.log 2>&1 &
for _ in $(seq 60); do
    docker info > /dev/null 2>&1 && break
    sleep 1
done
docker info > /dev/null || { cat /var/log/dockerd.log; exit 1; }

# Whatever the slot's last job left in its Docker Engine, if that job's container was stopped before its cleanup.
runnerd-job-cleanup

# The config registers this runner for one job, after which GitHub removes the runner. It is read once, and the
# file goes before any job runs.
jitconfig=$(< "$jitconfig_file")
rm -f "$jitconfig_file"
export HOME=/home/runner USER=runner LOGNAME=runner
cd "$HOME"
setpriv --reuid=runner --regid=runner --init-groups -- ./run.sh --jitconfig "$jitconfig" &
runner=$!
# docker stop's signal is the runner's, which cancels its job.
trap 'kill -TERM "$runner" 2> /dev/null' TERM INT
status=0
wait "$runner" || status=$?
# A trapped signal ends the wait before the runner exits.
while kill -0 "$runner" 2> /dev/null; do
    status=0
    wait "$runner" || status=$?
done
runnerd-scrub
exit "$status"
