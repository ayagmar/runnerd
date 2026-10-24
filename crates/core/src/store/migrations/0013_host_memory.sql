-- Version 13: `sock_throttled` is how many times the container's sockets were throttled for memory pressure
-- (`memory.events` `sock_throttled`), NULL while never read. `host_holds` are the spans measured admission held
-- starts back because the host was short of memory (ADR 0055), times in ms; `ended_at` is NULL while one lasts.
ALTER TABLE runners ADD COLUMN sock_throttled INTEGER;
CREATE TABLE host_holds (
    started_at INTEGER PRIMARY KEY,
    ended_at INTEGER
);
