-- Version 15: each repository's visibility as GitHub last gave it, and when (ms), for pricing its jobs at GitHub's
-- rates (ADR 0072). Rows stay when a repository leaves the config, as its jobs stay in the history.
CREATE TABLE repositories (
    name TEXT PRIMARY KEY NOT NULL,
    visibility TEXT NOT NULL,
    checked_at INTEGER NOT NULL
) WITHOUT ROWID;
