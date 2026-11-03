-- Terminal entries that are deliberately passed over (failed or cancelled
-- assistant replies) are counted separately from model-processed jobs.
ALTER TABLE mind_consolidation ADD COLUMN skipped INTEGER NOT NULL DEFAULT 0;
