#!/bin/bash
# The proof that a mise-tools image holds the exact pinned tools (docs/adr/0148). build.sh runs it in the image as
# `runner`, with no network: mise, with a fresh empty data dir and the repository's own mise.toml and mise.lock, must
# install nothing, and each locked tool version must run from the image's system installs and print its version.
# CHECKS holds `tool=command` lines for a tool whose version command is not `<name> --version`.
set -euo pipefail

mise=/opt/mise/bin/mise
system=/usr/local/share/mise/installs
work=$(mktemp -d)
export MISE_DATA_DIR="$work/data" MISE_CACHE_DIR="$work/cache" MISE_STATE_DIR="$work/state" \
  MISE_CONFIG_DIR="$work/config" MISE_TRUSTED_CONFIG_PATHS="$work/project" MISE_YES=1
mkdir "$work/project"
cp /opt/mise/share/mise.toml /opt/mise/share/mise.lock "$work/project/"
cd "$work/project"

fail() {
  echo "proof failed: $*" >&2
  exit 1
}

[ "$(id -un)" = runner ] || fail "runs as $(id -un), not runner"
[ ! -w "$system" ] || fail "$system is writable by runner"
"$mise" install --locked || fail "mise install --locked tried to install something; there is no network"
if [ -d "$MISE_DATA_DIR/installs" ] && [ -n "$(ls -A "$MISE_DATA_DIR/installs")" ]; then
  fail "mise installed into the job's data dir: $(ls "$MISE_DATA_DIR/installs")"
fi

checked=0
while IFS=$'\t' read -r tool request version name; do
  command="$name --version"
  while IFS= read -r line; do
    [ "${line%%=*}" = "$tool" ] && command="${line#*=}"
  done <<<"${CHECKS:-}"
  program=${command%% *}
  # shellcheck disable=SC2016 # expanded by the shell mise exec starts
  path=$("$mise" exec "$tool@$request" -- sh -c 'command -v "$1"' _ "$program") \
    || fail "$tool@$request: $program is not on the PATH mise exec sets"
  case "$path" in
    "$system"/*) ;;
    *) fail "$tool@$request: $program is $path, not under $system" ;;
  esac
  output=$("$mise" exec "$tool@$request" -- sh -c "$command" 2>&1) || fail "$tool@$request: '$command' failed: $output"
  grep -qF -- "$version" <<<"$output" || fail "$tool@$request: '$command' printed no $version: $output"
  echo "ok $tool@$request $path: $(head -n 1 <<<"$output")"
  checked=$((checked + 1))
done < <(/opt/mise/libexec/runnerd-mise-inputs tools mise.lock)
[ "$checked" -gt 0 ] || fail "mise.lock lists no tool"
echo "proof passed: every tool of mise.lock runs from $system, and nothing was downloaded"
