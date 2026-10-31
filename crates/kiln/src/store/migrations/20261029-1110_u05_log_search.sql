-- kiln:epoch 2
-- U05/W19-R04: product SQL log search, without C SQLite virtual/shadow tables.
-- Legacy FTS rows may be translated into ci_log_docs by the read-only importer.
CREATE TABLE IF NOT EXISTS ci_log_indexed (
    leg TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    first_row INTEGER,
    last_row INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_finished_any ON jobs (finished_at);
CREATE TABLE IF NOT EXISTS ci_log_docs (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    leg TEXT NOT NULL,
    step INTEGER NOT NULL,
    line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ci_log_docs_leg ON ci_log_docs (leg, id);
CREATE TABLE IF NOT EXISTS ci_log_grams (
    gram INTEGER NOT NULL,
    doc INTEGER NOT NULL,
    PRIMARY KEY (gram, doc)
);
CREATE INDEX IF NOT EXISTS ci_log_grams_doc ON ci_log_grams (doc, gram);
CREATE TABLE IF NOT EXISTS ci_log_terms (
    gram INTEGER PRIMARY KEY,
    documents INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ci_log_search_ready (doc INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS ci_log_search_pending (doc INTEGER PRIMARY KEY);
INSERT OR IGNORE INTO ci_log_search_pending (doc)
SELECT id FROM ci_log_docs d WHERE NOT EXISTS (SELECT 1 FROM ci_log_search_ready r WHERE r.doc = d.id);
-- No imported document: re-index the canonical log file. Existing documents keep their ids;
-- their postings rebuild incrementally before new finished legs are indexed.
DELETE FROM ci_log_indexed WHERE NOT EXISTS (SELECT 1 FROM ci_log_docs d WHERE d.leg = ci_log_indexed.leg);

-- The importer may omit old completion markers; preserve imported document ordering and avoid
-- indexing their files a second time after postings catch up.
INSERT OR IGNORE INTO ci_log_indexed (leg, at, bytes, first_row, last_row)
SELECT leg, 0, SUM(length(CAST(text AS BLOB))), MIN(id), MAX(id) FROM ci_log_docs GROUP BY leg;
