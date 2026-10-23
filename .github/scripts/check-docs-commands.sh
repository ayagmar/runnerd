#!/bin/sh
# Every `runnerd <command>` and `runnerctl <command>` the README and the site's docs show exists: each answers
# `--help`, and so does the subcommand after a command that has them (`runnerctl config show`). Reads commands from
# inline code, `sh` blocks (but not `cargo` lines), and the `$ ` lines of `console` blocks; output and prose are not
# commands.
# Usage: check-docs-commands.sh <directory with runnerd and runnerctl>, such as target/debug.
set -eu

bin=${1:?usage: check-docs-commands.sh <directory with runnerd and runnerctl>}
commands=$(
  awk '
    /^```/ { if (fence) { fence = "" } else { fence = substr($1, 4); if (fence == "") fence = "text" } next }
    fence == "sh" || fence == "bash" { print; next }
    fence == "console" { if (sub(/^\$ /, "")) print; next }
    fence { next }
    { while (match($0, /`[^`]+`/)) { print substr($0, RSTART + 1, RLENGTH - 2); $0 = substr($0, RSTART + RLENGTH) } }
  ' README.md site/src/content/docs/*.md site/src/content/docs/*.mdx |
    grep -v 'cargo ' |
    grep -oE '(^|[^-/.a-z])runner(d|ctl)( --?[a-z-]+)*( [a-z][a-z-]*){1,2}' |
    sed -E 's/^[^r]*//; s/ --?[a-z-]+//g' | sort -u
)
[ -n "$commands" ] || { echo "no commands found in the docs" >&2; exit 1; }
echo "$commands" | {
  status=0
  while read -r program command sub; do
    if ! "$bin/$program" "$command" --help >/dev/null 2>&1; then
      echo "the docs show \`$program $command\`, which $program does not have" >&2
      status=1
    elif [ -n "$sub" ] && "$bin/$program" "$command" --help | grep -q '^Usage: .*<COMMAND>' &&
      ! "$bin/$program" "$command" "$sub" --help >/dev/null 2>&1; then
      echo "the docs show \`$program $command $sub\`, which $program does not have" >&2
      status=1
    else
      echo "$program $command${sub:+ $sub}: ok"
    fi
  done
  exit "$status"
}
