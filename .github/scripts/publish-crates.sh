#!/bin/sh
# Publishes the workspace's crates to crates.io in dependency order, skipping any whose version is already there, and
# waits for the sparse index to list each before publishing the next. `cargo publish --workspace` gives up a minute
# after an upload the index has not listed yet, and a rerun then fails on the crate it did upload; this one can be
# rerun. Needs CARGO_REGISTRY_TOKEN.
set -eu

# How long to wait for the index to list one upload, in seconds.
wait_for=1800

version=$(cargo metadata --no-deps --format-version 1 --locked |
  jq -r '.packages[] | select(.name == "runnerd-core") | .version')

# Whether the sparse index lists version $version of crate $1 (every crate here has a name of four or more letters).
indexed() {
  path=$(printf '%s' "$1" | cut -c1-2)/$(printf '%s' "$1" | cut -c3-4)/$1
  curl -fsS "https://index.crates.io/$path" 2>/dev/null | jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null
}

# Whether crates.io's API has version $version of crate $1, which it does as soon as the upload is accepted.
uploaded() {
  curl -fsS -A "runnerd release (https://github.com/ayagmar/runnerd)" \
    "https://crates.io/api/v1/crates/$1/$version" >/dev/null 2>&1
}

for crate in runnerd-core runnerd-web runnerd runnerd-cli; do
  if indexed "$crate"; then
    echo "$crate $version is already on crates.io"
    continue
  fi
  # The upload is what matters: cargo's own wait for the index may time out after it, which the loop below covers.
  # A failure crates.io's API has no upload for is a real one.
  if ! cargo publish --locked -p "$crate" && ! uploaded "$crate"; then
    echo "publishing $crate $version failed" >&2
    exit 1
  fi
  waited=0
  until indexed "$crate"; do
    [ "$waited" -lt "$wait_for" ] || { echo "the index did not list $crate $version in ${wait_for}s" >&2; exit 1; }
    sleep 30
    waited=$((waited + 30))
  done
  echo "$crate $version is on crates.io"
done
