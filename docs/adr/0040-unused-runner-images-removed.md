# 0040. Runner images no profile runs are removed

Status: **Accepted** (2026-10-23).

## Context

A profile names its runner image by a tag: `ghcr.io/ayagmar/runnerd-runner:node-0.0.7`, or `runnerd-runner:node`
built on the host ([0025](0025-published-runner-images-named-by-reference.md), [0035](0035-runner-image-stages.md)).
An upgrade moves the profile to a new tag and runnerd pulls it, but nothing removed the old one, about 3.5 GB per
stage and version. Stages built on the host and renamed leftovers stayed too. `docker system prune -af` on a timer
would also delete the records of failed jobs ([0031](0031-records-of-failed-jobs.md)) and every image runnerd did
not make.

## Decision

- At start, after a reload and after a pull, the engine removes in the background every tag in a repository named
  `runnerd-runner`, whatever its registry and owner, that no profile of the config names, retired profiles included
  while they still run. Images in other repositories are never touched: records (`runnerd-record`), the mirror's
  `registry`, a custom image built from runnerd's and anything else on the host.
- A profile's reference is compared as Docker lists tags: `docker.io/` and `library/` dropped, `:latest` added when
  it has no tag. A reference with a digest keeps every tag of its repository, since Docker may list that image under
  any of them.
- It removes tags, without forcing. Docker refuses to remove the image of a container, running or stopped, so a
  runner that still runs a previous image keeps it; the next start, reload or pull tries again. A record keeps the
  layers it was committed from, which Docker frees with the record.
- No setting turns it off: `runnerd-runner` is runnerd's name for its images.

## Consequences

An upgraded host keeps one image per profile. A stage built on the host for a profile not yet in the config is
removed at the next start, reload or pull: build it, then change the config. A refused removal is logged as a
warning and not retried until the next trigger.
