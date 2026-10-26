-- Version 15, for pricing jobs at GitHub's rates (ADR 0072).
-- Each repository's visibility as GitHub last gave it, and when (ms). Rows stay when a repository leaves the config,
-- as its jobs stay in the history.
CREATE TABLE repositories (
    name TEXT PRIMARY KEY NOT NULL,
    visibility TEXT NOT NULL,
    checked_at INTEGER NOT NULL
) WITHOUT ROWID;
-- One row: the history holds every final job that ended since `jobs_since` (ms). Pruning raises it to its cutoff
-- before it deletes job records. It starts when the database does, or, for a history from before, at its oldest
-- final job, which an earlier prune left as the first it kept.
CREATE TABLE history_kept (
    jobs_since INTEGER NOT NULL
);
INSERT INTO history_kept (jobs_since) VALUES (coalesce(
    (SELECT min(coalesce(finished_at, queued_at)) FROM jobs WHERE state IN ('completed', 'lost', 'elsewhere')),
    CAST(strftime('%s', 'now') AS INTEGER) * 1000
));
