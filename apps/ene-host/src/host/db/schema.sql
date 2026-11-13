-- The host's relational state. The conversation itself is not here: it lives
-- in the rig session log (rig_memory::DirStore) and this file never mirrors it.
--
-- Baseline schema. Unknown newer user_version values are rejected at startup;
-- there is no in-place migration of older development databases.

PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;

-- Companions: the author's profile, the grown mind and the assigned models.
CREATE TABLE IF NOT EXISTS companions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- The immutable core of the persona (equivalent to MindProfile.core).
    mind TEXT NOT NULL,
    -- JSON: facets / examples / background / expression.
    profile TEXT NOT NULL,
    profile_revision INTEGER NOT NULL DEFAULT 1,
    -- JSON: emotion / relationships / tendencies / concerns.
    state TEXT NOT NULL,
    state_revision INTEGER NOT NULL DEFAULT 1,
    -- JSON: JsonDocument { schema, json }.
    body TEXT NOT NULL,
    voice TEXT NOT NULL,
    -- JSON: the seven ModelAssignment fields.
    assignments TEXT NOT NULL,
    persona TEXT,
    voice_identifier TEXT,
    revision INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS user_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    preferences TEXT NOT NULL DEFAULT '[]',
    static_constraints TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL
);

-- Episodic memory. The explicit INTEGER PRIMARY KEY is SQLite's rowid alias,
-- so the FTS5 external-content table below keeps a stable integer key.
CREATE TABLE IF NOT EXISTS memories (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL,
    audiences TEXT NOT NULL DEFAULT '[]',
    content TEXT NOT NULL,
    importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10),
    occurred_at TEXT,
    recorded_at TEXT NOT NULL,
    last_accessed_at TEXT,
    memory_revision INTEGER NOT NULL DEFAULT 1,
    content_revision INTEGER NOT NULL DEFAULT 1,
    emotion TEXT,
    source_context TEXT NOT NULL DEFAULT '{}',
    embedding BLOB,
    embedding_model TEXT,
    embedding_dimensions INTEGER,
    embedding_content_revision INTEGER
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    content,
    content='memories',
    content_rowid='rowid',
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content) VALUES('delete', old.rowid, old.content);
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TABLE IF NOT EXISTS memory_edges (
    from_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    to_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (from_id, relation, to_id)
);

-- Which memory ids the host injected into each turn: the citation guard reads
-- the verified injection set back after a restart (docs/design/mind.md §4.2).
CREATE TABLE IF NOT EXISTS memory_injections (
    companion_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    memory_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (companion_id, turn_id, memory_id)
);

CREATE TABLE IF NOT EXISTS consolidation_state (
    companion_id TEXT PRIMARY KEY,
    last_processed_turn_id TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Idempotency of mind_update: one row per tool operation.
CREATE TABLE IF NOT EXISTS mind_operations (
    operation_id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    request_hash BLOB NOT NULL,
    result TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schedules (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    cron_expr TEXT,
    run_at TEXT,
    title TEXT NOT NULL,
    instruction TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    last_triggered_at TEXT
);

-- Settings: one row per connection, kind and config kept apart so the kind can
-- be checked against the config oneof on every read.
CREATE TABLE IF NOT EXISTS connections (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL,
    config TEXT NOT NULL,
    credential_state TEXT NOT NULL DEFAULT 'absent',
    last_check TEXT,
    revision INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_rules (
    id TEXT PRIMARY KEY,
    tool_name TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Approval requests outlive the effect that asked, so a restart can expire
-- them instead of silently running the operation.
CREATE TABLE IF NOT EXISTS approval_requests (
    request_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    -- The work session of a work-originated approval; NULL for the head.
    work_id TEXT,
    action_payload_hash BLOB NOT NULL,
    tool_name TEXT NOT NULL,
    normalized_arguments TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    resolved_at TEXT,
    client_id TEXT,
    activation_revision INTEGER,
    capability TEXT
);

CREATE INDEX IF NOT EXISTS idx_approvals_status ON approval_requests(status);

-- Clients declare capabilities; the host records what it granted.
CREATE TABLE IF NOT EXISTS clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    platform TEXT NOT NULL DEFAULT '',
    capabilities TEXT NOT NULL DEFAULT '[]',
    granted_capabilities TEXT NOT NULL DEFAULT '[]',
    capability_specs TEXT NOT NULL DEFAULT '[]',
    revision INTEGER NOT NULL DEFAULT 1,
    last_seen TEXT NOT NULL
);

-- One activation row per companion: the active client and the revision that
-- binds audio, perception, approvals and device operations to a generation.
CREATE TABLE IF NOT EXISTS activations (
    companion_id TEXT PRIMARY KEY,
    client_id TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS work_sessions (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    client_id TEXT,
    activation_revision INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS work_runs (
    run_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    instruction TEXT NOT NULL,
    state TEXT NOT NULL,
    pending_request_id TEXT,
    failure TEXT,
    cancellation_requested_at TEXT,
    cancellation_reason TEXT,
    unverified_pids TEXT NOT NULL DEFAULT '[]',
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS work_inquiries (
    request_id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    choices TEXT NOT NULL DEFAULT '[]',
    answer TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_work ON work_runs(work_id, state);
CREATE INDEX IF NOT EXISTS idx_inquiries_work ON work_inquiries(work_id, resolved_at);

-- Device operations: the idempotency key, the bound client generation and the
-- terminal state the host can answer again after a reconnect.
CREATE TABLE IF NOT EXISTS device_operations (
    operation_id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    activation_revision INTEGER NOT NULL,
    capability TEXT NOT NULL,
    input TEXT NOT NULL,
    work_id TEXT,
    status TEXT NOT NULL,
    output TEXT,
    failure TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_operations_client ON device_operations(client_id, status);

CREATE INDEX IF NOT EXISTS idx_memories_owner_importance ON memories(owner, importance);
-- Recall lists one owner's recent page.
CREATE INDEX IF NOT EXISTS idx_memories_owner_rowid ON memories(owner, rowid);
-- The embedding backfill looks for valid vectors of one space.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_space
    ON memories(embedding_model, embedding_content_revision);
-- The vector index keeps one owner's key space, so it reads one owner only.
CREATE INDEX IF NOT EXISTS idx_memories_embedding_owner
    ON memories(owner, embedding_model, embedding_content_revision);
-- The citation guard reads the newest injection records of one companion.
CREATE INDEX IF NOT EXISTS idx_memory_injections_turn
    ON memory_injections(companion_id, turn_id);
-- `mind_read` walks both directions of the cognitive edges.
CREATE INDEX IF NOT EXISTS idx_memory_edges_to ON memory_edges(to_id, relation);
-- Consolidation context and receipts are read per companion.
CREATE INDEX IF NOT EXISTS idx_mind_operations_companion
    ON mind_operations(companion_id, created_at);
CREATE INDEX IF NOT EXISTS idx_schedules_companion_enabled ON schedules(companion_id, enabled);
