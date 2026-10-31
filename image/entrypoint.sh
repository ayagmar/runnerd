#!/bin/bash
# Runs as root under `dind` (cgroup nesting): lays out this slot's state, starts the container's Docker Engine, then
# runs the GitHub Actions runner as the unprivileged `runner` user from a just-in-time config. The runner takes one
# job and exits; scrub.sh deletes its credentials and what the job left among the runner's dot-files and in /tmp, and
# runnerd removes the container, or keeps it as the record of a failed job.
set -euo pipefail

# runnerd puts the runner's just-in-time config here before it starts the container: never in its environment, which
# Docker stores and clients may log.
jitconfig_file=/run/runnerd/jitconfig
[ -f "$jitconfig_file" ] || { echo "$jitconfig_file is missing: runnerd starts runner containers" >&2; exit 1; }
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh
# shellcheck source=SCRIPTDIR/build-cache.sh
. /usr/local/lib/runnerd-build-cache.sh

mount_layers
start_build_cache
# pnpm processes sharing the store must share their lock directory (Dockerfile).
install -d -o runner -g runner -m 700 "$XDG_RUNTIME_DIR"

start_docker

# Whatever the slot's last job left in its Docker Engine, if that job's container was stopped before its cleanup.
runnerd-job-cleanup

# The config registers this runner for one job, after which GitHub removes the runner. It is read once, and the
# file goes before any job runs.
jitconfig=$(< "$jitconfig_file")
rm -f "$jitconfig_file"
export HOME=/home/runner USER=runner LOGNAME=runner
cd "$HOME"
# What the job changes from here on, runnerd-scrub deletes from the dot-files and temporary directories.
touch /run/runnerd-since
# Not as `--jitconfig`: any user of the host reads a process's arguments. The listener reads the config from its
# environment, which only root and the runner user read, and drops it from what its job's processes inherit.
ACTIONS_RUNNER_INPUT_JITCONFIG=$jitconfig setpriv --reuid=runner --regid=runner --init-groups -- ./run.sh &
runner=$!
unset jitconfig
# docker stop's signal goes to the runner's listener, which cancels its job; run.sh passes on no signal. Before the
# listener runs, run.sh takes it.
trap 'pkill -TERM -x Runner.Listener || kill -TERM "$runner" 2> /dev/null' TERM INT
status=0
wait "$runner" || status=$?
# A trapped signal ends the wait before the runner exits.
while kill -0 "$runner" 2> /dev/null; do
    status=0
    wait "$runner" || status=$?
done
# Nothing of the runner's may write after the scrub: what it left running (a Gradle daemon) gets TERM, then 2 s to
# end, then KILL, within docker stop's 10 s.
if pkill -TERM -u runner 2> /dev/null; then
    for _ in $(seq 10); do
        pgrep -u runner > /dev/null || break
        sleep 0.2
    done
    pkill -KILL -u runner 2> /dev/null || true
fi
runnerd-scrub
exit "$status"
