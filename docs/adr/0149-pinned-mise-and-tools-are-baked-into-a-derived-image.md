# 0149. A repository's pinned mise and tools are baked into an image derived from runnerd's

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
  It never fetches a branch. It refuses a Docker Engine older than 28.3.0: BuildKit before it looks a base's digest
  up on the registry even when Docker holds the image, so a locally built base is not found.
- The version and SHA-256 must be a row of `image/mise-tools/tested-mise.txt`; today that is only mise 2026.10.18 at
  `e97c3897…6942`. Any other pair is refused before anything is downloaded. A new mise version joins the list only
  after `test.sh` passes on it, every wrong-digest build included, since each lock shape's refusal of wrong bytes is a
  behaviour of that mise version's code.
- The image adds `/opt/mise/bin/mise`, downloaded by exact version from mise's GitHub release and checked against the
  SHA-256 before anything runs it, and the tools of `mise.lock` in mise's system installs directory,
  `/usr/local/share/mise/installs`, installed as root with `mise install --system --locked` at the paths jobs run them
  from. Both are root's and read-only to `runner`; jobs have no sudo, and each container gets its own copy of the
  image, so nothing a job does to them reaches another job.
- The recipe accepts only the lock shapes it has tested at mise v2026.10.18, each with a build whose lock digest is
  wrong, which mise must refuse with a checksum mismatch naming the lock's digest (`test.sh`): `aqua:` and `github:`
  release assets of the backend's own repository, Node's linux-x64 build from nodejs.org with no `install` kind,
  Temurin JDKs from Adoptium's releases, and CPython 3.x.y from python-build-standalone. Every lock entry needs a full
  `sha256:` or `blake3:` digest and that exact URL: `Backend::verify_checksum` makes a digest up when the lock has
  none, and Node drops the lock's when the URL or install kind differs. Node's URL must equal, as a string,
  `https://nodejs.org/dist/v<version>/node-v<version>-linux-x64.tar.gz`; every other check matches the whole value and
  its type, so a trailing newline or a number where a string belongs is refused. `http:`, PyPy, any other backend, entry field
  or linux-x64 field is refused.
- Every `[tools]` request must be a literal version of a narrow grammar (no template, no `path:`, `ref:`, `latest`,
  `system`, no table or option) that selects exactly one lock entry, and every lock entry must be selected exactly
  once. `[plugins]`, `[alias]` and unknown tables are refused. All of this happens before mise is downloaded; each
  refusal names the tool and why.
- mise installs from `mise.toml`'s `[tools]` only, with a cleared environment, fresh data, cache, state and config
  directories, no hooks and `python.compile` off. The repository's hooks, tasks, `[env]` and settings never run while
  the image is built; a job runs them as on any runner.
- The image is built under a private candidate tag and proved by its image ID: as `runner`, with no network and no
  hooks, `mise install --locked` must install nothing, and for each `[tools]` request and the one lock entry the
  checker bound it to, mise must resolve the request to that entry's version directory under the system installs, and
  the program must resolve to a real path inside it, run with a literal argv and print exactly the locked version.
  Only then does the image get its release tag, which ends in a hash of every input and of the recipe; a release tag
  already on another image is never replaced. A failed build or proof removes only its candidate tag. `build.sh`
  prints `<tag>@sha256:<image ID>`.
- Promotion holds a lock per Docker daemon: a stopped container named `runnerd-mise-promote-<hash of the tag>`, which
  the daemon creates atomically for one caller. Two builds of one tag against one daemon, from any user, shell or
  temporary directory, never both tag; the second waits up to ten minutes, then refuses and names the container to
  remove if no build holds it. Builds against two daemons share nothing, as their images do not either.
- The image keeps its base's labels, `runnerd.version` among them, so `runnerd doctor` judges it as it judges the
  base. It adds `runnerd.mise.base`, `runnerd.mise.version`, `runnerd.mise.sha256`, `runnerd.mise.lock-sha256` and
  `runnerd.mise.toml-sha256`, and `/opt/mise/share/SHA256SUMS`, which names the SHA-256 of `mise.toml` and
  `mise.lock` relative to a checkout and of `/opt/mise/bin/mise`.
- A profile runs it as any local image, by the printed reference. runnerd pulls an `image` only when Docker lacks it
  ([0025](0025-published-runner-images-named-by-reference.md)) and removes only tags of
  `ghcr.io/ayagmar/runnerd-runner` ([0040](0040-unused-runner-images-removed.md)); `build.sh` refuses that name.

## What a job must do

The image is a supply, not a switch: mise uses a system install only when it has none of its own, and reads every
config file it discovers. A job that wants the guarantee, in every action or step that runs mise:

- refuses to start if the checkout holds a mise config besides `mise.toml` (`mise.local.toml`, `.mise.toml`,
  `mise.<env>.toml`, `mise/config.toml`, `.mise/config.toml`, `.config/mise.toml`, `.config/mise/`, `.tool-versions`),
  and restricts discovery to it: `MISE_OVERRIDE_CONFIG_FILENAMES=mise.toml`,
  `MISE_OVERRIDE_TOOL_VERSIONS_FILENAMES=none`, an empty `MISE_ENV`, and `MISE_CEILING_PATHS` at the checkout's parent;
- makes a fresh directory with `mktemp -d` and points `MISE_DATA_DIR`, `MISE_INSTALLS_DIR`, `MISE_CACHE_DIR`,
  `MISE_STATE_DIR`, `MISE_CONFIG_DIR`, `MISE_GLOBAL_CONFIG_FILE` and `MISE_SYSTEM_CONFIG_DIR` into it and
  `MISE_SHARED_INSTALL_DIRS` at nothing, never at the slot's tool cache or `~/.cache`, so that no writable install or
  config a previous job left is read;
- sets `MISE_SYSTEM_INSTALLS_DIR` explicitly on both branches. It is `/usr/local/share/mise/installs` only when
  `/opt/mise/bin/mise` and `SHA256SUMS` exist, `SHA256SUMS` names the job's own mise pin, and
  `sha256sum --check --strict` of it passes in the checkout: the whole reviewed config, lock and mise. Otherwise, the
  image missing included, it is an empty directory in the fresh one, and before anything is installed the job runs the
  image's checker (`/opt/mise/libexec/runnerd-mise-inputs check <checkout>`), or the recipe's `inputs.py` where the
  image has none, and fails if it refuses; then it downloads and verifies mise, and every tool installs fresh, verified
  by mise.lock;
- runs mise only by the path its setup exported.

## Consequences

- A cold slot, or one that started over from the image, runs the locked tools with no download. A change to
  `mise.toml`, `mise.lock` or the mise pin downloads every tool on every job until the image is rebuilt from it, since
  no warm slot copy is kept: the price of never running a copy a job could have changed, or one built from other
  inputs.
- The guarantee covers the image's bytes, which Docker keeps content-addressed under the image ID a profile names, and
  the inputs it was built from, not what a job does after it starts. Nothing rehashes the installed tree at job start.
  A nested `container:` job does not see `/opt/mise` or `/usr/local/share/mise`.
- A lock with another backend or shape is refused until the recipe adds it with its own wrong-digest test.
- Each lock change needs a rebuild and a config edit to the new reference; each new runnerd release needs a rebuild on
  its base. The image is 1.6 GB larger than its base for a lock of 20 tool versions with a JDK, Node and two Pythons.
- Changing a profile's image does not by itself start its snapshot over; the slots keep their caches.
