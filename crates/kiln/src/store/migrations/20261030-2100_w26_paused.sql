-- Pause and Resume (docs/ci.md W26-R22): a job paused since `paused_at` (unix seconds), and how
-- long its earlier pauses were, all told (`paused_ms`). A waiting job paused isn't dispatched; a
-- running one is frozen where it runs, its time limit stopped while it is. Older builds never name
-- either column.
ALTER TABLE jobs ADD COLUMN paused_at INTEGER;
ALTER TABLE jobs ADD COLUMN paused_ms INTEGER NOT NULL DEFAULT 0;
