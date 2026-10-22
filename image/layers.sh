# shellcheck shell=bash
# shellcheck disable=SC2034 # used by the scripts that source this file
# Sourced by entrypoint.sh, job-started.sh, job-cleanup.sh and seed.sh.

# The slot's copy-on-write layers, as name:path. Each path shows the profile's read-only snapshot
# /base/<name> (seed.sh) over what the image has there, with this slot's own changes on top in
# /state/<name>/upper.
LAYERS=(cache:/home/runner/.cache tools:/home/runner/_work/_tool android:/opt/android-sdk)

# Disk budgets per slot, in GB. A layer whose changes outgrow LAYER_BUDGET_GB starts over from the snapshot when
# a container starts; Docker images and build cache beyond DOCKER_BUDGET_GB are pruned after a job.
LAYER_BUDGET_GB=4
DOCKER_BUDGET_GB=20

# Size of a directory in GB, rounded up; 0 when it does not exist.
size_gb() {
    local size
    size=$(du -sx --block-size=1G "$1" 2> /dev/null | cut -f1) || true
    echo "${size:-0}"
}

# Bytes the container received on eth0. From /proc/net/dev, which is the container's own under sysbox, where
# /sys/class/net shows the host's interfaces. The count follows the colon without a space once it is long.
rx_bytes() {
    sed -n 's/^ *eth0: *\([0-9]*\).*/\1/p' /proc/net/dev
}
