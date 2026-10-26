-- W27's identities (docs/ci.md W27-R01): every certificate kiln's CA issues for an actor (a
-- machine by its name, the rest by their kiln:// URI), and every revocation, with when it took
-- effect, why and by whom. A revocation names a certificate (its serial) or a key (the SHA-256 of
-- its SubjectPublicKeyInfo), both hex; the TLS directory refuses revoked keys, and the chain
-- checker (kiln_ci::identity::chain) refuses what was signed from `at` on.
CREATE TABLE IF NOT EXISTS identities (
    serial TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    key_sha256 TEXT NOT NULL,
    not_before INTEGER NOT NULL,
    not_after INTEGER NOT NULL,
    issued_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS identities_subject ON identities (subject);

CREATE TABLE IF NOT EXISTS revocations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    serial TEXT,
    key_sha256 TEXT,
    subject TEXT NOT NULL,
    at INTEGER NOT NULL,
    reason TEXT NOT NULL,
    by TEXT NOT NULL,
    CHECK (serial IS NOT NULL OR key_sha256 IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS revocations_key ON revocations (key_sha256);
