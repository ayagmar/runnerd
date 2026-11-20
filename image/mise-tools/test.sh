#!/bin/bash
# The mise-tools recipe's test (docs/adr/0149), run by CI's `mise-tools` job and by hand from the repository root:
#
#   image/mise-tools/test.sh [<base image as name:tag@sha256:digest>]
#
# Without a base it builds image/Dockerfile's `base` stage, which needs Docker's containerd image store so that a
# local image has a digest to pin. It checks:
# - the fixture builds, passes its no-network proof, carries its labels and SHA256SUMS, and did not run the
#   repository's hook; another lock makes another tag; a release tag on another image is never replaced;
# - refusals before any download: a missing digest, a tool option, a plugin, a templated version in both forms, a
#   Node install kind, PyPy, an http backend; and a wrong mise SHA-256 before mise runs;
# - for each accepted shape (aqua, github, core node, java and python; sha256 and blake3), a lock digest that is not
#   the download's makes mise refuse the bytes at build time, naming the lock's digest.
# It removes exactly the images it made; a base it was given stays.
set -euo pipefail

recipe=$(cd "$(dirname "$0")" && pwd)
prefix="runnerd-mise-test-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
# The newest tested mise: a row joins tested-mise.txt only with a passing run of this test on it.
read -r mise_version mise_sha256 < <(grep -v '^#' "$recipe/tested-mise.txt" | tail -n 1)
work=$(mktemp -d)
made=()
lock_held=''
cleanup() {
  [ -z "$lock_held" ] || docker rm "$lock_held" >/dev/null 2>&1 || true
  # Candidate tags build.sh left if it was killed, then the tags this run made; the prefix is this run's own.
  docker images --format '{{.Repository}}:{{.Tag}}' | grep -F "$prefix/mise:candidate-" | xargs -r docker rmi >/dev/null || true
  [ ${#made[@]} -eq 0 ] || docker rmi "${made[@]}" >/dev/null 2>&1 || true
  rm -rf -- "$work"
}
trap cleanup EXIT
fail() {
  echo "FAIL: $*" >&2
  exit 1
}

base=${1:-}
if [ -z "$base" ]; then
  docker build --target base --tag "$prefix/base:test" "$recipe/.." >&2
  made+=("$prefix/base:test")
  digest=$(docker image inspect --format '{{index .RepoDigests 0}}' "$prefix/base:test" 2>/dev/null) \
    || fail "the base has no digest; Docker needs the containerd image store"
  # name:tag@digest: BuildKit finds a local image by digest only with its tag.
  base="$prefix/base:test@${digest#*@}"
fi

build() { # build <project dir> <log> [<mise sha256>] [<mise version>]: on success the last log line is name:tag@id
  "$recipe/build.sh" --base "$base" --mise-version "${4:-$mise_version}" --mise-sha256 "${3:-$mise_sha256}" \
    --project "$1" --name "$prefix/mise" >"$2" 2>&1 || return 1
  made+=("$(tail -n 1 "$2" | sed 's/@sha256:.*//')")
}
project() { # project <name>: a copy of the fixture to change
  cp -r "$recipe/tests/fixture" "$work/$1"
  echo "$work/$1"
}

echo "the fixture builds and proves itself" >&2
build "$recipe/tests/fixture" "$work/ok.log" || { cat "$work/ok.log" >&2; fail "the fixture did not build"; }
ref=$(tail -n 1 "$work/ok.log")
tag=${ref%@*}
id=${ref#*@}
[ "$(docker image inspect --format '{{.Id}}' "$tag")" = "$id" ] || fail "$tag is not the proved image $id"
grep -q "^proving $id with no network" "$work/ok.log" || fail "the proof did not run on $id"
grep -q '^proof passed: 2 tool versions' "$work/ok.log" || fail "no proof of both tools in the log"
if docker images --format '{{.Repository}}:{{.Tag}}' | grep -qF "$prefix/mise:candidate-"; then
  fail "a candidate tag was left"
fi
label() { docker image inspect --format "{{index .Config.Labels \"$1\"}}" "$tag"; }
[ "$(label runnerd.mise.version)" = "$mise_version" ] || fail "runnerd.mise.version"
[ "$(label runnerd.mise.sha256)" = "$mise_sha256" ] || fail "runnerd.mise.sha256"
[ "$(label runnerd.mise.lock-sha256)" = "$(sha256sum "$recipe/tests/fixture/mise.lock" | cut -c1-64)" ] \
  || fail "runnerd.mise.lock-sha256"
[ "$(label runnerd.mise.base)" = "$base" ] || fail "runnerd.mise.base"
# The base's labels stay, as runnerd.version does on a published base: runnerd reads them (docker::image_syncs).
[ "$(label runnerd.skip-syncs)" = 1 ] || fail "the base's labels are lost"
docker run --rm --network none --entrypoint bash -v "$recipe/tests/fixture:/project:ro" "$tag" -c \
  '[ ! -e /usr/local/share/mise/hook-ran ] && cd /project && sha256sum --check --strict /opt/mise/share/SHA256SUMS' \
  >/dev/null || fail "the hook ran at build time, or SHA256SUMS does not match the fixture"
docker run --rm --entrypoint grep "$tag" -qx "$mise_sha256  /opt/mise/bin/mise" /opt/mise/share/SHA256SUMS \
  || fail "SHA256SUMS does not name the mise binary"

echo "another lock makes another tag; a tag on another image stays" >&2
changed=$(project changed)
echo '# reviewed again' >>"$changed/mise.lock"
build "$changed" "$work/changed.log" || { cat "$work/changed.log" >&2; fail "the changed lock did not build"; }
other=$(tail -n 1 "$work/changed.log")
[ "${other%@*}" != "$tag" ] || fail "a changed lock kept the tag $tag"
docker tag "${other#*@}" "$tag"
if build "$recipe/tests/fixture" "$work/taken.log"; then fail "replaced $tag, which named another image"; fi
grep -qF "$tag is already image ${other#*@}" "$work/taken.log" || { cat "$work/taken.log" >&2; fail "no refusal for $tag"; }
[ "$(docker image inspect --format '{{.Id}}' "$tag")" = "${other#*@}" ] || fail "$tag was moved"
docker tag "$id" "$tag"

echo "a build waits for another build's promotion lock on the same Docker daemon" >&2
key=${tag##*-}
lock_held="runnerd-mise-promote-$key"
docker create --name "$lock_held" --entrypoint true "$id" >/dev/null
docker tag "$id" "$prefix/mise:keep"
made+=("$prefix/mise:keep")
docker rmi "$tag" >/dev/null
if MISE_TOOLS_PROMOTION_WAIT=2 build "$recipe/tests/fixture" "$work/locked.log"; then
  fail "promoted $tag while another build held the lock"
fi
grep -qF "another build of $tag holds the promotion lock" "$work/locked.log" \
  || { cat "$work/locked.log" >&2; fail "no wait on the promotion lock"; }
if docker image inspect "$tag" >/dev/null 2>&1; then fail "$tag was tagged without the lock"; fi
docker rm "$lock_held" >/dev/null
lock_held=''
docker tag "$id" "$tag"

echo "proof rows come from the config's requests bound to their lock entries" >&2
dir=$(project specifiers)
sed -i 's|^specifiers = \["1.8.1"\]|specifiers = ["1.8", "1.8.1"]|' "$dir/mise.lock"
python3 "$recipe/inputs.py" tools "$dir" >"$work/rows" || fail "the specifiers fixture is refused"
grep -qP '^jq\t1\.8\.1\t1\.8\.1\t' "$work/rows" || { cat "$work/rows" >&2; fail "jq's row is not the request 1.8.1"; }

refused() { # refused <project dir> <what the log must say> [<mise sha256>] [<mise version>]
  if build "$1" "$work/refused.log" "${3:-}" "${4:-}"; then fail "built what it must refuse: $2"; fi
  grep -qF -- "$2" "$work/refused.log" || { cat "$work/refused.log" >&2; fail "the refusal does not say: $2"; }
}
before_download() {
  if grep -q 'releases/download/v[0-9.]*/mise-v' "$work/refused.log"; then fail "mise was downloaded first"; fi
}

echo "refusals" >&2
dir=$(project no-checksum)
sed -i '/^checksum = .*020468de/d' "$dir/mise.lock"
refused "$dir" "refused: jq@1.8.1: no full sha256 or blake3 linux-x64 digest"
before_download
dir=$(project option)
sed -i 's|^jq = "1.8.1"|jq = { version = "1.8.1", postinstall = "id" }|' "$dir/mise.toml"
refused "$dir" "is not a literal version; tables and options are refused"
before_download
dir=$(project plugin)
printf '\n[plugins]\nexample = "https://github.com/example/asdf-example"\n' >>"$dir/mise.toml"
refused "$dir" "refused: [plugins]"
before_download
dir=$(project template)
sed -i "s|^jq = \"1.8.1\"|jq = \"{{ exec(command='id') }}\"|" "$dir/mise.toml"
refused "$dir" "refused: jq: \"{{ exec(command='id') }}\" is not a literal version"
before_download
dir=$(project template-table)
sed -i "s|^jq = \"1.8.1\"|jq = { version = \"{{ exec(command='id') }}\" }|" "$dir/mise.toml"
refused "$dir" "is not a literal version; tables and options are refused"
before_download
dir=$(project trailing-newline)
sed -i 's|^jq = "1.8.1"|jq = "1.8.1\\n"|' "$dir/mise.toml"
refused "$dir" 'refused: jq: "1.8.1\n" is not a literal version'
before_download

echo "only a tested mise version and binary" >&2
refused "$recipe/tests/fixture" "refused: mise $mise_version with SHA-256 $(printf '0%.0s' {1..64}) has not passed" \
  "$(printf '0%.0s' {1..64})"
if grep -q '^building' "$work/refused.log"; then fail "an untested mise reached the build"; fi
refused "$recipe/tests/fixture" "refused: mise 2026.11.9 with SHA-256 $mise_sha256 has not passed" "" 2026.11.9
if grep -q '^building' "$work/refused.log"; then fail "an untested mise reached the build"; fi

one_tool() { # one_tool <name> <tool> <request> <lock entries>: a project of one tool
  mkdir "$work/$1"
  printf '[tools]\n"%s" = "%s"\n' "$2" "$3" >"$work/$1/mise.toml"
  printf 'lockfile_version = 3\n\n%s\n' "$4" >"$work/$1/mise.lock"
  echo "$work/$1"
}
node_entry() { # node_entry <digest> [<extra field line>] [<url suffix, TOML-escaped>]
  printf '[[tools.node]]\nversion = "24.22.1"\nbackend = "core:node"\nspecifiers = ["24.22.1"]\n%s\n\n' "${2:-}"
  printf '[tools.node."platforms.linux-x64"]\nchecksum = "%s"\n' "$1"
  printf 'url = "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-x64.tar.gz%s"\n' "${3:-}"
}
node_sha=sha256:eb6728e77c576a35162c122c45ccadf224d89ff00abc8dae46cc2897c6002d7c
refused "$(one_tool node-install node 24.22.1 "$(node_entry "$node_sha" 'install = "source"')")" \
  "refused: node@24.22.1: field \`install\` is not one this recipe has tested"
before_download
# mise parses the URL, which drops the newline, and then no longer finds it equal to the lock's: it would replace the
# lock's digest with nodejs.org's (src/plugins/core/node.rs).
refused "$(one_tool node-newline node 24.22.1 "$(node_entry "sha256:$(printf 'e%.0s' {1..64})" '' '\n')")" \
  'refused: node@24.22.1: linux-x64 url "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-x64.tar.gz\n" is not exactly'
before_download
pypy='[[tools.python]]
version = "pypy3.11-7.3.20"
backend = "core:python"
specifiers = ["pypy3.11-7.3.20"]

[tools.python."platforms.linux-x64"]
checksum = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
url = "https://downloads.python.org/pypy/pypy3.11-v7.3.20-linux64.tar.bz2"'
refused "$(one_tool pypy python pypy3.11-7.3.20 "$pypy")" \
  "refused: python@pypy3.11-7.3.20: only CPython 3.x.y is accepted"
before_download
http='[[tools."http:example"]]
version = "1.0.0"
backend = "http:example"
specifiers = ["1.0.0"]

[tools."http:example"."platforms.linux-x64"]
checksum = "sha256:1111111111111111111111111111111111111111111111111111111111111111"
url = "https://example.com/example-1.0.0.tar.gz"'
refused "$(one_tool http "http:example" 1.0.0 "$http")" "refused: \"http:example\": not a literal tool name"
before_download

echo "a lock digest that is not the download's fails the build, for each accepted shape" >&2
wrong() { # wrong <project dir> <the wrong digest>: mise downloads, compares with the lock's digest, refuses
  if build "$1" "$work/wrong.log"; then fail "built $1 with a wrong digest"; fi
  if ! grep -qF 'Checksum mismatch for file' "$work/wrong.log" || ! grep -qF "Expected: $2" "$work/wrong.log"; then
    cat "$work/wrong.log" >&2
    fail "$1: no checksum mismatch against the lock's $2"
  fi
  if grep -q '^proving' "$work/wrong.log"; then fail "$1: an image with wrong bytes reached its proof"; fi
}
bad=sha256:$(printf 'e%.0s' {1..64})
dir=$(project wrong-aqua)
sed -i "s|^checksum = \"sha256:020468de[0-9a-f]*\"|checksum = \"$bad\"|" "$dir/mise.lock"
wrong "$dir" "$bad"
dir=$(project wrong-github)
sed -i "s|^checksum = \"sha256:0d6640f8[0-9a-f]*\"|checksum = \"$bad\"|" "$dir/mise.lock"
wrong "$dir" "$bad"
wrong "$(one_tool wrong-node node 24.22.1 "$(node_entry "$bad")")" "$bad"
java="[[tools.java]]
version = \"temurin-25.0.5+8.0.LTS\"
backend = \"core:java\"
specifiers = [\"temurin-25.0.5+8\"]

[tools.java.\"platforms.linux-x64\"]
checksum = \"$bad\"
url = \"https://github.com/adoptium/temurin25-binaries/releases/download/jdk-25.0.5%2B8/OpenJDK25U-jdk_x64_linux_hotspot_25.0.5_8.tar.gz\""
wrong "$(one_tool wrong-java java temurin-25.0.5+8 "$java")" "$bad"
bad_blake3=blake3:$(printf 'b%.0s' {1..64})
python="[[tools.python]]
version = \"3.14.10\"
backend = \"core:python\"
specifiers = [\"3.14.10\"]

[tools.python.\"platforms.linux-x64\"]
checksum = \"$bad_blake3\"
url = \"https://github.com/astral-sh/python-build-standalone/releases/download/20261110/cpython-3.14.10+20261110-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz\""
wrong "$(one_tool wrong-python python 3.14.10 "$python")" "$bad_blake3"

echo "PASS" >&2
