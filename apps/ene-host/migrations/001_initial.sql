-- Consolidated baseline. Development databases created before this baseline
-- are rejected on startup and must be recreated; the host does not migrate
-- them.
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE companions (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE rpc_state (key TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE plugin_records (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY(namespace, key)
);
-- Acceptance records are committed with the state change they describe.
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
-- The authored profile extras are separate from the lived mind.
CREATE TABLE mind_companions (
  companion_id TEXT PRIMARY KEY,
  profile_revision INTEGER NOT NULL,
  profile TEXT NOT NULL,
  state_revision INTEGER NOT NULL,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- Advances on memory changes, not on state-only updates.
  memory_revision INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE mind_memories (
  id TEXT PRIMARY KEY,
  owner TEXT NOT NULL,
  revision INTEGER NOT NULL,
  content TEXT NOT NULL,
  description TEXT NOT NULL,
  appraisal TEXT NOT NULL,
  acquisition TEXT NOT NULL,
  sources TEXT NOT NULL,
  recorded_at TEXT NOT NULL,
  occurred_at TEXT,
  related TEXT NOT NULL,
  supersedes TEXT NOT NULL,
  recall_preference TEXT NOT NULL,
  created_by TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  -- A shared memory keeps its owner and private sources; audience only widens
  -- which companions may recall it.
  audience TEXT NOT NULL DEFAULT 'private'
);
CREATE INDEX mind_memories_owner ON mind_memories(owner, recorded_at DESC);
CREATE TABLE mind_operations (
  owner TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  response TEXT NOT NULL,
  revision INTEGER NOT NULL,
  updated_by TEXT NOT NULL,
  source TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (owner, operation_id)
);
CREATE INDEX mind_operations_request ON mind_operations(owner, request_hash);
-- Canonical changes queue index work. The memory transaction never depends
-- on FTS or an embedding provider succeeding.
CREATE TABLE mind_index_pending (
  memory_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  updated_at TEXT NOT NULL,
  -- After the bounded attempts the row stays pending but is not retried until
  -- a relevant configuration change resets it.
  next_attempt_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (memory_id, kind)
);
CREATE VIRTUAL TABLE mind_memories_fts USING fts5(
  memory_id UNINDEXED, content, description, appraisal, tokenize='trigram'
);
CREATE TABLE mind_vectors (
  memory_id TEXT PRIMARY KEY,
  model TEXT NOT NULL,
  dimension INTEGER NOT NULL,
  preprocess TEXT NOT NULL,
  vector BLOB NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE mind_index_status (
  companion_id TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  reason TEXT,
  embedding_model TEXT,
  embedding_calls INTEGER NOT NULL DEFAULT 0,
  embedding_tokens INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
CREATE TABLE mind_consolidation (
  companion_id TEXT PRIMARY KEY,
  last_sequence INTEGER NOT NULL DEFAULT 0,
  method_revision TEXT NOT NULL,
  state TEXT NOT NULL,
  reason TEXT,
  jobs INTEGER NOT NULL DEFAULT 0,
  model_calls INTEGER NOT NULL DEFAULT 0,
  input_chars INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL,
  -- Character offset already consolidated inside last_sequence.
  last_offset INTEGER NOT NULL DEFAULT 0,
  -- Terminal entries deliberately passed over, counted separately from jobs.
  skipped INTEGER NOT NULL DEFAULT 0
);
