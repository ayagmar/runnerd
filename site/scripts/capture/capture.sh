#!/bin/sh
# Recaptures every dashboard screenshot, TUI frame and terminal transcript of the site from the dashboard's demo
# daemon (crates/web/examples/demo): a fake daemon with made-up repositories, on a socket in a scratch directory.
# It never touches a real runnerd's socket or config, and starts no real daemon.
#
#   site/scripts/capture/capture.sh [dashboard|tui|terminal ...]     (default: all three)
#
# Needs: cargo, uv (runs pyte and Pillow without installing them), a Chromium that playwright-core can drive
# (CHROMIUM=/path/to/chrome-headless-shell, else the one `pnpm -C site exec playwright-core install chromium` fetches),
# and `pnpm -C site install --frozen-lockfile`. Run it from the repository root; review `git diff --stat` after.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
what=${*:-dashboard tui terminal}
scratch=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/runnerd-capture.XXXXXX")
demo_pid=
idle_pid=
cleanup() {
  for pid in $demo_pid $idle_pid; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$scratch"
}
# The cleanup runs on every way out; a signal ends the script instead of letting it capture on without its daemon.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

nice -n 10 cargo build --locked -p runnerd-web --example demo
nice -n 10 cargo build --locked -p runnerd-cli
mkdir -p "$scratch/run" "$scratch/idle" "$scratch/home"
target/debug/examples/demo "$scratch/run" 127.0.0.1:7498 >"$scratch/run.log" 2>&1 &
demo_pid=$!
# A second demo with no runner at all, for the idle overview.
RUNNERD_DEMO_SCENE=idle target/debug/examples/demo "$scratch/idle" 127.0.0.1:7499 >"$scratch/idle.log" 2>&1 &
idle_pid=$!
for dir in run idle; do
  for _ in $(seq 50); do [ -S "$scratch/$dir/runnerd.sock" ] && break; sleep 0.2; done
  [ -S "$scratch/$dir/runnerd.sock" ] || { cat "$scratch/$dir.log" >&2; echo "a demo daemon did not start" >&2; exit 1; }
done
# The demo's clock runs from its start, so every capture happens within seconds of it.
export RUNNERD_CAPTURE_SOCKET="$scratch/run/runnerd.sock"
export RUNNERD_CAPTURE_LOGIN="$scratch/run/runnerd.web"
export RUNNERD_CAPTURE_IDLE_LOGIN="$scratch/idle/runnerd.web"
export RUNNERD_CAPTURE_HOME="$scratch/home"
here=site/scripts/capture
for part in $what; do
  case "$part" in
    terminal)
      for command in status caches insights; do
        target/debug/runnerctl --socket "$RUNNERD_CAPTURE_SOCKET" "$command" >"site/src/assets/terminal/$command.txt"
      done
      ;;
    tui) uv run --quiet --with pyte python "$here/tui.py" ;;
    dashboard)
      pnpm -C site exec node scripts/capture/dashboard.mjs
      uv run --quiet --with pillow python "$here/webp.py" "$scratch/run/shots" site/src/assets/dashboard
      ;;
    *) echo "unknown part $part" >&2; exit 2 ;;
  esac
done
