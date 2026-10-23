# 0025. Published runner images, named by reference

## Context

runnerd ran only `runnerd-runner:<image_stage>`, an image the user built from a clone of the repository. Releases
shipped binaries, so a new user had to clone and run `docker build` before the first job. The image is not a
separate product: its entrypoint, seed script and job hooks are the other half of the daemon's protocol with its
containers, so an image from one release can break a daemon from another.

## Decision

- Every `v*` tag publishes each stage of `image/Dockerfile` to `ghcr.io/ayagmar/runnerd-runner:<stage>-<version>`,
  built from the tag, for linux/amd64 (the Dockerfile downloads amd64 binaries), labelled
  `runnerd.version=<version>` (not only `org.opencontainers.image.version`, which every image on the runner base
  inherits from Ubuntu) and attested. `images.yml` does it; `release.yml` calls it before the
  GitHub release, and it can be run by hand for a tag that has no images. Its job may write packages and
  attestations and nothing else.
- A profile names its image with exactly one of two keys:
  - `image`: a reference runnerd pulls when Docker lacks it, in the background, before the profile's first runner.
    Until the pull is done the profile starts nothing; a failed pull is tried again later, longer each time.
  - `image_stage`: a stage built on this host as `runnerd-runner:<stage>`, never pulled. Existing configs keep
    working unchanged.
- `runnerd doctor` checks each image: present, or pullable (the registry has its manifest). It warns when a present
  image's version label is not the daemon's version.

We considered resolving `image_stage` to the published image of the daemon's own version. That would keep image and
daemon in step without a config edit, but it changes what existing configs run (a local build the owner changed would
be replaced by a pull), hides a network fetch behind a key that used to mean "built here", and leaves no way to run a
custom image or pin a digest.

## Consequences

A new user runs jobs with only the release binaries and a config. Upgrading the daemon means editing the tag in each
profile's `image`; doctor's version warning is the reminder. Any image works, including a user's own stage on top of
ours or a reference pinned by digest. aarch64 hosts still build their own image, once the Dockerfile supports them. A
new ghcr.io package is private until its owner makes it public, once.
