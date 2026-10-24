-- Version 12: what the shared build cache served each runner and took from it (ADR 0052), and whether its job could
-- write to it (1) or only read (0); NULL for a runner without the build cache, such as every runner from before.
ALTER TABLE runners ADD COLUMN build_cache_hits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN build_cache_misses INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN build_cache_read_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN build_cache_written_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN build_cache_refused INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runners ADD COLUMN build_cache_writer INTEGER;
