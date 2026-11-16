# 0144. The mirror keeps no blob descriptor cache, and its storage is repaired before it starts

Status: **Accepted** (2026-11-16).

## Context

On 2026-11-16 the mirror answered `GET /v2/library/caddy/blobs/sha256:881fc932…` with 200 and no bytes, every time,
for about an hour, and every job built on that Alpine layer failed with
`short read: expected 3853673 bytes but got 0: unexpected EOF`. The blob's data was gone while four repositories
still linked to it. The host had rebooted a minute before the failures began, and the mirror had been started again.

distribution 3.1.2, the `registry` image the mirror runs, does this by design of its proxy
(`registry/proxy/proxyregistry.go`, `OnBlobExpire`). The proxy keeps one expiry per repository and blob. When one
fires it deletes that repository's link (`linkedBlobStore.Delete`, which first stats the blob through the cache) and
then the blob's data for every repository (`Vacuum.RemoveBlob`). Other repositories' links stay. The image's config
keeps a blob descriptor cache in memory (`storage.cache.blobdescriptor: inmemory`), which still holds the blob. A pull
through another repository then reads its link, finds the blob in the cache with its size, opens data that is not
there, and the file reader answers a missing file with no bytes (`registry/storage/filereader.go`). The response is
200 with the full `Content-Length` and an empty body. The cache only empties when the registry stops. Expiries
overdue at a start all fire as it starts (`scheduler.Start`), so a restart after a long stop sets this up at once.

Reproduced on `registry:3.1.2` with the same layer, shared by `library/caddy` and `library/golang`, and a 60 s TTL:

| Scenario | Default config (in-memory cache) | `REGISTRY_STORAGE_CACHE_BLOBDESCRIPTOR=` (no cache) |
|---|---|---|
| caddy's copy expires while it runs, then golang pulls | 200, 0 bytes, every time | 200, the whole blob, fetched again |
| caddy's copy is overdue when it starts, then golang pulls | 200, 0 bytes, every time | 200, the whole blob |

Without the cache the registry stats the data on disk, finds it missing, and fetches the blob again from Docker Hub.
A link whose data is missing then costs one fetch.

Blob data that is there but empty, as a crash can leave a file whose rename the disk kept and whose contents it did
not, is worse. With or without the cache, the registry serves it as an empty blob. A fetch after its link is removed
keeps the empty file, because `blobWriter.moveBlob` keeps data already at a blob's path. Reproduced: the first pull
streams the blob from Docker Hub, the second gets 0 bytes.

## Decision

- The mirror runs with `REGISTRY_STORAGE_CACHE_BLOBDESCRIPTOR` set empty, which the registry reads as an unknown
  cache and runs without one (it logs `unknown cache type …, caching disabled`). Each pull stats one small file more.
  runnerd removes the mirror whenever it goes idle and starts it again from `cache::mirror_body` for the next job, so
  a mirror an older runnerd started is replaced once its jobs finish.
- Before `cache::mirror_up` starts the mirror, `cache::mirror::repair` runs `mirror_links.sh repair` in a container
  of the mirror's image. That container mounts the mirror's volume, has no network, and removes every `_layers` and
  `_manifests` link whose blob data is missing or empty, along with the empty data. The digest of no bytes is
  rightly empty and stays. The daemon logs one line with what it removed. The engine waits for it, so it is bounded at 15 s; one that
  fails or times out is logged and the mirror starts anyway. A repair container a daemon restart left behind is
  removed first, and the repair reads every link again.
- No pass runs while the mirror runs. Without the cache, a missing blob is fetched again by itself, and empty data
  comes from a crash, which a start follows.
- `runnerd doctor` reads a running mirror's links with `mirror_links.sh check` through `docker exec`, changing
  nothing. It fails while a link answers with no data (empty data, or missing data under the cache) and gives the
  `docker exec … rm -f` of those paths, and `docker restart` when the cache is on. Past 8 paths, the fix is `docker rm -f` on the mirror, which runnerd
  starts again, repaired, after it next goes idle. It warns about a mirror that keeps
  the cache, and only notes missing data without it.

## Consequences

- The incident cannot recur from an expiry: the registry fetches the blob again instead of serving nothing, except
  for a pull that races the expiry itself. Without the cache the registry still stats the data, then opens it
  (`blobServer.ServeBlob`), and an expiry that deletes it between the two gives that one pull 200 with no bytes. The
  next pull fetches it again.
- Each mirror start runs one short container first, and the engine waits for it: 5000 links take about a second,
  container start included, and 100,000 about 8 seconds. A repair that fails logs how many faulty links it printed
  and its error, never every link.
- A repair container a killed daemon left mounts the mirror's volume; `cache::mirror_down` removes it with the
  mirror, so stopping, going idle and `runnerctl caches clean` can remove the volume.
- An upgrade of the `registry` image needs this checked again: a layout other than `docker/registry/v2` is not read,
  and the cache setting is read through the image's own config.
