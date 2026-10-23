# 0034. Failure notifications from the daemon, the installable dashboard and an optional tray, not a GUI toolkit

Status: **Accepted** (2026-10-23). Supersedes [0004](0004-gpui-desktop-app-pinned-to-a-zed-commit.md).
Building it is deferred until after the 0.0.7 release. The installable dashboard (recommendation 2) was built
ahead of the notifications, at the owner's request, in [0042](0042-dashboard-behind-a-proxy-and-installable.md)
(2026-10-23), with a reverse proxy such as `tailscale serve` for other devices; the headless row's "only through an
SSH tunnel" now also reads "or such a proxy".

## Context

ADR 0004 picked GPUI for the desktop app before anything was built. The owner asked whether GPUI is the best choice
before plan 008 starts, and runnerd is for every Linux user, not one desktop: GNOME, KDE, XFCE, Cinnamon, wlroots
compositors, bare X11 window managers, and home servers and VMs with no desktop at all. What any answer has to respect,
from ARCHITECTURE.md and the gate:

- A desktop app is a client ([0020](0020-dashboard-is-an-api-client-on-loopback.md),
  [0030](0030-one-socket-client-in-core.md)): `runnerd_core::client` over the socket, no `bollard`, `reqwest` or
  core's `host`. `check-boundaries.sh` runs `cargo tree` on the host target.
- `cargo deny check` fails on any unmaintained or unsound advisory, on licenses outside the allow list, and on any
  git source ([0009](0009-newest-stable-and-locked-dependencies.md), `deny.toml`).
- Releases are cargo-dist archives for `x86_64-` and `aarch64-unknown-linux-musl`, static, no installers.
- Linux first ([0011](0011-linux-and-github-only-for-v1.md)).
- The dashboard (axum, maud, Datastar) already shows every view plan 008 lists: overview, live jobs, history, logs,
  caches, the config editor, sizing advice. The TUI shows most of them in a terminal. Both listen only on the
  machine runnerd runs on.
- The daemon already announces every finished job as a durable `job.finished` event with an ID a reader can resume
  from (ARCHITECTURE.md, Invariants).

What a desktop app would add over a browser tab is what a tab can't do: tell the user when a job fails even with the
dashboard closed, be there when no tab is open (tray status), act in one click (pause, resume, open the dashboard,
open a shell in a failed job's record), and start with the session. The first of these matters most and is also the
one a machine without a desktop needs.

## Options for a desktop app

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

## Where each piece works

What each environment offers out of the box, from the sources below. "Tray" is the StatusNotifierItem protocol
(ksni, libappindicator and Tauri's `tray-icon` all speak it); "notifications" is the freedesktop notification
service on the session bus.

| Environment | Tray (SNI host) | Desktop notifications | Installed dashboard: Chromium-based / Firefox | Native window: GPUI (Vulkan or GL) / Tauri 2 (WebKitGTK) | Daemon webhook (ntfy and others) |
|---|---|---|---|---|---|
| GNOME, stock (Fedora Workstation, Debian) | no: needs the AppIndicator extension, which GNOME declined to ship ([gnome-shell#1014](https://gitlab.gnome.org/GNOME/gnome-shell/issues/1014)); Debian's `gnome-core` and Fedora's default install leave it out | yes, GNOME Shell | yes / no | yes / yes | yes |
| Ubuntu GNOME | yes: `gnome-shell-extension-appindicator` is in Ubuntu's desktop seed ([noble](https://git.launchpad.net/~ubuntu-core-dev/ubuntu-seeds/+git/ubuntu/tree/desktop-minimal?h=noble)) | yes | yes / no | yes / yes | yes |
| KDE Plasma | yes: the protocol is KDE's ([spec](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)) | yes | yes / no | yes / yes | yes |
| XFCE | yes, since 4.16: the panel's Status Tray plugin ([Xfce docs](https://docs.xfce.org/panel-plugins/xfce4-statusnotifier-plugin/start)) | yes, `xfce4-notifyd` | yes / no | yes / yes | yes |
| Cinnamon | yes: `xapp-sn-watcher` ([xapps#89](https://github.com/linuxmint/xapps/pull/89)) | yes | yes / no | yes / yes | yes |
| wlroots compositors, niri, Hyprland with waybar or a shell | if the bar has a tray: waybar's `tray` module is SNI ([source](https://github.com/Alexays/Waybar/tree/master/src/modules/sni)), still marked beta; Noctalia has one | only with a daemon the user runs: mako, dunst, swaync, the shell's own | yes / no | yes / yes | yes |
| X11 window managers without a bar (i3 without one, Openbox, …) | no | only with a daemon such as dunst | yes / no | yes / yes | yes |
| Headless: home servers, VMs | no | no | no: the dashboard is loopback-only, so only through an SSH tunnel (not tried) | no | yes |

Sources for the other columns: [the notification spec](https://specifications.freedesktop.org/notification-spec/latest/);
[Chrome's installability criteria](https://developer.chrome.com/blog/update-install-criteria) (install from the menu
with a manifest alone; `http://127.0.0.1` is a secure context); Firefox shipped web apps only on Windows, in
[143](https://www.omgubuntu.co.uk/2025/09/firefox-143-release-key-changes), and this ADR found no Linux release as
of 158 (2026-10-13). GPUI renders through wgpu's Vulkan or GLES backends and falls back to llvmpipe on the CPU
([Zed on Linux](https://zed.dev/docs/linux), [zed#66030](https://github.com/zed-industries/zed/issues/66030));
WebKitGTK is packaged by every distribution above, and Tauri's Linux issues include
[#9304](https://github.com/tauri-apps/tauri/issues/9304) (NVIDIA). ntfy has
[Android and iOS apps](https://docs.ntfy.sh/subscribe/phone/).

The only piece that works in every row is a notification the daemon sends over the network. The installed dashboard
works on every desktop with a Chromium-based browser. The tray works on most desktops but not on stock GNOME, the
most common one, nor on bare window managers. A native window works on every desktop but adds the least.

## Option: failure notifications sent by the daemon

The daemon posts to URLs the config names when a job fails (and, if the user asks, when one is lost or a runner
cannot start). It already reads the durable `job.finished` events, so a notifier is another reader of the store's
events, off the engine's loop, that remembers the last event it delivered and retries, so a restart neither drops nor
repeats a failure. It lives in the daemon, which already builds `reqwest` for GitHub; clients stay as they are.

- **A generic webhook**: a JSON body with the job, repository, workflow, conclusion, the record if one was kept, and
  a link to the run on GitHub. Anything that takes JSON can read it.
- **ntfy through it**: ntfy turns a JSON body into a notification with its
  [message templates](https://docs.ntfy.sh/publish/#message-templating) (`X-Template`), or takes plain text with
  `Title`, `Priority` and `Tags` headers. Self-hosted ntfy or ntfy.sh reaches phones and every desktop with the ntfy
  app or a browser tab. Gotify, Apprise, Discord and Slack take a JSON body too; a `format` per URL covers the ones
  whose bodies differ, added when someone needs one.
- The URL and any token in a header are secrets: they get the same handling as the GitHub token (no log line,
  `Debug` output or error shows them). It is opt-in, since job and repository names leave the machine.

Cost: a `[notify]` section in the config, the notifier and its tests with a fake HTTP endpoint. No new dependency;
nothing changes on the client side.


## Recommendation

No GUI toolkit. In order of how many users each serves:

1. **Failure notifications from the daemon**: a generic webhook, with ntfy through it. It is the only piece that works
   on every row of the matrix, headless servers and phones included, and it needs no client at all.
2. **The dashboard as an installable app**: a web app manifest and icons in `crates/web`, so Chromium-based browsers
   install it as a window of its own on every desktop. Firefox users keep a tab. A few files, no dependency.
3. **An optional tray client**, `crates/desktop`, with ksni and notify-rust over zbus: running jobs and the budget,
   pause and resume, "open dashboard" (the login URL from `runnerd.web`, through `xdg-open`), and desktop
   notifications of failures for users without a webhook. It follows `/v1/events` through `runnerd_core::client` and
   the reducer `runnerctl watch` uses. It degrades: with no StatusNotifierItem host it only sends notifications, with
   no notification service it logs once and waits, and it never blocks or changes the daemon.

Why:

- Portability: the order follows the matrix. A tray first would leave out stock GNOME, bare window managers and every
  server; a native window would leave out every server and add the least on desktops.
- Reuse: the dashboard stays the one set of views, and the tray reuses the client and reducer the TUI already tests.
  Every toolkit option rewrites all the views in a second UI that has to track the API.
- Release fit: the notifier ships inside `runnerd`, the manifest inside `runnerd-web`; the tray is likely a static
  musl binary for cargo-dist. GPUI, Tauri and the others need glibc, GPU or WebKit libraries, and an AppImage, deb or
  Flatpak next to cargo-dist.
- Size: the tray spike is 4.4 MiB and 83 crates against GPUI's 26 MiB, 441 crates and six git sources. Its
  `cargo deny` failures are a week old and temporary (zbus 6 came out on 2026-10-16), plus one license line
  (`Unlicense`); GPUI's and Tauri's are structural.

Between the toolkits, if a native window is wanted later: GPUI renders well on niri and has AccessKit and
notifications, but means a git pin and three new licenses in `deny.toml`; Tauri 2 reuses the dashboard but sits on
unmaintained GTK 3 bindings until its CEF runtime ships; egui is the safest crates.io option but looks like a tool,
not like the dashboard.

## What would change it

- Users want views or interactions the dashboard can't offer in a browser window: then a toolkit, GPUI first.
- GPUI publishes `gpui_platform` and its other crates to crates.io with tagged releases: the git-pin cost goes.
- Tauri ships a stable CEF runtime or moves Linux to GTK 4: the deny failures go and it becomes the cheap way to a
  native window around the dashboard.
- Firefox ships web apps on Linux: the installed dashboard covers every desktop.
- ksni or notify-rust don't move to zbus 6 within a few weeks: talk D-Bus directly with zbus 6 for the two
  interfaces, or ignore the two advisories with a reason until they do.
- GNOME ships a tray: the tray covers every desktop and could move up the list.

## Consequences

- Plan 008 is rewritten as three pieces, in the order above. No GPUI spike, no `deny.toml` git source.
- The daemon gains an outgoing HTTP path other than GitHub's; its URLs are handled as secrets.
- `crates/desktop`, if built, is a client like the others; `check-boundaries.sh` covers it unchanged, and
  `deny.toml` gains `Unlicense` (ksni), with a comment.
- The release gains at most one Linux binary, the tray, built like `runnerctl` if the musl build works.
- On a server, a failure reaches the user only through a webhook; without one they see it in the dashboard or TUI.

## Open questions for the build

- Which events notify: failed jobs only, or also lost jobs, runners that could not start, and the daemon stopping?
- Which formats ship first: the generic JSON and ntfy, or also Discord or Slack bodies?
- The tray: a separate binary (`runnerd-desktop`, autostarted with the session) or a `runnerctl tray` subcommand
  behind a feature, as the TUI is? And in the first release with the other two, or later?
