-- Version 7: how each runner was admitted (`api::Admission` as JSON, NULL before), and how many times the kernel
-- killed a process in its container for memory (`memory.events` `oom_kill`, NULL before).
ALTER TABLE runners ADD COLUMN admission TEXT;
ALTER TABLE runners ADD COLUMN oom_kills INTEGER;
