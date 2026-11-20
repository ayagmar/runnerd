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
# The image is built under a private candidate tag, proved by its image ID, and only then tagged with its release tag,
# which ends in a hash of every input, so another lock, config, mise, base or recipe makes another tag. An existing
# release tag on another image is never replaced. Prints the release tag pinned to the image ID last
# (<name>:<tag>@sha256:<id>), as a profile's `image` names it.
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
    --check)
      [[ "$2" =~ ^[A-Za-z0-9:/._-]+=[A-Za-z0-9._+-]+(\ [A-Za-z0-9._=+-]+)*$ ]] || refuse "--check $2 is not <tool>=<program> [<argument>...]"
      checks+="$2"$'\n'
      ;;
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
project=$(cd -- "$project" && pwd -P) || refuse "--project is not a directory"
for file in mise.toml mise.lock; do
  [ -f "$project/$file" ] || refuse "$project has no $file"
done

sha() { sha256sum -- "$1" | cut -d' ' -f1; }
lock_sha256=$(sha "$project/mise.lock")
toml_sha256=$(sha "$project/mise.toml")
recipe_sha256=$(cat "$recipe/Dockerfile" "$recipe/inputs.py" | sha256sum | cut -d' ' -f1)
key=$(printf '%s\n' "$base" "$mise_version" "$mise_sha256" "$lock_sha256" "$toml_sha256" "$recipe_sha256" |
  sha256sum | cut -c1-16)
tag="$name:mise-$mise_version-$key"

candidate="$name:candidate-$key-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
drop_candidate() { docker rmi -- "$candidate" >/dev/null 2>&1 || true; }

echo "building $candidate" >&2
if ! docker build \
  --build-context "project=$project" \
  --build-arg "BASE=$base" \
  --build-arg "MISE_VERSION=$mise_version" \
  --build-arg "MISE_SHA256=$mise_sha256" \
  --build-arg "LOCK_SHA256=$lock_sha256" \
  --build-arg "TOML_SHA256=$toml_sha256" \
  --iidfile "$scratch/id" \
  --tag "$candidate" \
  "$recipe" >&2; then
  drop_candidate
  refuse "the build failed"
fi
id=$(cat "$scratch/id")
[[ "$id" =~ ^sha256:[0-9a-f]{64}$ ]] || { drop_candidate; refuse "docker build gave no image ID"; }

echo "proving $id with no network" >&2
if ! docker run --rm --network none --user runner --entrypoint bash \
  --env "CHECKS=$checks" \
  --volume "$recipe/prove.sh:/usr/local/lib/runnerd-mise-prove.sh:ro" \
  "$id" /usr/local/lib/runnerd-mise-prove.sh >&2; then
  drop_candidate
  refuse "$id failed its proof; its candidate tag was removed"
fi

# Promotes the proved image, one build of a tag at a time; a tag already on another image stays as it is.
exec 9>"${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/runnerd-mise-${key}.lock"
flock 9
existing=$(docker image inspect --format '{{.Id}}' -- "$tag" 2>/dev/null || true)
if [ -n "$existing" ] && [ "$existing" != "$id" ]; then
  drop_candidate
  refuse "$tag is already image $existing; remove it first to replace it with $id"
fi
docker tag "$id" "$tag"
drop_candidate
flock -u 9
echo "$tag@$id"
