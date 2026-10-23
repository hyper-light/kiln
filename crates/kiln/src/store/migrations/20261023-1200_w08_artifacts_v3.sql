-- An artifact that v1 to v3 of the artifact actions upload (docs/ci.md W08-R08): its files come
-- one at a time into its container, and another upload by its name adds to it, as v3 had it.
-- v4's are immutable once finalized.
ALTER TABLE ci_artifacts ADD COLUMN v3 INTEGER NOT NULL DEFAULT 0;
