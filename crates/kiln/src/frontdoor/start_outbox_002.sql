-- W21-R15. A process owns its unsent reservations. A non-null target records that
-- delivery may have begun, together with the digest of its expected founding proof.
CREATE TABLE IF NOT EXISTS start_attempts (
    key TEXT NOT NULL, slot TEXT NOT NULL, actor TEXT NOT NULL, digest TEXT NOT NULL,
    process TEXT NOT NULL, target TEXT, receipt TEXT, agents INTEGER,
    PRIMARY KEY(key,slot)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS child_start_key ON children(key,slot);
CREATE INDEX IF NOT EXISTS start_unsent ON start_attempts(process) WHERE target IS NULL;
CREATE INDEX IF NOT EXISTS start_uncertain ON start_attempts(slot) WHERE target IS NOT NULL;
