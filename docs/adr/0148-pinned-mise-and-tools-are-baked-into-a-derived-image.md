# 0148. A repository's pinned mise and tools are baked into an image derived from runnerd's

Status: **Accepted** (2026-11-20). Adds `image/mise-tools`, a recipe the owner runs; the daemon builds nothing. No
config or API change.

## Context

On 2026-11-17 twelve jobs of one repository failed downloading the 42 MB mise binary or the tools of its
`mise.lock`. Nine ran on a profile whose snapshot had just started over from the image
([0065](0065-a-snapshot-starts-over-from-the-image.md)), which empties the slots' tool cache
(`/home/runner/_work/_tool`), so every slot downloaded everything again. That cache is also writable by every job: a
tool a job changed there is what the next job of the slot runs, and `mise install --locked` does not rehash an
installed tool against the lock.

A host directory that runnerd fills and mounts into the runners was considered and rejected: it makes a repository's
config a build input of the daemon, its publication, removal and restart rules are a new store to get right, and the
tools would not be at the paths they were installed to.

## Decision

- `image/mise-tools/build.sh` builds an image on one of runnerd's runner images, named by digest
  (`<name>:<tag>@sha256:<digest>`), from a mise version, the SHA-256 of the extracted `mise` binary (what
  mise-action's `sha256` input checks) and a directory holding the repository's reviewed `mise.toml` and `mise.lock`.
  It never fetches a branch.
- The image adds `/opt/mise/bin/mise`, downloaded by exact version from mise's GitHub release and checked against the
  SHA-256 before anything runs it, and the tools of `mise.lock` in mise's system installs directory,
  `/usr/local/share/mise/installs`, installed as root with `mise install --system --locked` at the paths jobs run them
  from. Both are root's and read-only to `runner`; jobs have no sudo, and each container gets its own copy of the
  image, so nothing a job does to them reaches another job.
- Before anything is downloaded, the build refuses, naming the tool and why: a lock entry without a `linux-x64`
  checksum or URL; a backend other than aqua, github, http, and the core java, node and python plugins, which at mise
  v2026.10.18 check a download against the lock's checksum (`Backend::verify_checksum` makes one up when the lock has
  none, so a missing checksum would mean trusting the download); `[plugins]`, `[alias]`, a tool option other than
  `version` and `os` (a tool's `postinstall` among them), a `path:` or `ref:` version, and any table it does not
  know.
- mise installs from `mise.toml`'s `[tools]` only. The repository's hooks, tasks, `[env]` and settings never run as
  root while the image is built; a job still runs them as on any runner.
- The build proves its image before tagging it as done: as `runner`, with no network, a fresh empty `MISE_DATA_DIR`
  and the repository's own `mise.toml` and `mise.lock`, `mise install --locked` must install nothing, and every
  locked tool version must resolve under `/usr/local/share/mise/installs` and print its version through
  `mise exec`. An image that fails is removed.
- The image keeps its base's labels, `runnerd.version` among them, so `runnerd doctor` judges it as it judges the
  base. It adds `runnerd.mise.base`, `runnerd.mise.version`, `runnerd.mise.sha256`, `runnerd.mise.lock-sha256` and
  `runnerd.mise.toml-sha256`. Its tag ends in a hash of every input and of the recipe, so another lock makes another
  tag. It keeps `mise.toml` and `mise.lock` at `/opt/mise/share` and the helper
  `/opt/mise/libexec/runnerd-mise-inputs`, whose `match <mise.lock>` fails when a tool version both locks have
  differs between them.
- A profile runs it as any local image: `image = "<tag>"`. runnerd pulls an `image` only when Docker lacks it
  ([0025](0025-published-runner-images-named-by-reference.md)) and removes only tags of
  `ghcr.io/ayagmar/runnerd-runner` ([0040](0040-unused-runner-images-removed.md)); `build.sh` refuses that name.

## What a job must do

The image is a supply, not a switch: mise uses a system install only when it has none of its own. A job that wants
the guarantee:

- uses `/opt/mise/bin/mise` when its SHA-256 is the job's own pin, and otherwise downloads and verifies mise as
  before;
- points `MISE_DATA_DIR` at a fresh directory of its own, never at the slot's tool cache, so that no writable install
  a previous job left can shadow the image's;
- runs `runnerd-mise-inputs match mise.lock` and, when it fails, hides the system installs
  (`MISE_SYSTEM_INSTALLS_DIR` set to an empty directory), so a tool version locked to other bytes is installed fresh
  and verified;
- lets `mise install --locked` install, into that fresh directory, any tool version the image lacks, such as one a
  pull request bumps.

## Consequences

- A cold slot, or one that started over from the image, runs the locked tools with no download. A pull request that
  bumps a tool downloads that tool on every job, since no warm slot copy is kept for it: the price of never running a
  copy a job could have changed.
- The guarantee covers the image's bytes and the lock it was built from, not what a job does after it starts. A
  nested `container:` job does not see `/opt/mise` or `/usr/local/share/mise`.
- Each lock change needs a rebuild and a config edit to the new tag; each new runnerd release needs a rebuild on its
  base. The image is 1.6 GB larger than its base for a lock of 20 tool versions with a JDK, Node and two Pythons.
- Changing a profile's image does not by itself start its snapshot over; the slots keep their caches.
