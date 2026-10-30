-- W27-R08's custody (docs/ci.md W27; the Transfers plan, kiln plan 01a17856b1b477eda14a574ae312a123):
-- every signed custody record and delegation kiln took (kiln_ci::identity::custody), by its
-- statement's digest (sha256: over its PAE), its envelope kept as it was signed; what each record
-- follows; the things each is about, by every digest of theirs; the breaks found in a chain, with
-- the records around them; and the statements and acts kiln refused, with why.
CREATE TABLE IF NOT EXISTS custody (
    digest TEXT PRIMARY KEY,
    payload_type TEXT NOT NULL,
    -- A record's kind (handed-on, received, passed-on, taken); none for a delegation.
    kind TEXT,
    -- Its signer's kiln:// URI.
    custodian TEXT NOT NULL,
    -- Whom its things went to (handed on, passed on, taken) or came from (received).
    counterpart TEXT,
    stream TEXT,
    seq INTEGER,
    -- The delegation its signer acts under, by its digest.
    under TEXT,
    -- The Sigstore bundle, as kiln took it.
    envelope TEXT NOT NULL,
    -- What it came over: a runner process, an API caller.
    came_from TEXT NOT NULL DEFAULT '',
    taken_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS custody_stream ON custody (custodian, stream, seq);

CREATE TABLE IF NOT EXISTS custody_follows (
    record TEXT NOT NULL,
    follows TEXT NOT NULL,
    PRIMARY KEY (record, follows)
);
CREATE INDEX IF NOT EXISTS custody_followed ON custody_follows (follows);

CREATE TABLE IF NOT EXISTS custody_things (
    record TEXT NOT NULL,
    name TEXT NOT NULL,
    algorithm TEXT NOT NULL,
    hex TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS custody_things_digest ON custody_things (algorithm, hex);

CREATE TABLE IF NOT EXISTS custody_breaks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The record whose hop doesn't hold.
    record TEXT NOT NULL,
    -- kiln_ci's Break::code: changed, not-the-taker, gap, …
    kind TEXT NOT NULL,
    detail TEXT NOT NULL,
    -- JSON: the records around it, by digest.
    around TEXT NOT NULL DEFAULT '[]',
    -- What was refused because of it, if anything.
    refused TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS custody_breaks_record ON custody_breaks (record);

CREATE TABLE IF NOT EXISTS transfer_refusals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The refused statement's digest, when it has one.
    digest TEXT NOT NULL,
    -- What it was: a custody record, a delegation, an act (goals.done, a run).
    act TEXT NOT NULL,
    -- Who signed it, as its certificate says, when kiln can tell.
    signer TEXT NOT NULL DEFAULT '',
    why TEXT NOT NULL,
    envelope TEXT NOT NULL DEFAULT '',
    came_from TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL
);
