-- Version 11: what the notifier keeps per `[notify]` target (ADR 0051): `event`, the last durable event delivered;
-- `digest`, when the last digest went (ms); `insight:<key>`, when an insight told of was last seen to hold (ms).
CREATE TABLE notify (
    target TEXT NOT NULL,
    key TEXT NOT NULL,
    value INTEGER NOT NULL,
    PRIMARY KEY (target, key)
) WITHOUT ROWID;
