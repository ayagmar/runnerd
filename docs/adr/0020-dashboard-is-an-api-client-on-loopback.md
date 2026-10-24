# 0020. The dashboard is an API client, on loopback behind a one-time login

Amended by [0042](0042-dashboard-behind-a-proxy-and-installable.md): writes are also taken from the origins of
proxies listed in `[web] origins`, and a login through an HTTPS proxy sets a `Secure` cookie. Amended by
[0043](0043-phones-paired-by-qr-code-and-sessions-that-last.md): the session secret is kept in a file across
restarts, and a phone signs in by a QR code.

## Context

The browser needs HTTP, and the daemon's API is on a Unix socket only its user can open (ADR 0002). A loopback TCP
port is open to every local user, and any site the browser has open can send requests to it. The dashboard must not
become a second copy of the API's logic or a way around its owner check.

## Decision

`crates/web` is a client: it renders pages from `runnerd_core::api` types it reads over the daemon's socket, and
passes writes on to the same API. The daemon runs it in its own process on `127.0.0.1:7477` (`serve --web`); it
refuses non-loopback addresses. Every request but the login and the static assets needs a session cookie (random per
daemon run, `HttpOnly`, `SameSite=Strict`), and every write also needs an `Origin` of the dashboard itself. A browser
gets the cookie from a one-time login URL that the daemon writes to `runnerd.web` next to its socket, mode 0600, and
replaces after each use; `runnerctl web` reads that file. The pages load only what the binary serves, under a
Content-Security-Policy that allows nothing else.

## Consequences

The dashboard needs no route of its own in the API and could run in another process later. On the socket it acts as
the daemon's user, so the session check is what stands between other local users and the API. Cookies are not
scoped by port: a page another local user serves on `127.0.0.1` and the owner opens would receive the cookie. The
token lasts one daemon run, which bounds that. Rejected: an API route that hands out the URL (it would add an API
type for one client), a token in the URL of every page (it leaks into history and logs).
