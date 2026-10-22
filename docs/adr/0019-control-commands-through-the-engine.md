# 0019. Control commands go through the engine, a reload keeps running jobs

## Context

Clients need to pause a repository, cancel its running jobs, reseed a profile, remove a cache volume and replace the
config while the daemon runs. Each of these depends on what the slots hold at that moment: a pause is refused while
the repository runs a job, a volume is removed only when no runner mounts it. The engine owns that state and changes
it only between ticks.

## Decision

`Control` sends each such command on a channel with a reply; the engine answers it between ticks, against the state a
tick sees, and refuses with the jobs in the way (`api::Busy`, 409). The daemon validates a new config and sends it
with a commit that writes the file atomically; the engine runs the commit between ticks and runs with the config
before its next tick, or, if the file could not be written, changes nothing. A reload never stops a runner: removed
profiles and slots stay until their runners finish, and a profile starts nothing while it runs as many runners as its
new `slots`. A forced pause cancels the job's whole run on GitHub (the API has no per-job cancel), then waits for the
runner to report it cancelled and exit, removing it after 90 s if it does not.

## Consequences

A command waits for the tick in progress, a second or two while GitHub answers. Every client gets the same answers
the CLI does, and none of them talks to Docker or GitHub. Cancelling a run also cancels its jobs on other machines.
`history.database` takes effect only after a restart.
