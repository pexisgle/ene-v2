-- The authored profile extras are separate from the lived mind.
CREATE TABLE mind_companions (
  companion_id TEXT PRIMARY KEY,
  profile_revision INTEGER NOT NULL,
  profile TEXT NOT NULL,
  state_revision INTEGER NOT NULL,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
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
  updated_at TEXT NOT NULL
);
CREATE INDEX mind_memories_owner ON mind_memories(owner, recorded_at DESC);
CREATE TABLE mind_operations (
  owner TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  response TEXT NOT NULL,
  revision INTEGER NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (owner, operation_id)
);
