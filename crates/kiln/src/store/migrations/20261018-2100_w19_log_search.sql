-- Log search (docs/ci.md W19-R04): finished legs' step logs in segments of 100 lines, with a
-- trigram index (FTS5) for any substring of three characters or more.
CREATE VIRTUAL TABLE IF NOT EXISTS ci_log_index USING fts5(text, leg UNINDEXED, step UNINDEXED, line UNINDEXED, tokenize = 'trigram');
-- The legs in it, with the rowids their segments took (so pruning removes a range).
CREATE TABLE IF NOT EXISTS ci_log_indexed (
    leg TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    first_row INTEGER,
    last_row INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_finished_any ON jobs (finished_at);
