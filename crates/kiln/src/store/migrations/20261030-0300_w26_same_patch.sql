-- Whether a revision kiln replayed as its trunk moved (docs/ci.md W26-R16) has the same verbatim
-- patch-id as the revision it replays: its diff, context lines and whitespace included, unchanged
-- (Gerrit's trivial rebase). A clean replay is rebase-only either way, so approvals carry under the
-- sticky policy; a signed verdict (W27-R04) verifies only where this is true. Null for anything
-- else (a push, a fix, a change request's merge). Nullable: an older kiln never names it.
ALTER TABLE diff_versions ADD COLUMN same_patch INTEGER;
