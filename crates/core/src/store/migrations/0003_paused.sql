-- Version 3: the repositories paused through the API, so a restart keeps them paused. A reload that removes a
-- repository from the config removes its row.
CREATE TABLE paused (
    repository TEXT PRIMARY KEY NOT NULL
);
