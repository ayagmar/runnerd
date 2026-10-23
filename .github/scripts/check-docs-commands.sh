#!/bin/sh
# Every `runnerd …` and `runnerctl …` command line the README and the site's docs show exists: its command, its
# subcommand when the command has them (`runnerctl config show`), and every option, each checked against `--help`.
# Options before the command (`runnerctl --socket <path> status`) are checked against the program's own `--help`, and
# their values skipped; so are a command's options before its subcommand (`runnerctl sizing --since 7d apply`). Reads command lines from inline code, `sh`/`shell`/`bash` blocks (but not `cargo` lines),
# and the `$ ` lines of `console` blocks; output and prose are not commands.
# Usage: check-docs-commands.sh <directory with runnerd and runnerctl>, such as target/debug.
set -eu

bin=${1:?usage: check-docs-commands.sh <directory with runnerd and runnerctl>}
lines=$(
  awk '
    /^```/ { if (fence) { fence = "" } else { fence = substr($1, 4); if (fence == "") fence = "text" } next }
    fence == "sh" || fence == "shell" || fence == "bash" { print; next }
    fence == "console" { if (sub(/^\$ /, "")) print; next }
    fence { next }
    { while (match($0, /`[^`]+`/)) { print substr($0, RSTART + 1, RLENGTH - 2); $0 = substr($0, RSTART + RLENGTH) } }
  ' README.md site/src/content/docs/*.md site/src/content/docs/*.mdx |
    grep -v 'cargo ' |
    grep -oE '(^|[;&|(]) *runner(d|ctl)( [^ |;&#]+)+' |
    sed -E 's/^[^r]*//; s/[][]//g' | sort -u
)
[ -n "$lines" ] || { echo "no commands found in the docs" >&2; exit 1; }

# has_option HELP OPTION: whether the help text lists the option (`--name` or `-n`, before a space, comma or `=`).
has_option() {
  printf '%s\n' "$1" | grep -qE -- "(^|[ ,])$2([ ,=]|$)"
}

# takes_value HELP OPTION: whether the help text shows the option with a value (`--socket <SOCKET>`).
takes_value() {
  printf '%s\n' "$1" | grep -qE -- "(^|[ ,])$2 <"
}

# leading_options HELP WHERE WORD…: checks the options at the start of the words against HELP, skipping the value
# after one that takes it. Sets `consumed` to how many words they took, and `problem` when HELP lacks one.
leading_options() {
  options_help=$1
  options_where=$2
  shift 2
  consumed=0
  while [ -z "$problem" ] && [ $# -gt 0 ] && [ "${1#-}" != "$1" ]; do
    word=$1
    option=${word%%=*}
    shift
    consumed=$((consumed + 1))
    if ! has_option "$options_help" "$option"; then
      problem="$options_where has no $option"
    elif [ "$option" = "$word" ] && takes_value "$options_help" "$option" && [ $# -gt 0 ]; then
      shift
      consumed=$((consumed + 1))
    fi
  done
}

echo "$lines" | {
  status=0
  while read -r line; do
    # shellcheck disable=SC2086 # splits the command line into words on purpose
    set -- $line
    program=$1
    shift
    help=$("$bin/$program" --help)
    where=$program
    problem=
    # Options before the command, against the program's own help.
    leading_options "$help" "$where" "$@"
    shift "$consumed"
    # The command, a subcommand when the command has them, and every option after them.
    if [ -z "$problem" ] && [ $# -gt 0 ] && printf '%s' "$1" | grep -qE '^[a-z][a-z-]*$'; then
      if help=$("$bin/$program" "$1" --help 2>/dev/null); then
        where="$program $1"
        shift
        # A subcommand is required (`<COMMAND>`) or optional (`[COMMAND]`, as `runnerctl sizing --since 7d`), after
        # the command's own options.
        if printf '%s\n' "$help" | grep -qE '^Usage: .*(<COMMAND>|\[COMMAND\])'; then
          leading_options "$help" "$where" "$@"
          shift "$consumed"
          if [ -z "$problem" ] && [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then
            if help=$("$bin/$program" "${where#* }" "$1" --help 2>/dev/null); then
              where="$where $1"
              shift
            else
              problem="$where has no subcommand $1"
            fi
          fi
        fi
      else
        problem="$program has no command $1"
      fi
      for word in "$@"; do
        [ -z "$problem" ] || break
        case $word in
          -*) has_option "$help" "${word%%=*}" || problem="$where has no ${word%%=*}" ;;
        esac
      done
    fi
    if [ -n "$problem" ]; then
      echo "the docs show \`$line\`, but $problem" >&2
      status=1
    else
      echo "$line: ok"
    fi
  done
  exit "$status"
}
