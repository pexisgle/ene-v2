-- A long confirmed entry can be processed in chunks. last_offset is the
-- character offset already consolidated inside last_sequence; the sequence
-- itself is not marked processed until its remainder is handled.
ALTER TABLE mind_consolidation ADD COLUMN last_offset INTEGER NOT NULL DEFAULT 0;
