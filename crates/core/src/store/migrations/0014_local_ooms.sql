-- Version 14: `local_ooms` is how many times the container's own limit failed reclaim (`memory.events.local`
-- `oom`), the largest value sampled, NULL while never read. Unlike `ooms` it leaves out a limit a descendant cgroup,
-- such as a job's own service container, hit (ADR 0070).
ALTER TABLE runners ADD COLUMN local_ooms INTEGER;
