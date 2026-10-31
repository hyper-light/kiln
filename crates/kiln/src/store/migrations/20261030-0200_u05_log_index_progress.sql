-- kiln:epoch 3
-- The same derived index has bounded posting/pruning progress. Older whole-document writers
-- must not run while partial documents and packed gram blocks are present.
ALTER TABLE ci_log_grams RENAME TO ci_log_grams_legacy;
ALTER TABLE ci_log_terms RENAME TO ci_log_terms_legacy;
ALTER TABLE ci_log_search_ready RENAME TO ci_log_search_ready_legacy;
CREATE TABLE ci_log_grams (
    gram INTEGER NOT NULL,
    finished INTEGER NOT NULL,
    leg_order INTEGER NOT NULL,
    leg TEXT NOT NULL,
    block INTEGER NOT NULL,
    bits BLOB NOT NULL,
    PRIMARY KEY (gram, finished, leg_order, leg, block)
);
CREATE TABLE ci_log_terms (gram INTEGER PRIMARY KEY, documents INTEGER NOT NULL);
CREATE TABLE ci_log_search_ready (doc INTEGER PRIMARY KEY, version TEXT NOT NULL);
ALTER TABLE ci_log_search_pending ADD COLUMN after_gram INTEGER;
ALTER TABLE ci_log_search_pending ADD COLUMN bytes INTEGER;
ALTER TABLE ci_log_docs ADD COLUMN index_version TEXT;
ALTER TABLE ci_log_docs ADD COLUMN order_finished INTEGER;
ALTER TABLE ci_log_docs ADD COLUMN order_leg INTEGER;
ALTER TABLE ci_log_docs ADD COLUMN order_doc INTEGER;
ALTER TABLE ci_log_docs ADD COLUMN source_attempt INTEGER;
ALTER TABLE ci_log_indexed ADD COLUMN source_attempt INTEGER;
ALTER TABLE ci_log_indexed ADD COLUMN source_finished INTEGER;
ALTER TABLE ci_log_indexed ADD COLUMN source_order INTEGER;
-- Start the ordinal lookup empty, rather than CREATE INDEX scanning every historical body.
-- The same finite document stage/prune transaction owns these rows.
CREATE TABLE ci_log_doc_order (
    leg TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    doc INTEGER NOT NULL,
    version TEXT NOT NULL,
    PRIMARY KEY (leg, ordinal)
);
CREATE TABLE ci_log_doc_gram_pages (
    doc INTEGER NOT NULL,
    version TEXT NOT NULL,
    part INTEGER NOT NULL,
    grams BLOB NOT NULL,
    PRIMARY KEY (doc, version, part)
);
CREATE TABLE ci_log_index_progress (
    leg TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    step INTEGER NOT NULL DEFAULT 0,
    segment INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    first_row INTEGER,
    last_row INTEGER,
    pruning INTEGER NOT NULL DEFAULT 0,
    next_doc INTEGER NOT NULL DEFAULT 0,
    source_attempt INTEGER,
    source_finished INTEGER,
    source_order INTEGER,
    restart INTEGER NOT NULL DEFAULT 0,
    background INTEGER NOT NULL DEFAULT 0
);
-- No bulk posting deletion or bulk rebuild in this migration. The existing indexer drains
-- the retired row tables and legacy ready IDs in bounded transactions, alongside new work.
CREATE INDEX ci_log_index_progress_pruning ON ci_log_index_progress (pruning, background, leg);

-- Capture the historical frontier once. Fresh completion intents commit with their job,
-- independently of this bounded reconstruction scan (including backdated completions).
CREATE TABLE ci_log_index_backfill (
    id INTEGER PRIMARY KEY,
    after_finished INTEGER,
    after_order INTEGER,
    end_finished INTEGER,
    end_order INTEGER,
    done INTEGER NOT NULL DEFAULT 0
);
INSERT INTO ci_log_index_backfill (id,end_finished,end_order)
SELECT 1,finished_at,rowid FROM jobs WHERE finished_at IS NOT NULL ORDER BY finished_at DESC,rowid DESC LIMIT 1;
INSERT OR IGNORE INTO ci_log_index_backfill (id,done) VALUES (1,1);
