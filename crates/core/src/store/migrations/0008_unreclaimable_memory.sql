-- Version 8: samples count what the kernel cannot reclaim, not page cache read twice. `memory_measure` is
-- 'unreclaimable' for runners started from this version on, so their peaks are told from earlier ones (NULL); `ooms`
-- is how many times the container's memory reached its cap with nothing left to reclaim (`memory.events` `oom`),
-- NULL when never read.
ALTER TABLE runners ADD COLUMN memory_measure TEXT;
ALTER TABLE runners ADD COLUMN ooms INTEGER;
