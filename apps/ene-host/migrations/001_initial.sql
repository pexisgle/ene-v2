CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK(sequence > 0),
    status TEXT NOT NULL,
    body TEXT NOT NULL,
    UNIQUE(companion_id, sequence)
);
-- Acceptance records and messages will be committed in the same transaction.
CREATE TABLE receipts (
    principal TEXT NOT NULL,
    key TEXT NOT NULL,
    method TEXT NOT NULL,
    target TEXT NOT NULL,
    request TEXT NOT NULL,
    status INTEGER NOT NULL,
    response TEXT NOT NULL,
    PRIMARY KEY(principal, key)
);
