# 0043. Phones signed in by QR code, and sessions that outlive a restart

Status: **Accepted** (2026-10-24). Amends [0020](0020-dashboard-is-an-api-client-on-loopback.md) and
[0042](0042-dashboard-behind-a-proxy-and-installable.md).

## Context

The owner opens the dashboard on a phone through `tailscale serve`. Signing it in took `runnerctl web --no-open
--base https://<host>:8443` on the desktop and copying the link to the phone by hand, and every daemon restart
signed every browser out, because the session token was random per run. An installed app on the phone had to be
signed in again after each upgrade.

## Decision

- **A link per address.** `runnerd.web` holds one login link per line: the loopback address first, then each of
  `[web] origins`, each with its own code. Using one replaces only that line, so opening the dashboard on the desktop
  does not spend the phone's link. One code valid on every address was rejected for that reason: `runnerctl web`
  opens the loopback link as it prints the others. A code redeems on any address; the cookie it sets is the
  address's own, since cookies are scoped by host. `runnerctl web` opens the loopback link and prints the others;
  `--no-open` prints all of them; `--base` stays for a proxy not listed, and prints the listed proxy's own link
  when it names one.
- **A QR code from a screen the owner already has.** The Config page's **Add a device** asks the dashboard for a
  fresh code (`POST /pair`, a write: it needs the session and the dashboard's `Origin`) and shows a QR code of its
  link on one of `[web] origins`, with a choice when there are several and a note pointing at `[web] origins` when
  there are none. Such a code works once, within 5 minutes; at most 8 are alive at once. `runnerctl web --qr`
  prints QR codes of the links in `runnerd.web`, which work once and do not expire, as the printed links never
  did: the terminal cannot ask the dashboard for a fresh code without a session of its own. The QR matrix comes from
  the `qrcode` crate (0.14.1, no default features, no dependencies); the dashboard draws it as an inline SVG path
  and `runnerctl` as half-height blocks, black on white when it colours its output.
- **Sessions that last.** The session secret is kept in `dashboard-session` beside the job history, mode 0600 like
  `runnerd.web`, and read at start: one with other users' permissions, or malformed, is replaced. The cookie gets
  `Max-Age` of 30 days, set again on every authenticated response, so a phone used once a month stays signed in
  and one left unused for a month signs out. `HttpOnly`, `SameSite=Strict`, `Secure` behind https and host-only
  stay. `runnerctl web --sign-out-all` replaces the secret through `POST /v1/dashboard/sign-out` on the socket,
  which the daemon answers with `runnerd_web::Sessions`, the same object the dashboard checks cookies against.
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
- The daemon's socket API gains a route owned by the dashboard's crate. The dashboard still reads and writes the
  daemon only through the API; the daemon now also holds the dashboard's session object, as it already ran the
  dashboard in its process.
- A QR code printed with `--qr` and left in a terminal's scrollback works until it is used, like the link it
  replaces.
