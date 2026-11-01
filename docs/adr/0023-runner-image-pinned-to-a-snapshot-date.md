# 0023. Runner image pinned to an Ubuntu snapshot date

Status: **Accepted**.

## Context

The base images were pinned by digest ([0009](0009-newest-stable-and-locked-dependencies.md)), but `apt-get install`
took whatever Ubuntu served that day, and every new Android layer downloaded whatever platform-tools Google served.
Two builds of the same commit could give jobs different tools.

## Decision

Everything in `image/Dockerfile` resolves to fixed content:

- Images (`dockerfile` syntax, `docker:dind`, `actions-runner`): by digest.
- Ubuntu packages: from `snapshot.ubuntu.com` at the date in `UBUNTU_SNAPSHOT`, written to apt's config in the
  `base` stage so every stage uses it. apt maps the base image's git-core PPA to its dated snapshot on
  `snapshot.ppa.launchpadcontent.net` too.
- Downloads (GitHub CLI, Android command-line tools and platform-tools): an exact version and its SHA-256, checked at
  build time. Platform-tools is in the image instead of being installed when a container starts; the image writes
  the SDK license file that installing it used to leave.

To bump, edit the `ARG`s at the top of each step in `image/Dockerfile`: the snapshot date for every Ubuntu package
at once, and a version with its SHA-256 for each download. Rebuild the stages and run the ignored Docker tests
(README, "Building").

## Consequences

Rebuilding a commit gives the same packages. Security updates arrive only with a new snapshot date. A bump changes
the JDK, browser libraries and other tools that jobs see in every profile (`jvm` and `node`), so it is its own
commit.
