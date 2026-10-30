-- A conflicted replay kept as a revision (docs/ci.md W26-R16, R17): when a change replayed as its
-- trunk moved conflicts, kiln keeps git's merge (its conflict markers in) as a new version of the
-- change, why `replayed with conflicts`, so the changes above it replay onto it and keep moving, as
-- jj's do. `kept_version` is that version's number, on the replay it keeps; the replay's conflicts
-- hold the gate until a resolution is committed. Nullable: an older kiln never names it, and a
-- replay W26-R12 made as a gate was read keeps none.
ALTER TABLE review_replays ADD COLUMN kept_version INTEGER;
