-- Version 9: each change auto sizing made or meant to make to a profile's `memory` and `slots` (`api::SizingChange`),
-- written before the config file is: 'pending' until the file is written ('done') or is not ('failed'). The
-- interval between changes counts from the last pending or done one. Never pruned: a few rows a day at most.
-- Times in ms.
CREATE TABLE auto_sizing (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile TEXT NOT NULL,
    at INTEGER NOT NULL,
    from_memory INTEGER NOT NULL,
    from_slots INTEGER NOT NULL,
    to_memory INTEGER NOT NULL,
    to_slots INTEGER NOT NULL,
    reason TEXT NOT NULL,
    state TEXT NOT NULL
);
CREATE INDEX auto_sizing_by_profile ON auto_sizing (profile, at);
