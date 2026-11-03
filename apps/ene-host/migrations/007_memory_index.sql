-- A shared memory keeps its owner and private sources; audience only widens
-- which companions may recall it.
ALTER TABLE mind_memories ADD COLUMN audience TEXT NOT NULL DEFAULT 'private';

-- Canonical changes queue index work. The memory transaction never depends
-- on FTS or an embedding provider succeeding.
CREATE TABLE mind_index_pending (
  memory_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  updated_at TEXT NOT NULL,
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
  updated_at TEXT NOT NULL
);
