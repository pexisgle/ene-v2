-- Index work keeps its attempt count and a next-attempt time. After the
-- bounded attempts the row stays canonical-pending but is not retried until
-- a relevant configuration change resets it.
ALTER TABLE mind_index_pending ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0;
