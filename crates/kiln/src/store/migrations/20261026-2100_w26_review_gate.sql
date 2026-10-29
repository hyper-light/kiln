-- The review records the gate needs (docs/ci.md W26-R08, R11): who resolved a thread and when, and
-- the reason given with it (an owner resolving someone else's blocking thread says why); and when a
-- comment's author deleted it. A thread's first comment deleted while replies remain stays where it
-- was, saying `deleted`; a thread left with no comment goes. Every column is nullable: an older kiln
-- never names them.
ALTER TABLE review_threads ADD COLUMN resolved_by TEXT;
ALTER TABLE review_threads ADD COLUMN resolved_at INTEGER;
ALTER TABLE review_threads ADD COLUMN resolution TEXT;
ALTER TABLE review_comments ADD COLUMN deleted_at INTEGER;
