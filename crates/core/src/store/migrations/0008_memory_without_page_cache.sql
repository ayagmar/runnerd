-- Version 8: a container's samples leave out all page cache on the file LRU lists, not only the inactive part.
-- `memory_measure` is 'without_page_cache' for runners started from this version on, so their peaks are told from
-- earlier ones (NULL). `ooms` is how many times the container's memory reached a limit with nothing left to reclaim
-- (`memory.events` `oom`), NULL while never read.
ALTER TABLE runners ADD COLUMN memory_measure TEXT;
ALTER TABLE runners ADD COLUMN ooms INTEGER;
