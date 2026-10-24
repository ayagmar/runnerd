# 0042. The dashboard behind a proxy the owner runs, and installable as an app

Status: **Accepted** (2026-10-23), amended by [0043](0043-phones-paired-by-qr-code-and-sessions-that-last.md). Amends [0020](0020-dashboard-is-an-api-client-on-loopback.md); builds the second
piece of [0034](0034-desktop-framework.md).

## Context

The owner opens the dashboard from a phone and a laptop through `tailscale serve --https 8443
http://127.0.0.1:7477`. Pages loaded, but every write was 403: the session check took only the `Origin` of the
dashboard's loopback address, and the browser sends the proxy's, `https://<host>.ts.net:8443`. ADR 0034 ranked an
installable dashboard second, after failure notifications; the owner wants it now.

## Decision

The dashboard still listens on loopback only; a proxy in front of it is the owner's to run.

- `[web] origins` in `runnerd.toml` lists the proxies' origins. A write needs the session cookie and an `Origin`
  that is the dashboard's own or one of them. The config check refuses an origin with a path, query, user or a
  port that is not a number, and `http` for a host other than a loopback one, since the cookie would cross the
  network in the clear. Origins are kept as a browser sends them, as the URL standard serializes them: lower case,
  without the default port, an IPv6 address in its shortest form. An IPv4 address must be four decimal numbers:
  the standard also reads `2130706433` or `0x7f.1` as `127.0.0.1`, and those are refused rather than rewritten.
  The daemon reads `[web]` when it starts, as it does `--web`; a saved config does not change it.
- The login stays one-time and per browser. The URL in `runnerd.web` names the loopback address;
  `runnerctl web --base <origin>` swaps it for a proxy's. Cookies are scoped by host, so a browser on the proxy's
  address gets its own session.
- A login through a proxy that sends `X-Forwarded-Proto: https` (`tailscale serve` does) gets a `Secure` cookie. The
  header only adds that attribute, so a forged one gains nothing. `SameSite` (`Lax` since 0020's amendment) and `HttpOnly` stay.
- Every page links a web app manifest (`/assets/manifest.webmanifest`): name, `start_url` and `id` `/`,
  `display: standalone`, theme and background colour the mark's `#0a0a0a`, PNG icons of 192 and 512 px rendered
  from the mark, and a 512 px maskable one with the bars inside the safe zone. It and its icons are assets, served
  without a session, since browsers fetch a manifest without cookies. The Content-Security-Policy gains
  `manifest-src 'self'`.
- No service worker: live data must not be cached, and installing does not need one. Chrome dropped the
  requirement of a service worker with a `fetch` handler for installing from the menu in 108 on mobile and 112 on
  desktop ([Chrome's blog](https://developer.chrome.com/blog/update-install-criteria), 2023). Chrome's current
  criteria for offering the install itself (`beforeinstallprompt` and its own promotion) name no service worker
  either: the app is not installed yet, the user clicked or tapped the page once and spent 30 seconds on it, it is
  served over HTTPS, and its manifest has a `name` or `short_name`, 192 and 512 px icons, a `start_url`, a
  `display` of `standalone` or similar, and no `prefer_related_applications: true`
  ([web.dev](https://web.dev/articles/install-criteria), updated 2024-09-19). `http://127.0.0.1` is a secure
  context ([MDN](https://developer.mozilla.org/en-US/docs/Web/Security/Secure_Contexts)).

## Consequences

- Anyone who can reach the proxy (on Tailscale, the tailnet) can load the sign-in page; everything else still
  needs a session, which only a login link from `runnerd.web` gives. The proxy's TLS is what protects the cookie
  between devices. runnerd does not read Tailscale's identity headers.
- Cookies are not scoped by port: another service the owner exposes on the same host name would receive the
  session cookie, as on loopback (0020).
- Each address (loopback, each proxy) installs as its own app. The app has no offline view.
- Firefox installs web apps only on Windows; on Linux it stays a tab.
- Rejected: listening on a non-loopback address with TLS of runnerd's own (certificates to manage, and the proxy
  already does it); accepting any `Origin` that matches the request's `Host` (the check would then rest on a header
  the proxy sets; a list the owner writes says which addresses they meant).
