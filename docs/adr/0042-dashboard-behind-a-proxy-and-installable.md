# 0042. The dashboard behind a proxy the owner runs, and installable as an app

Status: **Accepted** (2026-10-23). Amends [0020](0020-dashboard-is-an-api-client-on-loopback.md); builds the second
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
  network in the clear. Origins are kept as a browser sends them: lower case, without the default port. The daemon
  reads `[web]` when it starts, as it does `--web`; a saved config does not change it.
- The login stays one-time and per browser. The URL in `runnerd.web` names the loopback address;
  `runnerctl web --base <origin>` swaps it for a proxy's. Cookies are scoped by host, so a browser on the proxy's
  address gets its own session.
- A login through a proxy that sends `X-Forwarded-Proto: https` (`tailscale serve` does) gets a `Secure` cookie. The
  header only adds that attribute, so a forged one gains nothing. `SameSite=Strict` and `HttpOnly` stay.
- Every page links a web app manifest (`/assets/manifest.webmanifest`): name, `start_url` and `id` `/`,
  `display: standalone`, theme and background colour the mark's `#0a0a0a`, PNG icons of 192 and 512 px rendered
  from the mark, and a 512 px maskable one with the bars inside the safe zone. It and its icons are assets, served
  without a session, since browsers fetch a manifest without cookies. The Content-Security-Policy gains
  `manifest-src 'self'`.
- No service worker. Chrome dropped the requirement for a service worker with a `fetch` handler for installing from
  the menu in 108 on mobile and 112 on desktop
  ([Chrome's blog](https://developer.chrome.com/blog/update-install-criteria)); the remaining criteria are HTTPS,
  and a manifest with a name, 192 and 512 px icons, `start_url` and a standalone `display`
  ([web.dev](https://web.dev/articles/install-criteria)). `http://127.0.0.1` is a secure context
  ([MDN](https://developer.mozilla.org/en-US/docs/Web/Security/Secure_Contexts)). Only the automatic install
  prompt still needs a `fetch` handler, and live data must not be cached, so there is none.

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
  the proxy sets; a list the owner writes says which addresses they meant); a service worker for the install prompt (code to keep correct for one prompt).
