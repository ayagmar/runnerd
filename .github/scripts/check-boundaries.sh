#!/bin/sh
# Clients (every workspace crate but the daemon and core) talk to the daemon's API only: none of them may build the
# Docker or GitHub clients, which come with runnerd-core's `host` feature. See ARCHITECTURE.md.
set -eu

status=0
metadata=$(cargo metadata --no-deps --format-version 1 --locked)
clients=$(printf '%s' "$metadata" | jq -r '.packages[].name | select(. != "runnerd" and . != "runnerd-core")')
[ -n "$clients" ] || { echo "no client crates found" >&2; exit 1; }
for client in $clients; do
  tree=$(cargo tree --locked -p "$client" -e normal,build,features --prefix none)
  found=$(echo "$tree" | grep -E '^(bollard|reqwest) v|^runnerd-core feature "host"' | sort -u || true)
  if [ -n "$found" ]; then
    printf '%s must not depend on:\n%s\n' "$client" "$found" >&2
    status=1
  else
    echo "$client: ok"
  fi
done
exit "$status"
