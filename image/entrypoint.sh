#!/bin/bash
# Runs as root under `dind` (cgroup nesting): lays out this slot's state, starts the container's Docker Engine, then
# runs the GitHub Actions runner as the unprivileged `runner` user from a just-in-time config. The runner takes one
# job and exits; scrub.sh deletes its credentials and what the job left among the runner's dot-files and in /tmp,
# probe.py tries the calls Sysbox serves, and runnerd records its verdict from the container's exit code, then removes
# the container, or keeps it as the record of a failed job.
set -euo pipefail

# The image also puts the slot's writable tool cache on PATH. Only jobs get that path.
runner_path=$PATH
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
cd /

# runnerd puts the runner's just-in-time config here before it starts the container: never in its environment, which
# Docker stores and clients may log.
jitconfig_file=/run/runnerd/jitconfig
[ -f "$jitconfig_file" ] || { echo "$jitconfig_file is missing: runnerd starts runner containers" >&2; exit 1; }
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh
# shellcheck source=SCRIPTDIR/build-cache.sh
. /usr/local/lib/runnerd-build-cache.sh

# The slot's own pnpm store beside the job's workspace, on the slot's volume, when runnerd asks for it
# (docs/adr/0167); runnerd then mounts no shared store. First: the tool cache's layer mounts inside the workspace.
if [[ ${RUNNERD_PNPM_SLOT_STORE:-} =~ ^[0-9]+$ ]]; then
    mount_slot_work "$RUNNERD_PNPM_SLOT_STORE" || true
else
    # A slot that kept its own store before its runners took the shared one again gives it up, with its workspace.
    rm -rf --one-file-system "$SLOT_WORK"
fi
mount_layers
start_build_cache
# pnpm processes sharing the store must share their lock directory (Dockerfile). Beside the slot's own store, which
# one container uses at a time, it is the container's own directory.
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
touch "$JOB_SINCE_FILE"
# Not as `--jitconfig`: any user of the host reads a process's arguments. The listener reads the config from its
# environment, which only root and the runner user read, and drops it from what its job's processes inherit.
ACTIONS_RUNNER_INPUT_JITCONFIG=$jitconfig /usr/bin/setpriv --reuid=runner --regid=runner --init-groups -- \
    /usr/bin/env "PATH=$runner_path" ./run.sh &
runner=$!
unset jitconfig
# docker stop's signal goes to the runner's listener, which cancels its job; run.sh passes on no signal. Before the
# listener runs, run.sh takes it. Do not probe on a requested stop.
stopping=0
trap 'stopping=1; pkill -TERM -x Runner.Listener || kill -TERM "$runner" 2> /dev/null || true' TERM INT
status=0
wait "$runner" || status=$?
# A trapped signal ends the wait before the runner exits.
while kill -0 "$runner" 2> /dev/null; do
    status=0
    wait "$runner" || status=$?
done
trap 'stopping=1' TERM INT
cd /
# Nothing of the runner's may write after the scrub: what it left running (a Gradle daemon) gets TERM, then 2 s to
# end, then KILL, within docker stop's 10 s.
if pkill -TERM -u runner 2> /dev/null; then
    for _ in $(seq 10); do
        pgrep -u runner > /dev/null || break
        sleep 0.2
    done
    pkill -KILL -u runner 2> /dev/null || true
fi
/usr/local/sbin/runnerd-scrub
# The job's workspace on the slot's volume goes with the job, as one on the container's disk goes with the container,
# but for the slot's pnpm store (docs/adr/0167). Not on a requested stop, which has seconds: the slot's next runner
# empties it before its job.
if [[ $stopping == 0 ]] && mountpoint -q "$WORK"; then
    empty_slot_work "$SLOT_WORK"
fi
# What the job-completed hook's cleanup measured of the slot's Docker store, for runnerd, which takes the last such
# line of the container's log (docs/adr/0156). Printed after every process of the runner's ended, and printed even
# without a measure, so a line a job wrote to this log earlier is never the last. It starts on a line of its own:
# what a killed process left unfinished on the last line must not swallow it.
report=$(cat "$STORE_REPORT_FILE" 2> /dev/null) || true
[[ $report =~ ^held=[0-9]+\ share=[0-9]+$ ]] || report=unmeasured
printf '\nrunnerd: docker store %s\n' "$report"
if [[ $stopping == 1 || ${RUNNERD_SYSBOX_PROBE:-} != 1 ]]; then
    # Leave the verdict codes reserved even if the runner itself returned one of them.
    if (( status >= 200 && status <= 205 )); then exit 1; fi
    exit "$status"
fi
# Wait asynchronously so a stop after the probe starts can interrupt this wait and discard the verdict.
/usr/bin/timeout --kill-after=1 3 /usr/local/sbin/runnerd-probe &
probe_pid=$!
probe=0
if [[ $stopping == 0 ]]; then wait "$probe_pid" || probe=$?; fi
if [[ $stopping == 1 ]]; then
    kill -TERM "$probe_pid" 2> /dev/null || true
    exit 143
fi
case $probe in
    200 | 201 | 202 | 203) ;;
    124 | 137) probe=204 ;;
    *) probe=205 ;;
esac
echo "runnerd: the runner exited with $status; the probe after it with $probe"
exit "$probe"
