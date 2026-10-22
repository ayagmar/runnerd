-- Version 2: the caps each runner's container ran with, which a reload may change in its profile since. NULL in rows
-- from before: memory_cap in bytes, cpu_cap in millicores.
ALTER TABLE runners ADD COLUMN memory_cap INTEGER;
ALTER TABLE runners ADD COLUMN cpu_cap INTEGER;
