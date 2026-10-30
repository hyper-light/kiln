-- W05-R34, R36 (docs/ci.md, docs/needs.md §8): a grant's fence, the epoch it was made in and its
-- number there, kept beside its attempt and runner: on the leg (its grant now) and on each attempt.
-- What a runner writes under another fence is refused, and an attempt's end is taken only while
-- its grant is the leg's. Nullable: an older build never names them, and a leg it granted has none.
ALTER TABLE jobs ADD COLUMN grant_epoch INTEGER;
ALTER TABLE jobs ADD COLUMN grant_n INTEGER;
ALTER TABLE ci_attempts ADD COLUMN grant_epoch INTEGER;
ALTER TABLE ci_attempts ADD COLUMN grant_n INTEGER;
