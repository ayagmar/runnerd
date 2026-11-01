# 0043. Phones signed in by QR code, and sessions that outlive a restart

Status: **Accepted** (2026-10-24), amended by [0078](0078-a-first-run-that-can-start-and-keeps-its-secrets.md). Amends [0020](0020-dashboard-is-an-api-client-on-loopback.md), [0042](0042-dashboard-behind-a-proxy-and-installable.md).

## Context

The owner opens the dashboard on a phone through `tailscale serve`. Signing it in took `runnerctl web --no-open
--base https://<host>:8443` on the desktop and copying the link to the phone by hand, and every daemon restart
signed every browser out, because the session token was random per run. An installed app on the phone had to be
signed in again after each upgrade.

## Decision

- **A link per address.** `runnerd.web` holds one login link per line: the loopback address first, then each of
  `[web] origins`, each with its own code. Using one replaces only that line, so opening the dashboard on the desktop
  does not spend the phone's link. One code valid on every address was rejected for that reason: `runnerctl web`
  opens the loopback link as it prints the others. A login link's code redeems on any address, so `--base` can move
  it to a proxy not listed; the cookie it sets is the address's own, since cookies are scoped by host. `runnerctl
  web` opens the loopback link and prints the others;
  `--no-open` prints all of them; `--base` stays for a proxy not listed, and prints the listed proxy's own link
  when it names one.
- **A QR code from a screen the owner already has.** The Config page's **Add a device** asks the dashboard for a
  fresh pairing code (`POST /pair`, a write: it needs the session and the dashboard's `Origin`) and shows a QR code
  of its link on one of `[web] origins`, with a choice when there are several and a note pointing at `[web]
  origins` when there are none. `runnerctl web --qr` asks the daemon for the same over its socket
  (`POST /v1/dashboard/pairings`, the owner only), one per origin or for the one `--base` names, fresh on every run.
  A pairing code works once, within 5 minutes, and at most 8 are alive at once, wherever they were asked for; the
  socket refuses to make more at once than that, before making any, and names `--base` to pick one origin. The
  dashboard makes its code only if the secret its request was checked against is still the current one, checked
  under the lock that makes the code: a request still arriving when every device is signed out gets 401, not a link. It
  works only on the origin it was made for: the request's `X-Forwarded-Proto` (else `http`) and
  `X-Forwarded-Host` (else `Host`), as a browser would serialize them, must name it. `tailscale serve` passes the
  browser's `Host` on and sets both forwarded headers (`ipn/ipnlocal/serve.go`, `addProxyForwardedHeaders`). The
  headers bind the code to where it was meant to be used; they are not what keeps others out, since only a holder
  of the code can redeem it. The QR matrix comes from the `qrcode` crate (0.14.1, no default features, no
  dependencies); the dashboard draws it as an inline SVG path and `runnerctl` as half-height blocks, black on white
  when it colours its output. Responses carrying a code or setting the cookie are `Cache-Control: no-store`.
- **Sessions that last.** The session secret is kept in `dashboard-session` beside the job history, mode 0600 like
  `runnerd.web`. At start runnerd refuses a directory there that other users can write, or that someone other than
  its user or root owns, since they could swap the file. It opens the file without following a symbolic link and
  checks the type, owner and mode on that same descriptor before reading it. A link or a non-regular file is
  refused, and a file other users can read, or one that is malformed, is replaced. The cookie gets `Max-Age` of 30
  days, set again on every authenticated response with the secret that request was checked against, never a newer
  one, so a phone used once a month stays signed in and one left unused for a month signs out. `HttpOnly`,
  `SameSite` (`Lax` since 0020's amendment), `Secure` behind https and host-only stay.
- **Signing every device out.** `runnerctl web --sign-out-all` goes through `POST /v1/dashboard/sign-out` on the
  socket, which the daemon answers with `runnerd_web::Sessions`, the object that owns the secret, the login links
  and the pairing codes under one lock. It replaces the secret, drops every pairing code and writes new login
  links, so no credential made before signs a device in afterwards.
- **The signed-out page** tells a visitor to scan the QR code from the dashboard on runnerd's computer or run
  `runnerctl web --qr` there, and to run `runnerctl web` on that computer itself. It names no address to type.
- **Not done: Tailscale identity headers.** `tailscale serve` sends `Tailscale-User-Login`, but any local user can
  send the same header straight to `127.0.0.1:7477`, so trusting it would let them past the session that exists to
  keep them out.

## Consequences

- One secret serves every browser: signing out is for all of them, and runnerd cannot list or end one device's
  session. A cookie copied out of a browser keeps working until `--sign-out-all`, whatever its `Max-Age`; the
  browser, not the server, enforces the 30 days.
- The secret's file lives in the state directory, so a backup of it holds a working session; it is 0600 like the
  database beside it.
- The daemon's socket API gains two routes served by the dashboard's crate, and returns a secret for the first
  time: a pairing link, to the owner only, which `Debug` leaves out. The dashboard still reads and writes the
  daemon only through the API; the daemon now also holds the dashboard's credentials object, as it already ran the
  dashboard in its process.
- A login link printed by `runnerctl web` still works until it is used or every device is signed out; only pairing
  codes expire.
- A job history configured in a directory other users can write, such as `/tmp`, now stops the daemon at start.
