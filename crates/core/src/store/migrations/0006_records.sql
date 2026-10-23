-- Version 6: what the history keeps about a failed job's record (`api::RecordNote`): when it was made and its size,
-- or why none was made; when it was removed and why. Times in ms. Pruned with its job.
CREATE TABLE records (
    job INTEGER PRIMARY KEY NOT NULL,
    created_at INTEGER,
    bytes INTEGER,
    skipped TEXT,
    removed_at INTEGER,
    removed_by TEXT
);
