#!/bin/bash
# Turns on Docker's containerd image store on a disposable hosted runner, so that an image built there has a digest
# to pin (image/mise-tools/test.sh). It keeps the daemon's other settings, writes the new config whole only after
# checking it, then restarts Docker. Never run it on a machine whose Docker serves anything else.
set -euo pipefail

config=/etc/docker/daemon.json
current=$(mktemp)
next=$(mktemp)
if sudo test -s "$config"; then
  sudo cat "$config" | tee "$current" >/dev/null
else
  echo '{}' >"$current"
fi
jq '.features["containerd-snapshotter"] = true' "$current" >"$next"
jq -e '.features["containerd-snapshotter"] == true' "$next" >/dev/null
sudo install -m 644 "$next" "$config"
sudo systemctl restart docker
docker info --format '{{json .DriverStatus}}' | grep -q io.containerd.snapshotter
