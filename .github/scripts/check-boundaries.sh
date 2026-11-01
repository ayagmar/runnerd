#!/bin/sh
# Clients (every workspace crate but the daemon and core) talk to the daemon's API only: none of them may build the
# Docker or GitHub clients, which come with runnerd-core's `host` feature. And they show the facts core computes: no
# client, nor the daemon's output, defines its own formatter or classifier for one. See ARCHITECTURE.md.
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

# Durations, ages, sizes and CPU figures are written by runnerd_core::api::units, ByteSize and api::cpus; a job's
# outcome, name, wait and run time, a slot's state and why a job waits are decided by core (ADR 0059, 0060, 0062).
# A function of the cli, the web dashboard or the daemon named for one of those formatters and returning text, or
# named for one of those classifiers and taking the API type or the string core decides it from, fails here: draw
# what core returns instead, or add the fact to core. So does a client reading a runner's `busy` or `warm`, which
# `Status::slot_state` reads. Mapping core's kind or tone to a colour is a client's own. Test code is not checked:
# tests.rs, testing.rs, testdir.rs, parity.rs, tests/ and an inline `mod tests` at the end of a file.
formatters='duration|durations|ago|age|elapsed|period|bytes|size|cores|cpus|cores_used|in_cores|cpu_time|millicores|percent|budget_line'
classifiers='durations|outcome|outcome_label|is_failure|is_failed|title|job_title|record_title|queued_title|waited|ran|took|step_took|slot_state|wait_reason'
# Each exception: file, function, and why it may stay.
allowed='
crates/cli/src/human.rs bytes: ByteSize itself, under the short name the tables use
crates/web/src/format.rs bytes: ByteSize itself, under the short name the templates use
crates/cli/src/human.rs outcome: JobSummary::outcome with the terminal colour of its kind
crates/daemon/src/simulate.rs duration: runnerd simulate keeps 4m20s, its columns split on spaces (ADR 0059)
'
nl='
'
# shellcheck disable=SC2016 # awk's own `$0`.
program='
FNR == 1 { sig = ""; done = 0 }
done { next }
/^mod tests \{/ { done = 1; next }
# Each `fn` starts a signature of its own, whatever came before it.
/(^|[^a-z_])fn [a-z_]+ *[<(]/ { sig = $0; at = FNR }
sig != "" && FNR != at { sig = sig " " $0 }
sig != "" && /[{;] *$/ {
  match(sig, /fn [a-z_]+/)
  name = substr(sig, RSTART + 3, RLENGTH - 3)
  if ((name ~ "^(" classifiers ")$" && sig ~ /[^A-Za-z](JobSummary|Job|QueuedJob|Record|Step|Timeline|SlotStatus|RunnerStatus|Waiting|str)[^A-Za-z]/) \
    || (name ~ "^(" formatters ")$" && sig ~ /\) *-> *(String|Option<String>|Cow<|Markup|PreEscaped|Span<|Line<|impl [A-Za-z:]*Display|&.static str|\(String)/))
    print FILENAME, name, at
  sig = ""
}
/(^|[^a-z_])runner\.(busy|warm)([^a-z_]|$)/ && FILENAME !~ /^crates\/daemon\// { print FILENAME, "runner.busy/warm", FNR }
'
found=$(find crates/cli/src crates/web/src crates/daemon/src -name '*.rs' ! -name tests.rs ! -name testing.rs \
  ! -name testdir.rs ! -name parity.rs ! -path '*/tests/*' -exec awk -v formatters="$formatters" \
  -v classifiers="$classifiers" "$program" {} + | sort)
duplicates=0
while read -r file name line; do
  [ -n "$file" ] || continue
  case "$allowed" in
    *"$nl$file $name:"*) ;;
    *)
      printf '%s:%s: %s: clients show what core computes; draw core'"'"'s fact, or add it to core\n' \
        "$file" "$line" "$name" >&2
      duplicates=1
      ;;
  esac
done <<FOUND
$found
FOUND
if [ "$duplicates" -eq 0 ]; then
  echo "client formatters and classifiers: ok"
else
  status=1
fi

# The names of the views (`api::View`) are core's: a client that writes one as a string literal of its own, rather than
# calling `View::title`, can drift from the others. Same files and exceptions for tests as above.
views=$(sed -n '/pub fn title(self) -> &.static str {/,/^    }/p' crates/core/src/api/labels.rs | head -12 | grep -o '=> "[^"]*"' | sed 's/=> "//; s/"$//')
pattern=$(printf '%s\n' "$views" | sed 's/.*/"&"/' | paste -sd'|' -)
spelled=$(find crates/cli/src crates/web/src crates/daemon/src -name '*.rs' ! -name tests.rs ! -name testing.rs \
  ! -name testdir.rs ! -name parity.rs ! -path '*/tests/*' -exec awk -v pattern="$pattern" \
  'FNR == 1 { done = 0 } /^mod tests \{/ { done = 1 } !done && $0 !~ /^ *\/\// && $0 ~ pattern { print FILENAME ":" FNR ": " $0 }' {} +)
if [ -n "$spelled" ]; then
  printf '%s\n' "$spelled" >&2
  echo "a view's name is written out: use api::View::title" >&2
  status=1
else
  echo "view names: ok"
fi

# What the build cache did for a job is worded by core (`BuildCacheActivity`): a client that writes a fragment of it
# itself can choose, cut or drop what core says. Same files and test exclusions as above.
worded=$(find crates/cli/src crates/web/src crates/daemon/src -name '*.rs' ! -name tests.rs ! -name testing.rs \
  ! -name testdir.rs ! -name parity.rs ! -path '*/tests/*' -exec awk \
  'FNR == 1 { done = 0 } /^mod tests \{/ { done = 1 } !done && $0 !~ /^ *\/\// && /% hit|uploads refused|lookups hit|read only"/ { print FILENAME ":" FNR ": " $0 }' {} +)
if [ -n "$worded" ]; then
  printf '%s\n' "$worded" >&2
  echo "the build cache's wording is core's: use BuildCacheActivity" >&2
  status=1
else
  echo "build cache wording: ok"
fi
exit "$status"
