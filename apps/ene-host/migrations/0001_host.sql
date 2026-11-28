CREATE TABLE records (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    body TEXT NOT NULL CHECK (json_valid(body)),
    PRIMARY KEY (kind, id)
) STRICT, WITHOUT ROWID;

CREATE TABLE receipts (
    key TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    response TEXT NOT NULL CHECK (json_valid(response))
) STRICT, WITHOUT ROWID;

CREATE TABLE audit (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    time INTEGER NOT NULL,
    operation TEXT NOT NULL,
    decision TEXT NOT NULL,
    source TEXT NOT NULL
) STRICT;

CREATE TABLE memories (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    companion_id TEXT NOT NULL,
    shared INTEGER NOT NULL CHECK (shared IN (0, 1)),
    revision INTEGER NOT NULL,
    text TEXT NOT NULL,
    importance INTEGER NOT NULL CHECK (importance BETWEEN 0 AND 10),
    provenance TEXT NOT NULL,
    superseded_by TEXT,
    updated_at INTEGER NOT NULL,
    last_used INTEGER NOT NULL
) STRICT;

CREATE INDEX memories_scope ON memories(companion_id, importance, last_used);
CREATE INDEX memories_text ON memories(companion_id, text);

CREATE VIRTUAL TABLE memory_fts USING fts5(text, content='memories', content_rowid='rowid', tokenize='trigram');

INSERT INTO memory_fts(memory_fts, rank) VALUES ('secure-delete', 1);

CREATE TRIGGER memories_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memory_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER memories_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER memories_update AFTER UPDATE OF text ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    INSERT INTO memory_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TABLE skills (
    companion_id TEXT NOT NULL,
    name TEXT NOT NULL,
    overview TEXT NOT NULL,
    instructions TEXT NOT NULL,
    PRIMARY KEY (companion_id, name)
) STRICT, WITHOUT ROWID;

CREATE TABLE schedules (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    instruction TEXT NOT NULL,
    cron TEXT,
    zone TEXT NOT NULL,
    due INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE INDEX schedules_due ON schedules(due);

CREATE TABLE reflection_checkpoints (
    companion_id TEXT PRIMARY KEY,
    through INTEGER NOT NULL,
    summary TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE credentials (
    connection_id TEXT PRIMARY KEY,
    destination TEXT NOT NULL,
    slot TEXT NOT NULL
) STRICT, WITHOUT ROWID;
