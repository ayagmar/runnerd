-- Version 5: `announced_profiles` holds each profile's latest `profile.*` event, as `announced` does for
-- repositories, so a restarted daemon announces a profile its config no longer has (`read::announced`).
CREATE TABLE announced_profiles (
    profile TEXT PRIMARY KEY NOT NULL,
    event INTEGER NOT NULL
) WITHOUT ROWID;
