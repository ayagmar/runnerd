-- Version 13: `sock_throttled` is how many times the container's sockets were throttled for memory pressure
-- (`memory.events` `sock_throttled`), NULL while never read. `host_waited` is how long, in ms, a job waited because
-- the host's memory could not take its runner (ADR 0055), which sizing does not read as a wait for a slot.
ALTER TABLE runners ADD COLUMN sock_throttled INTEGER;
ALTER TABLE jobs ADD COLUMN host_waited INTEGER NOT NULL DEFAULT 0;
