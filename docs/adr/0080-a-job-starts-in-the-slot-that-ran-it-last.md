# 0080. A job starts in the slot that ran it last

Status: **Accepted** (2026-10-30). Amends [0007](0007-warm-caches-snapshot-plus-slot-layer.md).

## Context

Each slot keeps its own layer of `~/.cache`, the tool cache, Docker images and BuildKit's cache over the profile's
snapshot (0007). A job that starts in the slot that ran the same job last finds what that run cached; in another
slot it finds only the snapshot. 0052 puts the difference at about 40 s against 2 to 3.5 minutes for a Gradle job
before the shared build cache, and calls which one a job gets luck: the plan gave every job the lowest free slot.

## Decision

- **Which slot.** For each queued job, `plan` starts it in the free slot whose last job was the same job
  (repository, workflow and job name, as admission tells jobs apart), else a free slot whose last job was of the same
  repository, else the lowest free slot, as before. Only slots free at that moment count: a job never waits for the
  slot it prefers. Jobs are still served oldest first. An older job that matches no slot exactly leaves a free slot
  that a younger job of the profile matches exactly to that job while another slot is free; if none is, it takes it.
- **Where "last job" comes from.** The engine keeps the key of the job each slot's runner ran when the slot is vacated.
  A runner that ran a job nobody named leaves its slot unknown; one that ran none leaves it as it was. After a
  restart the keys are read from the history: the newest named job whose runner was in each slot, unless it ended
  before the profile's last reseed that replaced the snapshot or failed (the `cache.reseeded` events). No schema
  change.
- **When it ends.** A reseed that replaced the snapshot dropped every slot's own layer, so the slots of its profile
  forget their jobs; one that left the layers (`not seeded`) does not. A reseed that failed forgets them too, since
  it may have dropped some. A slot's volume removed by `runnerctl caches clean`, or with a closed slot, forgets its
  job.
- **Warm runners.** A warm runner is bound to its slot before its job is known, and GitHub gives it any queued job
  its labels match. Warm runners are unchanged: the plan picks their slots as before, and a job a warm runner takes
  runs wherever that runner is. Neither does a runner registered for one job get that job: affinity is a preference
  at start, not a promise.

## Consequences

- Repeated jobs of a workflow land on warm layers more often when the profile has several slots. With one slot, or
  after a reseed, nothing changes.
- A reseed happens whenever slot 1 ran a job and the profile is then idle with enough change, so on a quiet host
  affinity helps only between such moments; on a busy one, where the profile seldom idles, it helps most.
- Measuring it: a job's record in the API (`GET /jobs`) names the slot of its runner (`runner.slot`) and has its
  start and end. Group one job's runs (repository, workflow, name) by slot, and by whether the slot ran that job
  before; the run times of the warm starts against the cold ones are the hit rate's effect.
