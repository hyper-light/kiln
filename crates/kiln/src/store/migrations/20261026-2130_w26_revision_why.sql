-- Each revision says why it exists (docs/ci.md W26-R04), as kiln's own revisions do: `why` is NULL
-- for a push (`rebase_only` still says when only its base moved), `suggestion applied` for Apply's
-- revision (W26-R10) and `conflict resolved` for a resolution's (W26-R12); `thread` is the thread a
-- revision answers; a suggestion applied names its comment and which of the comment's suggestions it
-- was (from 0: its body's blocks in order, then its old `suggestion` field), so the block says which
-- revision applied it. Every column is nullable: an older kiln never names them, and every version
-- before this migration was a push. Ship's triggers on diff_versions note the change.
ALTER TABLE diff_versions ADD COLUMN why TEXT;
ALTER TABLE diff_versions ADD COLUMN thread TEXT;
ALTER TABLE diff_versions ADD COLUMN comment TEXT;
ALTER TABLE diff_versions ADD COLUMN suggestion INTEGER;
