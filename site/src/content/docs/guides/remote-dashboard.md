---
title: How do I open the dashboard on my phone?
description: Put the dashboard behind an HTTPS proxy such as tailscale serve, sign a phone in with a QR code, and install it as an app.
---

To open the dashboard on a phone or another computer, put an HTTPS proxy in front of it, such as `tailscale serve`:

```sh
tailscale serve --https 8443 http://127.0.0.1:7477
```

Add the proxy's address to [`[web] origins`](../../reference/configuration-more/#web) and restart runnerd, so writes from its pages pass the `Origin` check:

```toml
[web]
origins = ["https://forge.tail0b5e.ts.net:8443"]
open = "https://forge.tail0b5e.ts.net:8443"   # optional: runnerctl web opens this, not 127.0.0.1
```

`runnerctl web` now prints a link for each origin too. To sign a phone in, scan a QR code: **Show a QR code** under **Add a device** on the Config page, or `runnerctl web --qr`. A QR code works once, within 5 minutes; a link works once. `--base <origin>` prints a read-only link for a proxy not in `origins`, `--local` opens the loopback address once. [Security model](../../security/#the-dashboard) says what a proxy may write and which headers runnerd trusts.

## Install it as an app

Chromium browsers install the dashboard as an app: the install button in the address bar, or **Add to home screen** in Chrome on Android. It works through the proxy and on `http://127.0.0.1:7477`, one app per address. Firefox installs web apps on Windows only.
