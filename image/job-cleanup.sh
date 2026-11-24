#!/bin/bash
# Runs as root after every job (job-completed.sh) and when the container starts. The container itself is removed
# after its one job; what outlives it is the slot's /state volume, so this removes what the job left in the Docker
# Engine there and keeps its disk within budget. Every step is best effort.
set -uo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

# For the job log: everything the container received during the job, the mirror's share included.
start=$(cat /home/runner/.runnerd-job-rx 2> /dev/null) now=$(rx_bytes)
if [ -n "$start" ] && [ -n "$now" ]; then
    echo "runnerd: this job received $(((now - start) / 1048576)) MiB"
fi

# Containers, volumes and networks the job started (Testcontainers, services, `docker compose up`).
docker ps -aq | xargs -r docker rm -f > /dev/null
docker volume prune -af > /dev/null
docker network prune -f > /dev/null
# The build cache's records, as `[{Type, Size, Shared}]`, from one call (Engine API 1.42's `type`) given 10 s. Nothing
# when the Engine does not answer in time, or answers without a list of records that each have a type and a size:
# every prune then runs. A prune of a type the cache holds no record of removes nothing, and neither does one to a
# --max-used-space above what the records no image shares hold, which is what BuildKit weighs (docs/adr/0170).
cache=$(curl -sf --max-time 10 --unix-socket /var/run/docker.sock 'http://docker/system/df?type=build-cache' \
    | jq -ce '.BuildCache | if type != "array" then error("no list") else map(
        if (.Type | type) == "string" and (.Size | type) == "number" then {Type, Size, Shared: (.Shared == true)}
        else error("not a record") end) end') || cache=
holds() { [ -z "$cache" ] || jq -e --arg type "$1" 'any(.[]; .Type == $type)' <<< "$cache" > /dev/null; }
# Whether the records no image shares hold at least $1 bytes; MB is the CLI's MiB.
at_least() {
    [ -z "$cache" ] || jq -e --argjson bytes "$1" '(map(select(.Shared | not) | .Size) | add // 0) >= $bytes' \
        <<< "$cache" > /dev/null
}
# What the store holds once the job's containers and volumes are gone, before the build cache gives way: what the
# slot's jobs keep in it, which runnerd sizes each profile's share by (docs/adr/0156). Only after a job: the entrypoint
# prints it once the runner exits, as the last such line of the container's log.
after_job=0
if [ -e "$JOB_SINCE_FILE" ]; then
    after_job=1
    stat -c %Y "$JOB_SINCE_FILE" > "$LAST_JOB_FILE" 2> /dev/null || true
fi
for type in source.local source.git.checkout; do
    if holds "$type"; then
        docker builder prune -af --filter "type=$type" > /dev/null
    fi
done
held_bytes=
if [ "$after_job" = 1 ]; then
    # A walk that fails reports nothing, which the entrypoint prints as unmeasured: never a low size for the shares
    # to go by. A report an earlier run of this job's left goes with it.
    held_bytes=$(du -sx --block-size=1 "$DOCKER_DATA_ROOT" 2> /dev/null | cut -f1) || held_bytes=
    [[ $held_bytes =~ ^[0-9]+$ ]] || held_bytes=
    if [ -n "$held_bytes" ]; then
        (umask 077 && echo "held=$held_bytes share=$DOCKER_BUDGET_GB" > "$STORE_REPORT_FILE") || true
    else
        rm -f "$STORE_REPORT_FILE"
    fi
fi
# The build cache first, to about half the budget, in BuildKit's GC order (docker_gc_policy in layers.sh): build
# contexts and git checkouts went above, then layers, least recently used first, never one the slot's last job or this
# one used (recent_use). Cache mounts, the package stores that cost the network to fill again, stay. One positive
# `type=` filter per prune, as in the GC policy.
half="$((DOCKER_BUDGET_GB * 512))MB"
most="$((DOCKER_BUDGET_GB * 768))MB"
recent="until=$(recent_use)s"
# The store measured above holds what it held then unless this prune runs: no second walk of it.
if holds regular && at_least $((DOCKER_BUDGET_GB * 512 * 1048576)); then
    docker builder prune -af --filter type=regular --filter "$recent" --max-used-space "$half" > /dev/null
    held_bytes=
fi
# A store still over its budget loses the layers no recent job used, then every layer the build cache keeps, then its
# least recently used cache mounts down to three quarters of the budget (the GC's last rule, now rather than at its
# next run), then its images, and only then everything. The job's log names what went, which the slot's next job
# downloads again.
over() { [ "$(size_gb "$DOCKER_DATA_ROOT")" -gt "$DOCKER_BUDGET_GB" ]; }
# Whether a prune, whose report it reads, reclaimed anything.
reclaimed() { ! grep -q 'Total:[[:space:]]*0B$'; }
if [ -n "$held_bytes" ]; then
    held=$(((held_bytes + (1 << 30) - 1) >> 30))
else
    held=$(size_gb "$DOCKER_DATA_ROOT")
fi
if [ "$held" -gt "$DOCKER_BUDGET_GB" ]; then
    gone="its build layers no recent job used"
    docker builder prune -af --filter type=regular --filter "$recent" > /dev/null
    if over; then
        gone="its build layers"
        docker builder prune -af --filter type=regular > /dev/null
    fi
    if over && docker builder prune -af --max-used-space "$most" | reclaimed; then
        gone="$gone, least recently used cache mounts"
    fi
    over && docker image prune -af > /dev/null && gone="$gone, images"
    if over; then
        gone="everything, cache mounts too"
        docker system prune -af > /dev/null
    fi
    echo "runnerd: the slot's Docker store held $held GB of its $DOCKER_BUDGET_GB GB share; removed $gone," \
        "which the slot's next job downloads again"
fi
exit 0
