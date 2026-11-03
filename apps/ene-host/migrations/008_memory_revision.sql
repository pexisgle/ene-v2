-- A separate revision for memory changes keeps a consolidation batch from
-- failing only because the parent's current mood changed. It advances on
-- memory additions, corrections and forget/share changes, not on state-only
-- updates.
ALTER TABLE mind_companions ADD COLUMN memory_revision INTEGER NOT NULL DEFAULT 1;
