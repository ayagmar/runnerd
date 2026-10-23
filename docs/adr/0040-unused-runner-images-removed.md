# 0040. Published runner images no profile runs are removed

Status: **Accepted** (2026-10-23).

## Context

A profile names its runner image by a tag: `ghcr.io/ayagmar/runnerd-runner:node-0.0.7`, or `runnerd-runner:node`
built on the host ([0025](0025-published-runner-images-named-by-reference.md), [0035](0035-runner-image-stages.md)).
An upgrade moves the profile to a new tag and runnerd pulls it, but nothing removed the old one, about 3.5 GB per
stage and version. `docker system prune -af` on a timer would also delete the records of failed jobs
([0031](0031-records-of-failed-jobs.md)) and every image runnerd did not make.

## Decision

- runnerd removes only tags in the repository its releases publish to, `ghcr.io/ayagmar/runnerd-runner`
  (`config::PUBLISHED_RUNNER`). Those are images runnerd pulls, and it can pull them again. A local build, such as
  an `image_stage` or a fork's image, is never removed: runnerd cannot tell whether someone still needs it, and
  cannot rebuild it.
- It keeps every tag of an image that any profile names, retired profiles included while they still run, or that a
  pull is fetching, and every tag of an image a container was made from, running or stopped. A wanted reference
  Docker has is resolved to its image ID, whether it is a tag, a digest or an ID. Docker refuses to
  remove a container's image only when the tag is its last, so runnerd lists every container's image ID itself.
- A profile's reference is compared as Docker normalises it, as `distribution/reference` does: `index.docker.io`
  is `docker.io`, a Docker Hub name without a namespace is under `library/`, a reference with neither tag nor
  digest has the tag `latest`. A reference with a digest names the image Docker pulled by that digest, under any
  tag.
- The engine removes one tag at a time, at start, after a reload and after a pull, until none is left. It picks the
  next tag against the config and pulls of that moment, between the commands that change them, and removes it in
  the background, never forced. When the removal ends and the image is now wanted, because a reload asked for that
  image while it was being removed, by any tag, digest or ID, the engine notices: after every removal it checks
  again whether Docker has each image it had found, and pulls those it lost.
- A removal Docker refuses is logged and not retried until the next start, reload or pull.

## Consequences

An upgraded host that runs published images keeps one per profile. Images built on the host, older
`runnerd-runner:<stage>` tags among them, stay until someone removes them. A record keeps the layers it was
committed from, which Docker frees with the record.
