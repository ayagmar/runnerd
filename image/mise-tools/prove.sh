#!/bin/bash
# The proof that a mise-tools image holds the pinned tools (docs/adr/0149). build.sh runs it in the candidate image as
# `runner`, with no network. mise, with fresh empty data, cache, state and config dirs, no hooks, and the build's own
# config (mise.toml's [tools]) with mise.lock, must install nothing; then each locked tool version's program must
# resolve to a real path inside that version's directory under the system installs, run with a literal argv, and
# print exactly its locked version. It shows the tools are there and used, not which digest the build checked: that is
# the build's (test.sh's wrong-digest builds).
#
# CHECKS holds `<tool>=<program> [<argument>...]` lines for a tool whose version command is not `<name> --version`.
# The program is a name mise resolves in that tool version; arguments are split on spaces and never reach a shell.
set -euo pipefail

mise=/opt/mise/bin/mise
system=$(realpath -e /usr/local/share/mise/installs)
work=$(mktemp -d)
export MISE_DATA_DIR="$work/data" MISE_CACHE_DIR="$work/cache" MISE_STATE_DIR="$work/state" \
  MISE_CONFIG_DIR="$work/config" MISE_SYSTEM_CONFIG_DIR="$work/etc" MISE_TRUSTED_CONFIG_PATHS="$work/project" \
  MISE_YES=1 MISE_NO_HOOKS=1 MISE_LOCKED=1
mkdir "$work/project" "$work/etc"
cp /opt/mise/share/build/mise.toml /opt/mise/share/build/mise.lock "$work/project/"
cd "$work/project"

fail() {
  echo "proof failed: $*" >&2
  exit 1
}

[ "$(id -un)" = runner ] || fail "runs as $(id -un), not runner"
[ ! -w "$system" ] || fail "$system is writable by runner"
/opt/mise/libexec/runnerd-mise-inputs tools mise.lock >"$work/inventory" || fail "cannot read mise.lock's tools"
[ -s "$work/inventory" ] || fail "mise.lock lists no tool"
awk -F'\t' 'NF != 4 { exit 1 }' "$work/inventory" || fail "the tool list is malformed"

"$mise" install --locked || fail "mise install --locked tried to install something; there is no network"
if [ -d "$MISE_DATA_DIR/installs" ] && [ -n "$(ls -A "$MISE_DATA_DIR/installs")" ]; then
  fail "mise installed into the job's data dir: $(ls "$MISE_DATA_DIR/installs")"
fi

checked=0
while IFS=$'\t' read -r tool request version name; do
  argv=("$name" --version)
  while IFS= read -r line; do
    if [ -n "$line" ] && [ "${line%%=*}" = "$tool" ]; then
      read -r -a argv <<<"${line#*=}"
    fi
  done <<<"${CHECKS:-}"
  program=${argv[0]}
  [[ "$program" =~ ^[A-Za-z0-9._+-]+$ ]] || fail "$tool: program '$program' is not a bare name"

  where=$("$mise" where "$tool@$request") || fail "$tool@$request: mise does not find it installed"
  dir=$(realpath -e "$where") || fail "$tool@$request: $where does not exist"
  case "$dir/" in
    "$system"/*/*/) ;;
    *) fail "$tool@$request: installed at $dir, not under $system" ;;
  esac
  which=$("$mise" which --tool "$tool@$request" "$program") || fail "$tool@$request: has no program $program"
  path=$(realpath -e "$which") || fail "$tool@$request: $which does not exist"
  case "$path" in
    "$dir"/*) ;;
    *) fail "$tool@$request: $program is $path, outside $dir" ;;
  esac

  output=$("$path" "${argv[@]:1}" 2>&1) || fail "$tool@$request: $path ${argv[*]:1} failed: $output"
  printed=$(grep -oE '[0-9]+(\.[0-9]+)+' <<<"$output" | head -n 1 || true)
  [ "$printed" = "$version" ] || fail "$tool@$request: $path printed version '$printed', not $version: $output"
  echo "ok $tool@$request $path $printed"
  checked=$((checked + 1))
done <"$work/inventory"
[ "$checked" -eq "$(wc -l <"$work/inventory")" ] || fail "checked $checked tools of $(wc -l <"$work/inventory")"
echo "proof passed: $checked tool versions run from $system, and nothing was downloaded"
