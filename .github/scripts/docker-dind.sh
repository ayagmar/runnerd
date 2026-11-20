#!/bin/bash
# Starts the Docker Engine of image/Dockerfile's pinned docker:dind in a privileged container, with the containerd
# image store, for image/mise-tools/test.sh on a disposable hosted runner, whose own Docker is older than the 28.3.0
# that build.sh needs (BuildKit before it looks a local base's digest up on the registry). Its socket is
# <dir>/docker.sock, for DOCKER_HOST. The checkout is mounted at its own path, because the test bind-mounts files of
# it. Never run it on a machine whose Docker serves anything else.
#
#   .github/scripts/docker-dind.sh <socket dir>
set -euo pipefail

[ $# -eq 1 ] || {
  echo "usage: $0 <socket dir>" >&2
  exit 2
}
dir=$1
root=$(cd "$(dirname "$0")/../.." && pwd)
image=$(sed -n 's/^FROM \(docker:[^ ]*-dind@sha256:[0-9a-f]\{64\}\) AS docker$/\1/p' "$root/image/Dockerfile")
[ -n "$image" ] || {
  echo "image/Dockerfile pins no docker:dind" >&2
  exit 1
}
mkdir -p "$dir"
docker run --detach --privileged --name runnerd-ci-dind \
  --env DOCKER_TLS_CERTDIR= \
  --volume "$root:$root" --volume "$dir:/run/dind" \
  "$image" --host=unix:///run/dind/docker.sock --feature=containerd-snapshotter --group="$(id -g)" >/dev/null
export DOCKER_HOST="unix://$dir/docker.sock"
for _ in $(seq 60); do
  docker version >/dev/null 2>&1 && break
  sleep 1
done
docker version --format 'Docker Engine {{.Server.Version}} at {{.Server.Os}}/{{.Server.Arch}}'
docker info --format '{{json .DriverStatus}}' | grep -q io.containerd.snapshotter
