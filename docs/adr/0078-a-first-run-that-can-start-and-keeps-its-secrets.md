# 0078. A first run that can start its first job and keeps its secrets

Status: **Accepted** (2026-10-30). Amends [0055](0055-admission-follows-the-hosts-memory.md) and
[0043](0043-phones-paired-by-qr-code-and-sessions-that-last.md).

## Context

`runnerd init` sized its runner to three quarters of the host's memory less the mirror. Admission also keeps a
margin of at least 2G free on the host and reserves the mirror's cap before the mirror starts (0055). On a 6G host
init wrote a 3.5G runner where admission left at most 2.6G, so the first job waited for the host's memory forever,
and auto sizing had no finished job to lower it from.

The config `init` wrote was mode 0644 in a directory of 0755. The dashboard later saves `[notify]` URLs and headers
into it, and a save kept the file's mode, so other local users could read them.

Neither `init` nor `doctor` said that a public repository lets anyone who opens a pull request run code on the host.
The website said it; the commands a newcomer runs did not.

Signing every device out replaced the session secret, but a live page opened before kept receiving updates, since
the cookie is checked only when a request starts.

## Decision

- `init` sizes a slot to no more than `admission::host::first_runner`: what the host's `MemAvailable` leaves the first
  runner past the margin, beside the mirror's cap, as the engine admits it into an empty slice, less 512M for what
  starts after `init` (the daemon, containerd unpacking the image, the mirror's shim). Without that headroom an 8G
  host got a runner with 61M to spare, which those take. `doctor` checks profiles with the same function, beside what
  the slice holds and the mirror's cap. The slice's budget
  stays three quarters of the host, or its `MemoryMax`, so later slots wait for the host's memory as any other start
  does. A host that leaves less than 2G for a runner, such as a 4G VM, gets no config and the reason.
- `init` writes the config 0600 in directories it makes 0700. Every save of the config through the API drops the
  group and other bits. `doctor` warns, with the `chmod`, when other users may read or write the config.
- `init` and `doctor` ask GitHub, through `gh`, for each configured repository's visibility and warn for a public
  one. They do not refuse: GitHub can require approval before fork pull requests run workflows, and the owner
  decides. The daemon does not ask or log it: `doctor`, which `GET /v1/doctor` also serves, is where the host's
  risks are listed.
- Signing every device out also ends every live page signed in with the old secret, with a notice, so a lost phone
  stops receiving logs and status at once.

## Consequences

- A starter config on a busy desktop gets a smaller runner than before when little memory is free at `init` time;
  auto sizing raises it as jobs need.
- A config made readable by others on purpose becomes 0600 at its next save.
- `doctor` runs one `gh api` request per repository, one after another, and nothing bounds a `gh` that hangs.
  `GET /v1/doctor` runs them too, so the dashboard's config page, which shows the checks, waits for them each time
  it opens. If that grows slow, the daemon can read the visibilities concurrently or keep them.
