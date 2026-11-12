CREATE TABLE settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE companions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    mind TEXT NOT NULL,
    body TEXT NOT NULL,
    voice TEXT NOT NULL,
    companion_connection TEXT NOT NULL,
    companion_model TEXT NOT NULL,
    task_connection TEXT NOT NULL,
    task_model TEXT NOT NULL,
    perception_connection TEXT NOT NULL,
    perception_model TEXT NOT NULL,
    compaction_connection TEXT NOT NULL,
    compaction_model TEXT NOT NULL,
    consolidation_connection TEXT NOT NULL,
    consolidation_model TEXT NOT NULL,
    decision_connection TEXT NOT NULL,
    decision_model TEXT NOT NULL,
    embedding_connection TEXT NOT NULL,
    embedding_model TEXT NOT NULL,
    profile TEXT NOT NULL,
    state TEXT NOT NULL,
    persona TEXT,
    voice_identifier TEXT,
    revision INTEGER NOT NULL,
    profile_revision INTEGER NOT NULL,
    state_revision INTEGER NOT NULL,
    memory_revision INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE activations (
    companion_id TEXT PRIMARY KEY REFERENCES companions(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    revision INTEGER NOT NULL
);

CREATE TABLE connections (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    adapter TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    options TEXT NOT NULL,
    credential_ref TEXT,
    credential_state TEXT NOT NULL,
    last_check TEXT NOT NULL,
    enabled INTEGER NOT NULL
);

CREATE TABLE policy_rules (
    id TEXT PRIMARY KEY,
    tool_name TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE approval_requests (
    request_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    action_payload_hash BLOB NOT NULL,
    tool_name TEXT NOT NULL,
    normalized_arguments TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX approval_requests_status ON approval_requests(status);

CREATE TABLE clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    platform TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    granted_capabilities TEXT NOT NULL,
    revision INTEGER NOT NULL,
    last_seen TEXT NOT NULL
);

CREATE TABLE work_sessions (
    id TEXT PRIMARY KEY,
    companion_id TEXT NOT NULL REFERENCES companions(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    reported_run TEXT,
    closed INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX work_sessions_companion ON work_sessions(companion_id);

CREATE TABLE device_operations (
    id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    companion_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    input TEXT NOT NULL,
    status TEXT NOT NULL,
    result TEXT,
    subscription_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX device_operations_client_status ON device_operations(client_id, status);

CREATE TABLE receipts (
    kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    request_key TEXT NOT NULL,
    hash TEXT NOT NULL,
    status TEXT NOT NULL,
    request_id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (kind, scope_id, request_key)
);

CREATE TABLE mind_operations (
    companion_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    hash TEXT NOT NULL,
    response TEXT NOT NULL,
    PRIMARY KEY (companion_id, operation_id)
);

CREATE TABLE memories (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    owner TEXT NOT NULL REFERENCES companions(id) ON DELETE CASCADE,
    audiences TEXT NOT NULL,
    content TEXT NOT NULL,
    description TEXT NOT NULL,
    appraisal TEXT NOT NULL,
    acquisition TEXT NOT NULL,
    importance INTEGER NOT NULL CHECK (importance >= 0 AND importance <= 10),
    occurred_at TEXT,
    recorded_at TEXT NOT NULL,
    last_accessed_at TEXT,
    memory_revision INTEGER NOT NULL,
    content_revision INTEGER NOT NULL,
    emotion TEXT,
    source_context TEXT NOT NULL,
    embedding BLOB,
    embedding_model TEXT,
    embedding_dimensions INTEGER,
    embedding_content_revision INTEGER,
    created_by TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX memories_owner_importance ON memories(owner, importance);
CREATE INDEX memories_owner_recorded_at ON memories(owner, recorded_at DESC);

CREATE VIRTUAL TABLE memories_fts USING fts5(
    content,
    description,
    appraisal,
    content = 'memories',
    content_rowid = 'rowid',
    tokenize = 'trigram'
);

CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, description, appraisal)
    VALUES (new.rowid, new.content, new.description, new.appraisal);
END;

CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, description, appraisal)
    VALUES ('delete', old.rowid, old.content, old.description, old.appraisal);
END;

CREATE TRIGGER memories_au AFTER UPDATE OF content, description, appraisal ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, description, appraisal)
    VALUES ('delete', old.rowid, old.content, old.description, old.appraisal);
    INSERT INTO memories_fts(rowid, content, description, appraisal)
    VALUES (new.rowid, new.content, new.description, new.appraisal);
END;

CREATE TABLE memory_edges (
    from_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    to_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (from_id, relation, to_id)
);

CREATE TABLE consolidation_state (
    companion_id TEXT PRIMARY KEY REFERENCES companions(id) ON DELETE CASCADE,
    last_sequence INTEGER NOT NULL,
    last_offset INTEGER NOT NULL,
    method_revision TEXT NOT NULL,
    state TEXT NOT NULL,
    reason TEXT,
    jobs INTEGER NOT NULL,
    model_calls INTEGER NOT NULL,
    input_chars INTEGER NOT NULL,
    skipped INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE schedules (
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
CREATE INDEX schedules_companion_enabled ON schedules(companion_id, enabled);

CREATE TABLE work_inquiries (
    id TEXT PRIMARY KEY,
    work_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    choices TEXT NOT NULL,
    created_at TEXT NOT NULL,
    answered_at TEXT
);
CREATE INDEX work_inquiries_work_open ON work_inquiries(work_id, answered_at);
