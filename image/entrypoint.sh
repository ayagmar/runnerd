#!/bin/bash
# Runs as root under `dind` (cgroup nesting): lays out this slot's state, starts the container's Docker Engine, then
# runs the GitHub Actions runner as the unprivileged `runner` user from a just-in-time config. The runner takes one
# job and exits, and runnerd removes the container.
set -euo pipefail

: "${RUNNER_JITCONFIG:?RUNNER_JITCONFIG is not set: runnerd passes the runner its just-in-time config}"
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

# /state is this slot's volume: the Docker Engine's data (daemon.json) and the slot's changes to each layer.
for layer in "${LAYERS[@]}"; do
    name=${layer%%:*} path=${layer#*:}
    [ "$(size_gb "/state/$name")" -le "$LAYER_BUDGET_GB" ] || rm -rf "/state/$name"
    lower=$path
    [ ! -d "/base/$name" ] || lower=/base/$name:$lower
    mkdir -p "/state/$name/upper" "/state/$name/work"
    chown runner:runner "/state/$name/upper"
    # Full copies on copy-up and no index, so an upper never depends on the snapshot it was made against.
    mount -t overlay overlay \
        -o "lowerdir=$lower,upperdir=/state/$name/upper,workdir=/state/$name/work,index=off,metacopy=off" "$path"
done
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

# The config registers this runner for one job, after which GitHub removes the runner. Jobs do not inherit it in
# their environment.
jitconfig=$RUNNER_JITCONFIG
unset RUNNER_JITCONFIG
export HOME=/home/runner USER=runner LOGNAME=runner
cd "$HOME"
as_runner=(setpriv --reuid=runner --regid=runner --init-groups --)
# This SDK command-line tools release ignores `sdkmanager --licenses`; installing platform-tools records the
# license, after which Android builds may download the platform and build tools they need.
[ -d "$ANDROID_HOME/platform-tools" ] \
    || { yes || true; } | "${as_runner[@]}" "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --install platform-tools > /dev/null
exec "${as_runner[@]}" ./run.sh --jitconfig "$jitconfig"
