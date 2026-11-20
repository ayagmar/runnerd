#!/bin/bash
# The mise-tools recipe's test (docs/adr/0148), run by CI's `mise-tools` job and by hand from the repository root:
#
#   image/mise-tools/test.sh [<base image pinned by digest>]
#
# Without a base it builds image/Dockerfile's `base` stage. Docker must have the containerd image store, so that an
# image built here has a digest to pin. It builds the fixture's image and its no-network proof, checks the labels,
# that the repository's hooks did not run at build time, and that another lock makes another tag; then that the
# recipe refuses a lock without a checksum, a tool's postinstall, a plugin and a wrong mise SHA-256. It removes every
# image it made.
set -euo pipefail

recipe=$(cd "$(dirname "$0")" && pwd)
prefix=runnerd-mise-test
mise_version=2026.10.18
mise_sha256=e97c38973336aa2ff350fea50f47b412b266fe263b102b7200ff1e9b118c6942
work=$(mktemp -d)
cleanup() {
  docker images --format '{{.Repository}}:{{.Tag}}' | grep "^$prefix/" | xargs -r docker rmi -f >/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*" >&2
  exit 1
}

base=${1:-}
if [ -z "$base" ]; then
  docker build --target base --tag "$prefix/base:test" "$recipe/.." >&2
  # name:tag@digest: BuildKit finds a local image by digest only with its tag.
  digest=$(docker image inspect --format '{{index .RepoDigests 0}}' "$prefix/base:test" 2>/dev/null) \
    || fail "the base has no digest; Docker needs the containerd image store"
  base="$prefix/base:test@${digest#*@}"
fi

build() { # build <project dir> <log> [<mise sha256>]
  "$recipe/build.sh" --base "$base" --mise-version "$mise_version" --mise-sha256 "${3:-$mise_sha256}" \
    --project "$1" --name "$prefix/mise" >"$2" 2>&1
}
project() { # project <name>: a copy of the fixture to change
  cp -r "$recipe/tests/fixture" "$work/$1"
  echo "$work/$1"
}

echo "the fixture builds and proves itself" >&2
build "$recipe/tests/fixture" "$work/ok.log" || { cat "$work/ok.log" >&2; fail "the fixture did not build"; }
tag=$(tail -n 1 "$work/ok.log")
grep -q '^proof passed' "$work/ok.log" || fail "no proof in the log"
label() { docker image inspect --format "{{index .Config.Labels \"$1\"}}" "$tag"; }
[ "$(label runnerd.mise.version)" = "$mise_version" ] || fail "runnerd.mise.version"
[ "$(label runnerd.mise.sha256)" = "$mise_sha256" ] || fail "runnerd.mise.sha256"
[ "$(label runnerd.mise.lock-sha256)" = "$(sha256sum "$recipe/tests/fixture/mise.lock" | cut -c1-64)" ] \
  || fail "runnerd.mise.lock-sha256"
[ "$(label runnerd.mise.base)" = "$base" ] || fail "runnerd.mise.base"
# The base's labels stay, as runnerd.version does on a published base: runnerd reads them (docker::image_syncs).
[ "$(label runnerd.skip-syncs)" = 1 ] || fail "the base's labels are lost"
docker run --rm --entrypoint bash "$tag" -c '[ ! -e /usr/local/share/mise/hook-ran ]' \
  || fail "the repository's postinstall hook ran at build time"

echo "another lock makes another tag" >&2
changed=$(project changed)
echo '# reviewed again' >>"$changed/mise.lock"
build "$changed" "$work/changed.log" || { cat "$work/changed.log" >&2; fail "the changed lock did not build"; }
[ "$(tail -n 1 "$work/changed.log")" != "$tag" ] || fail "a changed lock kept the tag $tag"

refused() { # refused <project dir> <what the log must say> [<mise sha256>]
  if build "$1" "$work/refused.log" "${3:-}"; then
    fail "built what it must refuse: $2"
  fi
  grep -qF -- "$2" "$work/refused.log" || { cat "$work/refused.log" >&2; fail "the refusal does not say: $2"; }
}

echo "a lock without a checksum is refused" >&2
dir=$(project no-checksum)
sed -i '/^checksum = .*020468de/d' "$dir/mise.lock"
refused "$dir" "refused: jq@1.8.1: no linux-x64 checksum in mise.lock"

echo "a tool's postinstall is refused" >&2
dir=$(project postinstall)
sed -i 's|^jq = "1.8.1"|jq = { version = "1.8.1", postinstall = "id" }|' "$dir/mise.toml"
refused "$dir" "refused: jq: option \`postinstall\` may run a command"

echo "a plugin is refused" >&2
dir=$(project plugin)
printf '\n[plugins]\nexample = "https://github.com/example/asdf-example"\n' >>"$dir/mise.toml"
refused "$dir" "refused: [plugins]"

echo "a wrong mise SHA-256 fails before mise runs" >&2
refused "$recipe/tests/fixture" "/tmp/mise/bin/mise: FAILED" "$(printf '0%.0s' {1..64})"
if grep -q "mise $mise_version" "$work/refused.log"; then fail "the unverified mise ran"; fi

echo "PASS" >&2
