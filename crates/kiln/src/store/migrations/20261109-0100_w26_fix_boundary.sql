-- W26-AC10: a person's published words start the thread's fix count again, including when a
-- version and the comment share a whole-second timestamp. Keep the version already durable at
-- publication and its order in Ship's canonical mutation sequence. Existing comments stay NULL:
-- their order against same-second versions cannot be recovered, so readers keep the conservative
-- timestamp rule for them. Additive, so an older kiln can still read and publish comments.
ALTER TABLE review_comments ADD COLUMN fix_base_version INTEGER;
ALTER TABLE review_comments ADD COLUMN published_seq INTEGER;

CREATE TRIGGER review_comments_fix_boundary_ai AFTER INSERT ON review_comments
WHEN NEW.published = 1 BEGIN
    UPDATE ship_seq SET n = n + 1;
    UPDATE review_comments SET
        fix_base_version = COALESCE((SELECT MAX(v.n) FROM diff_versions v JOIN review_threads t ON t.diff_id = v.diff_id WHERE t.id = NEW.thread_id), 0),
        published_seq = (SELECT n FROM ship_seq)
    WHERE id = NEW.id;
END;

CREATE TRIGGER review_comments_fix_boundary_au AFTER UPDATE OF published ON review_comments
WHEN OLD.published = 0 AND NEW.published = 1 AND NEW.published_seq IS NULL BEGIN
    UPDATE ship_seq SET n = n + 1;
    UPDATE review_comments SET
        fix_base_version = COALESCE((SELECT MAX(v.n) FROM diff_versions v JOIN review_threads t ON t.diff_id = v.diff_id WHERE t.id = NEW.thread_id), 0),
        published_seq = (SELECT n FROM ship_seq)
    WHERE id = NEW.id;
END;

CREATE INDEX review_comments_publication ON review_comments (thread_id, published_seq);
