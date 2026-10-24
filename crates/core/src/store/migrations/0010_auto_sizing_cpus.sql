-- Version 10: the `cpus` auto sizing changed a profile from and to (`api::Resize::cpus`); NULL in rows from before,
-- and in changes that left them.
ALTER TABLE auto_sizing ADD COLUMN from_cpus INTEGER;
ALTER TABLE auto_sizing ADD COLUMN to_cpus INTEGER;
