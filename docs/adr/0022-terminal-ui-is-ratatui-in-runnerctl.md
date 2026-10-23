# 0022. The terminal UI is ratatui in runnerctl, behind a feature

## Context

Hosts reached over SSH have no browser open on the dashboard's loopback port. `runnerctl watch` redraws a table,
but shows no history, step timeline, charts or log, and takes no keys. A full-screen client needs a terminal
library for raw mode, the alternate screen, key events and layout.

## Decision

`runnerctl tui` is a module of `crates/cli` (`src/tui/`), built with `ratatui` and its default `crossterm` backend,
behind the `tui` cargo feature, which is on by default so release binaries have it. It is an API client like the
rest of `runnerctl`:

- the snapshot and the event stream come from `client` and `watch::State`, the reducer `runnerctl watch` uses;
- its own state is a pure reducer (`tui/app.rs`) over keys, events and answers, and drawing (`tui/draw.rs`) reads
  that state only, so both are tested with ratatui's `TestBackend` and the fake daemon;
- one task owns the event stream, so redraws never drop a pending read; when the stream fails for good it shows
  "reconnecting" and resumes after the last event ID.

Taking over and giving back the terminal goes through `output.rs`, still the only module that writes to it. The
TUI handles SIGINT, SIGTERM and SIGHUP by quitting and SIGTSTP (and Ctrl-Z, which raw mode delivers as a key) by
giving the terminal back and stopping its process group with SIGSTOP; SIGCONT takes the terminal again.

## Consequences

- ratatui (0.30) brings crossterm, unicode width and segmentation crates, and `kasuari` for layout. The calendar
  widget is left out, so `time` is not built. None of it is Docker or GitHub code, so the boundary check passes.
- `rustix` (already in the lockfile through crossterm) sends that SIGSTOP without `unsafe`, which the workspace
  forbids; its tests use it to drive `runnerctl tui` in a pseudo-terminal.
- `cargo build -p runnerd-cli --no-default-features` builds a CLI without the TUI and these crates.
- Colours are xterm-256 approximations of the dashboard's palette; `NO_COLOR` leaves only bold, dim, reverse and
  block glyphs.
