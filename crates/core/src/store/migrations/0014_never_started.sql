-- Version 14: a job that left the queue without any runner taking it, as one a newer push cancelled, has no
-- `started_at`. GitHub gives such a job its creation time as its start, which versions before recorded; a job another
-- machine's runner took starts when that runner does, after it was queued.
UPDATE jobs SET started_at = NULL
WHERE state = 'elsewhere' AND conclusion IN ('cancelled', 'skipped') AND started_at = queued_at;
