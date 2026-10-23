-- Version 4: what pruning needs without scanning. `announced` holds each repository's latest `repository.*` event,
-- which pruning keeps and a restarted daemon reads (`read::announced_repositories`); pruning used to find them by
-- parsing every event's JSON. Samples are pruned by runner now, through their primary key, so `samples_by_at`, more
-- than half the table's size, goes.
CREATE TABLE announced (
    repository TEXT PRIMARY KEY NOT NULL,
    event INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO announced (repository, event)
    SELECT coalesce(json_extract(body, '$.repository.name'), json_extract(body, '$.repository')), max(id)
    FROM events WHERE json_extract(body, '$.type') LIKE 'repository.%' GROUP BY 1;
DROP INDEX samples_by_at;
