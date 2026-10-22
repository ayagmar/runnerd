#!/bin/bash
# Runs as root after every job (job-completed.sh) and when the container starts. The container itself is removed
# after its one job; what outlives it is the slot's /state volume, so this removes what the job left in the Docker
# Engine there and keeps its disk within budget. Every step is best effort.
set -uo pipefail
# shellcheck source=SCRIPTDIR/layers.sh
. /usr/local/lib/runnerd-layers.sh

# For the job log: everything the container received during the job, the mirror's share included.
if [ -f /home/runner/.runnerd-job-rx ]; then
    rx=$(($(cat /sys/class/net/eth0/statistics/rx_bytes) - $(cat /home/runner/.runnerd-job-rx)))
    echo "runnerd: this job received $((rx / 1048576)) MiB"
fi

# Containers, volumes and networks the job started (Testcontainers, services, `docker compose up`).
docker ps -aq | xargs -r docker rm -f > /dev/null
docker volume prune -af > /dev/null
docker network prune -f > /dev/null
[ "$(size_gb /state/docker)" -le "$DOCKER_BUDGET_GB" ] || docker system prune -af > /dev/null
exit 0
