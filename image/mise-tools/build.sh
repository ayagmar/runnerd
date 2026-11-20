#!/bin/bash
# Builds a runner image with a repository's pinned mise and the tools of its mise.lock on one of runnerd's runner
# images, then proves it with no network (prove.sh). docs/adr/0148 says what it guarantees; the site's "Pinned tools"
# page says how to use it.
#
#   image/mise-tools/build.sh --base ghcr.io/ayagmar/runnerd-runner:jvm-0.1.4@sha256:<digest> \
#     --mise-version 2026.10.18 --mise-sha256 <sha256 of the extracted mise binary> \
#     --project <directory with the reviewed mise.toml and mise.lock> [--name runnerd-mise] \
#     [--check '<tool>=<version command>']...
#
# Prints the image's tag last. The tag ends in a hash of every input, so another lock, config, mise, base or recipe
# makes another tag.
set -euo pipefail

recipe=$(cd "$(dirname "$0")" && pwd)
name=runnerd-mise
base='' mise_version='' mise_sha256='' project='' checks=''
usage() {
  sed -n '6,9p' "$0" | sed 's/^#   \{0,1\}//' >&2
  exit 2
}
refuse() {
  echo "refused: $*" >&2
  exit 1
}
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || usage
  case "$1" in
    --base) base=$2 ;;
    --mise-version) mise_version=$2 ;;
    --mise-sha256) mise_sha256=$2 ;;
    --project) project=$2 ;;
    --name) name=$2 ;;
    --check) checks+="$2"$'\n' ;;
    *) usage ;;
  esac
  shift 2
done
[ -n "$base" ] && [ -n "$mise_version" ] && [ -n "$mise_sha256" ] && [ -n "$project" ] || usage

[[ "$base" =~ @sha256:[0-9a-f]{64}$ ]] || refuse "--base $base is not pinned by digest (<name>@sha256:<digest>)"
[[ "$mise_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || refuse "--mise-version $mise_version is not an exact version"
[[ "$mise_sha256" =~ ^[0-9a-f]{64}$ ]] || refuse "--mise-sha256 is not a SHA-256"
# runnerd removes the published repository's tags that no profile names, and would try to pull this one (ADR 0040).
case "$name" in
  ghcr.io/ayagmar/runnerd-runner) refuse "--name must not be runnerd's published repository" ;;
esac
[[ "$name" =~ ^[a-z0-9][a-z0-9._/:-]*$ ]] && [[ "$name" != *:* ]] || refuse "--name $name is not a repository name without a tag"
for file in mise.toml mise.lock; do
  [ -f "$project/$file" ] || refuse "$project has no $file"
done

sha() { sha256sum "$1" | cut -d' ' -f1; }
lock_sha256=$(sha "$project/mise.lock")
toml_sha256=$(sha "$project/mise.toml")
recipe_sha256=$(cat "$recipe/Dockerfile" "$recipe/inputs.py" | sha256sum | cut -d' ' -f1)
key=$(printf '%s\n' "$base" "$mise_version" "$mise_sha256" "$lock_sha256" "$toml_sha256" "$recipe_sha256" |
  sha256sum | cut -c1-16)
tag="$name:mise-$mise_version-$key"

echo "building $tag" >&2
docker build \
  --build-context "project=$project" \
  --build-arg "BASE=$base" \
  --build-arg "MISE_VERSION=$mise_version" \
  --build-arg "MISE_SHA256=$mise_sha256" \
  --build-arg "LOCK_SHA256=$lock_sha256" \
  --build-arg "TOML_SHA256=$toml_sha256" \
  --tag "$tag" \
  "$recipe" >&2

echo "proving $tag with no network" >&2
if ! docker run --rm --network none --user runner --entrypoint bash \
  --env "CHECKS=$checks" \
  --volume "$recipe/prove.sh:/usr/local/lib/runnerd-mise-prove.sh:ro" \
  "$tag" /usr/local/lib/runnerd-mise-prove.sh >&2; then
  docker rmi "$tag" >/dev/null || true
  refuse "$tag failed its proof and was removed"
fi
echo "$tag"
