-- A run of a Pipelines draft by its author's hand (docs/ci.md W29-R14): the draft's id
-- (ci_pipeline_drafts). Such a run reports no status to its change and deploys nothing. A
-- nullable column older builds never name.
ALTER TABLE runs ADD COLUMN draft TEXT;
