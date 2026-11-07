-- dbq store (2026-11-06): the audit log's search (`q`, docs/ci.md W19-R05), a trigram index (FTS5) of
-- each row's principal, on-behalf-of, action, target and detail, which this node keeps for itself
-- (store.rs's LOCAL) and builds from audit2 as its rows come (`crate::audit::keep_search`). A search of
-- text no row holds read every row: 23 ms at production's size, 245 ms at ten times it; with this,
-- 0.5 ms and 0.6 ms (an id, 1.4 ms and 2.4 ms). Contentless, a row's trigrams and not where they fall
-- (`detail = none`: a quarter of the size and of the time to build): 13 MB for production's 129k rows,
-- taken in by 65 turns of the writer, 0.95 s in all. A search ANDs some of its text's trigrams and reads
-- each row they find, which holds the text or doesn't.
CREATE VIRTUAL TABLE IF NOT EXISTS audit2_search USING fts5(principal, on_behalf_of, action, target, detail, content = '', contentless_delete = 1, detail = none, tokenize = 'trigram');
-- How far the search has come: every row of audit2 up to `newest` is in it, none below `oldest`.
CREATE TABLE IF NOT EXISTS audit2_indexed (
    one INTEGER PRIMARY KEY CHECK (one = 1),
    newest INTEGER NOT NULL,
    oldest INTEGER NOT NULL
);
INSERT OR IGNORE INTO audit2_indexed (one, newest, oldest) VALUES (1, 0, 0);
