#!/bin/bash
# The runner's bundled node (externals/node*/bin/node), which the Dockerfile moved to node.real beside this. With
# GitHub Actions' cache on this runner (build-cache.sh), every node action the runner starts loads the preload that
# points it at runnerd (docs/adr/0082); a job's own NODE_OPTIONS still applies on top. Nothing reaches the job's
# environment, so its own containers get nothing. In a `container:` job's container, which mounts these externals
# without the preload or the port, node runs as it is. bash, not sh: dash drops variables whose names have a hyphen,
# and an action's inputs arrive as such, INPUT_FETCH-DEPTH.
real="${0%/*}/node.real"
preload=/usr/local/lib/runnerd-actions-cache.cjs
if [ -n "${RUNNERD_ACTIONS_CACHE_PORT:-}" ] && [ -r "$preload" ]; then
    exec "$real" --require="$preload" "$@"
fi
exec "$real" "$@"
