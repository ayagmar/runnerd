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
# The build cache first, least recently used first, to about half the budget: what images and running builds do not
# use. Only a store still over its budget then goes whole, images too.
docker builder prune -af --max-used-space "$((DOCKER_BUDGET_GB * 512))MB" > /dev/null
[ "$(size_gb "$DOCKER_DATA_ROOT")" -le "$DOCKER_BUDGET_GB" ] || docker system prune -af > /dev/null
exit 0
