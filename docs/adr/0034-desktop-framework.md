# 0034. Desktop app: a tray client and the installable dashboard, not a GUI toolkit yet

Status: **Proposed** (2026-10-23). If accepted, it supersedes [0004](0004-gpui-desktop-app-pinned-to-a-zed-commit.md).

## Context

ADR 0004 picked GPUI for the desktop app before anything was built. The owner asked whether GPUI is the best choice
before plan 008 starts. What the app has to respect, from ARCHITECTURE.md and the gate:

- It is a client ([0020](0020-dashboard-is-an-api-client-on-loopback.md), [0030](0030-one-socket-client-in-core.md)):
  `runnerd_core::client` over the socket, no `bollard`, `reqwest` or core's `host`. `check-boundaries.sh` runs
  `cargo tree` on the host target.
- `cargo deny check` fails on any unmaintained or unsound advisory, on licenses outside the allow list, and on any
  git source ([0009](0009-newest-stable-and-locked-dependencies.md), `deny.toml`).
- Releases are cargo-dist archives for `x86_64-` and `aarch64-unknown-linux-musl`, static, no installers.
- Linux first ([0011](0011-linux-and-github-only-for-v1.md)). The owner runs niri with the Noctalia shell, which
  provides the StatusNotifierItem watcher (tray) and the notification daemon; the default browser is Helium
  (Chromium).
- The dashboard (axum, maud, Datastar) already shows every view plan 008 lists: overview, live jobs, history, logs,
  caches, the config editor, sizing advice. The TUI shows most of them in a terminal.

What a desktop app adds over a browser tab is what a tab can't do: be there when no tab is open (tray status),
tell the owner when a job fails (a notification, even with the dashboard closed), act in one click (pause, resume,
open the dashboard, open a shell in a failed job's record), and start with the session.

## Options

Researched 2026-10-23 from crates.io, the projects' repositories and the RustSec advisory database (local copy of
that day). Spike numbers are from release builds on the owner's machine (32 cores, Rust 1.99,
`x86_64-unknown-linux-gnu`), checked against a copy of runnerd's `deny.toml`; the spikes live outside the repository.

| | GPUI (Zed, git pin) | Tauri 2 (shell around the dashboard) | Slint 1.18 | Iced 0.14 | egui/eframe 0.37 | Dioxus 0.7 desktop | Tray client + installable dashboard |
|---|---|---|---|---|---|---|---|
| Latest stable | crates.io 0.2.2 (2025-10-22); the code moves daily, Zed ships weekly (v1.25.2, 2026-10-21) | 2.12.5 (2026-10-21) | 1.18.1 (2026-09-21) | 0.14.0 (2025-12-07) | 0.37.0 (2026-10-15) | 0.7.10 (2026-10-16) | ksni 0.3.7 (2026-10-20), notify-rust 4.18.2 (2026-10-07) |
| Reuse | none: every view rewritten | all of the dashboard, unchanged | none | none | none | none (RSX, not maud) | all of the dashboard; the tray reuses `client` and `watch::State` |
| Wayland, niri | works: spike window opened and rendered under niri; wgpu renderer | WebKitGTK; open Linux issues such as [#9304](https://github.com/tauri-apps/tauri/issues/9304) (NVIDIA) | winit | winit; `wayland`/`x11` features | winit | WebKitGTK | no window of its own; tray and notifications over D-Bus, the dashboard in Chromium |
| Tray, notifications | notifications yes (`gpui_linux` `system_notifications`); no tray | both, through plugins and `tray-icon` (libappindicator by default) | neither | neither | neither | tray through `tray-icon` | both: spike registered with Noctalia's watcher and showed a notification |
| `cargo deny` as configured | fails: 6 git sources (37 crates), MPL-2.0, BSD-2-Clause, CC0-1.0, unmaintained `paste`, `rustybuzz`, `ttf-parser`, zbus 5's parts | fails: gtk-rs 0.18 (GTK 3) is unmaintained ([RUSTSEC-2024-0412](https://rustsec.org/advisories/RUSTSEC-2024-0412) to 0420), its glib 0.18 unsound ([RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429)) | license is GPL-3.0 or Slint's own, outside the allow list | not run | not run | same gtk-rs advisories as Tauri (wry) | fails until ksni and notify-rust move to zbus 6: `zbus_names`, `zvariant` ([RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371), 0370, from 2026-10-16); ksni is `Unlicense` |
| Static musl release | no: links libxcb, libxkbcommon, loads Vulkan | no: links WebKitGTK | no (GPU) | no (GPU) | no (GPU) | no | likely: pure Rust over D-Bus, links only libc (musl build not tried) |
| Size (spike, stripped) | 26 MiB, 441 crates, 1 m 46 s | not built: WebKitGTK is not installed here | | | | | 4.4 MiB, 83 crates, 1 m 30 s |
| Accessibility | AccessKit, `accesskit_unix` on Linux | the browser's | AccessKit | none yet ([#552](https://github.com/iced-rs/iced/issues/552) open) | AccessKit, a feature | the browser's | the browser's for the dashboard |
| Maintenance risk | pre-1.0, breaks between Zed releases; `gpui_platform`, which the README says to add, is not on crates.io; outside Zed, Longbridge's [gpui-kit](https://github.com/longbridge/gpui-kit) republishes snapshots as `gpui-pre` | stable 2.x, large team; v3 with a CEF runtime is in alpha (`tauri-runtime-cef` 3.0.0-alpha.6) | 1.x, company-backed | pre-1.0, one main maintainer, ten months without a release | pre-1.0, active, breaking minor releases | pre-1.0, 0.8 in alpha | small crates; ksni has one maintainer |

Dropped: Freya (0.5 in beta, Skia, small team), Xilem (0.4, 2025-10, experimental), Floem (0.2.0, last release
2024-11).

Sources: crates.io pages of [gpui](https://crates.io/crates/gpui), [gpui-pre](https://crates.io/crates/gpui-pre),
[tauri](https://crates.io/crates/tauri), [tauri-runtime-cef](https://crates.io/crates/tauri-runtime-cef),
[slint](https://crates.io/crates/slint), [iced](https://crates.io/crates/iced),
[eframe](https://crates.io/crates/eframe), [dioxus-desktop](https://crates.io/crates/dioxus-desktop),
[ksni](https://crates.io/crates/ksni), [notify-rust](https://crates.io/crates/notify-rust),
[tray-icon](https://crates.io/crates/tray-icon), [zbus](https://crates.io/crates/zbus) (6.0.0, 2026-10-16);
[Zed releases](https://github.com/zed-industries/zed/releases),
[GPUI's README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md),
[iced's changelog](https://github.com/iced-rs/iced/blob/master/CHANGELOG.md),
[Slint's royalty-free license](https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md),
[Chrome's installability criteria](https://developer.chrome.com/blog/update-install-criteria),
[Firefox web apps, Windows only](https://www.omgubuntu.co.uk/2025/09/firefox-143-release-key-changes).

## Recommendation

No GUI toolkit for v1. The desktop app is two small pieces:

1. **A tray client**, `crates/desktop`, with ksni (StatusNotifierItem) and notify-rust, both over zbus. It follows
   `/v1/events` through `runnerd_core::client` and the reducer `runnerctl watch` uses, shows running jobs and the
   budget in the tray, offers pause and resume, "open dashboard" (the login URL from `runnerd.web`, through
   `xdg-open`) and, on a failed job, a notification with "open record". It autostarts with an XDG autostart entry or a
   systemd user unit.
2. **The dashboard as an installable app**: a web app manifest and icons in `crates/web`, so Chromium-based browsers
   install it as a window of its own. `http://127.0.0.1` is a secure context, and Chrome installs from the menu with
   a manifest alone.

Why:

- It reuses everything: the dashboard keeps being the one set of views, and the tray reuses the client and reducer
  the TUI already tests. Every toolkit option rewrites all the views in a second UI that has to track the API.
- It is what a tab can't do. A native window with the same views adds little over the installed dashboard.
- It fits the release as is: likely a static musl binary for cargo-dist; GPUI, Tauri and the others need glibc,
  system GPU or WebKit libraries, and an AppImage, deb or Flatpak next to cargo-dist.
- It is small: 4.4 MiB and 83 crates against GPUI's 26 MiB, 441 crates and six git sources.
- Its `cargo deny` failures are a week old and temporary (zbus 6 came out on 2026-10-16), and one license line
  (`Unlicense`). GPUI's and Tauri's are structural.

Between the toolkits, if a native window is wanted later: GPUI renders well on niri and has AccessKit and
notifications, but means a git pin and three new licenses in `deny.toml`; Tauri 2 reuses the dashboard but sits on
unmaintained GTK 3 bindings until its CEF runtime ships; egui is the safest crates.io option but looks like a tool,
not like the dashboard.

## What would change it

- The owner wants views or interactions the dashboard can't offer in a browser window: then a toolkit, GPUI first.
- GPUI publishes `gpui_platform` and its other crates to crates.io with tagged releases: the git-pin cost goes.
- Tauri ships a stable CEF runtime or moves Linux to GTK 4: the deny failures go and it becomes the cheap way to a
  native window around the dashboard.
- ksni or notify-rust don't move to zbus 6 within a few weeks: talk D-Bus directly with zbus 6 for the two
  interfaces, or ignore the two advisories with a reason until they do.
- A target desktop has no StatusNotifierItem host (GNOME without the AppIndicator extension): notifications still
  work, the tray doesn't.

## Consequences

- Plan 008 is rewritten: the tray client, the manifest, autostart. No GPUI spike, no `deny.toml` git source.
- `crates/desktop` is a client like the others; `check-boundaries.sh` covers it unchanged.
- `deny.toml` gains `Unlicense` (ksni), with a comment.
- The release gains one Linux binary, built like `runnerctl` if the musl build works; that is the first thing to try.
- Firefox can't install the dashboard as an app on Linux (Firefox 143 added web apps on Windows only); there it stays a
  tab, and the tray's "open dashboard" opens it.

## Questions for the owner

- Is a tray and notifications what you want from "the desktop app", or do you want a native window with the views?
  If the window, GPUI or Tauri is the next choice, and the table above is the comparison.
- A separate binary (`runnerd-desktop`, autostarted with the session) or a `runnerctl tray` subcommand behind a
  feature, as the TUI is?
