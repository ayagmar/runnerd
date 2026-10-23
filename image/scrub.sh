#!/bin/bash
# Runs as root when the runner exits (entrypoint.sh), before runnerd may keep the stopped container as a failed job's
# record: deletes the runner's registration and credentials and the files known to hold a job's secrets, so no record
# holds them. runnerd checks a container's changes for the same paths before it keeps one (runnerd_core::record).
# $1 is the root to scrub, for runnerd's test; `/` when empty.
set -uo pipefail
root=${1:-}
home=$root/home/runner

rm -rf "$root/run/runnerd" \
    "$home/.runner" "$home/.credentials" "$home/.credentials_rsaparams" \
    "$home/.docker/config.json" "$home/.git-credentials" "$home/.netrc" "$home/.npmrc" "$home/.pypirc" \
    "$home/.cargo/credentials" "$home/.cargo/credentials.toml" \
    "$home/.ssh" "$home/.config/gh" "$home/.kube" \
    "$home/_work/_temp"
# The token actions/checkout puts in a repository's config, if the job ended before its post step took it out.
find "$home/_work" -path "$home/_work/_*" -prune -o -type f \
    \( -path '*/.git/config' -o -path '*/.git/modules/*/config' \) -print0 2> /dev/null \
    | xargs -0 -r sed -i '/extraheader/d'
exit 0
